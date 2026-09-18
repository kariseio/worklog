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
//!   - 모든 메시지 레코드에 uuid, parentUuid(대화의 첫 레코드만 null)
//!
//! resume/fork 는 **같은 대화를 새 sessionId 의 새 파일에 통째로 다시 적는다**.
//! 파일마다 세션을 만들면 같은 대화가 2~4개로 부풀어 보이므로,
//! (1) 뿌리 uuid 를 `Session::thread_id` 로 들고 나가고(정규화의 1순위 병합 키),
//! (2) 하루치 파일을 다 읽은 뒤 uuid 집합이 다른 세션에 90% 이상 포함되면 접는다.
//! (3) 그래도 남는 '메아리'(같은 질문만 다시 적히고 답이 하나도 없는 조각)를 접는다.

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
    /// 메시지 레코드 고유 id. resume/fork 로 복사돼도 **값이 유지된다** — 중복 판정의 근거.
    pub uuid: Option<String>,
    /// 부모 메시지 uuid. 대화의 첫 레코드만 null/없음 → 그 uuid 가 대화의 뿌리.
    #[serde(rename = "parentUuid")]
    pub parent_uuid: Option<String>,
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
    build_session_with_uuids(recs, ctx, cfg).map(|(s, _)| s)
}

/// [`build_session`] + 그 파일에 들어 있던 **메시지 uuid 전체**(중복 세션 판정용).
///
/// uuid 집합은 Session 에 담지 않는다 — 저장·직렬화 대상이 아니라 수집기 안에서만 쓰는
/// 한 번짜리 판정 재료다.
pub(crate) fn build_session_with_uuids(
    recs: &[Rec],
    ctx: &CollectContext,
    cfg: &ClaudeConfig,
) -> Option<(Session, BTreeSet<String>)> {
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
    // 파일 전체(= 대화 전체)의 메시지 uuid. 날짜로 자르지 않는다 — 이어받기 사본은
    // 하루치만 보면 안 겹칠 수 있고, 대화 단위로 봐야 포함 관계가 드러난다.
    let mut uuids: BTreeSet<String> = BTreeSet::new();
    let mut root_uuid: Option<&str> = None;
    let mut first_uuid: Option<&str> = None;

    for r in recs {
        if let Some(u) = r.uuid.as_deref().filter(|u| !u.is_empty()) {
            if first_uuid.is_none() {
                first_uuid = Some(u);
            }
            if root_uuid.is_none() && r.parent_uuid.is_none() {
                root_uuid = Some(u);
            }
            uuids.insert(u.to_string());
        }
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

    // 뿌리 레코드가 없는(앞이 잘린) 파일이면 처음 본 uuid 로 대신한다.
    s.thread_id = root_uuid.or(first_uuid).map(str::to_string);
    let (turns, dropped) = qa.finish(cfg.max_qa_turns);
    s.qa = turns;
    s.qa_dropped = dropped;
    s.files_edited = files_edited.into_iter().collect();
    if cfg.include_read {
        s.files_read = files_read.into_iter().collect();
    }
    Some((s, uuids))
}

// --------------------------------------------------------------------------- //
// 중복 세션 접기 (resume/fork)
// --------------------------------------------------------------------------- //

/// 같은 대화의 사본으로 볼 uuid 포함 비율(%).
const CONTAINMENT_PERCENT: usize = 90;

/// `a` 가 `b` 의 사본(부분집합)인가.
///
/// 같은 작업 폴더이거나 같은 대화 뿌리인 세션끼리만 본다. uuid 는 레코드마다 유일하므로
/// 서로 다른 대화가 우연히 겹칠 일은 없다 — 겹침이 크면 같은 대화를 두 번 읽은 것이다.
fn is_copy_of(a: &(Session, BTreeSet<String>), b: &(Session, BTreeSet<String>)) -> bool {
    let ((sa, ua), (sb, ub)) = (a, b);
    if ua.is_empty() || ub.len() < ua.len() {
        return false;
    }
    let same_thread = sa.thread_id.is_some() && sa.thread_id == sb.thread_id;
    if !same_thread && sa.cwd != sb.cwd {
        return false;
    }
    let shared = ua.iter().filter(|u| ub.contains(*u)).count();
    shared * 100 >= ua.len() * CONTAINMENT_PERCENT
}

/// resume/fork 로 같은 대화가 여러 파일에 적힌 세션을 하나만 남긴다.
///
/// 가장 완전한 쪽(uuid 많은 → 질답 많은 → 토큰 많은 → 늦게 끝난)부터 훑으면서 이미 남긴
/// 세션에 포함되는 사본을 버린다. uuid 가 겹치지 않는 진짜 반복 세션은 모두 남는다.
fn fold_duplicate_copies(
    items: Vec<(Session, BTreeSet<String>)>,
) -> Vec<(Session, BTreeSet<String>)> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        let ((sa, ua), (sb, ub)) = (&items[a], &items[b]);
        ub.len()
            .cmp(&ua.len())
            .then(sb.qa.len().cmp(&sa.qa.len()))
            .then(sb.output_tokens.cmp(&sa.output_tokens))
            .then(sb.last_ts.cmp(&sa.last_ts))
            .then(a.cmp(&b))
    });

    let mut kept: Vec<usize> = Vec::with_capacity(items.len());
    let mut folded = 0usize;
    for i in order {
        if kept.iter().any(|&k| is_copy_of(&items[i], &items[k])) {
            folded += 1;
        } else {
            kept.push(i);
        }
    }
    if folded > 0 {
        tracing::debug!("Claude 중복 세션 {folded}개 접음(uuid 포함 관계 — resume/fork)");
    }

    let mut keep_flag = vec![false; items.len()];
    for i in kept {
        keep_flag[i] = true;
    }
    items
        .into_iter()
        .zip(keep_flag)
        .filter_map(|(pair, keep)| keep.then_some(pair))
        .collect()
}

