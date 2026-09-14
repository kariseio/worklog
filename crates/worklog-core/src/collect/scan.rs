//! 저장소 자동 탐색 — 루트 아래 `depth` 단계까지 내려가며 `.git` 을 가진 디렉터리를 찾는다.
//!
//! `ignore` 크레이트의 병렬 워커를 쓴다(.gitignore 규칙은 적용하지 않고 순수 파일시스템 탐색).
//! 점(.)으로 시작하는 폴더는 건너뛰고, 저장소를 찾으면 그 안쪽은 더 내려가지 않는다.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use ignore::{WalkBuilder, WalkState};

/// `root` 아래 `depth` 깊이까지 `.git` 을 포함한 디렉터리(정렬됨). 루트 자체도 검사한다.
pub fn scan_root(root: &Path, depth: u32) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let found: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut builder = WalkBuilder::new(root);
    builder
        .max_depth(Some(depth as usize))
        .hidden(true) // 점으로 시작하는 폴더 제외(.git 안으로도 안 들어감)
        .follow_links(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .require_git(false);
    builder.build_parallel().run(|| {
        Box::new(|entry| {
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_dir()) {
                return WalkState::Continue;
            }
            let p = entry.path();
            if p.join(".git").exists() {
                if let Ok(mut v) = found.lock() {
                    v.push(p.to_path_buf());
                }
                return WalkState::Skip; // 저장소 안쪽은 더 안 내려감
            }
            WalkState::Continue
        })
    });
    let mut v = found.into_inner().unwrap_or_default();
    v.sort();
    v
}

/// 여러 루트를 탐색해 합친다(루트 순서 유지, 각 루트 안은 정렬).
pub fn scan_roots<P: AsRef<Path>>(roots: &[P], depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for r in roots {
        let r = r.as_ref();
        if r.as_os_str().is_empty() {
            continue;
        }
        out.extend(scan_root(r, depth));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn mk_repo(p: &Path) {
        fs::create_dir_all(p.join(".git")).unwrap();
    }

    #[test]
    fn finds_repos_respects_depth_and_skips_nested_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("A").join("repoA");
        let b = dir.path().join("B").join("repoB");
        mk_repo(&a);
        mk_repo(&b);
        mk_repo(&a.join("vendor").join("inner")); // 저장소 안쪽 → 무시
        mk_repo(&dir.path().join(".hidden").join("repoH")); // 숨김 폴더 아래 → 무시
        fs::create_dir_all(dir.path().join("A").join("plain")).unwrap();

        let names = |v: Vec<PathBuf>| {
            v.iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(scan_root(dir.path(), 3)), vec!["repoA", "repoB"]);
        assert_eq!(names(scan_root(dir.path(), 1)), Vec::<String>::new()); // 두 단계 아래는 못 찾음
        assert_eq!(names(scan_root(dir.path(), 2)), vec!["repoA", "repoB"]);
        // 루트 자체가 저장소면 그것만
        assert_eq!(scan_root(&a, 5), vec![a.clone()]);
        // 여러 루트: 순서 유지
        let both = scan_roots(&[dir.path().join("B"), dir.path().join("A")], 2);
        assert_eq!(names(both), vec!["repoB", "repoA"]);
        assert!(scan_root(&dir.path().join("nope"), 3).is_empty());
    }
}
