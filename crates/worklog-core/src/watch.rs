//! 파일 감시 — Claude/Codex 세션 로그 폴더와 활성 git 저장소의 reflog 를 지켜보다가
//! 변경을 디바운스(잠깐 모아서)해 "무엇이 바뀌었나"로 알린다.
//!
//! notify(Windows: ReadDirectoryChangesW) 핸들 몇 개로 끝나며, 폴링이 없다.
//! 소비자는 [`FileWatcher::receiver`] 에서 `Vec<Changed>` 배치를 받아 해당 소스만 다시 읽는다.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// 감시 대상.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchTargets {
    /// `~/.claude/projects`
    pub claude_projects: Option<PathBuf>,
    /// `~/.codex/sessions`
    pub codex_sessions: Option<PathBuf>,
    /// 저장소 git-common-dir 들(`…/.git`). `logs/`(reflog)와 `worktrees/` 를 본다.
    pub git_common_dirs: Vec<PathBuf>,
}

/// 바뀐 것.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Changed {
    /// Claude 세션 파일(.jsonl)
    ClaudeFile(PathBuf),
    /// Codex 롤아웃 파일(.jsonl / .jsonl.zst)
    CodexFile(PathBuf),
    /// 저장소(git-common-dir)에 커밋/체크아웃 등 변화
    GitRepo(PathBuf),
}

fn normalize(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// 경로 → 어느 대상에 속하는지 분류. 관심 없는 파일이면 None.
pub fn classify(targets: &WatchTargets, path: &Path) -> Option<Changed> {
    let p = normalize(path);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(root) = &targets.claude_projects
        && p.starts_with(root)
    {
        return name.ends_with(".jsonl").then_some(Changed::ClaudeFile(p));
    }
    if let Some(root) = &targets.codex_sessions
        && p.starts_with(root)
    {
        let ok = name.starts_with("rollout-")
            && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"));
        return ok.then_some(Changed::CodexFile(p));
    }
    targets
        .git_common_dirs
        .iter()
        .find(|g| p.starts_with(g))
        .map(|g| Changed::GitRepo(g.clone()))
}

/// 감시기. 드롭하면 감시가 멈춘다.
pub struct FileWatcher {
    watcher: RecommendedWatcher,
    targets: WatchTargets,
    rx: mpsc::Receiver<Vec<Changed>>,
}

impl FileWatcher {
    /// 감시 시작. `debounce` 동안 조용해지면 모인 변경을 한 배치로 보낸다.
    pub fn start(targets: WatchTargets, debounce: Duration) -> notify::Result<Self> {
        let targets = WatchTargets {
            claude_projects: targets.claude_projects.as_deref().map(normalize),
            codex_sessions: targets.codex_sessions.as_deref().map(normalize),
            git_common_dirs: targets
                .git_common_dirs
                .iter()
                .map(|p| normalize(p))
                .collect(),
        };
        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = raw_tx.send(res);
        })?;
        if let Some(root) = &targets.claude_projects
            && root.is_dir()
        {
            watcher.watch(root, RecursiveMode::Recursive)?;
        }
        if let Some(root) = &targets.codex_sessions
            && root.is_dir()
        {
            watcher.watch(root, RecursiveMode::Recursive)?;
        }
        for g in &targets.git_common_dirs {
            watch_git_dir(&mut watcher, g);
        }

        let (tx, rx) = mpsc::channel::<Vec<Changed>>();
        let t = targets.clone();
        thread::Builder::new()
            .name("worklog-watch-debounce".into())
            .spawn(move || debounce_loop(raw_rx, tx, t, debounce))
            .map_err(|e| notify::Error::generic(&e.to_string()))?;

        Ok(Self {
            watcher,
            targets,
            rx,
        })
    }

    /// 변경 배치 수신 채널.
    pub fn receiver(&self) -> &mpsc::Receiver<Vec<Changed>> {
        &self.rx
    }

    pub fn targets(&self) -> &WatchTargets {
        &self.targets
    }

    /// 저장소 감시 목록 갱신(새로 발견된 저장소 추가, 사라진 것 제거).
    pub fn set_git_dirs(&mut self, dirs: Vec<PathBuf>) {
        let new: BTreeSet<PathBuf> = dirs.iter().map(|p| normalize(p)).collect();
        let old: BTreeSet<PathBuf> = self.targets.git_common_dirs.iter().cloned().collect();
        for gone in old.difference(&new) {
            for sub in ["logs", "worktrees"] {
                let _ = self.watcher.unwatch(&gone.join(sub));
            }
        }
        for added in new.difference(&old) {
            watch_git_dir(&mut self.watcher, added);
        }
        self.targets.git_common_dirs = new.into_iter().collect();
    }
}

