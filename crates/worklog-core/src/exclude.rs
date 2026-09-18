//! 제외 글롭 — 요약 프롬프트에 **보내지 않을** 저장소·폴더.
//!
//! `sources.exclude` 의 글롭에 걸린 세션·커밋은
//!
//! * 요약기에 넣는 신호([`filter_for_signal`])에서 **통째로 빠지고**,
//! * 문서(사실 정리·지표)에는 [`PRIVATE_PROJECT`] 한 행의 **집계로만 남는다**
//!   ([`redact_for_document`]).
//!
//! 수집 자체는 막지 않는다 — "규정 있는 날 앱을 끈다"는 회피 행동이 하루를 통째로
//! 날리는 것을 대신하는 장치이기 때문이다. (product-plan §3 D7 · §4 원칙 4 · §5-1 N0)
//!
//! 매칭은 순수 함수다. Windows 경로를 염두에 두고 `\` 와 `/` 를 같게 보고
//! 대소문자를 구분하지 않는다.

use std::collections::HashSet;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::{
    model::{DailyData, GitCommit, GitData, Session, SessionData},
    render::{WORKLOG_SENTINEL, is_meta_session},
};

/// 제외된 저장소가 문서에 남는 이름. 여러 저장소가 걸려도 이 한 행으로 합쳐진다.
pub const PRIVATE_PROJECT: &str = "[비공개 프로젝트]";

/// 제외된 커밋의 제목 자리표시자 — 타입(`feat:` 등)만 남기고 내용은 지운다.
const PRIVATE_SUBJECT: &str = "(비공개)";

// --------------------------------------------------------------------------- //
// 매처
// --------------------------------------------------------------------------- //

/// 글롭 목록을 컴파일한 매처. 목록이 비었거나 전부 잘못된 글롭이면 아무것도 걸리지 않는다.
#[derive(Debug, Clone, Default)]
pub struct Excluder {
    set: Option<GlobSet>,
    patterns: Vec<String>,
}

impl Excluder {
    /// 설정의 글롭 목록으로 매처를 만든다. 이해할 수 없는 글롭은 경고만 남기고 건너뛴다
    /// (한 줄이 잘못됐다고 나머지 보호가 통째로 풀리면 안 된다).
    pub fn new(patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut kept: Vec<String> = Vec::new();
        for raw in patterns {
            let variants = variants(raw);
            if variants.is_empty() {
                continue;
            }
            let mut ok = false;
            for v in &variants {
                match GlobBuilder::new(v)
                    .case_insensitive(true)
                    .literal_separator(true)
                    .build()
                {
                    Ok(g) => {
                        builder.add(g);
                        ok = true;
                    }
                    Err(e) => tracing::warn!("제외 글롭을 이해하지 못했습니다({raw:?}): {e}"),
                }
            }
            if ok {
                kept.push(raw.trim().to_string());
            }
        }
        if kept.is_empty() {
            return Self::default();
        }
        match builder.build() {
            Ok(set) => Self {
                set: Some(set),
                patterns: kept,
            },
            Err(e) => {
                tracing::warn!("제외 목록을 컴파일하지 못했습니다: {e} — 제외 없이 진행합니다");
                Self::default()
            }
        }
    }

    /// 걸리는 글롭이 하나도 없는 상태(= 제외 기능 꺼짐).
    pub fn is_empty(&self) -> bool {
        self.set.is_none()
    }

    /// 살아 있는 글롭 목록(입력 그대로).
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// 경로 또는 이름 하나가 제외 대상인지.
    pub fn matches(&self, value: &str) -> bool {
        let Some(set) = &self.set else {
            return false;
        };
        let v = normalize(value);
        !v.is_empty() && set.is_match(&v)
    }

    fn matches_opt(&self, value: Option<&str>) -> bool {
        value.is_some_and(|v| self.matches(v))
    }

    /// 세션은 작업 폴더(cwd) · **물리 저장소 루트**(worktree 면 본체) · 프로젝트 이름으로 건다.
    ///
    /// cwd 만 보면 형제 worktree(`C:\…\workspaces\foo\wt`)가 `D:\works\foo\**` 를 빠져나간다.
    /// 루트는 [`crate::service::normalize_sessions`] 가 미리 채워 둔다(여기서는 디스크를 보지 않는다).
    pub fn matches_session(&self, s: &Session) -> bool {
        self.matches_opt(s.cwd.as_deref())
            || self.matches_opt(s.repo_root.as_deref())
            || self.matches_opt(s.project.as_deref())
    }

