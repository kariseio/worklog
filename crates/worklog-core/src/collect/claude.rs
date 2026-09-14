//! Claude Code 세션 로그 수집기.
//!
//! `~/.claude/projects/<sanitized>/<session-uuid>.jsonl` 을 읽어 그날 무슨 작업을 했는지
//! (세션 의도, 수정한 파일, 실행한 명령, 도구 사용량, 질답 흐름)를 뽑는다.
//!
//! 레코드 스키마 요약(실측):
//!   - type=user       : message.content = 문자열 또는 블록 리스트. tool_result 블록은 도구 출력이므로 제외.
//!   - type=assistant  : message.content = [text | tool_use ...], message.usage.output_tokens
//!   - type=ai-title   : aiTitle (세션 요약 한 줄)
//!   - 모든 user/assistant 레코드에 timestamp(UTC), cwd(실제 경로), gitBranch
//!   - type=file-history-snapshot : snapshot.trackedFileBackups 키 = 수정 대상 파일 경로들

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::{
    CollectContext, Collector, CollectorResult, modified_since,
    qa::{QaAccumulator, truncate_chars},
};
use crate::{
    config::ClaudeConfig,
    model::{Agent, Session, SessionData},
    paths,
    time::{fmt_time, parse_iso},
};

pub const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
pub const SHELL_TOOLS: [&str; 2] = ["Bash", "PowerShell"];
/// 시스템/슬래시 명령이 주입한 합성 user 레코드의 시작 문구 — 사람 입력이 아니다.
const SYNTHETIC_PREFIXES: [&str; 5] = [
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command-stdout>",
    "<system-reminder>",
];
/// 명령어 저장 최대 길이(글자).
const COMMAND_MAX_CHARS: usize = 200;

