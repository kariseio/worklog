//! 오늘 라이브 상태 — 전체 수집 한 번 뒤로는 **바뀐 파일·저장소만** 다시 읽어 피드를 갱신한다.
//!
//! 앱 셸이 [`crate::watch::FileWatcher`] 의 변경 배치를 받아 [`Live::apply_changes`] 로 넘기고,
//! 돌아온 [`FeedDelta`] 를 UI 에 밀어 넣는다. 회의(NaverWorks)는 푸시가 없어 주기 폴링,
//! 메모는 저장 직후 [`Live::refresh_notes`] 로 반영한다.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use rayon::prelude::*;

use crate::{
    collect::{
        CollectContext, Collector, SourceState, SourceStatus,
        claude::{self, ClaudeCollector},
        codex::CodexCollector,
        git::{GitCollector, RepoIdentity, identify, log_repo, repo_root_of},
        modified_since,
        naverworks::NaverWorksCollector,
    },
    config::Config,
    feed::{self, Feed, FeedDelta},
    model::{CalendarData, DailyData, GitCommit, GitData, NoteItem, Session, SessionData},
    notes,
    render::is_meta_session,
    service::{self, disambiguate_repo_names},
    store::Store,
    time::TimeError,
    watch::{Changed, WatchTargets},
};

const SOURCES: [&str; 4] = ["git", "claude", "codex", "naverworks"];

pub struct Live {
    cfg: Config,
    ctx: CollectContext,
    /// Claude 세션 파일 → 그날 세션(활동 없는 파일은 없음).
    claude: BTreeMap<PathBuf, Session>,
    codex: BTreeMap<PathBuf, Session>,
    /// git-common-dir → (식별, 그날 커밋)
    git: BTreeMap<String, (RepoIdentity, Vec<GitCommit>)>,
    calendar: Option<CalendarData>,
    notes: Vec<NoteItem>,
    states: BTreeMap<&'static str, SourceState>,
    warnings: BTreeMap<&'static str, Vec<String>>,
    data: DailyData,
    feed: Feed,
}

impl Live {
    /// 비어 있는 상태로 시작. [`full_refresh`](Self::full_refresh) 를 불러야 데이터가 찬다.
    pub fn new(cfg: Config, date_spec: Option<&str>) -> Result<Self, TimeError> {
        let ctx = service::make_context(&cfg, date_spec)?;
        let data = DailyData::new(ctx.target_date(), cfg.timezone.clone());
        let feed = feed::build(&data, &[], ctx.tz(), Utc::now());
        let states = SOURCES
            .iter()
            .map(|n| (*n, SourceState::Disabled))
            .collect();
        Ok(Self {
            cfg,
            ctx,
            claude: BTreeMap::new(),
            codex: BTreeMap::new(),
            git: BTreeMap::new(),
            calendar: None,
            notes: Vec::new(),
            states,
            warnings: BTreeMap::new(),
            data,
            feed,
        })
    }

    pub fn ctx(&self) -> &CollectContext {
        &self.ctx
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn feed(&self) -> &Feed {
        &self.feed
    }

    pub fn data(&self) -> &DailyData {
        &self.data
    }

    /// 설정을 바꿔 끼운다(다음 전체 수집부터 반영).
    pub fn set_config(&mut self, cfg: Config) {
        self.cfg = cfg;
    }

    /// 대상 날짜가 지났는지(자정을 넘김). true 면 새 `Live` 를 만들어야 한다.
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        now.with_timezone(&self.ctx.tz()).date_naive() != self.ctx.target_date()
    }

    /// 감시해야 할 대상(설정된 로그 폴더 + 현재 알고 있는 저장소).
    pub fn watch_targets(&self) -> WatchTargets {
        let s = &self.cfg.sources;
        WatchTargets {
            claude_projects: s
                .claude
                .enabled
                .then(|| ClaudeCollector::new(s.claude.clone()).projects_dir())
                .filter(|p| p.is_dir()),
            codex_sessions: s
                .codex
                .enabled
                .then(|| CodexCollector::new(s.codex.clone()).sessions_dir())
                .filter(|p| p.is_dir()),
            git_common_dirs: self.git.keys().map(PathBuf::from).collect(),
        }
    }

