//! OpenAI Codex CLI 롤아웃 세션 로그 수집기.
//!
//! 위치: `${CODEX_HOME|~/.codex}/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl[.zst]`
//! 롤아웃 파일이 source of truth 이므로 sqlite 인덱스는 읽지 않고 직접 스트리밍 파싱한다.
//!
//! 라인 스키마: `{"timestamp": "<RFC3339 UTC>", "type": "<kind>", "payload": {...}}`
//!   - session_meta : payload.id/session_id, cwd, git.branch
//!   - response_item: payload.type ∈ message | function_call | local_shell_call | custom_tool_call …
//!     message: {role, content:[{type:"input_text"|"output_text", text}]}
//!   - event_msg    : payload.type=token_count → info.total_token_usage(누적)
//!   - turn_context / compacted / world_state 등은 무시
//!
//! 세션은 [`Session`](crate::model::Session) 에 `agent = Codex` 로 담아 렌더·분석을 재사용한다.

use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDate, Utc};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::sync::LazyLock;

use super::{
    CollectContext, Collector, CollectorResult,
    claude::project_name,
    modified_since,
    qa::{QaAccumulator, truncate_chars},
};
use crate::{
    config::CodexConfig,
    model::{Agent, Session, SessionData},
    paths,
    time::{fmt_time, parse_iso},
};

/// 세션 시작에 '사용자 메시지'로 주입되는 합성 래퍼(사람 입력이 아님).
const WRAPPERS: [&str; 3] = [
    "<environment_context>",
    "<user_instructions>",
    "<INSTRUCTIONS>",
];
/// 셸/도구 실행 항목의 payload.type (버전별 명칭 차이 흡수).
const TOOL_CALL_TYPES: [&str; 3] = ["function_call", "local_shell_call", "custom_tool_call"];
/// 한 줄 길이 상한(거대한 tool 출력 라인 방어).
const LINE_CAP: usize = 2_000_000;
const COMMAND_MAX_CHARS: usize = 200;

/// apply_patch 패치 본문의 파일 경로 마커. rename 은 'Update File:'(원본) + 'Move to:'(대상).
static PATCH_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\*\*\* (?:Add|Update|Delete) File: (.+)$|^\*\*\* Move to: (.+)$")
        .expect("regex")
});

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    typ: String,
    payload: Option<Value>,
}

// --------------------------------------------------------------------------- //
// 파싱 헬퍼
// --------------------------------------------------------------------------- //

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// message payload 에서 사람의 '진짜' 프롬프트 텍스트만. 합성 래퍼는 제외.
pub(crate) fn real_codex_user_text(payload: &Value) -> Option<String> {
    let text = match payload.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|b| str_field(b, "type") == Some("input_text"))
            .filter_map(|b| str_field(b, "text"))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() || WRAPPERS.iter().any(|w| text.starts_with(w)) {
        return None;
    }
    Some(text.to_string())
}

/// assistant message payload 의 output_text 를 이어붙인 프로즈.
fn output_text(payload: &Value) -> String {
    match payload.get("content") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter(|b| str_field(b, "type") == Some("output_text"))
            .filter_map(|b| str_field(b, "text"))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// function_call.arguments(문자열 JSON 또는 객체) 또는 local_shell_call.action → 객체.
fn tool_args(payload: &Value) -> Option<Value> {
    match payload.get("arguments") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(Value::is_object),
        Some(v @ Value::Object(_)) => Some(v.clone()),
        _ => payload.get("action").filter(|a| a.is_object()).cloned(),
    }
}