/// 그날 어시스턴트 쪽 기록이 하나도 없는 세션인가 — 사용자 말만 적혀 있다.
fn is_silent(s: &Session) -> bool {
    s.output_tokens == 0 && s.tool_counts.is_empty() && s.files_edited.is_empty()
}

/// 질문 본문만 시간순으로(빈 줄 제외).
fn questions(s: &Session) -> Vec<&str> {
    s.qa.iter()
        .map(|t| t.question.trim())
        .filter(|q| !q.is_empty())
        .collect()
}

/// 답이 하나도 없이 남의 질문만 되풀이하는 '메아리' 조각을 버린다.
///
/// fork/compaction 직전 파일에는 사용자가 마지막으로 친 말이 한 줄 남고, 이어받은 새 파일이
/// 같은 말을 **새 uuid 로 다시 적은 뒤** 실제 작업을 이어간다. 그날치 uuid 가 겹치지 않아
/// [`is_copy_of`] 의 포함 판정으로는 안 걸리지만(2026-09-15 `c510ce02`/`a3a8a7d6`:
/// 하루치 교집합 0, 파일 전체 포함률 0.65), 그 조각이 담은 정보는 남는 세션에 그대로 있다.
///
/// 버리는 조건은 넷 다 만족할 때뿐이다.
///   1. 출력 토큰·도구·수정 파일이 전부 비었다(사용자 말만 적힌 조각).
///   2. 질문이 하나 이상 있고, 같은 작업 폴더의 **실제로 일한** 세션이 그 질문을 모두 갖고 있다.
///   3. 두 파일이 **uuid 를 하나라도 공유한다** — 같은 대화에서 갈라져 나왔다는 증거.
///
/// 3번이 없으면 같은 문장을 다시 던진 진짜 반복 세션까지 먹는다(2026-09-15 suda-local
/// `list_projects` 3회: 서로 uuid 를 하나도 공유하지 않는 별개 세션이라 그대로 남아야 한다).
fn drop_echo_sessions(items: Vec<(Session, BTreeSet<String>)>) -> Vec<Session> {
    let worked: Vec<usize> = (0..items.len())
        .filter(|&i| !is_silent(&items[i].0))
        .collect();
    let mut keep = vec![true; items.len()];
    let mut folded = 0usize;
    for (i, (s, uuids)) in items.iter().enumerate() {
        if !is_silent(s) {
            continue;
        }
        let qs = questions(s);
        if qs.is_empty() || s.cwd.is_none() {
            continue;
        }
        let echoed = worked.iter().any(|&j| {
            let (other, other_uuids) = &items[j];
            other.cwd == s.cwd
                && uuids.iter().any(|u| other_uuids.contains(u))
                && qs
                    .iter()
                    .all(|q| other.qa.iter().any(|t| t.question.trim() == *q))
        });
        if echoed {
            keep[i] = false;
            folded += 1;
        }
    }
    if folded > 0 {
        tracing::debug!("Claude 메아리 세션 {folded}개 접음(답 없이 같은 질문만 반복)");
    }
    items
        .into_iter()
        .zip(keep)
        .filter_map(|((s, _), k)| k.then_some(s))
        .collect()
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
    pub(crate) fn candidate_files(&self, projects: &Path, since: &DateTime<Utc>) -> Vec<PathBuf> {
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
                    if let Some(pair) = build_session_with_uuids(&recs, ctx, &self.cfg) {
                        sessions.push(pair);
                    }
                }
                Err(e) => warnings.push(format!("세션 파싱 실패({name}): {e}")),
            }
        }
        // 하루치 파일을 다 읽은 뒤에야 사본 관계가 보인다.
        let mut sessions = drop_echo_sessions(fold_duplicate_copies(sessions));
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

    /// 이어받기(resume): 같은 대화가 새 sessionId 의 새 파일에 통째로 다시 적힌다.
    /// 뿌리 uuid 가 같고 uuid 집합이 포함 관계이므로 **더 완전한 쪽 하나만** 남아야 한다.
    #[test]
    fn resume_chain_folds_into_the_superset() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let pd = projects.join("D--demo-repo");
        let ask = |id: &str, uuid: &str, parent: Value, t: &str, q: &str| {
            json!({"type":"user","sessionId":id,"uuid":uuid,"parentUuid":parent,
                   "cwd":"D:\\demo\\repo","timestamp":t,"message":{"content":q}})
        };
        let answer = |id: &str, uuid: &str, parent: &str, t: &str, file: &str, tok: u64| {
            json!({"type":"assistant","sessionId":id,"uuid":uuid,"parentUuid":parent,
                   "cwd":"D:\\demo\\repo","timestamp":t,
                   "message":{"usage":{"output_tokens":tok},
                              "content":[{"type":"text","text":"네"},
                                         {"type":"tool_use","name":"Edit","input":{"file_path":file}}]}})
        };
        // 원본 대화(앞부분만)
        let head = vec![
            ask(
                "old",
                "u0",
                Value::Null,
                "2026-09-14T02:00:00Z",
                "MCP 기능을 지원할 수 있어?",
            ),
            answer(
                "old",
                "u1",
                "u0",
                "2026-09-14T02:01:00Z",
                "D:\\demo\\repo\\a.rs",
                100,
            ),
        ];
        write_session(&pd, "old.jsonl", &head);
        // 이어받기 파일: 같은 레코드를 **새 sessionId 로 다시 적고**(uuid 는 유지) 뒤를 잇는다.
        let mut resumed = vec![
            ask(
                "new",
                "u0",
                Value::Null,
                "2026-09-14T02:00:00Z",
                "MCP 기능을 지원할 수 있어?",
            ),
            answer(
                "new",
                "u1",
                "u0",
                "2026-09-14T02:01:00Z",
                "D:\\demo\\repo\\a.rs",
                100,
            ),
        ];
        resumed.push(ask(
            "new",
            "u2",
            json!("u1"),
            "2026-09-14T03:00:00Z",
            "계속해줘",
        ));
        resumed.push(answer(
            "new",
            "u3",
            "u2",
            "2026-09-14T03:01:00Z",
            "D:\\demo\\repo\\b.rs",
            200,
        ));
        write_session(&pd, "new.jsonl", &resumed);

        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 9, 14));
        let data = res.data.unwrap();
        assert_eq!(data.total_sessions(), 1, "이어받기 사본은 접힌다");
        let s = &data.sessions[0];
        assert_eq!(s.thread_id.as_deref(), Some("u0")); // 대화 뿌리
        assert_eq!(s.session_id.as_deref(), Some("new")); // 더 완전한 쪽이 남는다
        assert_eq!(
            s.files_edited,
            vec!["D:\\demo\\repo\\a.rs", "D:\\demo\\repo\\b.rs"]
        );
        assert_eq!(s.output_tokens, 300);
        assert_eq!(s.qa.len(), 2);
        // 정규화(병합 키)도 같은 대화로 본다 — 실시간 재조립 경로 대비.
        let old = build_session(
            &head.iter().cloned().map(rec).collect::<Vec<_>>(),
            &ctx(2026, 9, 14),
            &ClaudeConfig::default(),
        )
        .unwrap();
        assert_eq!(old.dedupe_key(), s.dedupe_key());
    }

    /// 압축(compaction) 분기: 뿌리 uuid 는 달라도 uuid 집합이 90% 이상 포함되면 같은 대화다.
    #[test]
    fn compaction_fork_with_different_root_folds_by_containment() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let pd = projects.join("D--demo-repo");
        let root = |id: &str, uuid: &str, q: &str| {
            json!({"type":"user","sessionId":id,"uuid":uuid,"parentUuid":null,
                   "cwd":"D:\\demo\\repo","timestamp":"2026-09-15T02:00:00Z","message":{"content":q}})
        };
        let body = |id: &str, uuid: &str, t: &str, tok: u64| {
            json!({"type":"assistant","sessionId":id,"uuid":uuid,"parentUuid":"p",
                   "cwd":"D:\\demo\\repo","timestamp":t,
                   "message":{"usage":{"output_tokens":tok},"content":[{"type":"text","text":"진행"}]}})
        };
        // 공통 레코드 9개(분기 이전) — uuid 가 그대로 복사된다.
        let shared: Vec<Value> = (1..=9)
            .map(|i| {
                body(
                    "a",
                    &format!("c{i}"),
                    &format!("2026-09-15T02:{i:02}:00Z"),
                    10,
                )
            })
            .collect();

        let mut a = vec![root("a", "ra", "폴더 요약 기능 추가")];
        a.extend(shared.iter().cloned()); // uuid 10개
        write_session(&pd, "a.jsonl", &a);

        let mut b = vec![root("b", "rb", "폴더 요약 기능 추가")];
        b.extend(shared.iter().cloned());
        b.push(body("b", "x1", "2026-09-15T03:00:00Z", 500)); // uuid 12개
        b.push(body("b", "x2", "2026-09-15T03:10:00Z", 500));
        write_session(&pd, "b.jsonl", &b);

        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 9, 15));
        let data = res.data.unwrap();
        assert_eq!(data.total_sessions(), 1, "9/10 = 90% 포함 → 접힌다");
        let s = &data.sessions[0];
        assert_eq!(s.session_id.as_deref(), Some("b")); // 큰 쪽(상위집합)이 남는다
        assert_eq!(s.thread_id.as_deref(), Some("rb"));
        assert_eq!(s.output_tokens, 1_090);
    }

    /// 이어받기 직전 파일에 사용자 말만 한 줄 남고, 새 파일이 그 말을 **새 uuid 로 다시 적은 뒤**
    /// 작업을 이어간 경우(2026-09-15 `c510ce02`/`a3a8a7d6`). uuid 가 안 겹쳐 포함 판정으로는
    /// 못 접지만, 답이 하나도 없는 쪽은 남는 세션에 든 정보의 메아리일 뿐이다.
    #[test]
    fn silent_echo_of_another_sessions_question_is_folded() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let pd = projects.join("D--demo-file");
        let ask = |id: &str, uuid: &str, t: &str| {
            json!({"type":"user","sessionId":id,"uuid":uuid,"parentUuid":null,
                   "cwd":"D:\\demo\\file","timestamp":t,
                   "message":{"content":"지금 폴더요약 어디서 되고 있어? 이상한 값 들어가는데"}})
        };
        // 갈라지기 전의 공통 기록(그 전날) — 두 파일이 같은 대화에서 나왔다는 증거.
        let before = |id: &str| -> Vec<Value> {
            (1..=3)
                .map(|i| {
                    json!({"type":"assistant","sessionId":id,"uuid":format!("c{i}"),"parentUuid":"p",
                           "cwd":"D:\\demo\\file","timestamp":format!("2026-09-13T01:0{i}:00Z"),
                           "message":{"usage":{"output_tokens":10},"content":[{"type":"text","text":"네"}]}})
                })
                .collect()
        };
        // 꼬리 파일: 그날 기록은 사용자 말 한 줄뿐 — 출력 토큰·도구·수정 파일 전부 없음.
        let mut tail = before("tail");
        tail.push(ask("tail", "t0", "2026-09-15T02:03:33Z"));
        write_session(&pd, "tail.jsonl", &tail);
        // 이어받은 파일: 같은 말을 다른 uuid 로 다시 적고 실제로 일한다.
        let mut live = before("live");
        live.push(ask("live", "L0", "2026-09-15T02:03:44Z"));
        live.push(
            json!({"type":"assistant","sessionId":"live","uuid":"L1","parentUuid":"L0",
                   "cwd":"D:\\demo\\file","timestamp":"2026-09-15T02:10:00Z",
                   "message":{"usage":{"output_tokens":22800},
                              "content":[{"type":"text","text":"고쳤습니다"},
                                         {"type":"tool_use","name":"Edit",
                                          "input":{"file_path":"D:\\demo\\file\\x.py"}}]}}),
        );
        live.push(
            json!({"type":"assistant","sessionId":"live","uuid":"L2","parentUuid":"L1",
                   "cwd":"D:\\demo\\file","timestamp":"2026-09-15T03:00:00Z",
                   "message":{"usage":{"output_tokens":0},"content":[{"type":"text","text":"끝"}]}}),
        );
        write_session(&pd, "live.jsonl", &live);

        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 9, 15));
        let data = res.data.unwrap();
        assert_eq!(data.total_sessions(), 1, "답 없는 메아리는 접힌다");
        let s = &data.sessions[0];
        assert_eq!(s.session_id.as_deref(), Some("live"));
        assert_eq!(s.output_tokens, 22_800);
        // 버린 조각이 갖고 있던 질문은 남는 세션에 그대로 있다.
        assert_eq!(s.qa.len(), 1);
        assert!(s.qa[0].question.starts_with("지금 폴더요약"));
    }

    /// 같은 문장을 다시 던진 **진짜 반복 세션**은 한쪽에 답이 없어도 남는다 —
    /// uuid 를 하나도 공유하지 않으면 갈라져 나온 사본이 아니다
    /// (2026-09-15 suda-local `list_projects` 3회).
    #[test]
    fn repeated_question_without_shared_uuids_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let pd = projects.join("D--demo-repo");
        const Q: &str = "list_projects 툴을 호출해서 결과 JSON 만 출력해.";
        let ask = |id: &str, uuid: &str, t: &str| {
            json!({"type":"user","sessionId":id,"uuid":uuid,"parentUuid":null,
                   "cwd":"D:\\demo\\repo","timestamp":t,"message":{"content":Q}})
        };
        // 답까지 받은 시도.
        write_session(
            &pd,
            "r1.jsonl",
            &[
                ask("r1", "p0", "2026-09-15T06:50:00Z"),
                json!({"type":"assistant","sessionId":"r1","uuid":"p1","parentUuid":"p0",
                       "cwd":"D:\\demo\\repo","timestamp":"2026-09-15T06:50:10Z",
                       "message":{"usage":{"output_tokens":3576},"content":[{"type":"text","text":"{}"}]}}),
            ],
        );
        // 같은 말을 다시 던졌지만 답이 안 온 시도 — 별개 대화이므로 접지 않는다.
        write_session(&pd, "r2.jsonl", &[ask("r2", "q0", "2026-09-15T06:50:30Z")]);
        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 9, 15));
        assert_eq!(res.data.unwrap().total_sessions(), 2);
    }

    /// 같은 폴더에서 연달아 연 **진짜 반복 세션**은 uuid 가 안 겹치므로 그대로 남는다.
    #[test]
    fn unrelated_sessions_in_same_cwd_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let pd = projects.join("D--demo-repo");
        let one = |id: &str, uuid: &str, t: &str, q: &str| {
            vec![
                json!({"type":"user","sessionId":id,"uuid":format!("{uuid}0"),"parentUuid":null,
                       "cwd":"D:\\demo\\repo","timestamp":t,"message":{"content":q}}),
                json!({"type":"assistant","sessionId":id,"uuid":format!("{uuid}1"),"parentUuid":format!("{uuid}0"),
                       "cwd":"D:\\demo\\repo","timestamp":t,
                       "message":{"usage":{"output_tokens":5},"content":[{"type":"text","text":"네"}]}}),
            ]
        };
        write_session(
            &pd,
            "s1.jsonl",
            &one("s1", "m", "2026-09-16T04:41:00Z", "list_projects 호출해줘"),
        );
        write_session(
            &pd,
            "s2.jsonl",
            &one("s2", "n", "2026-09-16T04:42:00Z", "get_workflow 호출해줘"),
        );
        let res = ClaudeCollector::new(ClaudeConfig {
            projects_dir: projects.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect(&ctx(2026, 9, 16));
        let data = res.data.unwrap();
        assert_eq!(data.total_sessions(), 2);
        let keys: Vec<String> = data.sessions.iter().map(|s| s.dedupe_key()).collect();
        assert_ne!(keys[0], keys[1]);
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
