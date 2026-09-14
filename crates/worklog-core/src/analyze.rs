//! 결정론적 분석 계층.
//!
//! 이미 수집되지만 표현에 안 쓰이던 데이터(세션 시간, 도구 사용량, 출력 토큰, 커밋 변경량)를
//! 뽑아 지표·핵심 성과·프로젝트 집중시간·커밋 타입·타임라인을 만든다. LLM 을 쓰지 않으므로
//! 항상 사실과 정확히 일치한다.
//!
//! 주의: '변경량 = 노력' 프록시는 리팩터/생성 코드에 편향된다. 참고 지표로만 볼 것.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    model::{DailyData, GitCommit, Session},
    render::is_meta_session,
    time::parse_iso_in,
};

// --------------------------------------------------------------------------- //
// 커밋 타입 분류 (conventional commit 우선, 없으면 키워드 휴리스틱)
// --------------------------------------------------------------------------- //

static CONV_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(\w+)(?:\([^)]*\))?!?:").expect("regex"));

const TYPE_MAP: &[(&str, (&str, &str))] = &[
    ("feat", ("feat", "기능")),
    ("feature", ("feat", "기능")),
    ("fix", ("fix", "버그")),
    ("bugfix", ("fix", "버그")),
    ("hotfix", ("fix", "버그")),
    ("refactor", ("refactor", "리팩터")),
    ("perf", ("perf", "성능")),
    ("docs", ("docs", "문서")),
    ("doc", ("docs", "문서")),
    ("test", ("test", "테스트")),
    ("tests", ("test", "테스트")),
    ("chore", ("chore", "기타")),
    ("build", ("chore", "빌드")),
    ("ci", ("chore", "CI")),
    ("style", ("style", "스타일")),
];
const INFRA_HINTS: &[&str] = &[
    "terraform",
    "ebs",
    "helm",
    "k8s",
    "kubernetes",
    "deploy",
    "infra",
    "docker",
];
const KEYWORD_HINTS: &[(&[&str], (&str, &str))] = &[
    (
        &["추가", "신설", "add", "새로", "생성", "구축", "도입"],
        ("feat", "기능"),
    ),
    (
        &["수정", "버그", "fix", "오류", "문제", "해결", "고침"],
        ("fix", "버그"),
    ),
    (
        &["리팩", "refactor", "정리", "개선"],
        ("refactor", "리팩터"),
    ),
    (&["문서", "docs", "readme"], ("docs", "문서")),
    (&["테스트", "test"], ("test", "테스트")),
    (&["롤백", "revert", "되돌"], ("revert", "롤백")),
];

/// 커밋 subject → (type_key, 한글 라벨). 예: "feat: X" → ("feat", "기능").
pub fn classify_commit(subject: &str) -> (&'static str, &'static str) {
    let low = subject.to_lowercase();
    if INFRA_HINTS.iter().any(|h| low.contains(h)) {
        return ("infra", "인프라");
    }
    if let Some(cap) = CONV_RE.captures(subject) {
        let key = cap[1].to_lowercase();
        if let Some((_, res)) = TYPE_MAP.iter().find(|(k, _)| *k == key) {
            return *res;
        }
    }
    for (words, res) in KEYWORD_HINTS {
        if words.iter().any(|w| low.contains(w)) {
            return *res;
        }
    }
    ("other", "기타")
}

fn type_weight(key: &str) -> i64 {
    match key {
        "feat" => 3,
        "infra" | "fix" | "refactor" | "perf" => 2,
        "docs" => 1,
        _ => 1,
    }
}

