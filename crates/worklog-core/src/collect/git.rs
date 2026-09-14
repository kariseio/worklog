//! Git 수집기 — 설정된 저장소 + 자동 탐색 + Claude/Codex 작업 폴더에서 '그날 내 커밋'을 모은다.
//!
//! v1 은 저장소마다 `git log` 프로세스를 띄웠지만, v2 는 gix(gitoxide)로 인프로세스에서
//! revwalk 와 tree diff(numstat)를 수행한다. 프로세스 spawn 이 없다.
//!
//! 동작 규칙(v1 과 동일):
//!   - HEAD + 로컬 브랜치 + 원격 추적 브랜치 전부에서 출발, 머지 커밋 제외.
//!   - 커밋 날짜(committer date) 기준 `[그날 00:00, 다음날 00:00)`.
//!   - 작성자 필터: `author` 명시값 → 그것만. 없으면 `authors`(추가 신원) + 저장소 git 사용자
//!     (user.email 없으면 user.name)를 OR. 이메일은 '핸들@' 패턴이라 도메인이 달라도 잡힌다.
//!   - 같은 물리 저장소(worktree 포함)는 git-common-dir 로 하나로 합친다.

use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use gix::bstr::ByteSlice;
use rayon::prelude::*;
use regex::RegexSet;

use super::{CollectContext, Collector, CollectorResult, scan};
use crate::{
    config::GitConfig,
    drives::fixed_drives,
    model::{GitCommit, GitData},
    paths,
};

/// 저장소 식별 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    /// 후보로 들어온 경로(작업 트리 안 어딘가일 수 있음).
    pub path: PathBuf,
    /// git-common-dir(정규화). 물리 저장소 식별 키 — worktree 들이 같은 값을 공유한다.
    pub common_dir: String,
    /// 표시 이름(저장소 루트 폴더명).
    pub name: String,
}

// --------------------------------------------------------------------------- //
// 식별
// --------------------------------------------------------------------------- //

/// `path` 가 속한 저장소의 공용 `.git` 경로(정규화). 저장소가 아니면 None.
pub fn common_dir_of(path: &Path) -> Option<PathBuf> {
    let repo = gix::discover(path).ok()?;
    let common = repo.common_dir();
    Some(dunce::canonicalize(common).unwrap_or_else(|_| common.to_path_buf()))
}

/// git-common-dir(`…/.git`) → 저장소 루트 경로.
pub fn repo_root_of(common_dir: &Path) -> PathBuf {
    if common_dir.file_name().is_some_and(|n| n == ".git") {
        common_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| common_dir.to_path_buf())
    } else {
        common_dir.to_path_buf()
    }
}

fn file_name_string(p: &Path) -> Option<String> {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}

/// 후보 경로 → 저장소 식별. 저장소가 아니면 None.
pub fn identify(path: &Path) -> Option<RepoIdentity> {
    let common = common_dir_of(path)?;
    let root = repo_root_of(&common);
    let name = file_name_string(&root)
        .or_else(|| {
            dunce::canonicalize(path)
                .ok()
                .and_then(|p| file_name_string(&p))
        })
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    Some(RepoIdentity {
        path: path.to_path_buf(),
        common_dir: common.to_string_lossy().into_owned(),
        name,
    })
}

/// 후보 경로들을 병렬로 식별하고 입력 순서를 보존한다(저장소 아니면 제외).
pub fn identify_many(paths: &[PathBuf]) -> Vec<RepoIdentity> {
    paths
        .par_iter()
        .map(|p| identify(p))
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}

/// 같은 물리 저장소는 첫 번째 것만 남긴다(first-seen).
pub fn dedupe_by_common_dir(idents: Vec<RepoIdentity>) -> Vec<RepoIdentity> {
    let mut out: Vec<RepoIdentity> = Vec::new();
    for i in idents {
        if !out.iter().any(|o| o.common_dir == i.common_dir) {
            out.push(i);
        }
    }
    out
}

// --------------------------------------------------------------------------- //
// 작성자 필터
// --------------------------------------------------------------------------- //