    pub fn statuses(&self) -> Vec<SourceStatus> {
        SOURCES
            .iter()
            .map(|n| {
                let state = self.states.get(n).copied().unwrap_or(SourceState::Disabled);
                let count = match (*n, state) {
                    (_, SourceState::Ok) => match *n {
                        "git" => self.git.values().map(|(_, c)| c.len()).sum(),
                        "claude" => self.claude.values().filter(|s| !is_meta_session(s)).count(),
                        "codex" => self.codex.values().filter(|s| !is_meta_session(s)).count(),
                        "naverworks" => self.calendar.as_ref().map(|c| c.events.len()).unwrap_or(0),
                        _ => 0,
                    },
                    _ => 0,
                };
                SourceStatus {
                    name: n.to_string(),
                    state,
                    count,
                    note: self.warnings.get(n).and_then(|w| w.first().cloned()),
                }
            })
            .collect()
    }

    // ---- 전체 수집 ------------------------------------------------------- //

    /// 모든 소스를 다시 수집한다(처음·주기 보정·'지금 갱신').
    pub fn full_refresh(&mut self, store: Option<&Store>, now: DateTime<Utc>) -> FeedDelta {
        self.refresh_claude_all();
        self.refresh_codex_all();
        self.refresh_git_all();
        self.refresh_calendar_inner();
        self.notes = notes::items_for(store, self.ctx.target_date());
        self.rebuild(now)
    }

    fn since(&self) -> DateTime<Utc> {
        self.ctx.day.start.with_timezone(&Utc)
    }

    fn refresh_claude_all(&mut self) {
        self.claude.clear();
        let cfg = &self.cfg.sources.claude;
        if !cfg.enabled {
            self.states.insert("claude", SourceState::Disabled);
            self.warnings.remove("claude");
            return;
        }
        let col = ClaudeCollector::new(cfg.clone());
        let projects = col.projects_dir();
        if !projects.is_dir() {
            self.states.insert("claude", SourceState::Skipped);
            self.warnings.insert(
                "claude",
                vec![format!(
                    "건너뜀: Claude 로그 폴더가 없습니다: {}",
                    projects.display()
                )],
            );
            return;
        }
        let mut warnings = Vec::new();
        for path in col.candidate_files(&projects, &self.since()) {
            match claude::parse_records(&path) {
                Ok(recs) => {
                    if let Some(s) = claude::build_session(&recs, &self.ctx, cfg) {
                        self.claude.insert(path, s);
                    }
                }
                Err(e) => warnings.push(format!("세션 파싱 실패({}): {e}", file_name(&path))),
            }
        }
        self.states.insert("claude", SourceState::Ok);
        self.warnings.insert("claude", warnings);
    }

    fn refresh_codex_all(&mut self) {
        self.codex.clear();
        let cfg = &self.cfg.sources.codex;
        if !cfg.enabled {
            self.states.insert("codex", SourceState::Disabled);
            self.warnings.remove("codex");
            return;
        }
        let col = CodexCollector::new(cfg.clone());
        let root = col.sessions_dir();
        if !root.is_dir() {
            self.states.insert("codex", SourceState::Skipped);
            self.warnings.insert(
                "codex",
                vec![format!(
                    "건너뜀: Codex 세션 폴더가 없습니다: {}",
                    root.display()
                )],
            );
            return;
        }
        let mut warnings = Vec::new();
        for path in col.candidate_files(&root, &self.since()) {
            match col.parse(&path, &self.ctx, &mut warnings) {
                Ok(Some(s)) => {
                    self.codex.insert(path, s);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("세션 파싱 실패({}): {e}", file_name(&path))),
            }
        }
        self.states.insert("codex", SourceState::Ok);
        self.warnings.insert("codex", warnings);
    }