    /// 커밋은 저장소 경로(git-common-dir) 또는 저장소 이름으로 건다.
    pub fn matches_commit(&self, c: &GitCommit) -> bool {
        self.matches(&c.repo_path) || self.matches(&c.repo)
    }

    /// 이 데이터에 실제로 걸리는 항목이 있는지(로그·표시용).
    pub fn hits(&self, data: &DailyData) -> bool {
        if self.is_empty() {
            return false;
        }
        data.all_sessions().iter().any(|s| self.matches_session(s))
            || data
                .git
                .as_ref()
                .is_some_and(|g| g.commits.iter().any(|c| self.matches_commit(c)))
    }

    /// 글롭에 **직접** 걸린 세션·커밋이 속한 프로젝트·저장소 이름 집합(소문자 키).
    ///
    /// 경로로 걸러 내는 것만으로는 부족하다 — 형제 worktree 의 경로를 끝내 알아내지
    /// 못한 세션(폴더가 지워졌거나 git 을 못 읽은 경우)이 이름만 달고 남는다. 그래서
    /// "걸린 프로젝트는 통째로 간다"는 닫힘 규칙을 한 번 더 적용한다.
    pub fn excluded_projects(&self, data: &DailyData) -> HashSet<String> {
        let mut out: HashSet<String> = HashSet::new();
        if self.is_empty() {
            return out;
        }
        for s in data.all_sessions() {
            if self.matches_session(s)
                && let Some(key) = name_key(s.project.as_deref())
            {
                out.insert(key);
            }
        }
        if let Some(git) = &data.git {
            for c in &git.commits {
                if self.matches_commit(c)
                    && let Some(key) = name_key(Some(&c.repo))
                {
                    out.insert(key);
                }
            }
        }
        out
    }

    /// 세션이 빠져야 하는지 — 직접 걸렸거나, 걸린 프로젝트에 속하거나.
    /// `projects` 는 [`Excluder::excluded_projects`] 의 결과.
    pub fn session_excluded(&self, s: &Session, projects: &HashSet<String>) -> bool {
        self.matches_session(s) || in_projects(s.project.as_deref(), projects)
    }

    /// 커밋이 빠져야 하는지 — 직접 걸렸거나, 걸린 저장소 이름에 속하거나.
    pub fn commit_excluded(&self, c: &GitCommit, projects: &HashSet<String>) -> bool {
        self.matches_commit(c) || in_projects(Some(&c.repo), projects)
    }
}

/// 이름 하나 → 비교용 키(대소문자·슬래시 방향 무시). 빈 이름은 키가 없다 —
/// 이름을 모르는 세션까지 싸잡아 빼면 하루가 통째로 사라진다.
fn name_key(value: Option<&str>) -> Option<String> {
    let v = normalize(value?);
    let v = v.trim_matches('/');
    (!v.is_empty()).then(|| v.to_lowercase())
}

fn in_projects(value: Option<&str>, projects: &HashSet<String>) -> bool {
    !projects.is_empty() && name_key(value).is_some_and(|k| projects.contains(&k))
}

/// `\` → `/`, 앞뒤 공백과 끝의 `/` 를 떼어 낸 비교용 문자열.
fn normalize(value: &str) -> String {
    let v = value.trim().replace('\\', "/");
    let trimmed = v.trim_end_matches('/');
    if trimmed.is_empty() {
        v
    } else {
        trimmed.into()
    }
}

/// 글롭 한 줄 → 실제로 등록할 패턴들.
///
/// 사용자는 폴더를 적지 하위까지 도는 글롭을 적지 않는다. 그래서 적힌 것과 그 아래를 모두 건다:
/// `D:\works\a-corp` → `D:/works/a-corp` + `D:/works/a-corp/**`,
/// `D:\works\a-corp\**` → 둘 다(자기 자신도 포함). 구분자가 없는 이름(`a-corp`)은
/// 프로젝트·저장소 이름과 경로의 마지막 조각 양쪽에 걸리게 `**/a-corp`(+`/**`)도 넣는다.
fn variants(pattern: &str) -> Vec<String> {
    let p = normalize(pattern);
    if p.is_empty() {
        return Vec::new();
    }
    let mut out = vec![p.clone()];
    match p.strip_suffix("/**") {
        Some(base) if !base.is_empty() => out.push(base.to_string()),
        _ => out.push(format!("{p}/**")),
    }
    if !p.contains('/') {
        out.push(format!("**/{p}"));
        out.push(format!("**/{p}/**"));
    }
    out
}

