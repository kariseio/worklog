//! 앱이 쓰는 경로들. 환경변수로 덮어쓸 수 있어 테스트·이식이 쉽다.
//!
//! - `WORKLOG_HOME`      : 기본 `~/.worklog`
//! - `WORKLOG_SETTINGS`  : 기본 `<HOME>/settings.json`
//! - `WORKLOG_DB`        : 기본 `<HOME>/worklog.db`
//! - `CLAUDE_CONFIG_DIR` : Claude Code 데이터 폴더(기본 `~/.claude`)
//! - `CODEX_HOME`        : Codex 데이터 폴더(기본 `~/.codex`)
//!
//! 빈 값으로 설정된 변수는 없는 것으로 본다.

use std::path::{Path, PathBuf};

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// 현재 사용자 홈. 못 찾으면 현재 폴더.
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `~/.worklog` (설정·DB·로그).
pub fn worklog_dir() -> PathBuf {
    env_path("WORKLOG_HOME").unwrap_or_else(|| home_dir().join(".worklog"))
}

pub fn settings_path() -> PathBuf {
    env_path("WORKLOG_SETTINGS").unwrap_or_else(|| worklog_dir().join("settings.json"))
}

pub fn db_path() -> PathBuf {
    env_path("WORKLOG_DB").unwrap_or_else(|| worklog_dir().join("worklog.db"))
}

/// 현재 사용자의 문서(Documents) 폴더. Windows 는 Known Folder(OneDrive 리다이렉션 대응).
pub fn documents_dir() -> PathBuf {
    dirs::document_dir().unwrap_or_else(|| home_dir().join("Documents"))
}

/// 기본 마크다운 저장 폴더: 문서\업무일지.
pub fn default_markdown_dir() -> PathBuf {
    documents_dir().join("업무일지")
}

/// Claude Code 세션 로그 루트(`.../projects`).
pub fn claude_projects_dir() -> PathBuf {
    env_path("CLAUDE_CONFIG_DIR")
        .unwrap_or_else(|| home_dir().join(".claude"))
        .join("projects")
}

/// Codex 롤아웃 세션 루트(`.../sessions`).
pub fn codex_sessions_dir() -> PathBuf {
    env_path("CODEX_HOME")
        .unwrap_or_else(|| home_dir().join(".codex"))
        .join("sessions")
}

/// `~`, `~/...`, `~\...` 를 홈으로 확장. 그 외는 그대로.
pub fn expand_user(p: &str) -> PathBuf {
    let p = p.trim();
    if p == "~" {
        return home_dir();
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }
    PathBuf::from(p)
}

/// 설정에 적힌 폴더 문자열 → 경로. 비었으면 `fallback`.
pub fn dir_or(p: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    if p.trim().is_empty() {
        fallback()
    } else {
        expand_user(p)
    }
}

/// 경로가 존재하는 디렉터리인지.
pub fn is_dir(p: &Path) -> bool {
    p.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 환경변수를 만지는 테스트는 직렬로 돈다(프로세스 전역 상태).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 변수들을 잠시 바꾸고(None 은 제거) 돌려놓는다.
    fn with_env<T>(pairs: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, Option<std::ffi::OsString>)> = pairs
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var_os(k)))
            .collect();
        for (k, v) in pairs {
            // SAFETY: ENV_LOCK 으로 이 모듈의 env 테스트를 직렬화했고, 다른 테스트는 env 를 쓰지 않는다.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let out = f();
        for (k, v) in saved {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
        out
    }

    #[test]
    fn expand_user_forms() {
        assert_eq!(expand_user("~"), home_dir());
        assert_eq!(expand_user("~/x/y"), home_dir().join("x/y"));
        assert_eq!(expand_user("~\\x"), home_dir().join("x"));
        assert_eq!(expand_user("D:/plain"), PathBuf::from("D:/plain"));
        assert_eq!(dir_or("", || PathBuf::from("fb")), PathBuf::from("fb"));
        assert_eq!(
            dir_or(" D:/a ", || PathBuf::from("fb")),
            PathBuf::from("D:/a")
        );
    }

    #[test]
    fn default_locations_are_under_home() {
        with_env(
            &[
                ("WORKLOG_HOME", None),
                ("WORKLOG_SETTINGS", None),
                ("WORKLOG_DB", None),
                ("CLAUDE_CONFIG_DIR", None),
                ("CODEX_HOME", None),
            ],
            || {
                assert_eq!(worklog_dir(), home_dir().join(".worklog"));
                assert_eq!(
                    settings_path(),
                    home_dir().join(".worklog").join("settings.json")
                );
                assert_eq!(db_path(), home_dir().join(".worklog").join("worklog.db"));
                assert_eq!(
                    claude_projects_dir(),
                    home_dir().join(".claude").join("projects")
                );
                assert_eq!(
                    codex_sessions_dir(),
                    home_dir().join(".codex").join("sessions")
                );
                assert!(default_markdown_dir().ends_with("업무일지"));
            },
        );
    }

    #[test]
    fn env_overrides_are_honoured_and_empty_is_ignored() {
        with_env(
            &[
                ("WORKLOG_HOME", Some("D:/wl-home")),
                ("WORKLOG_SETTINGS", Some("")),
                ("WORKLOG_DB", Some("E:/x/w.db")),
                ("CLAUDE_CONFIG_DIR", Some("D:/cfg")),
                ("CODEX_HOME", Some("D:/cx")),
            ],
            || {
                assert_eq!(worklog_dir(), PathBuf::from("D:/wl-home"));
                // 빈 값은 없는 것 → HOME 아래 기본 파일명
                assert_eq!(
                    settings_path(),
                    PathBuf::from("D:/wl-home").join("settings.json")
                );
                assert_eq!(db_path(), PathBuf::from("E:/x/w.db"));
                assert_eq!(
                    claude_projects_dir(),
                    PathBuf::from("D:/cfg").join("projects")
                );
                assert_eq!(
                    codex_sessions_dir(),
                    PathBuf::from("D:/cx").join("sessions")
                );
            },
        );
        with_env(&[("WORKLOG_SETTINGS", Some("D:/s.json"))], || {
            assert_eq!(settings_path(), PathBuf::from("D:/s.json"));
        });
    }
}