// --------------------------------------------------------------------------- //
// 데이터 구조
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Kpis {
    pub commits: u32,
    pub insertions: u64,
    pub deletions: u64,
    pub repos: u32,
    pub sessions: u32,
    pub tokens: u64,
    pub files_edited: u32,
    pub meetings: u32,
    pub span_start: Option<String>,
    pub span_end: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectRollup {
    pub project: String,
    pub minutes: i64,
    pub sessions: u32,
    pub files: u32,
    pub commits: u32,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// "session" | "commit" | "meeting"
    pub kind: String,
    /// HH:MM
    pub start: String,
    /// HH:MM (세션·회의 종료)
    pub end: Option<String>,
    pub project: String,
    pub label: String,
    /// 커밋 타입 라벨
    pub ctype: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    pub kpis: Kpis,
    pub projects: Vec<ProjectRollup>,
    /// 라벨 → 건수 (첫 등장 순서 유지)
    pub commit_types: IndexMap<String, u32>,
    /// 도구 → 호출 수 (첫 등장 순서 유지)
    pub tool_profile: IndexMap<String, u32>,
    pub work_style: String,
    pub highlights: Vec<String>,
    pub timeline: Vec<TimelineEvent>,
}

// --------------------------------------------------------------------------- //
// 분석
// --------------------------------------------------------------------------- //

fn hm(dt: &DateTime<Utc>, tz: Tz) -> String {
    dt.with_timezone(&tz).format("%H:%M").to_string()
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn analyze(data: &DailyData, tz: Tz) -> Analysis {
    let mut a = Analysis::default();

    let commits: &[GitCommit] = data
        .git
        .as_ref()
        .map(|g| g.commits.as_slice())
        .unwrap_or(&[]);
    // 업무일지 생성기 자신의 요약 세션(피드백 루프)은 지표/타임라인에서 제외.
    let sessions: Vec<&Session> = data
        .all_sessions()
        .into_iter()
        .filter(|s| !is_meta_session(s))
        .collect();

    // --- KPI ---
    a.kpis.commits = commits.len() as u32;
    a.kpis.insertions = commits.iter().map(|c| c.insertions as u64).sum();
    a.kpis.deletions = commits.iter().map(|c| c.deletions as u64).sum();
    a.kpis.repos = data
        .git
        .as_ref()
        .map(|g| g.repos().len() as u32)
        .unwrap_or(0);
    a.kpis.sessions = sessions.len() as u32;
    a.kpis.tokens = sessions.iter().map(|s| s.output_tokens).sum();
    a.kpis.files_edited = sessions.iter().map(|s| s.files_edited.len() as u32).sum();

    let mut times: Vec<DateTime<Utc>> = commits.iter().map(|c| c.when).collect();
    times.extend(sessions.iter().filter_map(|s| s.first_ts));
    times.extend(sessions.iter().filter_map(|s| s.last_ts));
    if let (Some(min), Some(max)) = (times.iter().min(), times.iter().max()) {
        a.kpis.span_start = Some(hm(min, tz));
        a.kpis.span_end = Some(hm(max, tz));
    }

    // --- 커밋 타입 분포 ---
    for c in commits {
        let (_, label) = classify_commit(&c.subject);
        *a.commit_types.entry(label.to_string()).or_insert(0) += 1;
    }

    // --- 도구 프로파일 + 작업 성격 ---
    for s in &sessions {
        for (tool, n) in &s.tool_counts {
            *a.tool_profile.entry(tool.clone()).or_insert(0) += n;
        }
    }
    a.work_style = work_style(&a.tool_profile);

    // --- 프로젝트별 롤업 (세션 project ↔ git repo 이름으로 조인, 첫 등장 순서) ---
    let mut roll: IndexMap<String, ProjectRollup> = IndexMap::new();
    for s in &sessions {
        let proj = s.project.clone().unwrap_or_else(|| "?".into());
        let r = roll.entry(proj.clone()).or_insert_with(|| ProjectRollup {
            project: proj,
            ..Default::default()
        });
        r.sessions += 1;
        r.files += s.files_edited.len() as u32;
        if let (Some(f), Some(l)) = (s.first_ts, s.last_ts) {
            let mins = (l - f).num_seconds().div_euclid(60);
            r.minutes += mins.max(0);
        }
    }
    for c in commits {
        let r = roll.entry(c.repo.clone()).or_insert_with(|| ProjectRollup {
            project: c.repo.clone(),
            ..Default::default()
        });
        r.commits += 1;
        r.insertions += c.insertions as u64;
        r.deletions += c.deletions as u64;
    }
    let mut projects: Vec<ProjectRollup> = roll.into_values().collect();
    projects.sort_by_key(|p| std::cmp::Reverse((p.minutes, p.commits, p.files)));
    a.projects = projects;

    // --- 핵심 성과 (Top 3) ---
    a.highlights = highlights(commits, &sessions);

    // --- 타임라인 (세션 구간 + 커밋 시각 + 캘린더 회의) ---
    let mut events: Vec<TimelineEvent> = Vec::new();
    for s in &sessions {
        let Some(first) = s.first_ts else { continue };
        let title = s
            .title
            .as_deref()
            .or(s.intent.as_deref())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        events.push(TimelineEvent {
            kind: "session".into(),
            start: hm(&first, tz),
            end: s.last_ts.map(|l| hm(&l, tz)),
            project: s.project.clone().unwrap_or_else(|| "?".into()),
            label: take_chars(&title, 70),
            ctype: None,
        });
    }
    for c in commits {
        let (_, label) = classify_commit(&c.subject);
        events.push(TimelineEvent {
            kind: "commit".into(),
            start: hm(&c.when, tz),
            end: None,
            project: c.repo.clone(),
            label: take_chars(&c.subject, 70),
            ctype: Some(label.to_string()),
        });
    }
    events.extend(meeting_events(data, tz, &mut a.kpis));
    events.sort_by(|x, y| x.start.cmp(&y.start));
    a.timeline = events;

    a
}

/// NaverWorks 캘린더 회의를 타임라인 이벤트로 (회의를 시간축에 끼워넣음).
fn meeting_events(data: &DailyData, tz: Tz, kpis: &mut Kpis) -> Vec<TimelineEvent> {
    let mut out = Vec::new();
    let Some(cal) = &data.calendar else {
        return out;
    };
    for e in &cal.events {
        let title = e
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "회의".into());
        if e.all_day {
            out.push(TimelineEvent {
                kind: "meeting".into(),
                start: "00:00".into(),
                end: None,
                project: "회의".into(),
                label: format!("{title} (종일)"),
                ctype: None,
            });
            kpis.meetings += 1;
            continue;
        }
        let Some(st) = e.start.as_deref().and_then(|t| parse_iso_in(t, tz)) else {
            continue; // 시작 파싱 실패 → 타임라인에 안 넣고 카운트도 안 함
        };
        let en = e.end.as_deref().and_then(|t| parse_iso_in(t, tz));
        let loc = e
            .location
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|l| format!(" @{l}"))
            .unwrap_or_default();
        out.push(TimelineEvent {
            kind: "meeting".into(),
            start: hm(&st, tz),
            end: en.map(|t| hm(&t, tz)),
            project: "회의".into(),
            label: format!("{title}{loc}"),
            ctype: None,
        });
        kpis.meetings += 1;
    }
    out
}