// --------------------------------------------------------------------------- //
// 신호용 — 통째로 빼기
// --------------------------------------------------------------------------- //

/// 요약기에 보낼 사본. 걸린 세션·커밋은 흔적 없이 사라진다.
///
/// 일정·메모는 프로젝트에 매이지 않으므로 그대로 둔다(전송 고지가 밝히는 범위 그대로).
pub fn filter_for_signal(data: &DailyData, ex: &Excluder) -> DailyData {
    let mut out = data.clone();
    if ex.is_empty() {
        return out;
    }
    let projects = ex.excluded_projects(data);
    if let Some(git) = &mut out.git {
        git.commits.retain(|c| !ex.commit_excluded(c, &projects));
    }
    for sd in [out.claude.as_mut(), out.codex.as_mut()]
        .into_iter()
        .flatten()
    {
        sd.sessions.retain(|s| !ex.session_excluded(s, &projects));
    }
    out
}

// --------------------------------------------------------------------------- //
// 문서용 — [비공개 프로젝트] 한 행으로
// --------------------------------------------------------------------------- //

/// 문서(사실 정리·지표)용 사본. 걸린 항목은 지우지 않고 **이름과 내용만** 지운다 —
/// 커밋 수·변경 줄 수·세션 수·집중 시간·수정 파일 수는 [`PRIVATE_PROJECT`] 행에 그대로 남는다.
pub fn redact_for_document(data: &DailyData, ex: &Excluder) -> DailyData {
    let mut out = data.clone();
    if ex.is_empty() {
        return out;
    }
    let projects = ex.excluded_projects(data);
    if let Some(git) = &mut out.git {
        redact_commits(git, ex, &projects);
    }
    for sd in [out.claude.as_mut(), out.codex.as_mut()]
        .into_iter()
        .flatten()
    {
        redact_sessions(sd, ex, &projects);
    }
    out
}

fn redact_commits(git: &mut GitData, ex: &Excluder, projects: &HashSet<String>) {
    for c in &mut git.commits {
        if !ex.commit_excluded(c, projects) {
            continue;
        }
        // 타입(feat·fix…)은 살려 커밋 타입 분포가 어긋나지 않게 하고, 제목 내용만 지운다.
        let (key, _) = crate::analyze::classify_commit(&c.subject);
        c.subject = format!("{key}: {PRIVATE_SUBJECT}");
        c.repo = PRIVATE_PROJECT.to_string();
        c.repo_path = PRIVATE_PROJECT.to_string();
        c.author = String::new();
    }
}

fn redact_sessions(sd: &mut SessionData, ex: &Excluder, projects: &HashSet<String>) {
    for s in &mut sd.sessions {
        if !ex.session_excluded(s, projects) {
            continue;
        }
        // 자동요약(meta) 세션은 지표에서 빠져야 하는데 판별이 제목·요청문에 달려 있다.
        // 제목을 지우면 판별이 뒤집히므로 표식만 남겨 둔다.
        let meta = is_meta_session(s);
        s.project = Some(PRIVATE_PROJECT.to_string());
        s.cwd = None;
        s.repo_root = None;
        s.git_branch = None;
        s.title = None;
        s.intent = meta.then(|| WORKLOG_SENTINEL.to_string());
        // 질답은 내용만 지운다 — 시각은 집중시간 구간(유휴 컷)을 가르는 근거라 남겨야 집계가 맞는다.
        for turn in &mut s.qa {
            turn.question.clear();
            turn.answer.clear();
        }
        s.commands.clear();
        s.files_read.clear();
        // 파일은 '몇 개를 고쳤나'만 남긴다 — 이름은 지우되 중복 제거 수는 보존.
        for f in &mut s.files_edited {
            *f = opaque_file(f);
        }
    }
}

/// 파일 경로 → 이름을 알 수 없는 안정적인 표식. 같은 파일은 같은 표식이라 개수 집계가 맞는다.
fn opaque_file(path: &str) -> String {
    format!("[비공개 파일 {}]", fingerprint(path))
}