    fn session_cwds(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in self.claude.values().chain(self.codex.values()) {
            if let Some(c) = &s.cwd
                && !out.contains(c)
            {
                out.push(c.clone());
            }
        }
        out
    }

    fn refresh_git_all(&mut self) {
        self.git.clear();
        let cfg = &self.cfg.sources.git;
        if !cfg.enabled {
            self.states.insert("git", SourceState::Disabled);
            self.warnings.remove("git");
            return;
        }
        let mut warnings = Vec::new();
        let repos = GitCollector::new(cfg.clone())
            .with_extra_repos(self.session_cwds())
            .resolve_repos(&mut warnings);
        if repos.is_empty() {
            self.states.insert("git", SourceState::Skipped);
            warnings.insert(0, "건너뜀: 감시할 git 저장소가 없습니다.".into());
            self.warnings.insert("git", warnings);
            return;
        }
        let ctx = &self.ctx;
        let logged: Vec<(RepoIdentity, Result<Vec<GitCommit>, String>)> = repos
            .into_par_iter()
            .map(|ident| {
                let r = log_repo(&ident, cfg, ctx);
                (ident, r)
            })
            .collect();
        for (ident, r) in logged {
            match r {
                Ok(commits) => {
                    self.git.insert(ident.common_dir.clone(), (ident, commits));
                }
                Err(e) => {
                    warnings.push(format!("git 로그 실패({}): {e}", ident.path.display()));
                    self.git
                        .insert(ident.common_dir.clone(), (ident, Vec::new()));
                }
            }
        }
        self.states.insert("git", SourceState::Ok);
        self.warnings.insert("git", warnings);
    }

    fn refresh_calendar_inner(&mut self) {
        let cfg = &self.cfg.sources.naverworks;
        if !cfg.enabled {
            self.states.insert("naverworks", SourceState::Disabled);
            self.warnings.remove("naverworks");
            self.calendar = None;
            return;
        }
        let res = NaverWorksCollector::new(cfg.clone()).collect(&self.ctx);
        let st = SourceStatus::from_result(&res, |d| d.events.len());
        self.states.insert("naverworks", st.state);
        let mut w = res.warnings.clone();
        if res.skipped {
            w.insert(
                0,
                format!("건너뜀: {}", res.skip_reason.clone().unwrap_or_default()),
            );
        }
        self.warnings.insert("naverworks", w);
        if let Some(d) = res.data {
            self.calendar = Some(d);
        }
    }

    // ---- 증분 갱신 ------------------------------------------------------- //

    /// 감시기가 보낸 변경 배치를 반영한다.
    pub fn apply_changes(&mut self, changes: &[Changed], now: DateTime<Utc>) -> FeedDelta {
        for c in changes {
            match c {
                Changed::ClaudeFile(p) => self.refresh_claude_file_inner(p),
                Changed::CodexFile(p) => self.refresh_codex_file_inner(p),
                Changed::GitRepo(p) => self.refresh_git_repo_inner(p),
            }
        }
        self.rebuild(now)
    }

    /// Claude 세션 파일 하나만 다시 읽는다.
    pub fn refresh_claude_file(&mut self, path: &Path, now: DateTime<Utc>) -> FeedDelta {
        self.refresh_claude_file_inner(path);
        self.rebuild(now)
    }

    fn refresh_claude_file_inner(&mut self, path: &Path) {
        let cfg = &self.cfg.sources.claude;
        if !cfg.enabled {
            return;
        }
        let key = path.to_path_buf();
        if !path.is_file() || !modified_since(path, &self.since()) {
            self.claude.remove(&key);
            return;
        }
        match claude::parse_records(path) {
            Ok(recs) => match claude::build_session(&recs, &self.ctx, cfg) {
                Some(s) => {
                    self.claude.insert(key, s);
                }
                None => {
                    self.claude.remove(&key);
                }
            },
            Err(e) => tracing::warn!("세션 파싱 실패({}): {e}", file_name(path)),
        }
    }