fn work_style(tool_profile: &IndexMap<String, u32>) -> String {
    if tool_profile.is_empty() {
        return String::new();
    }
    let sum = |names: &[&str]| -> u32 {
        names
            .iter()
            .map(|n| tool_profile.get(*n).copied().unwrap_or(0))
            .sum()
    };
    let edit = sum(&["Edit", "Write", "MultiEdit", "NotebookEdit"]);
    let read = sum(&["Read", "Grep", "Glob"]);
    let run = sum(&["Bash", "PowerShell"]);
    let mut ranked = [("구현형", edit), ("탐색형", read), ("실행형", run)];
    ranked.sort_by_key(|r| std::cmp::Reverse(r.1)); // 안정 정렬: 동점이면 구현형 > 탐색형 > 실행형 순 유지
    let (top, second) = (ranked[0], ranked[1]);
    if top.1 == 0 {
        return String::new();
    }
    if second.1 > 0 && (top.1 as f64) <= (second.1 as f64) * 1.3 {
        return format!("{}·{} 혼합", top.0, second.0);
    }
    top.0.to_string()
}

fn highlights(commits: &[GitCommit], sessions: &[&Session]) -> Vec<String> {
    let mut scored: Vec<(i64, u32, String)> = commits
        .iter()
        .map(|c| {
            let (key, label) = classify_commit(&c.subject);
            let mut score = type_weight(key);
            if c.insertions > 300 {
                score += 2;
            } else if c.insertions > 50 {
                score += 1;
            }
            (
                score,
                c.insertions,
                format!("[{label}] {} ({})", c.subject, c.repo),
            )
        })
        .collect();
    if !scored.is_empty() {
        scored.sort_by_key(|s| std::cmp::Reverse((s.0, s.1)));
        return scored.into_iter().take(3).map(|s| s.2).collect();
    }
    // 커밋이 없으면 파일 많이 만진 세션 상위
    let mut sess: Vec<&Session> = sessions.to_vec();
    sess.sort_by_key(|s| std::cmp::Reverse(s.files_edited.len()));
    let mut out = Vec::new();
    for s in sess.into_iter().take(3) {
        let title = s
            .title
            .as_deref()
            .or(s.intent.as_deref())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let n = s.files_edited.len();
        let proj = s.project.as_deref().unwrap_or("?");
        let files = if n > 0 {
            format!(", {n}파일")
        } else {
            String::new()
        };
        out.push(format!("{title} ({proj}{files})"));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{CalendarData, CalendarEvent, GitData, SessionData};
    use crate::time::get_tz;
    use crate::time::parse_iso;
    use chrono::NaiveDate;

    fn t(s: &str) -> DateTime<Utc> {
        parse_iso(s).unwrap()
    }

    pub(crate) fn sample() -> DailyData {
        let mut d = DailyData::new(NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(), "Asia/Seoul");
        d.git = Some(GitData {
            commits: vec![
                GitCommit {
                    repo: "repoA".into(),
                    hash: "a".repeat(10),
                    author: "me".into(),
                    when: t("2026-07-06T01:00:00Z"),
                    subject: "feat: 큰 기능".into(),
                    files_changed: 5,
                    insertions: 400,
                    deletions: 2,
                    repo_path: String::new(),
                },
                GitCommit {
                    repo: "repoA".into(),
                    hash: "b".repeat(10),
                    author: "me".into(),
                    when: t("2026-07-06T03:00:00Z"),
                    subject: "fix: 작은 버그".into(),
                    files_changed: 1,
                    insertions: 3,
                    deletions: 1,
                    repo_path: String::new(),
                },
            ],
        });
        let mut tools = IndexMap::new();
        tools.insert("Edit".to_string(), 10);
        tools.insert("Read".to_string(), 2);
        d.claude = Some(SessionData {
            sessions: vec![Session {
                session_id: Some("1".into()),
                project: Some("repoA".into()),
                cwd: Some("D:\\repoA".into()),
                git_branch: Some("main".into()),
                title: Some("기능 구현".into()),
                intent: Some("x".into()),
                files_edited: vec!["a.py".into(), "b.py".into()],
                tool_counts: tools,
                output_tokens: 5000,
                first_ts: Some(t("2026-07-06T00:00:00Z")),
                last_ts: Some(t("2026-07-06T01:30:00Z")),
                ..Default::default()
            }],
        });
        d
    }

    #[test]
    fn classify() {
        assert_eq!(classify_commit("feat: X").0, "feat");
        assert_eq!(classify_commit("fix(auth): Y").0, "fix");
        assert_eq!(classify_commit("refactor: Z").0, "refactor");
        assert_eq!(classify_commit("Feat: stage-suda EBS 확장").0, "infra"); // EBS 힌트 우선
        assert_eq!(classify_commit("로그인 버그 수정").0, "fix"); // 키워드
        assert_eq!(classify_commit("검색 노드 추가").0, "feat"); // 키워드
        assert_eq!(classify_commit("random text").0, "other");
        assert_eq!(
            classify_commit("chore(release): 버전 0.1.19"),
            ("chore", "기타")
        );
        assert_eq!(classify_commit("docs!: breaking"), ("docs", "문서"));
    }

    #[test]
    fn kpis_and_rollup() {
        let a = analyze(&sample(), get_tz("Asia/Seoul"));
        assert_eq!(a.kpis.commits, 2);
        assert_eq!(a.kpis.insertions, 403);
        assert_eq!(a.kpis.repos, 1);
        assert_eq!(a.kpis.sessions, 1);
        assert_eq!(a.kpis.tokens, 5000);
        assert_eq!(a.kpis.files_edited, 2);
        assert_eq!(a.kpis.span_start.as_deref(), Some("09:00"));
        assert_eq!(a.kpis.span_end.as_deref(), Some("12:00"));
        let p = &a.projects[0];
        assert_eq!(p.project, "repoA");
        assert_eq!(p.minutes, 90);
        assert_eq!((p.commits, p.files, p.insertions), (2, 2, 403));
    }

    #[test]
    fn types_style_highlights_timeline() {
        let a = analyze(&sample(), get_tz("Asia/Seoul"));
        assert_eq!(a.commit_types.get("기능"), Some(&1));
        assert_eq!(a.commit_types.get("버그"), Some(&1));
        assert!(a.work_style.contains("구현형"));
        assert!(a.highlights[0].contains("큰 기능"));
        let kinds: Vec<&str> = a.timeline.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds.iter().filter(|k| **k == "session").count(), 1);
        assert_eq!(kinds.iter().filter(|k| **k == "commit").count(), 2);
        let starts: Vec<&String> = a.timeline.iter().map(|e| &e.start).collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted);
    }

    #[test]
    fn weaves_meetings_into_timeline() {
        let mut d = sample();
        d.calendar = Some(CalendarData {
            events: vec![
                CalendarEvent {
                    title: Some("스프린트 회의".into()),
                    start: Some("2026-07-06T01:30:00+00:00".into()),
                    end: Some("2026-07-06T02:00:00+00:00".into()),
                    all_day: false,
                    location: Some("회의실A".into()),
                    ..Default::default()
                },
                CalendarEvent {
                    title: Some("휴가".into()),
                    start: Some("2026-07-06".into()),
                    all_day: true,
                    ..Default::default()
                },
                CalendarEvent {
                    title: Some("깨진 시각".into()),
                    start: Some("nope".into()),
                    ..Default::default()
                },
            ],
        });
        let a = analyze(&d, get_tz("Asia/Seoul"));
        assert_eq!(a.kpis.meetings, 2);
        let mtgs: Vec<&TimelineEvent> = a.timeline.iter().filter(|e| e.kind == "meeting").collect();
        assert_eq!(mtgs.len(), 2);
        assert_eq!(mtgs[0].label, "휴가 (종일)");
        assert_eq!(mtgs[0].start, "00:00");
        assert_eq!(mtgs[1].label, "스프린트 회의 @회의실A");
        assert_eq!(mtgs[1].start, "10:30");
        assert_eq!(mtgs[1].end.as_deref(), Some("11:00"));
        let starts: Vec<&String> = a.timeline.iter().map(|e| &e.start).collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted);
    }

    #[test]
    fn work_style_mix_and_no_commit_highlights() {
        let mut p = IndexMap::new();
        p.insert("Edit".to_string(), 10);
        p.insert("Read".to_string(), 9);
        assert_eq!(work_style(&p), "구현형·탐색형 혼합");
        p.insert("Bash".to_string(), 40);
        assert_eq!(work_style(&p), "실행형");
        assert_eq!(work_style(&IndexMap::new()), "");
        let mut only_web = IndexMap::new();
        only_web.insert("WebFetch".to_string(), 3);
        assert_eq!(work_style(&only_web), "");

        let mut d = sample();
        d.git = None;
        let a = analyze(&d, get_tz("Asia/Seoul"));
        assert_eq!(a.highlights, vec!["기능 구현 (repoA, 2파일)"]);
        assert!(a.kpis.span_start.is_some());
    }

    #[test]
    fn meta_sessions_excluded() {
        let mut d = sample();
        d.claude.as_mut().unwrap().sessions.push(Session {
            intent: Some(format!("{} 요약", crate::render::WORKLOG_SENTINEL)),
            output_tokens: 99_999,
            first_ts: Some(t("2026-07-06T10:00:00Z")),
            ..Default::default()
        });
        let a = analyze(&d, get_tz("Asia/Seoul"));
        assert_eq!(a.kpis.sessions, 1);
        assert_eq!(a.kpis.tokens, 5000);
    }
}