/// FNV-1a 64 → 16진 8자리.
fn fingerprint(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", h & 0xffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, QaTurn};
    use chrono::{NaiveDate, Utc};

    fn ex(pats: &[&str]) -> Excluder {
        Excluder::new(&pats.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn empty_list_matches_nothing() {
        let e = ex(&[]);
        assert!(e.is_empty());
        assert!(!e.matches("D:/works/a-corp"));
        assert!(e.patterns().is_empty());
        // 공백뿐인 줄도 없는 것과 같다.
        assert!(ex(&["  ", ""]).is_empty());
    }

    #[test]
    fn windows_and_posix_paths_match_case_insensitively() {
        let e = ex(&[r"D:\works\a-corp\**"]);
        assert!(!e.is_empty());
        // 적힌 폴더 자신과 그 아래 전부, 슬래시 방향·대소문자 무관.
        assert!(e.matches(r"D:\works\a-corp"));
        assert!(e.matches("D:/works/a-corp"));
        assert!(e.matches(r"d:\WORKS\A-Corp\api\src"));
        assert!(e.matches("D:/works/a-corp/api"));
        assert!(e.matches("D:/works/a-corp/"));
        // 형제 폴더는 걸리지 않는다.
        assert!(!e.matches(r"D:\works\a-corp-public"));
        assert!(!e.matches("D:/works/b-corp"));
    }

    #[test]
    fn folder_without_glob_covers_children() {
        let e = ex(&[r"D:\works\payments-core"]);
        assert!(e.matches(r"D:\works\payments-core"));
        assert!(e.matches(r"D:\works\payments-core\services\api"));
        assert!(!e.matches(r"D:\works\payments-core-docs"));
    }

    #[test]
    fn single_star_does_not_cross_separators() {
        let e = ex(&[r"D:\works\a-*"]);
        assert!(e.matches("D:/works/a-corp"));
        assert!(e.matches("D:/works/a-corp/src")); // 걸린 폴더의 하위는 포함
        assert!(!e.matches("D:/other/a-corp"));
        let deep = ex(&["D:/works/**/secret"]);
        assert!(deep.matches("D:/works/x/y/secret"));
        assert!(!deep.matches("D:/elsewhere/secret"));
    }

    #[test]
    fn bare_name_matches_project_name_and_any_path_segment() {
        let e = ex(&["a-corp"]);
        assert!(e.matches("a-corp")); // 세션 project · 커밋 repo
        assert!(e.matches(r"D:\works\a-corp"));
        assert!(e.matches("D:/works/a-corp/src"));
        assert!(!e.matches("a-corp-public"));
        // 이름 글롭도 같은 규칙으로 동작한다.
        assert!(ex(&["*-corp"]).matches("works/a-corp"));
    }

    #[test]
    fn broken_glob_is_skipped_without_losing_the_rest() {
        let e = ex(&["D:/works/a-corp", "["]);
        assert!(e.matches("D:/works/a-corp"));
        assert_eq!(e.patterns(), ["D:/works/a-corp"]);
    }

    // --- 데이터 변환 ---------------------------------------------------- //

    fn session(project: &str, cwd: &str) -> Session {
        Session {
            session_id: Some(format!("s-{project}")),
            project: Some(project.into()),
            cwd: Some(cwd.into()),
            git_branch: Some("feature/billing".into()),
            title: Some("정산 배치 오류 수정".into()),
            intent: Some("정산 배치가 왜 실패하는지 봐 줘".into()),
            agent: Agent::Claude,
            files_edited: vec![
                format!(r"{cwd}\src\billing.py"),
                format!(r"{cwd}\src\tax.py"),
            ],
            files_read: vec![format!(r"{cwd}\README.md")],
            commands: vec!["pytest tests/billing".into()],
            output_tokens: 1200,
            first_ts: Some(Utc::now()),
            last_ts: Some(Utc::now()),
            qa: vec![QaTurn {
                time: "14:03".into(),
                question: "정산 테이블 스키마가 뭐야".into(),
                answer: "settlement 테이블은…".into(),
            }],
            ..Default::default()
        }
    }

    fn commit(repo: &str, path: &str) -> GitCommit {
        GitCommit {
            repo: repo.into(),
            hash: "0123456789abcdef".into(),
            author: "me@a-corp.com".into(),
            when: Utc::now(),
            subject: "feat: 정산 배치 재시도 추가".into(),
            files_changed: 3,
            insertions: 120,
            deletions: 8,
            repo_path: path.into(),
        }
    }

    fn sample() -> DailyData {
        let mut data = DailyData::new(NaiveDate::from_ymd_opt(2026, 9, 18).unwrap(), "Asia/Seoul");
        data.claude = Some(SessionData {
            sessions: vec![
                session("a-corp-billing", r"D:\works\a-corp\billing"),
                session("worklog", r"D:\study\Daily Work Log"),
            ],
        });
        data.git = Some(GitData {
            commits: vec![
                commit("a-corp-billing", r"D:\works\a-corp\billing\.git"),
                commit("worklog", r"D:\study\Daily Work Log\.git"),
            ],
        });
        data
    }

    #[test]
    fn signal_copy_drops_matched_sessions_and_commits() {
        let data = sample();
        let out = filter_for_signal(&data, &ex(&[r"D:\works\a-corp\**"]));
        let sessions = out.claude.as_ref().unwrap();
        assert_eq!(sessions.sessions.len(), 1);
        assert_eq!(sessions.sessions[0].project.as_deref(), Some("worklog"));
        let commits = &out.git.as_ref().unwrap().commits;
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].repo, "worklog");
        // 제외가 비면 원본 그대로.
        assert_eq!(filter_for_signal(&data, &ex(&[])), data);
    }

    #[test]
    fn document_copy_keeps_counts_but_no_names() {
        let data = sample();
        let out = redact_for_document(&data, &ex(&["a-corp-billing"]));
        let s = &out.claude.as_ref().unwrap().sessions[0];
        assert_eq!(s.project.as_deref(), Some(PRIVATE_PROJECT));
        assert!(s.cwd.is_none() && s.title.is_none() && s.intent.is_none());
        assert!(s.git_branch.is_none() && s.commands.is_empty() && s.files_read.is_empty());
        // 질답은 시각만 남고 내용은 사라진다(집중시간 구간 계산이 어긋나지 않게).
        assert_eq!(s.qa.len(), 1);
        assert_eq!(s.qa[0].time, "14:03");
        assert!(s.qa[0].question.is_empty() && s.qa[0].answer.is_empty());
        // 개수·토큰·시각은 남는다.
        assert_eq!(s.files_edited.len(), 2);
        assert_eq!(s.output_tokens, 1200);
        assert_eq!(
            s.first_ts,
            data.claude.as_ref().unwrap().sessions[0].first_ts
        );
        assert!(
            s.files_edited
                .iter()
                .all(|f| f.starts_with("[비공개 파일 "))
        );
        // 같은 파일은 같은 표식 — 중복 제거 개수가 어긋나지 않는다.
        assert_eq!(opaque_file(r"D:\a\b.rs"), opaque_file(r"D:\a\b.rs"));
        assert_ne!(opaque_file(r"D:\a\b.rs"), opaque_file(r"D:\a\c.rs"));

        let c = &out.git.as_ref().unwrap().commits[0];
        assert_eq!(c.repo, PRIVATE_PROJECT);
        assert_eq!(c.subject, "feat: (비공개)");
        assert_eq!((c.insertions, c.deletions, c.files_changed), (120, 8, 3));
        // 커밋 타입은 그대로 분류된다.
        assert_eq!(crate::analyze::classify_commit(&c.subject).1, "기능");
        // 걸리지 않은 쪽은 손대지 않는다.
        assert_eq!(
            out.claude.as_ref().unwrap().sessions[1],
            data.claude.as_ref().unwrap().sessions[1]
        );
    }

    #[test]
    fn meta_sessions_stay_meta_after_redaction() {
        let mut data = sample();
        let mut meta = session("a-corp-billing", r"D:\works\a-corp\billing");
        meta.session_id = Some("meta".into());
        meta.intent = Some(format!("{WORKLOG_SENTINEL} 자동 요약"));
        data.claude.as_mut().unwrap().sessions.push(meta);
        let out = redact_for_document(&data, &ex(&["a-corp-billing"]));
        let redacted = out.claude.as_ref().unwrap().sessions.last().unwrap();
        assert!(is_meta_session(redacted));
        assert_eq!(redacted.intent.as_deref(), Some(WORKLOG_SENTINEL));
    }

    // --- worktree · 프로젝트 닫힘 ---------------------------------------- //

    #[test]
    fn worktree_session_matches_by_physical_repo_root() {
        // 형제 worktree 는 cwd 가 글롭 밖(다른 드라이브)에 있다 — 본체 루트로 걸려야 한다.
        let mut s = session(
            "ai-web-novel",
            r"C:\Users\me\orca\workspaces\ai-web-novel\wt",
        );
        s.project = Some("ai-web-novel".into());
        let e = ex(&[r"D:\study\ai-web-novel\**"]);
        assert!(!e.matches_session(&s)); // 루트를 모르면 cwd·이름만으로는 못 건다
        s.repo_root = Some(r"D:\study\ai-web-novel".into());
        assert!(e.matches_session(&s));
    }

    #[test]
    fn excluded_project_takes_its_siblings_with_it() {
        let mut data = sample();
        // 경로로는 어디에도 걸리지 않지만 프로젝트 이름이 같은 세션(루트 해석 실패).
        let mut orphan = session("a-corp-billing", r"C:\tmp\detached-worktree");
        orphan.session_id = Some("orphan".into());
        data.claude.as_mut().unwrap().sessions.push(orphan);
        // 커밋도 마찬가지 — 이름만 같고 경로는 딴 곳.
        data.git
            .as_mut()
            .unwrap()
            .commits
            .push(commit("a-corp-billing", r"C:\tmp\detached-worktree\.git"));

        let e = ex(&[r"D:\works\a-corp\**"]);
        let projects = e.excluded_projects(&data);
        assert!(projects.contains("a-corp-billing"));
        assert!(!projects.contains("worklog"));

        let sig = filter_for_signal(&data, &e);
        let left = &sig.claude.as_ref().unwrap().sessions;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].project.as_deref(), Some("worklog"));
        let commits = &sig.git.as_ref().unwrap().commits;
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].repo, "worklog");

        // 문서에서는 셋 다 한 행으로 합쳐진다(개수는 그대로).
        let doc = redact_for_document(&data, &e);
        let projects_in_doc: Vec<&str> = doc
            .all_sessions()
            .iter()
            .filter_map(|s| s.project.as_deref())
            .collect();
        assert_eq!(
            projects_in_doc,
            [PRIVATE_PROJECT, "worklog", PRIVATE_PROJECT]
        );
        assert_eq!(
            doc.git.as_ref().unwrap().repos(),
            vec![PRIVATE_PROJECT, "worklog"]
        );
        assert_eq!(doc.git.as_ref().unwrap().commits.len(), 3);
    }

    #[test]
    fn project_closure_ignores_nameless_and_is_case_insensitive() {
        let mut data = sample();
        // 이름 없는 세션은 닫힘 규칙에 걸리지 않는다(하루가 통째로 사라지면 안 된다).
        let mut nameless = session("", r"C:\tmp\unknown");
        nameless.session_id = Some("nameless".into());
        nameless.project = None;
        data.claude.as_mut().unwrap().sessions.push(nameless);
        // 대소문자·슬래시 방향만 다른 같은 프로젝트는 함께 빠진다.
        let mut shouty = session("A-Corp-Billing", r"C:\tmp\other");
        shouty.session_id = Some("shouty".into());
        data.claude.as_mut().unwrap().sessions.push(shouty);

        let out = filter_for_signal(&data, &ex(&[r"D:\works\a-corp\**"]));
        let ids: Vec<&str> = out
            .claude
            .as_ref()
            .unwrap()
            .sessions
            .iter()
            .filter_map(|s| s.session_id.as_deref())
            .collect();
        assert_eq!(ids, ["s-worklog", "nameless"]);
    }

    #[test]
    fn several_excluded_repos_merge_into_one_row() {
        let mut data = sample();
        data.git
            .as_mut()
            .unwrap()
            .commits
            .push(commit("a-corp-pay", r"D:\works\a-corp\pay\.git"));
        let out = redact_for_document(&data, &ex(&[r"D:\works\a-corp\**"]));
        assert_eq!(
            out.git.as_ref().unwrap().repos(),
            vec![PRIVATE_PROJECT, "worklog"]
        );
    }
}