    pub fn refresh_codex_file(&mut self, path: &Path, now: DateTime<Utc>) -> FeedDelta {
        self.refresh_codex_file_inner(path);
        self.rebuild(now)
    }

    fn refresh_codex_file_inner(&mut self, path: &Path) {
        let cfg = &self.cfg.sources.codex;
        if !cfg.enabled {
            return;
        }
        let key = path.to_path_buf();
        if !path.is_file() || !modified_since(path, &self.since()) {
            self.codex.remove(&key);
            return;
        }
        let col = CodexCollector::new(cfg.clone());
        let mut warnings = Vec::new();
        match col.parse(path, &self.ctx, &mut warnings) {
            Ok(Some(s)) => {
                self.codex.insert(key, s);
            }
            Ok(None) => {
                self.codex.remove(&key);
            }
            Err(e) => tracing::warn!("세션 파싱 실패({}): {e}", file_name(path)),
        }
    }

    /// 저장소 하나(git-common-dir 또는 작업 폴더)만 다시 로그한다. 모르는 저장소면 새로 식별해 넣는다.
    pub fn refresh_git_repo(&mut self, path: &Path, now: DateTime<Utc>) -> FeedDelta {
        self.refresh_git_repo_inner(path);
        self.rebuild(now)
    }

    fn refresh_git_repo_inner(&mut self, path: &Path) {
        let cfg = &self.cfg.sources.git;
        if !cfg.enabled {
            return;
        }
        let key = path.to_string_lossy().into_owned();
        let ident = match self.git.get(&key) {
            Some((ident, _)) => ident.clone(),
            None => {
                let probe = if path.file_name().is_some_and(|n| n == ".git") {
                    repo_root_of(path)
                } else {
                    path.to_path_buf()
                };
                let Some(ident) = identify(&probe) else {
                    return;
                };
                ident
            }
        };
        match log_repo(&ident, cfg, &self.ctx) {
            Ok(commits) => {
                self.git.insert(ident.common_dir.clone(), (ident, commits));
            }
            Err(e) => tracing::warn!("git 로그 실패({}): {e}", ident.path.display()),
        }
    }

    /// 회의 다시 조회(주기 폴링).
    pub fn refresh_calendar(&mut self, now: DateTime<Utc>) -> FeedDelta {
        self.refresh_calendar_inner();
        self.rebuild(now)
    }

    /// 메모 다시 읽기(저장·수정·삭제 직후).
    pub fn refresh_notes(&mut self, store: Option<&Store>, now: DateTime<Utc>) -> FeedDelta {
        self.notes = notes::items_for(store, self.ctx.target_date());
        self.rebuild(now)
    }

    // ---- 조립 ----------------------------------------------------------- //

