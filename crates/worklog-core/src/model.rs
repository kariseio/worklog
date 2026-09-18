//! 수집된 데이터와 최종 업무일지를 담는 데이터 모델.
//!
//! 각 수집기는 여기 정의된 타입을 채워 돌려주고, 분석·렌더·요약·출력은 이 타입만 안다.
//! 시각은 전부 UTC `DateTime<Utc>` 로 들고, 표시할 때만 설정 시간대로 바꾼다.

use chrono::{DateTime, NaiveDate, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

// --------------------------------------------------------------------------- //
// Git
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitCommit {
    pub repo: String,
    pub hash: String,
    pub author: String,
    pub when: DateTime<Utc>,
    pub subject: String,
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub insertions: u32,
    #[serde(default)]
    pub deletions: u32,
    /// 물리적 저장소 식별 키(git-common-dir). 동명이repo 구분용.
    #[serde(default)]
    pub repo_path: String,
}

impl GitCommit {
    /// 해시 앞 8자.
    pub fn short_hash(&self) -> &str {
        let end = self
            .hash
            .char_indices()
            .nth(8)
            .map(|(i, _)| i)
            .unwrap_or(self.hash.len());
        &self.hash[..end]
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GitData {
    pub commits: Vec<GitCommit>,
}

impl GitData {
    /// 커밋 등장 순서를 유지한 저장소 이름 목록(중복 제거).
    pub fn repos(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = Vec::new();
        for c in &self.commits {
            if !seen.contains(&c.repo.as_str()) {
                seen.push(c.repo.as_str());
            }
        }
        seen
    }

    pub fn commits_for<'a>(&'a self, repo: &'a str) -> impl Iterator<Item = &'a GitCommit> + 'a {
        self.commits.iter().filter(move |c| c.repo == repo)
    }
}

// --------------------------------------------------------------------------- //
// AI 세션 (Claude Code · Codex 공통)
// --------------------------------------------------------------------------- //

/// 세션 안의 한 '질답' — 사용자 질문 + 그에 대한 어시스턴트 응답 요지.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QaTurn {
    /// 로컬 시:분 (예: "14:03"). 없으면 빈 문자열.
    pub time: String,
    pub question: String,
    #[serde(default)]
    pub answer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    #[default]
    Claude,
    Codex,
}

impl Agent {
    /// 직렬화 이름과 같은 소문자 태그.
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub session_id: Option<String>,
    /// cwd 의 basename (표시용). worktree 면 실제 저장소명.
    pub project: Option<String>,
    /// 실제 프로젝트 절대경로.
    pub cwd: Option<String>,
    /// cwd 가 속한 **물리 저장소 루트** 경로(worktree 면 본체 루트). 저장소가 아니면 None.
    /// [`crate::service::normalize_sessions`] 가 git-common-dir 로 찾아 채운다 —
    /// 제외 글롭(§5-1 N0)이 형제 worktree 를 놓치지 않게 하는 근거.
    #[serde(default)]
    pub repo_root: Option<String>,
    pub git_branch: Option<String>,
    /// ai-title (세션 요약 한 줄).
    pub title: Option<String>,
    /// 그날 첫 사용자 프롬프트.
    pub intent: Option<String>,
    #[serde(default)]
    pub agent: Agent,
    #[serde(default)]
    pub files_edited: Vec<String>,
    #[serde(default)]
    pub files_read: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    /// 도구 → 호출 수. **첫 사용 순서를 유지**한다 — 동점일 때 표시 순서가 v1(Python dict)과 같아야 한다.
    #[serde(default)]
    pub tool_counts: IndexMap<String, u32>,
    #[serde(default)]
    pub output_tokens: u64,
    pub first_ts: Option<DateTime<Utc>>,
    pub last_ts: Option<DateTime<Utc>>,
    /// 세션 내 질답 흐름(시간순).
    #[serde(default)]
    pub qa: Vec<QaTurn>,
    /// 상한 초과로 생략된(앞부분) 질답 수.
    #[serde(default)]
    pub qa_dropped: u32,
}

impl Session {
    /// 같은 세션이 여러 로그 파일(worktree·이어받기)로 쪼개져 들어왔을 때 하나로 묶는 키.
    ///
    /// `session_id` 가 있으면 그것을, 없으면 `(cwd, 시작 분)` 을 쓴다. 에이전트가 다르면
    /// id 공간이 달라 우연히 겹칠 수 있으므로 항상 앞에 에이전트 태그를 붙인다.
    pub fn dedupe_key(&self) -> String {
        let agent = self.agent.as_str();
        if let Some(id) = self
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return format!("{agent}:id:{id}");
        }
        let cwd = self.cwd.as_deref().unwrap_or("");
        let minute = self
            .first_ts
            .map(|t| t.format("%Y-%m-%dT%H:%M").to_string())
            .unwrap_or_default();
        format!("{agent}:cwd:{cwd}@{minute}")
    }

    /// 표시용 제목: ai-title → 첫 프롬프트 → 첫 발화 → 편집 파일명 → `[프로젝트] 세션`.
    ///
    /// 정제는 [`crate::render::clean_title`] 한 곳에서만 한다 — 시스템 주입 문구·Traceback·
    /// 로컬 절대경로가 피드·문서 어디로도 새지 않게(N2).
    pub fn display_title(&self) -> String {
        crate::render::session_title(self)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionData {
    pub sessions: Vec<Session>,
}

impl SessionData {
    pub fn total_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// 세션 cwd 목록(등장 순서, 중복 제거).
    pub fn cwds(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = Vec::new();
        for s in &self.sessions {
            if let Some(c) = s.cwd.as_deref()
                && !seen.contains(&c)
            {
                seen.push(c);
            }
        }
        seen
    }
}

// --------------------------------------------------------------------------- //
// 캘린더 (NaverWorks)
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub title: Option<String>,
    /// RFC3339 또는 all-day 날짜("YYYY-MM-DD").
    pub start: Option<String>,
    pub end: Option<String>,
    #[serde(default)]
    pub all_day: bool,
    pub location: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub attendees: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalendarData {
    pub events: Vec<CalendarEvent>,
}

// --------------------------------------------------------------------------- //
// 메모 (사용자가 직접 남긴 구두 요청·결정·할 일)
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteItem {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
    /// app | tray | cli
    #[serde(default)]
    pub source: String,
}

// --------------------------------------------------------------------------- //
// 하루치 종합 + 최종 산출물
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DailyData {
    pub target_date: NaiveDate,
    pub tz_name: String,
    pub git: Option<GitData>,
    pub claude: Option<SessionData>,
    pub codex: Option<SessionData>,
    pub calendar: Option<CalendarData>,
    /// 그날 메모(시간순). 자동 수집이 아니라 사용자가 직접 남긴 1차 사실.
    #[serde(default)]
    pub notes: Vec<NoteItem>,
    /// 커밋을 찾으려고 훑은 git 저장소 수(수집기가 채운다). 커밋 0 의 이유를 쓰는 데 쓴다.
    #[serde(default)]
    pub git_repos_scanned: usize,
    /// 커밋 매칭에 쓴 '내 작성자' 신원 수.
    #[serde(default)]
    pub git_authors: usize,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl DailyData {
    pub fn new(target_date: NaiveDate, tz_name: impl Into<String>) -> Self {
        Self {
            target_date,
            tz_name: tz_name.into(),
            git: None,
            claude: None,
            codex: None,
            calendar: None,
            notes: Vec::new(),
            git_repos_scanned: 0,
            git_authors: 0,
            warnings: Vec::new(),
        }
    }

    /// Claude + Codex 세션을 합친 목록(렌더·분석에서 공통 소비).
    pub fn all_sessions(&self) -> Vec<&Session> {
        let mut out: Vec<&Session> = Vec::new();
        if let Some(c) = &self.claude {
            out.extend(c.sessions.iter());
        }
        if let Some(x) = &self.codex {
            out.extend(x.sessions.iter());
        }
        out
    }

    /// 요약을 돌릴 만한 실제 데이터가 하나라도 있는지. (`Some(빈 목록)` 은 없는 것과 같다)
    pub fn is_empty(&self) -> bool {
        let has_events = self.calendar.as_ref().is_some_and(|c| !c.events.is_empty());
        let has_commits = self.git.as_ref().is_some_and(|g| !g.commits.is_empty());
        let has_claude = self.claude.as_ref().is_some_and(|c| !c.sessions.is_empty());
        let has_codex = self.codex.as_ref().is_some_and(|c| !c.sessions.is_empty());
        !(has_events || has_commits || has_claude || has_codex || !self.notes.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkLog {
    pub target_date: NaiveDate,
    /// 수집 데이터를 결정론적으로 정리한 부분.
    pub facts_markdown: String,
    /// 최종 문서(요약 + 지표 + 선택적 원본 부록).
    pub full_markdown: String,
    pub data: DailyData,
    /// LLM 이 만든 자연어 요약 (없을 수 있음).
    pub summary_markdown: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(repo: &str, hash: &str) -> GitCommit {
        GitCommit {
            repo: repo.into(),
            hash: hash.into(),
            author: "me".into(),
            when: Utc::now(),
            subject: "s".into(),
            files_changed: 1,
            insertions: 2,
            deletions: 3,
            repo_path: String::new(),
        }
    }

    #[test]
    fn short_hash_and_repo_order() {
        let g = GitData {
            commits: vec![
                commit("b", "0123456789abcdef"),
                commit("a", "abc"),
                commit("b", "fedcba9876543210"),
            ],
        };
        assert_eq!(g.commits[0].short_hash(), "01234567");
        assert_eq!(g.commits[1].short_hash(), "abc");
        assert_eq!(g.repos(), vec!["b", "a"]);
        assert_eq!(g.commits_for("b").count(), 2);
    }

    #[test]
    fn daily_data_empty_and_sessions() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        let mut data = DailyData::new(d, "Asia/Seoul");
        assert!(data.is_empty());
        // Some(빈 목록) 도 비어 있는 것
        data.git = Some(GitData::default());
        data.calendar = Some(CalendarData::default());
        data.claude = Some(SessionData::default());
        data.codex = Some(SessionData::default());
        assert!(data.is_empty());

        data.claude = Some(SessionData {
            sessions: vec![Session {
                cwd: Some("D:/a".into()),
                ..Default::default()
            }],
        });
        data.codex = Some(SessionData {
            sessions: vec![Session {
                cwd: Some("D:/a".into()),
                agent: Agent::Codex,
                ..Default::default()
            }],
        });
        assert!(!data.is_empty());
        assert_eq!(data.all_sessions().len(), 2);
        assert_eq!(data.claude.as_ref().unwrap().cwds(), vec!["D:/a"]);

        let mut only_cal = DailyData::new(d, "Asia/Seoul");
        only_cal.calendar = Some(CalendarData {
            events: vec![CalendarEvent::default()],
        });
        assert!(!only_cal.is_empty());
        // 메모만 있어도 요약 대상
        let mut only_note = DailyData::new(d, "Asia/Seoul");
        only_note.notes.push(NoteItem {
            id: 1,
            ts: Utc::now(),
            text: "구두 요청".into(),
            tags: vec![],
            mentions: vec![],
            source: "app".into(),
        });
        assert!(!only_note.is_empty());
    }

    #[test]
    fn display_title_fallbacks() {
        let mut s = Session::default();
        assert_eq!(s.display_title(), "세션");
        s.project = Some("repoA".into());
        assert_eq!(s.display_title(), "[repoA] 세션");
        // 제목·요청이 없으면 편집 파일명이 다음 후보
        s.files_edited = vec!["D:/repoA/auth.rs".into(), "D:/repoA/main.rs".into()];
        assert_eq!(s.display_title(), "auth.rs, main.rs");
        s.intent = Some("아주 긴 요청 ".repeat(20));
        let long = s.display_title();
        assert!(long.chars().count() <= 60 && long.ends_with('…'));
        s.title = Some(" 제목 ".into());
        assert_eq!(s.display_title(), "제목");
        // 시스템 주입 문구는 제목 자리에 오지 않는다(N2) — 다음 후보로 내려간다.
        s.title = Some("<recommended_plugins>".into());
        assert!(s.display_title().starts_with("아주 긴 요청"));
    }

    #[test]
    fn dedupe_key_prefers_session_id_then_cwd_and_minute() {
        let base = Session {
            session_id: Some("S".into()),
            cwd: Some("D:/repo".into()),
            first_ts: Some(Utc::now()),
            ..Default::default()
        };
        // 같은 id 면 cwd(worktree)가 달라도 같은 세션
        let other = Session {
            cwd: Some("D:/wt".into()),
            first_ts: None,
            ..base.clone()
        };
        assert_eq!(base.dedupe_key(), other.dedupe_key());
        // 에이전트가 다르면 id 가 같아도 다른 세션
        let codex = Session {
            agent: Agent::Codex,
            ..base.clone()
        };
        assert_ne!(base.dedupe_key(), codex.dedupe_key());
        assert!(codex.dedupe_key().starts_with("codex:id:"));

        // id 가 없으면 (cwd, 시작 분)
        let ts = |s: &str| Some(DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc));
        let no_id = |cwd: &str, t: &str| Session {
            session_id: None,
            cwd: Some(cwd.into()),
            first_ts: ts(t),
            ..Default::default()
        };
        assert_eq!(
            no_id("D:/a", "2026-09-16T01:00:10Z").dedupe_key(),
            no_id("D:/a", "2026-09-16T01:00:50Z").dedupe_key()
        );
        assert_ne!(
            no_id("D:/a", "2026-09-16T01:00:10Z").dedupe_key(),
            no_id("D:/a", "2026-09-16T01:01:10Z").dedupe_key()
        );
        assert_ne!(
            no_id("D:/a", "2026-09-16T01:00:10Z").dedupe_key(),
            no_id("D:/b", "2026-09-16T01:00:10Z").dedupe_key()
        );
        // 빈 문자열 id 는 없는 것으로 본다
        let blank = Session {
            session_id: Some("  ".into()),
            cwd: Some("D:/a".into()),
            ..Default::default()
        };
        assert!(blank.dedupe_key().contains(":cwd:"));
        assert_eq!(Agent::Claude.as_str(), "claude");
    }

    #[test]
    fn agent_serializes_lowercase_and_tool_order_kept() {
        assert_eq!(serde_json::to_string(&Agent::Codex).unwrap(), "\"codex\"");
        let s: Session = serde_json::from_str(r#"{"agent":"codex"}"#).unwrap();
        assert_eq!(s.agent, Agent::Codex);
        let s: Session = serde_json::from_str("{}").unwrap();
        assert_eq!(s.agent, Agent::Claude);

        // 도구 사용 순서(첫 사용 순)가 JSON 왕복에서도 유지된다.
        let mut s = Session::default();
        *s.tool_counts.entry("Read".into()).or_insert(0) += 3;
        *s.tool_counts.entry("Edit".into()).or_insert(0) += 3;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""tool_counts":{"Read":3,"Edit":3}"#));
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.tool_counts.keys().collect::<Vec<_>>(),
            vec!["Read", "Edit"]
        );
    }
}