/// 셸 계열 도구 호출의 실행 커맨드 전체 문자열(비절삭, best-effort).
fn command_text(payload: &Value) -> Option<String> {
    let args = tool_args(payload)?;
    match args.get("command")? {
        Value::Array(parts) => {
            let s = parts
                .iter()
                .map(|p| match p {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ");
            (!s.is_empty()).then_some(s)
        }
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        _ => None,
    }
}

/// 전용 apply_patch 툴의 패치 본문. 최신 freeform custom_tool_call 은 `input` 에,
/// 구버전 function_call 은 `arguments`(JSON 문자열/객체)에 담는다.
fn apply_patch_text(payload: &Value) -> String {
    if let Some(Value::String(s)) = payload.get("input")
        && !s.is_empty()
    {
        return s.clone();
    }
    let pick = |obj: &Value| {
        ["input", "patch", "content"]
            .iter()
            .find_map(|k| str_field(obj, k).map(str::to_string))
            .unwrap_or_default()
    };
    match payload.get("arguments") {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(obj) if obj.is_object() => pick(&obj),
            _ => s.clone(),
        },
        Some(obj @ Value::Object(_)) => pick(obj),
        _ => String::new(),
    }
}

/// apply_patch 패치 본문에서 수정/추가/삭제/이동 대상 파일 경로 추출(등장 순서, 중복 제거).
pub(crate) fn patch_files_from(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for cap in PATCH_FILE_RE.captures_iter(text) {
        let p = cap
            .get(1)
            .or_else(|| cap.get(2))
            .map(|m| m.as_str().trim())
            .unwrap_or("");
        if !p.is_empty() && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    }
    out
}

fn lenient_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

// --------------------------------------------------------------------------- //
// 수집기
// --------------------------------------------------------------------------- //

pub struct CodexCollector {
    cfg: CodexConfig,
}

impl CodexCollector {
    pub fn new(cfg: CodexConfig) -> Self {
        Self { cfg }
    }

    /// 세션 루트. 설정이 비었으면 `CODEX_HOME`(있으면) → `~/.codex` 아래 `sessions`.
    pub fn sessions_dir(&self) -> PathBuf {
        paths::dir_or(&self.cfg.sessions_dir, paths::codex_sessions_dir)
    }

    /// 롤아웃 파일을 줄 단위 리더로 연다(`.zst` 는 스트리밍 해제).
    fn open(path: &Path) -> io::Result<Box<dyn BufRead>> {
        let file = File::open(path)?;
        if path.extension().is_some_and(|e| e == "zst") {
            let dec = zstd::stream::read::Decoder::new(file)?;
            return Ok(Box::new(BufReader::new(dec)));
        }
        Ok(Box::new(BufReader::new(file)))
    }

    /// 그날 시작 이후 수정된 `rollout-*.jsonl[.zst]` (재귀).
    pub(crate) fn candidate_files(&self, root: &Path, since: &DateTime<Utc>) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if !name.starts_with("rollout-")
                || !(name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
            {
                continue;
            }
            let p = entry.into_path();
            if modified_since(&p, since) {
                out.push(p);
            }
        }
        out.sort();
        out
    }

    pub(crate) fn parse(
        &self,
        path: &Path,
        ctx: &CollectContext,
        warnings: &mut Vec<String>,
    ) -> io::Result<Option<Session>> {
        let mut reader = Self::open(path)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tz = ctx.tz();
        let target: NaiveDate = ctx.target_date();

        let mut s = Session {
            agent: Agent::Codex,
            ..Default::default()
        };
        let mut files_edited: BTreeSet<String> = BTreeSet::new();
        let mut qa = QaAccumulator::new(self.cfg.max_intent_len, self.cfg.max_answer_len);
        let mut matched = false;
        let mut last_total_out: Option<u64> = None;
        let hm = |ts: Option<&DateTime<Utc>>| ts.map(|t| fmt_time(Some(t), tz)).unwrap_or_default();

        let mut buf: Vec<u8> = Vec::new();
        let mut i = 0usize;
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            if i >= self.cfg.max_lines {
                warnings.push(format!(
                    "행 상한({}) 초과로 일부 생략: {name}",
                    self.cfg.max_lines
                ));
                break;
            }
            i += 1;
            if buf.len() > LINE_CAP {
                continue;
            }
            let text = String::from_utf8_lossy(&buf);
            let line = text.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(l) = serde_json::from_str::<Line>(line) else {
                continue;
            };
            let payload = l.payload.unwrap_or(Value::Null);
            let payload = if payload.is_object() {
                payload
            } else {
                Value::Object(Default::default())
            };

            // --- 날짜 무관 헤더 ---
            if l.typ == "session_meta" {
                if s.session_id.is_none() {
                    s.session_id = str_field(&payload, "session_id")
                        .or_else(|| str_field(&payload, "id"))
                        .map(str::to_string);
                }
                if s.cwd.is_none()
                    && let Some(cwd) = str_field(&payload, "cwd")
                    && !cwd.is_empty()
                {
                    s.cwd = Some(cwd.to_string());
                    s.project = Some(project_name(cwd));
                }
                if s.git_branch.is_none()
                    && let Some(b) = payload.get("git").and_then(|g| str_field(g, "branch"))
                    && !b.is_empty()
                {
                    s.git_branch = Some(b.to_string());
                }
                continue;
            }
            if l.typ == "turn_context" {
                continue;
            }

            let ts = l.timestamp.as_deref().and_then(parse_iso);
            let d = ts.map(|t| t.with_timezone(&tz).date_naive());
            let ptype = str_field(&payload, "type").unwrap_or("");
            let role = str_field(&payload, "role").unwrap_or("");

            // carry: 날짜 무관, 마지막 '진짜' 사용자 프롬프트
            if l.typ == "response_item"
                && ptype == "message"
                && role == "user"
                && let Some(txt) = real_codex_user_text(&payload)
            {
                qa.remember_carry(&hm(ts.as_ref()), &txt);
            }

            if d != Some(target) {
                continue;
            }
            matched = true;
            if let Some(t) = ts {
                if s.first_ts.is_none_or(|f| t < f) {
                    s.first_ts = Some(t);
                }
                if s.last_ts.is_none_or(|l| t > l) {
                    s.last_ts = Some(t);
                }
            }

            match l.typ.as_str() {
                "response_item" => {
                    if ptype == "message" {
                        match role {
                            "user" => {
                                if let Some(txt) = real_codex_user_text(&payload) {
                                    let q = qa.start_question(&hm(ts.as_ref()), &txt);
                                    if s.intent.is_none() {
                                        s.intent = Some(q);
                                    }
                                }
                            }
                            "assistant" => {
                                if let Some(q) = qa.open_from_carry_if_needed()
                                    && s.intent.is_none()
                                {
                                    s.intent = Some(q);
                                }
                                let t = output_text(&payload);
                                if !t.is_empty() {
                                    qa.push_answer(&t);
                                }
                            }
                            _ => {} // developer/system 은 intent 대상 아님
                        }
                    } else if TOOL_CALL_TYPES.contains(&ptype) {
                        let nm = str_field(&payload, "name")
                            .filter(|n| !n.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                if ptype == "local_shell_call" {
                                    "shell".into()
                                } else {
                                    "?".into()
                                }
                            });
                        *s.tool_counts.entry(nm.clone()).or_insert(0) += 1;
                        let cmd_full = command_text(&payload);
                        if let Some(c) = &cmd_full {
                            s.commands.push(truncate_chars(c, COMMAND_MAX_CHARS));
                        }
                        // 수정 파일 추출 — apply_patch 는 (1) 전용 툴, (2) 셸 경유 두 경로.
                        let patch = if nm == "apply_patch" {
                            apply_patch_text(&payload)
                        } else if let Some(c) = &cmd_full
                            && (c.contains("apply_patch") || c.contains("*** Begin Patch"))
                        {
                            c.clone()
                        } else {
                            String::new()
                        };
                        for f in patch_files_from(&patch) {
                            files_edited.insert(f);
                        }
                    }
                }
                "event_msg" => {
                    // total_token_usage 는 세션 시작부터의 '누적' 스냅샷 → 그날 마지막 값을 쓴다.
                    if ptype == "token_count"
                        && let Some(tot) =
                            payload.get("info").and_then(|i| i.get("total_token_usage"))
                        && let Some(v) = tot.get("output_tokens")
                        && let Some(n) = lenient_u64(v)
                    {
                        last_total_out = Some(n);
                    }
                }
                _ => {} // compacted / world_state / inter_agent_* 무시
            }
        }

        if !matched {
            return Ok(None);
        }
        let (turns, dropped) = qa.finish(self.cfg.max_qa_turns);
        s.qa = turns;
        s.qa_dropped = dropped;
        if let Some(n) = last_total_out {
            s.output_tokens = n;
        }
        s.files_edited = files_edited.into_iter().collect();
        Ok(Some(s))
    }
}