    fn rebuild(&mut self, now: DateTime<Utc>) -> FeedDelta {
        let tz = self.ctx.tz();
        let mut data = DailyData::new(self.ctx.target_date(), self.cfg.timezone.clone());
        data.notes = self.notes.clone();
        for n in SOURCES {
            if let Some(ws) = self.warnings.get(n) {
                data.warnings
                    .extend(ws.iter().map(|w| format!("[{n}] {w}")));
            }
        }
        let sorted = |m: &BTreeMap<PathBuf, Session>| {
            let mut v: Vec<Session> = m.values().cloned().collect();
            v.sort_by_key(|s| (s.first_ts.is_none(), s.first_ts));
            v
        };
        if self.states.get("claude") == Some(&SourceState::Ok) {
            data.claude = Some(SessionData {
                sessions: sorted(&self.claude),
            });
        }
        if self.states.get("codex") == Some(&SourceState::Ok) {
            data.codex = Some(SessionData {
                sessions: sorted(&self.codex),
            });
        }
        if self.states.get("git") == Some(&SourceState::Ok) {
            let mut commits: Vec<GitCommit> = self
                .git
                .values()
                .flat_map(|(_, c)| c.iter().cloned())
                .collect();
            commits.sort_by_key(|c| c.when);
            data.git = Some(GitData { commits });
        }
        data.calendar = self.calendar.clone();
        disambiguate_repo_names(&mut data);

        let statuses = self.statuses();
        let new_feed = feed::build(&data, &statuses, tz, now);
        let delta = feed::delta(&self.feed, &new_feed);
        self.feed = new_feed;
        self.data = data;
        delta
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::FeedKind;
    use crate::time::DayBounds;
    use serde_json::json;
    use std::{fs, process::Command};

    fn write_jsonl(path: &Path, recs: &[serde_json::Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = recs.iter().map(|r| r.to_string()).collect();
        fs::write(path, body.join("\n") + "\n").unwrap();
    }

    fn git(repo: &Path, args: &[&str], env: &[(&str, &str)]) {
        let mut c = Command::new("git");
        c.arg("-C").arg(repo).args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 오늘(설정 시간대) 기준 컨텍스트와, 그날 안의 시각 문자열 두 개.
    fn today() -> (Config, DayBounds, String, String) {
        let cfg = Config::default();
        let tz = crate::time::get_tz(&cfg.timezone);
        let day = DayBounds::for_date(Utc::now().with_timezone(&tz).date_naive(), tz);
        let t1 = (day.start + chrono::Duration::hours(9)).with_timezone(&Utc);
        let t2 = (day.start + chrono::Duration::hours(10)).with_timezone(&Utc);
        (cfg, day, t1.to_rfc3339(), t2.to_rfc3339())
    }

    #[test]
    fn full_refresh_then_incremental_updates() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let sessions = dir.path().join("sessions");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"], &[]);
        git(&repo, &["config", "user.email", "me@x"], &[]);
        git(&repo, &["config", "user.name", "Me"], &[]);
        git(&repo, &["config", "commit.gpgsign", "false"], &[]);

        let (mut cfg, day, t1, t2) = today();
        cfg.sources.claude.projects_dir = projects.to_string_lossy().into_owned();
        cfg.sources.codex.sessions_dir = sessions.to_string_lossy().into_owned();
        cfg.sources.git.scan_all_drives = false;
        cfg.sources.git.scan_roots = vec![];
        cfg.sources.git.repos = vec![repo.to_string_lossy().into_owned()];
        cfg.sources.naverworks.enabled = false;

        let sess_file = projects.join("D--repo").join("s1.jsonl");
        write_jsonl(
            &sess_file,
            &[
                json!({"type":"user","sessionId":"s1","cwd":repo.to_string_lossy(),"timestamp":t1,
                       "message":{"content":"첫 요청"}}),
                json!({"type":"assistant","timestamp":t1,
                       "message":{"content":[{"type":"text","text":"답"}],"usage":{"output_tokens":3}}}),
            ],
        );
        fs::write(repo.join("a.txt"), "x\n").unwrap();
        git(&repo, &["add", "."], &[]);
        git(
            &repo,
            &["commit", "-q", "-m", "feat: first"],
            &[("GIT_AUTHOR_DATE", &t1), ("GIT_COMMITTER_DATE", &t1)],
        );

        let store = Store::open_in_memory().unwrap();
        let mut live = Live::new(cfg, Some(&day.date.to_string())).unwrap();
        let now = Utc::now();
        let d = live.full_refresh(Some(&store), now);
        let kinds = |f: &Feed| f.items.iter().map(|i| i.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds(live.feed()),
            vec![FeedKind::Session, FeedKind::Commit]
        );
        assert_eq!(d.added.len(), 2);
        let st = live.statuses();
        assert_eq!(st[0].name, "git");
        assert_eq!(st[0].state, SourceState::Ok);
        assert_eq!(st[0].count, 1);
        assert_eq!(st[1].count, 1); // claude
        assert_eq!(st[2].state, SourceState::Ok); // codex 폴더는 있지만 파일 없음
        assert_eq!(st[3].state, SourceState::Disabled);
        let targets = live.watch_targets();
        assert_eq!(targets.git_common_dirs.len(), 1);
        assert!(targets.claude_projects.is_some());
        assert!(!live.is_stale(now));

        // 세션 파일에 질답이 붙음 → 그 파일만 다시 읽어 세션이 갱신
        write_jsonl(
            &sess_file,
            &[
                json!({"type":"user","sessionId":"s1","cwd":repo.to_string_lossy(),"timestamp":t1,
                       "message":{"content":"첫 요청"}}),
                json!({"type":"assistant","timestamp":t1,
                       "message":{"content":[{"type":"text","text":"답"}],"usage":{"output_tokens":3}}}),
                json!({"type":"user","sessionId":"s1","cwd":repo.to_string_lossy(),"timestamp":t2,
                       "message":{"content":"둘째 요청"}}),
            ],
        );
        let d = live.apply_changes(&[Changed::ClaudeFile(sess_file.clone())], now);
        assert_eq!(d.updated.len(), 1);
        assert_eq!(d.updated[0].kind, FeedKind::Session);
        assert_eq!(live.data().claude.as_ref().unwrap().sessions[0].qa.len(), 2);

        // 새 커밋 → 저장소만 다시 로그
        fs::write(repo.join("b.txt"), "y\n").unwrap();
        git(&repo, &["add", "."], &[]);
        git(
            &repo,
            &["commit", "-q", "-m", "fix: second"],
            &[("GIT_AUTHOR_DATE", &t2), ("GIT_COMMITTER_DATE", &t2)],
        );
        let common = PathBuf::from(&targets.git_common_dirs[0]);
        let d = live.apply_changes(&[Changed::GitRepo(common)], now);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].label, "fix: second");
        assert_eq!(live.feed().kpis.commits, 2);

        // 메모 저장 → 메모만 다시 읽음
        notes::add_note(
            &store,
            day.tz(),
            "#요청 @김팀장 확인",
            "app",
            Some(day.start.with_timezone(&Utc) + chrono::Duration::hours(11)),
        )
        .unwrap();
        let d = live.refresh_notes(Some(&store), now);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].kind, FeedKind::Note);
        assert_eq!(live.feed().kpis.notes, 1);

        // 세션 파일이 사라지면 피드에서도 빠진다
        fs::remove_file(&sess_file).unwrap();
        let d = live.refresh_claude_file(&sess_file, now);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(live.feed().kpis.sessions, 0);

        // 변경 없는 재적용은 델타 없음
        assert!(live.apply_changes(&[], now).is_empty());
    }

    #[test]
    fn disabled_and_missing_sources() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.sources.claude.projects_dir = dir.path().join("none").to_string_lossy().into_owned();
        cfg.sources.codex.enabled = false;
        cfg.sources.git.enabled = false;
        cfg.sources.naverworks.enabled = false;
        let mut live = Live::new(cfg, Some("2026-09-04")).unwrap();
        let d = live.full_refresh(None, Utc::now());
        assert!(d.is_empty());
        let st = live.statuses();
        assert_eq!(st[0].state, SourceState::Disabled);
        assert_eq!(st[1].state, SourceState::Skipped);
        assert!(st[1].note.as_deref().unwrap().contains("Claude 로그 폴더"));
        assert!(
            live.data()
                .warnings
                .iter()
                .any(|w| w.starts_with("[claude] 건너뜀"))
        );
        assert!(live.watch_targets().claude_projects.is_none());
        assert!(live.is_stale(Utc::now())); // 2026-09-04 는 오늘이 아님
    }
}