/// 이메일/이름 하나 → 작성자 정규식 패턴.
///
/// 이메일이면 로컬파트(@ 앞)를 `핸들@` 로 → 도메인이 달라도(회사·개인·github noreply) 같은 핸들이면
/// 매칭, 남의 '이름'엔 안 걸림. 로컬파트가 너무 짧으면(흔한 핸들 오검출 위험) 전체 이메일로.
pub fn identity_pattern(identity: &str) -> Option<String> {
    let id = identity.trim();
    if id.is_empty() {
        return None;
    }
    if let Some((local, _)) = id.split_once('@') {
        if local.chars().count() >= 3 {
            return Some(format!("{}@", regex::escape(local)));
        }
        return Some(regex::escape(id));
    }
    Some(regex::escape(id))
}

/// 저장소에 유효한 git 사용자(user.email, 없으면 user.name) → 패턴. 둘 다 없으면 None.
fn repo_identity_pattern(repo: &gix::Repository) -> Option<String> {
    let cfg = repo.config_snapshot();
    for key in ["user.email", "user.name"] {
        if let Some(v) = cfg.string(key) {
            let s = v.to_str_lossy();
            if !s.trim().is_empty() {
                return identity_pattern(&s);
            }
        }
    }
    None
}

/// UI 표시용 '자동 감지' 신원: 전역 git user.email(없으면 user.name) 하나.
pub fn detected_identities() -> Vec<String> {
    let Ok(file) = gix::config::File::from_globals() else {
        return Vec::new();
    };
    for key in ["user.email", "user.name"] {
        if let Some(v) = file.string(key) {
            let s = v.to_str_lossy().trim().to_string();
            if !s.is_empty() {
                return vec![s];
            }
        }
    }
    Vec::new()
}