fn watch_git_dir(watcher: &mut RecommendedWatcher, common_dir: &Path) {
    // 커밋은 logs/HEAD(reflog)에 기록되고, 연결된 worktree 의 커밋은 worktrees/<name>/logs/HEAD 에 남는다.
    for sub in ["logs", "worktrees"] {
        let p = common_dir.join(sub);
        if p.is_dir()
            && let Err(e) = watcher.watch(&p, RecursiveMode::Recursive)
        {
            tracing::debug!("감시 등록 실패({}): {e}", p.display());
        }
    }
}

fn interesting(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

fn debounce_loop(
    raw_rx: mpsc::Receiver<notify::Result<Event>>,
    tx: mpsc::Sender<Vec<Changed>>,
    targets: WatchTargets,
    debounce: Duration,
) {
    let mut pending: BTreeSet<Changed> = BTreeSet::new();
    let mut quiet_since: Option<Instant> = None;
    loop {
        let wait = if pending.is_empty() {
            Duration::from_secs(3600)
        } else {
            debounce.saturating_sub(quiet_since.map(|q| q.elapsed()).unwrap_or_default())
        };
        match raw_rx.recv_timeout(wait) {
            Ok(Ok(ev)) => {
                if interesting(&ev.kind) {
                    for p in &ev.paths {
                        if let Some(c) = classify(&targets, p) {
                            pending.insert(c);
                        }
                    }
                }
                if !pending.is_empty() {
                    quiet_since = Some(Instant::now());
                }
            }
            Ok(Err(e)) => tracing::debug!("감시 오류: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !pending.is_empty() {
                    let batch: Vec<Changed> = pending.iter().cloned().collect();
                    pending.clear();
                    quiet_since = None;
                    if tx.send(batch).is_err() {
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn classify_by_root_and_name() {
        let t = WatchTargets {
            claude_projects: Some(PathBuf::from("D:/home/.claude/projects")),
            codex_sessions: Some(PathBuf::from("D:/home/.codex/sessions")),
            git_common_dirs: vec![PathBuf::from("D:/repo/.git")],
        };
        assert_eq!(
            classify(&t, Path::new("D:/home/.claude/projects/x/s.jsonl")),
            Some(Changed::ClaudeFile(PathBuf::from(
                "D:/home/.claude/projects/x/s.jsonl"
            )))
        );
        assert_eq!(
            classify(&t, Path::new("D:/home/.claude/projects/x/s.tmp")),
            None
        );
        assert_eq!(
            classify(
                &t,
                Path::new("D:/home/.codex/sessions/2026/09/14/rollout-a.jsonl.zst")
            ),
            Some(Changed::CodexFile(PathBuf::from(
                "D:/home/.codex/sessions/2026/09/14/rollout-a.jsonl.zst"
            )))
        );
        assert_eq!(
            classify(
                &t,
                Path::new("D:/home/.codex/sessions/2026/09/14/other.jsonl")
            ),
            None
        );
        assert_eq!(
            classify(&t, Path::new("D:/repo/.git/logs/HEAD")),
            Some(Changed::GitRepo(PathBuf::from("D:/repo/.git")))
        );
        assert_eq!(classify(&t, Path::new("D:/elsewhere/file")), None);
    }

    #[test]
    fn watcher_batches_changes_after_quiet_period() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let sessions = dir.path().join("sessions");
        let git = dir.path().join("repo").join(".git");
        fs::create_dir_all(projects.join("p")).unwrap();
        fs::create_dir_all(sessions.join("2026").join("09")).unwrap();
        fs::create_dir_all(git.join("logs")).unwrap();

        let w = FileWatcher::start(
            WatchTargets {
                claude_projects: Some(projects.clone()),
                codex_sessions: Some(sessions.clone()),
                git_common_dirs: vec![git.clone()],
            },
            Duration::from_millis(300),
        )
        .unwrap();
        // 감시가 붙을 시간을 잠깐 준다.
        thread::sleep(Duration::from_millis(300));
        fs::write(projects.join("p").join("s1.jsonl"), "{}\n").unwrap();
        fs::write(projects.join("p").join("ignore.txt"), "x").unwrap();
        fs::write(
            sessions.join("2026").join("09").join("rollout-1.jsonl"),
            "{}\n",
        )
        .unwrap();
        fs::write(git.join("logs").join("HEAD"), "reflog\n").unwrap();

        let batch = w
            .receiver()
            .recv_timeout(Duration::from_secs(10))
            .expect("변경 배치 수신");
        let claude = normalize(&projects.join("p").join("s1.jsonl"));
        let codex = normalize(&sessions.join("2026").join("09").join("rollout-1.jsonl"));
        let repo = normalize(&git);
        assert!(batch.contains(&Changed::ClaudeFile(claude)), "{batch:?}");
        assert!(batch.contains(&Changed::CodexFile(codex)), "{batch:?}");
        assert!(batch.contains(&Changed::GitRepo(repo)), "{batch:?}");
        assert!(
            !batch
                .iter()
                .any(|c| format!("{c:?}").contains("ignore.txt"))
        );
        assert_eq!(w.targets().git_common_dirs.len(), 1);
    }
}