impl Collector for CodexCollector {
    type Data = SessionData;
    const NAME: &'static str = "codex";

    fn collect(&self, ctx: &CollectContext) -> CollectorResult<SessionData> {
        let root = self.sessions_dir();
        if !root.is_dir() {
            return CollectorResult::skip(
                Self::NAME,
                format!("Codex 세션 폴더가 없습니다: {}", root.display()),
            );
        }
        let since = ctx.day.start.with_timezone(&Utc);
        let mut sessions = Vec::new();
        let mut warnings = Vec::new();
        for path in self.candidate_files(&root, &since) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match self.parse(&path, ctx, &mut warnings) {
                Ok(Some(s)) => sessions.push(s),
                Ok(None) => {}
                Err(e) => warnings.push(format!("세션 파싱 실패({name}): {e}")),
            }
        }
        sessions.sort_by_key(|s| (s.first_ts.is_none(), s.first_ts));
        CollectorResult::ok_with_warnings(Self::NAME, SessionData { sessions }, warnings)
    }
}

/// zstd 압축 해제 스트림 (테스트·진단용 export).
pub fn decompress_zst(path: &Path) -> io::Result<String> {
    let mut dec = zstd::stream::read::Decoder::new(File::open(path)?)?;
    let mut s = String::new();
    dec.read_to_string(&mut s)?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{DayBounds, get_tz};
    use serde_json::json;
    use std::fs;

    fn ctx(y: i32, m: u32, d: u32) -> CollectContext {
        let tz = get_tz("Asia/Seoul");
        CollectContext::new(
            DayBounds::for_date(NaiveDate::from_ymd_opt(y, m, d).unwrap(), tz),
            "Asia/Seoul",
        )
    }

    fn meta(cwd: &str, branch: &str) -> Value {
        json!({"timestamp":"2026-07-10T00:12:03.101Z","type":"session_meta",
               "payload":{"id":"019c9c21-2a46-77c0-87d8-7cf3716a28e6","session_id":"019c9c21-2a46-77c0-87d8-7cf3716a28e6",
                          "timestamp":"2026-07-10T00:12:02.994Z","cwd":cwd,"originator":"codex_cli_rs",
                          "cli_version":"0.105.0","source":"cli","model_provider":"openai",
                          "git":{"commit_hash":"8130207a","branch":branch,"repository_url":"https://github.com/x/y.git"}}})
    }
    fn user(text: &str, ts: &str) -> Value {
        json!({"timestamp":ts,"type":"response_item",
               "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
    }
    fn assistant(text: &str, ts: &str) -> Value {
        json!({"timestamp":ts,"type":"response_item",
               "payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}})
    }
    fn call(name: &str, args: Value, ts: &str) -> Value {
        json!({"timestamp":ts,"type":"response_item",
               "payload":{"type":"function_call","name":name,"arguments":args.to_string(),"call_id":"call_1"}})
    }
    fn custom_call(name: &str, input: &str, ts: &str) -> Value {
        json!({"timestamp":ts,"type":"response_item",
               "payload":{"type":"custom_tool_call","name":name,"input":input,"call_id":"call_2"}})
    }
    fn local_shell(cmd: Vec<&str>, ts: &str) -> Value {
        json!({"timestamp":ts,"type":"response_item",
               "payload":{"type":"local_shell_call","action":{"type":"exec","command":cmd},"call_id":"c3"}})
    }
    fn fn_shell(cmd: Vec<&str>, ts: &str) -> Value {
        json!({"timestamp":ts,"type":"response_item",
               "payload":{"type":"function_call","name":"shell","arguments":json!({"command":cmd}).to_string(),"call_id":"c4"}})
    }
    fn tokens(out: u64, ts: &str) -> Value {
        json!({"timestamp":ts,"type":"event_msg",
               "payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":8123,"cached_input_tokens":4096,
                          "output_tokens":out,"reasoning_output_tokens":512,"total_tokens":10015},
                          "last_token_usage":{"output_tokens":320},"model_context_window":272000},"rate_limits":null}})
    }

    fn write_rollout(root: &Path, name: &str, recs: &[Value]) -> PathBuf {
        let d = root.join("2026").join("07").join("10");
        fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        let body: Vec<String> = recs.iter().map(|r| r.to_string()).collect();
        fs::write(&p, body.join("\n") + "\n").unwrap();
        p
    }

    fn collect(root: &Path, cfg: CodexConfig, c: &CollectContext) -> CollectorResult<SessionData> {
        CodexCollector::new(CodexConfig {
            sessions_dir: root.to_string_lossy().into_owned(),
            ..cfg
        })
        .collect(c)
    }

    #[test]
    fn parses_intent_files_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let recs = vec![
            meta("D:\\demo\\repo", "main"),
            user(
                "<environment_context>\n<cwd>D:\\demo\\repo</cwd>\n</environment_context>",
                "2026-07-10T00:12:03.300Z",
            ),
            user("로그인 버그 고쳐줘", "2026-07-10T00:12:20.512Z"),
            assistant("고치겠습니다", "2026-07-10T00:13:00.000Z"),
            call(
                "shell",
                json!({"command":["pytest","-q"]}),
                "2026-07-10T00:13:10.000Z",
            ),
            call(
                "apply_patch",
                json!({"input":"*** Begin Patch\n*** Update File: D:\\demo\\repo\\auth.py\n@@\n-x\n+y\n*** End Patch"}),
                "2026-07-10T00:13:20.000Z",
            ),
            tokens(1892, "2026-07-10T00:13:45.010Z"),
        ];
        write_rollout(&root, "rollout-2026-07-10T00-12-02-uuid.jsonl", &recs);
        let res = collect(&root, CodexConfig::default(), &ctx(2026, 7, 10));
        assert!(res.ok);
        let data = res.data.unwrap();
        assert_eq!(data.total_sessions(), 1);
        let s = &data.sessions[0];
        assert_eq!(s.agent, Agent::Codex);
        assert_eq!(s.intent.as_deref(), Some("로그인 버그 고쳐줘"));
        assert_eq!(s.project.as_deref(), Some("repo"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(
            s.session_id.as_deref(),
            Some("019c9c21-2a46-77c0-87d8-7cf3716a28e6")
        );
        assert_eq!(s.output_tokens, 1892);
        assert_eq!(s.tool_counts.get("shell"), Some(&1));
        assert_eq!(s.tool_counts.get("apply_patch"), Some(&1));
        assert!(s.commands.iter().any(|c| c.contains("pytest")));
        assert!(
            s.files_edited
                .contains(&"D:\\demo\\repo\\auth.py".to_string())
        );
        assert_eq!(s.qa[0].question, "로그인 버그 고쳐줘");
        assert!(s.qa[0].answer.contains("고치겠습니다"));
        assert_eq!(data.cwds(), vec!["D:\\demo\\repo"]);
    }

    #[test]
    fn apply_patch_freeform_shell_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let patch = "*** Begin Patch\n*** Update File: D:\\demo\\repo\\server.py\n@@\n-a\n+b\n*** Add File: D:\\demo\\repo\\new.py\n+print(1)\n*** Delete File: D:\\demo\\repo\\old.py\n*** End Patch";
        write_rollout(
            &root,
            "rollout-freeform.jsonl",
            &[
                meta("D:\\demo\\repo", "main"),
                user("세 파일 고쳐줘", "2026-07-10T03:00:00.000Z"),
                custom_call("apply_patch", patch, "2026-07-10T03:01:00.000Z"),
                tokens(100, "2026-07-10T03:02:00.000Z"),
            ],
        );
        let shell_patch =
            "*** Begin Patch\n*** Update File: src/app/main.py\n@@\n-a\n+b\n*** End Patch";
        let heredoc = format!("apply_patch <<'EOF'\n{shell_patch}\nEOF");
        write_rollout(
            &root,
            "rollout-shellpatch.jsonl",
            &[
                meta("D:\\demo\\repo2", "main"),
                user("셸로 패치", "2026-07-10T05:00:00.000Z"),
                local_shell(vec!["apply_patch", shell_patch], "2026-07-10T05:01:00.000Z"),
                fn_shell(vec!["apply_patch", shell_patch], "2026-07-10T05:02:00.000Z"),
                fn_shell(vec!["bash", "-lc", &heredoc], "2026-07-10T05:03:00.000Z"),
            ],
        );
        let rename = "*** Begin Patch\n*** Update File: src/old.py\n*** Move to: src/new.py\n@@\n-a\n+b\n*** End Patch";
        write_rollout(
            &root,
            "rollout-rename.jsonl",
            &[
                meta("D:\\demo\\repo3", "main"),
                user("리네임", "2026-07-10T06:00:00.000Z"),
                custom_call("apply_patch", rename, "2026-07-10T06:01:00.000Z"),
            ],
        );

        let data = collect(&root, CodexConfig::default(), &ctx(2026, 7, 10))
            .data
            .unwrap();
        assert_eq!(data.sessions.len(), 3);
        let by_proj = |p: &str| {
            data.sessions
                .iter()
                .find(|s| s.project.as_deref() == Some(p))
                .unwrap()
        };
        let s = by_proj("repo");
        assert_eq!(s.tool_counts.get("apply_patch"), Some(&1));
        for f in [
            "D:\\demo\\repo\\server.py",
            "D:\\demo\\repo\\new.py",
            "D:\\demo\\repo\\old.py",
        ] {
            assert!(s.files_edited.contains(&f.to_string()), "{f}");
        }
        let s = by_proj("repo2");
        assert!(s.files_edited.contains(&"src/app/main.py".to_string()));
        assert_eq!(s.tool_counts.get("shell"), Some(&3));
        let s = by_proj("repo3");
        assert!(s.files_edited.contains(&"src/new.py".to_string()));
        assert!(s.files_edited.contains(&"src/old.py".to_string()));
    }

    #[test]
    fn tokens_last_value_not_summed_and_missing_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_rollout(
            &root,
            "rollout-x.jsonl",
            &[
                meta("D:\\demo\\a", "main"),
                user("작업", "2026-07-10T00:10:00.000Z"),
                assistant("A", "2026-07-10T00:11:00.000Z"),
                tokens(500, "2026-07-10T00:11:01.000Z"),
                assistant("B", "2026-07-10T00:12:00.000Z"),
                tokens(1300, "2026-07-10T00:12:01.000Z"),
            ],
        );
        write_rollout(
            &root,
            "rollout-notok.jsonl",
            &[
                meta("D:\\demo\\b", "main"),
                user("토큰 라인 없음", "2026-07-10T07:00:00.000Z"),
                assistant("응", "2026-07-10T07:01:00.000Z"),
            ],
        );
        let data = collect(&root, CodexConfig::default(), &ctx(2026, 7, 10))
            .data
            .unwrap();
        let a = data
            .sessions
            .iter()
            .find(|s| s.project.as_deref() == Some("a"))
            .unwrap();
        let b = data
            .sessions
            .iter()
            .find(|s| s.project.as_deref() == Some("b"))
            .unwrap();
        assert_eq!(a.output_tokens, 1300);
        assert_eq!(b.output_tokens, 0);
        // 첫 활동 시각순 정렬
        assert_eq!(data.sessions[0].project.as_deref(), Some("a"));
    }

    #[test]
    fn other_day_ignored_midnight_carry_and_multiday() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_rollout(
            &root,
            "rollout-old.jsonl",
            &[
                meta("D:\\demo\\old", "main"),
                user("어제 일", "2026-07-09T02:00:00.000Z"),
                assistant("응", "2026-07-09T02:01:00.000Z"),
            ],
        );
        write_rollout(
            &root,
            "rollout-carry.jsonl",
            &[
                meta("D:\\demo\\carry", "main"),
                user("자정 넘기는 질문", "2026-07-09T14:55:00.000Z"),
                assistant("자정 후 답", "2026-07-09T15:05:00.000Z"),
            ],
        );
        write_rollout(
            &root,
            "rollout-multiday.jsonl",
            &[
                meta("D:\\demo\\multi", "main"),
                user("어제 질문", "2026-07-09T05:00:00.000Z"),
                assistant("어제 답", "2026-07-09T05:01:00.000Z"),
                user("오늘 질문", "2026-07-10T05:00:00.000Z"),
                assistant("오늘 답", "2026-07-10T05:01:00.000Z"),
            ],
        );
        let data = collect(&root, CodexConfig::default(), &ctx(2026, 7, 10))
            .data
            .unwrap();
        assert_eq!(data.sessions.len(), 2);
        let carry = data
            .sessions
            .iter()
            .find(|s| s.project.as_deref() == Some("carry"))
            .unwrap();
        assert_eq!(carry.intent.as_deref(), Some("자정 넘기는 질문"));
        assert!(carry.qa[0].answer.contains("자정 후 답"));
        let multi = data
            .sessions
            .iter()
            .find(|s| s.project.as_deref() == Some("multi"))
            .unwrap();
        assert_eq!(multi.intent.as_deref(), Some("오늘 질문"));
        assert_eq!(multi.qa.len(), 1);
        assert_eq!(multi.qa[0].question, "오늘 질문");
        assert!(!multi.qa[0].answer.contains("어제"));
    }

    #[test]
    fn missing_dir_skips_bad_lines_survive_and_zst() {
        let dir = tempfile::tempdir().unwrap();
        let res = collect(
            &dir.path().join("nope"),
            CodexConfig::default(),
            &ctx(2026, 7, 10),
        );
        assert!(res.skipped && res.data.is_none());

        let root = dir.path().join("sessions");
        let d = root.join("2026").join("07").join("10");
        fs::create_dir_all(&d).unwrap();
        let body = [
            meta("D:\\demo\\repo", "main").to_string(),
            "{ this is not json ".to_string(),
            user("정상 프롬프트", "2026-07-10T02:00:00.000Z").to_string(),
            assistant("답", "2026-07-10T02:01:00.000Z").to_string(),
        ]
        .join("\n");
        fs::write(d.join("rollout-broken.jsonl"), &body).unwrap();

        let zrecs = [
            meta("D:\\demo\\zrepo", "main"),
            user("압축 세션", "2026-07-10T01:00:00.000Z"),
            assistant("네", "2026-07-10T01:01:00.000Z"),
            tokens(50, "2026-07-10T01:02:00.000Z"),
        ];
        let raw = zrecs
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let compressed = zstd::stream::encode_all(raw.as_bytes(), 3).unwrap();
        fs::write(
            d.join("rollout-2026-07-10T01-00-00-uuid.jsonl.zst"),
            compressed,
        )
        .unwrap();
        // 이름 규칙에 안 맞는 파일은 무시
        fs::write(d.join("other.jsonl"), &body).unwrap();

        let data = collect(&root, CodexConfig::default(), &ctx(2026, 7, 10))
            .data
            .unwrap();
        assert_eq!(data.sessions.len(), 2);
        let broken = data
            .sessions
            .iter()
            .find(|s| s.project.as_deref() == Some("repo"))
            .unwrap();
        assert_eq!(broken.intent.as_deref(), Some("정상 프롬프트"));
        let z = data
            .sessions
            .iter()
            .find(|s| s.project.as_deref() == Some("zrepo"))
            .unwrap();
        assert_eq!(z.intent.as_deref(), Some("압축 세션"));
        assert_eq!(z.output_tokens, 50);
    }

    #[test]
    fn max_lines_cap_warns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_rollout(
            &root,
            "rollout-big.jsonl",
            &[
                meta("D:\\demo\\big", "main"),
                user("첫 줄", "2026-07-10T02:00:00.000Z"),
                assistant("답", "2026-07-10T02:01:00.000Z"),
                user("잘림", "2026-07-10T02:02:00.000Z"),
            ],
        );
        let res = collect(
            &root,
            CodexConfig {
                max_lines: 3,
                ..Default::default()
            },
            &ctx(2026, 7, 10),
        );
        assert!(res.warnings.iter().any(|w| w.contains("행 상한")));
        let s = &res.data.unwrap().sessions[0];
        assert_eq!(s.qa.len(), 1);
    }

    #[test]
    fn helpers() {
        assert_eq!(
            patch_files_from(
                "*** Begin Patch\n*** Update File: a.py\n*** Move to: b.py\n*** Add File: a.py\n*** End Patch"
            ),
            vec!["a.py", "b.py"]
        );
        assert!(patch_files_from("").is_empty());
        let p = json!({"type":"message","role":"user","content":"  <user_instructions>x"});
        assert_eq!(real_codex_user_text(&p), None);
        let p = json!({"type":"message","role":"user","content":"plain"});
        assert_eq!(real_codex_user_text(&p).as_deref(), Some("plain"));
        assert_eq!(
            command_text(&json!({"arguments":"{\"command\":\"ls -la\"}"})).as_deref(),
            Some("ls -la")
        );
        assert_eq!(command_text(&json!({"arguments":"not json"})), None);
        assert_eq!(
            command_text(&json!({"action":{"command":["a",1]}})).as_deref(),
            Some("a 1")
        );
        assert_eq!(apply_patch_text(&json!({"arguments":{"patch":"P"}})), "P");
        assert_eq!(
            apply_patch_text(&json!({"arguments":"raw text"})),
            "raw text"
        );
        assert_eq!(output_text(&json!({"content":"  x "})), "x");
    }
}