fn dedupe_strings(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in items {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

// --------------------------------------------------------------------------- //
// 로그
// --------------------------------------------------------------------------- //

/// 한 저장소의 그날 커밋(작성자 필터 적용, 머지 제외, numstat 포함).
pub fn log_repo(
    ident: &RepoIdentity,
    cfg: &GitConfig,
    ctx: &CollectContext,
) -> Result<Vec<GitCommit>, String> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;

    let mut repo = gix::discover(&ident.path).map_err(|e| e.to_string())?;
    // 같은 객체를 여러 번 읽지 않게(revwalk + tree diff).
    repo.object_cache_size_if_unset(8 * 1024 * 1024);

    let start = ctx.day.start.timestamp();
    let end = ctx.day.end.timestamp();

    // 작성자 패턴: 명시값이 있으면 그것만, 없으면 추가 신원 + 저장소 자동감지(OR).
    let patterns: Vec<String> = if !cfg.author.trim().is_empty() {
        vec![cfg.author.trim().to_string()]
    } else {
        let mut v: Vec<String> = cfg
            .authors
            .iter()
            .filter_map(|a| identity_pattern(a))
            .collect();
        v.extend(repo_identity_pattern(&repo));
        dedupe_strings(v)
    };
    let author_set = if patterns.is_empty() {
        None
    } else {
        Some(RegexSet::new(&patterns).map_err(|e| format!("작성자 패턴 오류: {e}"))?)
    };

    // 출발점: HEAD + 로컬 브랜치 + 원격 추적 브랜치.
    let mut tips: Vec<gix::ObjectId> = Vec::new();
    if let Ok(id) = repo.head_id() {
        tips.push(id.detach());
    }
    if let Ok(refs) = repo.references() {
        for iter in [refs.local_branches(), refs.remote_branches()] {
            let Ok(iter) = iter else { continue };
            for r in iter.flatten() {
                if let Ok(id) = r.into_fully_peeled_id() {
                    tips.push(id.detach());
                }
            }
        }
    }
    tips.sort();
    tips.dedup();
    if tips.is_empty() {
        return Ok(Vec::new()); // 커밋 없는(unborn) 저장소
    }

    // 최신순으로 걷되, `start` 보다 오래된 커밋이 연속 SLOP 개 나오면 멈춘다.
    // (git 의 --since 와 같은 방식: 자식이 부모보다 오래된 시계 뒤틀림·amend 를 몇 개는 견딘다.
    //  gix 의 ByCommitTimeCutoff 는 첫 오래된 커밋에서 바로 멈춰 그 뒤의 오늘 커밋을 놓친다.)
    const SLOP: u32 = 5;
    let walk = repo
        .rev_walk(tips)
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    let mut older_streak = 0u32;
    for info in walk {
        let info = info.map_err(|e| e.to_string())?;
        let commit = info.object().map_err(|e| e.to_string())?;
        let committer = commit.committer().map_err(|e| e.to_string())?;
        let ctime = committer.time().map_err(|e| e.to_string())?.seconds;
        if ctime < start {
            older_streak += 1;
            if older_streak > SLOP {
                break;
            }
            continue;
        }
        older_streak = 0;
        if info.parent_ids.len() > 1 {
            continue; // --no-merges
        }
        if ctime >= end {
            continue; // 커밋 날짜 기준 [start, end)
        }
        let author = commit.author().map_err(|e| e.to_string())?;
        let author_name = author.name.to_str_lossy().into_owned();
        if let Some(set) = &author_set {
            let ident_line = format!("{author_name} <{}>", author.email.to_str_lossy());
            if !set.is_match(&ident_line) {
                continue;
            }
        }
        let subject = commit
            .message()
            .map_err(|e| e.to_string())?
            .summary()
            .to_str_lossy()
            .into_owned();

        // numstat: 첫 부모(없으면 빈 트리)와의 tree diff.
        let tree = commit.tree().map_err(|e| e.to_string())?;
        let parent_tree = match info.parent_ids.first() {
            Some(pid) => repo
                .find_commit(*pid)
                .map_err(|e| e.to_string())?
                .tree()
                .map_err(|e| e.to_string())?,
            None => repo.empty_tree(),
        };
        let (files_changed, insertions, deletions) = numstat(&repo, &parent_tree, &tree)?;

        out.push(GitCommit {
            repo: ident.name.clone(),
            hash: info.id.to_hex().to_string(),
            author: author_name,
            when: Utc.timestamp_opt(ctime, 0).single().unwrap_or_default(),
            subject,
            files_changed,
            insertions,
            deletions,
            repo_path: ident.common_dir.clone(),
        });
    }
    Ok(out)
}

/// `git log --numstat` 과 같은 (파일 수, 추가 줄, 삭제 줄). 바이너리 파일은 파일 수에만 들어간다.
fn numstat(
    repo: &gix::Repository,
    old: &gix::Tree<'_>,
    new: &gix::Tree<'_>,
) -> Result<(u32, u32, u32), String> {
    use gix::object::tree::diff::Change;
    use std::ops::ControlFlow;

    let mut cache = repo
        .diff_resource_cache_for_tree_diff()
        .map_err(|e| e.to_string())?;
    let (mut files, mut ins, mut del) = (0u32, 0u32, 0u32);
    old.changes()
        .map_err(|e| e.to_string())?
        .for_each_to_obtain_tree(
            new,
            |change| -> Result<ControlFlow<()>, std::convert::Infallible> {
                let is_tree = match &change {
                    Change::Addition { entry_mode, .. }
                    | Change::Deletion { entry_mode, .. }
                    | Change::Modification { entry_mode, .. }
                    | Change::Rewrite { entry_mode, .. } => entry_mode.is_tree(),
                };
                if is_tree {
                    return Ok(ControlFlow::Continue(()));
                }
                files += 1;
                if let Ok(mut platform) = change.diff(&mut cache)
                    && let Ok(Some(counts)) = platform.line_counts()
                {
                    ins += counts.insertions;
                    del += counts.removals;
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .map_err(|e| e.to_string())?;
    Ok((files, ins, del))
}

// --------------------------------------------------------------------------- //
// 수집기
// --------------------------------------------------------------------------- //

pub struct GitCollector {
    cfg: GitConfig,
    /// Claude/Codex 세션의 작업 폴더(저장소 하위 폴더여도 됨).
    extra_repos: Vec<String>,
    /// 테스트용: `scan_all_drives` 일 때 쓸 드라이브 루트 대체.
    drive_roots_override: Option<Vec<String>>,
    /// 이미 알고 있는 저장소(캐시). 있으면 디스크 탐색을 건너뛰고 이 목록을 쓴다.
    known_repos: Option<Vec<PathBuf>>,
}

impl GitCollector {
    pub fn new(cfg: GitConfig) -> Self {
        Self {
            cfg,
            extra_repos: Vec::new(),
            drive_roots_override: None,
            known_repos: None,
        }
    }

    /// 디스크 탐색 대신 캐시된 저장소 목록을 쓴다(설정 `repos` 와 세션 cwd 는 그대로 더해진다).
    pub fn with_known_repos(mut self, repos: Vec<PathBuf>) -> Self {
        self.known_repos = Some(repos);
        self
    }

    pub fn with_extra_repos(mut self, cwds: Vec<String>) -> Self {
        self.extra_repos = cwds;
        self
    }

    pub fn with_drive_roots_override(mut self, roots: Vec<String>) -> Self {
        self.drive_roots_override = Some(roots);
        self
    }

    /// (경로, 저장소명, 식별키) 목록. 같은 저장소의 여러 worktree 는 하나로 합친다.
    ///
    /// 후보: repos → scan_roots(또는 모든 고정 디스크) → Claude/Codex cwd 순서.
    pub fn resolve_repos(&self, warnings: &mut Vec<String>) -> Vec<RepoIdentity> {
        let mut candidates: Vec<PathBuf> = Vec::new();

        for r in &self.cfg.repos {
            let p = paths::expand_user(r);
            if p.join(".git").exists() {
                candidates.push(p);
            } else if p.exists() {
                warnings.push(format!("git 저장소가 아님(.git 없음): {r}"));
            } else {
                warnings.push(format!("경로 없음: {r}"));
            }
        }

        if let Some(known) = &self.known_repos {
            candidates.extend(known.iter().cloned());
        } else {
            let roots: Vec<PathBuf> = if self.cfg.scan_all_drives {
                self.drive_roots_override
                    .clone()
                    .unwrap_or_else(fixed_drives)
                    .into_iter()
                    .map(PathBuf::from)
                    .collect()
            } else {
                self.cfg
                    .scan_roots
                    .iter()
                    .map(|r| paths::expand_user(r))
                    .collect()
            };
            candidates.extend(scan::scan_roots(&roots, self.cfg.scan_depth));
        }

        if self.cfg.include_claude_cwds {
            // cwd 가 저장소 하위폴더여도 되도록 .git 검사 없이 후보로 넣는다(discover 가 위로 올라감).
            candidates.extend(
                self.extra_repos
                    .iter()
                    .filter(|c| !c.trim().is_empty())
                    .map(|c| paths::expand_user(c)),
            );
        }

        dedupe_by_common_dir(identify_many(&candidates))
    }
}

impl Collector for GitCollector {
    type Data = GitData;
    const NAME: &'static str = "git";

    fn collect(&self, ctx: &CollectContext) -> CollectorResult<GitData> {
        let mut warnings = Vec::new();
        let repos = self.resolve_repos(&mut warnings);
        if repos.is_empty() {
            return CollectorResult::skip(
                Self::NAME,
                "감시할 git 저장소가 없습니다. 설정의 저장소 / 스캔 범위를 확인하세요.",
            );
        }

        // 저장소별 병렬. 개별 저장소 실패는 전체를 막지 않는다.
        let results: Vec<Result<Vec<GitCommit>, String>> = repos
            .par_iter()
            .map(|ident| {
                log_repo(ident, &self.cfg, ctx)
                    .map_err(|e| format!("git 로그 실패({}): {e}", ident.path.display()))
            })
            .collect();

        let mut commits: Vec<GitCommit> = Vec::new();
        for r in results {
            match r {
                Ok(cs) => commits.extend(cs),
                Err(w) => warnings.push(w),
            }
        }
        commits.sort_by_key(|c| c.when);
        CollectorResult::ok_with_warnings(Self::NAME, GitData { commits }, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{DayBounds, get_tz};
    use std::{fs, process::Command};

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn git(repo: &Path, args: &[&str], env: &[(&str, &str)]) {
        let mut c = Command::new("git");
        c.arg("-C").arg(repo).args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().expect("git 실행");
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo(dir: &Path, email: &str) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"], &[]);
        git(dir, &["config", "user.email", email], &[]);
        git(dir, &["config", "user.name", "Me"], &[]);
        git(dir, &["config", "commit.gpgsign", "false"], &[]);
    }

    /// 파일 하나 만들고 커밋. `when` 은 (author, committer) ISO 시각.
    fn commit(
        dir: &Path,
        msg: &str,
        content: &str,
        when: (&str, &str),
        ident: Option<(&str, &str)>,
    ) {
        fs::write(
            dir.join(format!("{}.txt", msg.replace(['/', ':', ' '], "_"))),
            content,
        )
        .unwrap();
        git(dir, &["add", "."], &[]);
        let mut env = vec![("GIT_AUTHOR_DATE", when.0), ("GIT_COMMITTER_DATE", when.1)];
        if let Some((name, email)) = ident {
            env.extend([
                ("GIT_AUTHOR_NAME", name),
                ("GIT_AUTHOR_EMAIL", email),
                ("GIT_COMMITTER_NAME", name),
                ("GIT_COMMITTER_EMAIL", email),
            ]);
        }
        git(dir, &["commit", "-q", "-m", msg], &env);
    }

    fn today_ctx() -> (CollectContext, String) {
        let tz = get_tz("Asia/Seoul");
        let now = Utc::now().with_timezone(&tz);
        let day = DayBounds::for_date(now.date_naive(), tz);
        // 자정 직후 실행되어도 그날 안에 들어가게, 그날 정오로 고정.
        let noon = day.start + chrono::Duration::hours(12);
        (CollectContext::new(day, "Asia/Seoul"), noon.to_rfc3339())
    }

    fn collect(
        cfg: GitConfig,
        extra: Vec<String>,
        ctx: &CollectContext,
    ) -> CollectorResult<GitData> {
        GitCollector::new(cfg).with_extra_repos(extra).collect(ctx)
    }

    fn cfg_repos(repos: &[&Path]) -> GitConfig {
        GitConfig {
            repos: repos
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            scan_roots: vec![],
            scan_all_drives: false,
            ..Default::default()
        }
    }

    #[test]
    fn collects_todays_commit_with_numstat() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo, "me@example.com");
        let (ctx, now) = today_ctx();
        commit(&repo, "feat: 첫 커밋", "hello\nworld\n", (&now, &now), None);

        let res = collect(cfg_repos(&[&repo]), vec![], &ctx);
        assert!(res.ok && !res.skipped, "{:?}", res.warnings);
        let data = res.data.unwrap();
        assert_eq!(data.commits.len(), 1);
        let c = &data.commits[0];
        assert_eq!(c.subject, "feat: 첫 커밋");
        assert_eq!(c.insertions, 2);
        assert_eq!(c.deletions, 0);
        assert_eq!(c.files_changed, 1);
        assert_eq!(c.repo, "repo");
        assert_eq!(c.hash.len(), 40);
        assert!(!c.repo_path.is_empty());
        assert!(ctx.day.contains(&c.when));
    }

    #[test]
    fn only_my_commits_across_domains_and_authors_or() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo, "myhandle@example.com"); // 내 핸들 = myhandle
        let (ctx, now) = today_ctx();
        commit(&repo, "mine", "x\n", (&now, &now), None);
        commit(
            &repo,
            "mine_otherdomain",
            "x\n",
            (&now, &now),
            Some(("Me", "myhandle@other.org")),
        );
        commit(
            &repo,
            "teammate",
            "x\n",
            (&now, &now),
            Some(("Teammate", "team@x.com")),
        );
        commit(
            &repo,
            "alt",
            "x\n",
            (&now, &now),
            Some(("Alt", "othername@other.org")),
        );

        let subjects = |res: CollectorResult<GitData>| {
            let mut v: Vec<String> = res
                .data
                .unwrap()
                .commits
                .into_iter()
                .map(|c| c.subject)
                .collect();
            v.sort();
            v
        };
        // 도메인 달라도 내 핸들이면 잡고, 팀원·다른 신원은 제외
        assert_eq!(
            subjects(collect(cfg_repos(&[&repo]), vec![], &ctx)),
            vec!["mine", "mine_otherdomain"]
        );
        // 추가 신원(authors) OR
        let cfg = GitConfig {
            authors: vec!["othername@other.org".into()],
            ..cfg_repos(&[&repo])
        };
        assert_eq!(
            subjects(collect(cfg, vec![], &ctx)),
            vec!["alt", "mine", "mine_otherdomain"]
        );
        // author 명시값은 그것만
        let cfg = GitConfig {
            author: "Teammate".into(),
            ..cfg_repos(&[&repo])
        };
        assert_eq!(subjects(collect(cfg, vec![], &ctx)), vec!["teammate"]);
        assert_eq!(
            identity_pattern("myhandle@example.com").as_deref(),
            Some("myhandle@")
        );
        assert_eq!(identity_pattern("ab@x.com").as_deref(), Some("ab@x\\.com"));
        assert_eq!(identity_pattern("Some Name").as_deref(), Some("Some Name"));
        assert_eq!(identity_pattern("  "), None);
    }

    #[test]
    fn claude_cwd_subdir_discovers_enclosing_repo() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo, "me@example.com");
        fs::create_dir_all(repo.join("backend")).unwrap();
        let (ctx, now) = today_ctx();
        fs::write(repo.join("backend").join("a.txt"), "x\n").unwrap();
        git(&repo, &["add", "."], &[]);
        git(
            &repo,
            &["commit", "-q", "-m", "하위폴더 작업"],
            &[("GIT_AUTHOR_DATE", &now), ("GIT_COMMITTER_DATE", &now)],
        );

        let cfg = GitConfig {
            repos: vec![],
            scan_roots: vec![],
            scan_all_drives: false,
            ..Default::default()
        };
        let res = collect(
            cfg,
            vec![repo.join("backend").to_string_lossy().into_owned()],
            &ctx,
        );
        assert!(
            res.data
                .unwrap()
                .commits
                .iter()
                .any(|c| c.subject == "하위폴더 작업")
        );
    }

    #[test]
    fn uses_committer_date_not_author_and_excludes_other_days() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo, "me@example.com");
        let (ctx, now) = today_ctx();
        let yesterday = (ctx.day.start - chrono::Duration::hours(12)).to_rfc3339();
        commit(&repo, "amended", "x\n", (&yesterday, &now), None); // 저자=어제, 커밋=오늘
        commit(&repo, "old", "y\n", (&yesterday, &yesterday), None); // 둘 다 어제

        let data = collect(cfg_repos(&[&repo]), vec![], &ctx).data.unwrap();
        assert_eq!(data.commits.len(), 1);
        assert_eq!(data.commits[0].subject, "amended");
    }

    #[test]
    fn no_repos_skips_and_bad_paths_warn() {
        let cfg = GitConfig {
            repos: vec!["Z:/definitely/missing".into()],
            scan_roots: vec![],
            scan_all_drives: false,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let mut cfg2 = cfg.clone();
        cfg2.repos.push(dir.path().to_string_lossy().into_owned()); // 존재하지만 저장소 아님
        let c = GitCollector::new(cfg2);
        let mut warnings = Vec::new();
        assert!(c.resolve_repos(&mut warnings).is_empty());
        assert!(warnings.iter().any(|w| w.contains("경로 없음")));
        assert!(warnings.iter().any(|w| w.contains(".git 없음")));
        let (ctx, _) = today_ctx();
        assert!(c.collect(&ctx).skipped);
    }

    #[test]
    fn scan_roots_parallel_and_drive_override() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("roots");
        let (ctx, now) = today_ctx();
        for i in 0..3 {
            let r = root.join(format!("grp{i}")).join(format!("repo{i}"));
            init_repo(&r, "me@x");
            commit(&r, &format!("feat: c{i}"), "a\n", (&now, &now), None);
        }
        let cfg = GitConfig {
            repos: vec![],
            scan_roots: vec![root.to_string_lossy().into_owned()],
            scan_all_drives: false,
            scan_depth: 5,
            ..Default::default()
        };
        let data = collect(cfg, vec![], &ctx).data.unwrap();
        let mut repos: Vec<String> = data.commits.iter().map(|c| c.repo.clone()).collect();
        repos.sort();
        assert_eq!(repos, vec!["repo0", "repo1", "repo2"]);

        // scan_all_drives: 실제 디스크 대신 임시 폴더를 '드라이브'로
        let cfg = GitConfig {
            repos: vec![],
            scan_roots: vec![],
            scan_all_drives: true,
            scan_depth: 3,
            ..Default::default()
        };
        let res = GitCollector::new(cfg)
            .with_drive_roots_override(vec![root.to_string_lossy().into_owned()])
            .collect(&ctx);
        assert_eq!(res.data.unwrap().commits.len(), 3);
    }

    #[test]
    fn identify_dedupes_worktrees_and_same_paths() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("A").join("repoA");
        init_repo(&a, "me@x");
        let (_, now) = today_ctx();
        commit(&a, "init", "a\n", (&now, &now), None);
        let wt = dir.path().join("wt");
        git(
            &a,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "feature",
            ],
            &[],
        );

        let idents = identify_many(&[
            a.clone(),
            a.clone(),
            wt.clone(),
            a.join("nested").join("deeper"),
        ]);
        // 마지막 후보는 존재하지 않는 폴더 → discover 실패 → 제외
        assert_eq!(idents.len(), 3);
        let deduped = dedupe_by_common_dir(idents);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].name, "repoA");
        assert_eq!(deduped[0].path, a);
        assert!(identify(dir.path()).is_none());
    }
}