// --------------------------------------------------------------------------- //
// 레코드 (필요한 필드만 타입으로 — 나머지는 파싱 시 건너뛴다)
// --------------------------------------------------------------------------- //

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Rec {
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    pub git_branch: Option<String>,
    #[serde(rename = "aiTitle")]
    pub ai_title: Option<String>,
    pub timestamp: Option<String>,
    #[serde(rename = "isMeta")]
    pub is_meta: Option<bool>,
    pub message: Option<Message>,
    pub snapshot: Option<Snapshot>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Message {
    pub content: Option<Content>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Content {
    Text(String),
    Blocks(Vec<Block>),
    Other(serde::de::IgnoredAny),
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Block {
    #[serde(rename = "type")]
    pub typ: String,
    pub text: Option<String>,
    pub name: Option<String>,
    pub input: Option<ToolInput>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ToolInput {
    pub file_path: Option<String>,
    pub command: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Usage {
    pub output_tokens: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Snapshot {
    pub timestamp: Option<String>,
    #[serde(rename = "trackedFileBackups")]
    pub tracked_file_backups: Option<serde_json::Map<String, Value>>,
}

impl Rec {
    /// 레코드 시각(UTC). user/assistant 는 `timestamp`, 스냅샷은 `snapshot.timestamp`.
    fn any_timestamp(&self) -> Option<DateTime<Utc>> {
        self.timestamp
            .as_deref()
            .or_else(|| self.snapshot.as_ref().and_then(|s| s.timestamp.as_deref()))
            .and_then(parse_iso)
    }
}

/// 숫자/숫자문자열을 관용적으로 u64 로.
fn lenient_u64(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
            .unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// 명령 값(문자열 또는 JSON) → 표시 문자열(200자).
fn command_text(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Null => return None,
        other => other.to_string(),
    };
    let s = s.trim();
    (!s.is_empty()).then(|| truncate_chars(s, COMMAND_MAX_CHARS))
}

// --------------------------------------------------------------------------- //
// 파싱 규칙
// --------------------------------------------------------------------------- //

/// 사용자 '진짜' 프롬프트만. tool_result / 시스템 주입 프롬프트는 제외.
pub(crate) fn real_user_text(rec: &Rec) -> Option<String> {
    if rec.typ != "user" || rec.is_meta == Some(true) {
        return None;
    }
    let content = rec.message.as_ref()?.content.as_ref()?;
    let text = match content {
        Content::Text(s) => s.clone(),
        Content::Blocks(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.typ == "text")
                .map(|b| b.text.as_deref().unwrap_or(""))
                .collect();
            let has_tool_result = blocks.iter().any(|b| b.typ == "tool_result");
            if has_tool_result && !parts.iter().any(|p| !p.is_empty()) {
                return None; // 도구 출력이지 사람 입력이 아님
            }
            parts.join("\n")
        }
        Content::Other(_) => return None,
    };
    let text = text.trim();
    if text.is_empty() || SYNTHETIC_PREFIXES.iter().any(|p| text.starts_with(p)) {
        return None;
    }
    Some(text.to_string())
}

/// cwd → 표시용 프로젝트명. git worktree(`.claude/worktrees/<name>`)면 실제 저장소명으로.
pub fn project_name(cwd: &str) -> String {
    let norm = cwd.replace('\\', "/");
    let marker = "/.claude/worktrees/";
    let base = match norm.find(marker) {
        Some(i) => &norm[..i],
        None => norm.as_str(),
    };
    base.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| cwd.to_string())
}

/// jsonl 한 파일의 레코드들. 깨진 줄(JSON 오류·잘못된 UTF-8)은 건너뛴다.
pub(crate) fn parse_records(path: &Path) -> io::Result<Vec<Rec>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut recs = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<Rec>(line) {
            recs.push(rec);
        }
    }
    Ok(recs)
}

/// 레코드들 → 그날의 세션. 그날 활동이 하나도 없으면 None.
pub(crate) fn build_session(
    recs: &[Rec],
    ctx: &CollectContext,
    cfg: &ClaudeConfig,
) -> Option<Session> {
    let tz = ctx.tz();
    let target: NaiveDate = ctx.target_date();
    let local_date = |r: &Rec| r.any_timestamp().map(|d| d.with_timezone(&tz).date_naive());

    if !recs.iter().any(|r| local_date(r) == Some(target)) {
        return None;
    }

    let mut s = Session {
        agent: Agent::Claude,
        ..Default::default()
    };
    let mut files_edited: BTreeSet<String> = BTreeSet::new();
    let mut files_read: BTreeSet<String> = BTreeSet::new();
    let mut qa = QaAccumulator::new(cfg.max_intent_len, cfg.max_answer_len);
    let hm = |ts: Option<&DateTime<Utc>>| ts.map(|t| fmt_time(Some(t), tz)).unwrap_or_default();

    for r in recs {
        if s.session_id.is_none()
            && let Some(id) = &r.session_id
        {
            s.session_id = Some(id.clone());
        }
        if s.cwd.is_none()
            && let Some(cwd) = &r.cwd
        {
            s.cwd = Some(cwd.clone());
            s.project = Some(project_name(cwd));
        }
        if s.git_branch.is_none()
            && let Some(b) = &r.git_branch
        {
            s.git_branch = Some(b.clone());
        }
        if r.typ == "ai-title"
            && let Some(t) = &r.ai_title
            && !t.is_empty()
        {
            s.title = Some(t.clone());
        }

        // 날짜 무관: 마지막 사용자 프롬프트 기억(자정 연속 대비)
        if r.typ == "user"
            && let Some(txt) = real_user_text(r)
        {
            let ts = r.timestamp.as_deref().and_then(parse_iso);
            qa.remember_carry(&hm(ts.as_ref()), &txt);
        }

        if local_date(r) != Some(target) {
            continue;
        }

        let ts = r.timestamp.as_deref().and_then(parse_iso);
        if let Some(t) = ts {
            if s.first_ts.is_none_or(|f| t < f) {
                s.first_ts = Some(t);
            }
            if s.last_ts.is_none_or(|l| t > l) {
                s.last_ts = Some(t);
            }
        }

        match r.typ.as_str() {
            "user" => {
                if let Some(txt) = real_user_text(r) {
                    let q = qa.start_question(&hm(ts.as_ref()), &txt);
                    if s.intent.is_none() {
                        s.intent = Some(q);
                    }
                }
            }
            "assistant" => {
                // 전날 밤 프롬프트의 답이 자정을 넘겨 오늘 시작되는 경우, 그 질문을 이어붙임.
                if let Some(q) = qa.open_from_carry_if_needed()
                    && s.intent.is_none()
                {
                    s.intent = Some(q);
                }
                let Some(msg) = &r.message else { continue };
                if let Some(u) = &msg.usage
                    && let Some(v) = &u.output_tokens
                {
                    s.output_tokens += lenient_u64(v);
                }
                let Some(Content::Blocks(blocks)) = &msg.content else {
                    continue;
                };
                for b in blocks {
                    match b.typ.as_str() {
                        "text" => {
                            if let Some(t) = &b.text
                                && !t.is_empty()
                            {
                                qa.push_answer(t);
                            }
                        }
                        "tool_use" => {
                            let name = b.name.as_deref().unwrap_or("?");
                            *s.tool_counts.entry(name.to_string()).or_insert(0) += 1;
                            let Some(inp) = &b.input else { continue };
                            let file = inp.file_path.as_deref().filter(|f| !f.is_empty());
                            if EDIT_TOOLS.contains(&name) {
                                if let Some(f) = file {
                                    files_edited.insert(f.to_string());
                                }
                            } else if name == "Read" {
                                if let Some(f) = file {
                                    files_read.insert(f.to_string());
                                }
                            } else if SHELL_TOOLS.contains(&name)
                                && let Some(cmd) = inp.command.as_ref().and_then(command_text)
                            {
                                s.commands.push(cmd);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "file-history-snapshot" => {
                if let Some(snap) = &r.snapshot
                    && let Some(map) = &snap.tracked_file_backups
                {
                    for p in map.keys() {
                        files_edited.insert(p.clone());
                    }
                }
            }
            _ => {}
        }
    }

    let (turns, dropped) = qa.finish(cfg.max_qa_turns);
    s.qa = turns;
    s.qa_dropped = dropped;
    s.files_edited = files_edited.into_iter().collect();
    if cfg.include_read {
        s.files_read = files_read.into_iter().collect();
    }
    Some(s)
}

// --------------------------------------------------------------------------- //
// 수집기
// --------------------------------------------------------------------------- //

pub struct ClaudeCollector {
    cfg: ClaudeConfig,
}

impl ClaudeCollector {
    pub fn new(cfg: ClaudeConfig) -> Self {
        Self { cfg }
    }

    /// 세션 로그 루트. 설정이 비었으면 `CLAUDE_CONFIG_DIR`(있으면) → `~/.claude` 아래 `projects`.
    pub fn projects_dir(&self) -> PathBuf {
        paths::dir_or(&self.cfg.projects_dir, paths::claude_projects_dir)
    }

    /// 그날 시작 이후에 수정된 `<projects>/*/*.jsonl` 목록.
    fn candidate_files(&self, projects: &Path, since: &DateTime<Utc>) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(dirs) = fs::read_dir(projects) else {
            return out;
        };
        for d in dirs.flatten() {
            let dp = d.path();
            if !dp.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&dp) else {
                continue;
            };
            for f in files.flatten() {
                let fp = f.path();
                if fp.extension().is_some_and(|e| e == "jsonl") && modified_since(&fp, since) {
                    out.push(fp);
                }
            }
        }
        out.sort();
        out
    }
}

impl Collector for ClaudeCollector {
    type Data = SessionData;
    const NAME: &'static str = "claude";

    fn collect(&self, ctx: &CollectContext) -> CollectorResult<SessionData> {
        let projects = self.projects_dir();
        if !projects.is_dir() {
            return CollectorResult::skip(
                Self::NAME,
                format!("Claude 로그 폴더가 없습니다: {}", projects.display()),
            );
        }
        let since = ctx.day.start.with_timezone(&Utc);
        let mut sessions = Vec::new();
        let mut warnings = Vec::new();
        for path in self.candidate_files(&projects, &since) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match parse_records(&path) {
                Ok(recs) => {
                    if let Some(s) = build_session(&recs, ctx, &self.cfg) {
                        sessions.push(s);
                    }
                }
                Err(e) => warnings.push(format!("세션 파싱 실패({name}): {e}")),
            }
        }
        sessions.sort_by_key(|s| (s.first_ts.is_none(), s.first_ts));
        CollectorResult::ok_with_warnings(Self::NAME, SessionData { sessions }, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{DayBounds, get_tz};
    use serde_json::json;

    fn ctx(y: i32, m: u32, d: u32) -> CollectContext {
        let tz = get_tz("Asia/Seoul");
        CollectContext::new(
            DayBounds::for_date(NaiveDate::from_ymd_opt(y, m, d).unwrap(), tz),
            "Asia/Seoul",
        )
    }

    fn write_session(dir: &Path, name: &str, recs: &[Value]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        let body: Vec<String> = recs.iter().map(|r| r.to_string()).collect();
        fs::write(&p, body.join("\n") + "\n").unwrap();
        p
    }

    fn rec(v: Value) -> Rec {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn extracts_intent_files_and_tools() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        // 2026-07-06 02:00Z == 11:00 KST
        let recs = vec![
            json!({"type":"user","sessionId":"s1","cwd":"D:\\demo\\repo","gitBranch":"main",
                   "timestamp":"2026-07-06T02:00:00.000Z",
                   "message":{"role":"user","content":"로그인 버그 고쳐줘"}}),
            json!({"type":"assistant","sessionId":"s1","cwd":"D:\\demo\\repo",
                   "timestamp":"2026-07-06T02:01:00.000Z",
                   "message":{"role":"assistant","model":"claude-opus-4-8","usage":{"output_tokens":123},
                              "content":[{"type":"text","text":"고치겠습니다"},
                                         {"type":"tool_use","name":"Edit","input":{"file_path":"D:\\demo\\repo\\auth.py"}},
                                         {"type":"tool_use","name":"Bash","input":{"command":"pytest -q"}}]}}),
            json!({"type":"user","cwd":"D:\\demo\\repo","timestamp":"2026-07-06T02:01:05.000Z",
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"1 passed"}]}}),
            json!({"type":"ai-title","aiTitle":"로그인 버그 수정","sessionId":"s1"}),
        ];
        write_session(&projects.join("D--demo-repo"), "sess-1.jsonl", &recs);

        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 7, 6));
        assert!(res.ok && !res.skipped);
        let data = res.data.unwrap();
        assert_eq!(data.total_sessions(), 1);
        let s = &data.sessions[0];
        assert_eq!(s.intent.as_deref(), Some("로그인 버그 고쳐줘"));
        assert_eq!(s.title.as_deref(), Some("로그인 버그 수정"));
        assert_eq!(s.project.as_deref(), Some("repo"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.session_id.as_deref(), Some("s1"));
        assert_eq!(s.output_tokens, 123);
        assert!(
            s.files_edited
                .contains(&"D:\\demo\\repo\\auth.py".to_string())
        );
        assert_eq!(s.tool_counts.get("Edit"), Some(&1));
        assert_eq!(s.tool_counts.get("Bash"), Some(&1));
        assert!(s.commands.iter().any(|c| c.contains("pytest")));
        assert_eq!(data.cwds(), vec!["D:\\demo\\repo"]);
        assert_eq!(s.qa.len(), 1);
        assert_eq!(s.qa[0].time, "11:00");
        assert_eq!(s.qa[0].answer, "고치겠습니다");
        assert_eq!(s.first_ts, parse_iso("2026-07-06T02:00:00Z"));
        assert_eq!(s.last_ts, parse_iso("2026-07-06T02:01:05Z"));
    }

    #[test]
    fn other_day_session_ignored_and_missing_dir_skips() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let recs = vec![
            json!({"type":"user","cwd":"D:\\demo\\repo","timestamp":"2026-07-01T02:00:00.000Z",
                               "message":{"role":"user","content":"어제 일"}}),
        ];
        write_session(&projects.join("D--demo-repo"), "sess-1.jsonl", &recs);
        let cfg = ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let res = ClaudeCollector::new(cfg).collect(&ctx(2026, 7, 6));
        assert_eq!(res.data.unwrap().total_sessions(), 0);

        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: dir.path().join("nope").to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 7, 6));
        assert!(res.skipped);
        assert!(res.data.is_none());
    }

    #[test]
    fn qa_capture_multiple_topics() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let recs = vec![
            json!({"type":"user","sessionId":"s1","cwd":"/home/u/proj","timestamp":"2026-07-08T01:00:00Z",
                   "message":{"content":"첫 요청 내용"}}),
            json!({"type":"assistant","timestamp":"2026-07-08T01:00:05Z",
                   "message":{"content":[{"type":"text","text":"첫 응답 요지"},
                                         {"type":"tool_use","name":"Edit","input":{"file_path":"/home/u/proj/a.py"}}],
                              "usage":{"output_tokens":10}}}),
            json!({"type":"user","sessionId":"s1","cwd":"/home/u/proj","timestamp":"2026-07-08T02:00:00Z",
                   "message":{"content":"둘째 다른 주제"}}),
            json!({"type":"assistant","timestamp":"2026-07-08T02:00:05Z",
                   "message":{"content":[{"type":"text","text":"둘째 응답"}],"usage":{"output_tokens":5}}}),
            json!({"type":"user","timestamp":"2026-07-08T02:00:10Z",
                   "message":{"content":[{"type":"tool_result","content":"출력"}]}}),
        ];
        write_session(&projects.join("-home-u-proj"), "sess.jsonl", &recs);
        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 7, 8));
        let data = res.data.unwrap();
        assert_eq!(data.sessions.len(), 1);
        let s = &data.sessions[0];
        assert_eq!(s.qa.len(), 2);
        assert_eq!(s.qa[0].question, "첫 요청 내용");
        assert!(s.qa[0].answer.contains("첫 응답 요지"));
        assert_eq!(s.qa[0].time, "10:00");
        assert_eq!(s.qa[1].question, "둘째 다른 주제");
        assert_eq!(s.project.as_deref(), Some("proj"));
        assert_eq!(s.output_tokens, 15);
    }

    #[test]
    fn qa_cap_keeps_recent_and_intent_is_first() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let mut recs = Vec::new();
        for i in 0..5 {
            recs.push(json!({"type":"user","sessionId":"s1","cwd":"/home/u/proj",
                             "timestamp":format!("2026-07-08T0{}:00:00Z", i + 1),
                             "message":{"content":format!("주제{i}")}}));
            recs.push(json!({"type":"assistant","timestamp":format!("2026-07-08T0{}:00:05Z", i + 1),
                             "message":{"content":[{"type":"text","text":format!("답{i}")}],"usage":{"output_tokens":1}}}));
        }
        write_session(&projects.join("-home-u-proj"), "sess.jsonl", &recs);
        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            max_qa_turns: 3,
            ..Default::default()
        })
        .collect(&ctx(2026, 7, 8));
        let s = &res.data.unwrap().sessions[0];
        assert_eq!(s.qa.len(), 3);
        assert_eq!(s.qa_dropped, 2);
        assert_eq!(
            s.qa.iter().map(|t| t.question.as_str()).collect::<Vec<_>>(),
            vec!["주제2", "주제3", "주제4"]
        );
        assert_eq!(s.intent.as_deref(), Some("주제0"));
    }

    #[test]
    fn midnight_carry_and_multiday_only_today() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let recs = vec![
            json!({"type":"user","sessionId":"s1","cwd":"/p","timestamp":"2026-07-09T14:55:00Z",
                   "message":{"content":"자정 넘기는 질문"}}), // KST 07-09 23:55
            json!({"type":"assistant","timestamp":"2026-07-09T15:05:00Z",
                   "message":{"content":[{"type":"text","text":"자정 후 답"}],"usage":{"output_tokens":1}}}), // KST 07-10 00:05
            json!({"type":"user","sessionId":"s1","cwd":"/p","timestamp":"2026-07-10T05:00:00Z",
                   "message":{"content":"오늘 질문"}}),
            json!({"type":"assistant","timestamp":"2026-07-10T05:01:00Z",
                   "message":{"content":[{"type":"text","text":"오늘 답"}],"usage":{"output_tokens":1}}}),
        ];
        write_session(&projects.join("-p"), "sess.jsonl", &recs);
        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 7, 10));
        let s = &res.data.unwrap().sessions[0];
        assert_eq!(s.intent.as_deref(), Some("자정 넘기는 질문"));
        assert_eq!(s.qa.len(), 2);
        assert_eq!(s.qa[0].question, "자정 넘기는 질문");
        assert_eq!(s.qa[0].time, "23:55");
        assert!(s.qa[0].answer.contains("자정 후 답"));
        assert_eq!(s.qa[1].question, "오늘 질문");
        // 전날 레코드는 시간 범위에 안 들어감
        assert_eq!(s.first_ts, parse_iso("2026-07-09T15:05:00Z"));
    }

    #[test]
    fn real_user_text_filters_synthetic_and_tool_results() {
        let r = |c: Value| rec(json!({"type":"user","message":{"content":c}}));
        assert_eq!(
            real_user_text(&r(json!("진짜 질문입니다"))).as_deref(),
            Some("진짜 질문입니다")
        );
        assert_eq!(
            real_user_text(&r(json!(
                "<local-command-stdout>출력</local-command-stdout>"
            ))),
            None
        );
        assert_eq!(
            real_user_text(&r(json!("<command-message>foo</command-message>"))),
            None
        );
        assert_eq!(
            real_user_text(&r(json!("<system-reminder>x</system-reminder>"))),
            None
        );
        assert_eq!(real_user_text(&r(json!("   "))), None);
        assert_eq!(
            real_user_text(&r(json!([{"type":"tool_result","content":"o"}]))),
            None
        );
        assert_eq!(
            real_user_text(&r(
                json!([{"type":"tool_result","content":"o"},{"type":"text","text":"덧붙임"}])
            ))
            .as_deref(),
            Some("덧붙임")
        );
        assert_eq!(real_user_text(&r(json!(42))), None);
        assert_eq!(
            real_user_text(&rec(
                json!({"type":"user","isMeta":true,"message":{"content":"주입"}})
            )),
            None
        );
        assert_eq!(
            real_user_text(&rec(json!({"type":"assistant","message":{"content":"x"}}))),
            None
        );
    }

    #[test]
    fn project_name_handles_worktrees_and_slashes() {
        assert_eq!(project_name("D:\\demo\\repo"), "repo");
        assert_eq!(project_name("/home/u/proj/"), "proj");
        assert_eq!(
            project_name("D:\\study\\Daily Work Log\\.claude\\worktrees\\git-login-x"),
            "Daily Work Log"
        );
        assert_eq!(project_name("/"), "/");
    }

    #[test]
    fn bad_lines_and_snapshot_files() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let pd = projects.join("-p");
        fs::create_dir_all(&pd).unwrap();
        let body = [
            "{ not json".to_string(),
            json!({"type":"user","sessionId":"s","cwd":"/p","timestamp":"2026-07-10T02:00:00Z","message":{"content":"정상"}}).to_string(),
            json!({"type":"file-history-snapshot","snapshot":{"timestamp":"2026-07-10T02:00:01Z",
                   "trackedFileBackups":{"/p/x.py":{},"/p/y.py":{}}}}).to_string(),
            json!({"type":"assistant","timestamp":"2026-07-10T02:00:02Z",
                   "message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/r.py"}},
                                         {"type":"tool_use","name":"PowerShell","input":{"command":["a","b"]}}],
                              "usage":{"output_tokens":"7"}}}).to_string(),
        ]
        .join("\n");
        fs::write(pd.join("s.jsonl"), body).unwrap();
        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            include_read: true,
            ..Default::default()
        })
        .collect(&ctx(2026, 7, 10));
        let s = &res.data.unwrap().sessions[0];
        assert_eq!(s.intent.as_deref(), Some("정상"));
        assert_eq!(s.files_edited, vec!["/p/x.py", "/p/y.py"]);
        assert_eq!(s.files_read, vec!["/p/r.py"]);
        assert_eq!(s.output_tokens, 7);
        assert_eq!(s.commands, vec!["[\"a\",\"b\"]"]);
        assert_eq!(s.tool_counts.get("PowerShell"), Some(&1));
    }
}
