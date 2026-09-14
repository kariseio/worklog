//! Obsidian vault 출력: `<vault>/<subdir>/YYYY-MM-DD.md`.
//!
//! Obsidian 은 결국 로컬 Markdown 폴더이므로 vault 안에 파일을 직접 쓴다. 파일 앞에 간단한
//! YAML frontmatter(태그/날짜)를 붙여 vault 에서 잘 검색되게 하고, 그 표식이 없는 같은 이름의
//! 기존 노트(사용자의 데일리노트)는 덮어쓰지 않는다.

use std::fs;

use super::{Sink, SinkResult};
use crate::{config::ObsidianOutputConfig, model::WorkLog, paths};

/// 우리가 쓴 업무일지임을 나타내는 frontmatter 표식.
pub const MARKER: &str = "tags: [업무일지]";

/// vault 경로가 존재하고 하위 폴더에 쓰기 가능한지 실제로 확인.
pub fn test_connection(cfg: &ObsidianOutputConfig) -> (bool, String) {
    if cfg.vault_dir.trim().is_empty() {
        return (false, "vault 경로를 입력하세요.".into());
    }
    let vault = paths::expand_user(&cfg.vault_dir);
    if !vault.exists() {
        return (false, format!("경로가 없습니다: {}", vault.display()));
    }
    if !vault.is_dir() {
        return (false, format!("폴더가 아닙니다: {}", vault.display()));
    }
    let out_dir = if cfg.subdir.trim().is_empty() {
        vault.clone()
    } else {
        vault.join(cfg.subdir.trim())
    };
    let probe = out_dir.join(".worklog_write_test");
    let result = fs::create_dir_all(&out_dir)
        .and_then(|_| fs::write(&probe, "ok"))
        .and_then(|_| fs::remove_file(&probe));
    if let Err(e) = result {
        return (false, format!("쓰기 실패: {e}"));
    }
    let vault_name = vault
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let where_ = if cfg.subdir.trim().is_empty() {
        vault_name
    } else {
        format!("{vault_name}/{}", cfg.subdir.trim())
    };
    (true, format!("연결됨 · '{where_}' 에 쓰기 가능"))
}

pub struct ObsidianSink {
    cfg: ObsidianOutputConfig,
}

impl ObsidianSink {
    pub fn new(cfg: ObsidianOutputConfig) -> Self {
        Self { cfg }
    }
}

impl Sink for ObsidianSink {
    fn name(&self) -> &'static str {
        "obsidian"
    }

    fn write(&self, worklog: &WorkLog) -> SinkResult {
        if self.cfg.vault_dir.trim().is_empty() {
            return SinkResult::failure(self.name(), "outputs.obsidian.vault_dir 미설정");
        }
        let vault = paths::expand_user(&self.cfg.vault_dir);
        if !vault.exists() {
            return SinkResult::failure(
                self.name(),
                format!("vault 경로 없음: {}", vault.display()),
            );
        }
        let out_dir = if self.cfg.subdir.trim().is_empty() {
            vault
        } else {
            vault.join(self.cfg.subdir.trim())
        };
        if let Err(e) = fs::create_dir_all(&out_dir) {
            return SinkResult::failure(self.name(), e.to_string());
        }
        let path = out_dir.join(format!("{}.md", worklog.target_date));
        // YYYY-MM-DD.md 는 옵시디언 데일리노트 파일명과 동일하다. 기존 파일이 '우리 업무일지'
        // (frontmatter 표식)가 아니면 덮어쓰지 않는다.
        if path.exists() {
            let head: String = fs::read_to_string(&path)
                .unwrap_or_default()
                .chars()
                .take(400)
                .collect();
            if !head.contains(MARKER) {
                return SinkResult::failure(
                    self.name(),
                    format!(
                        "같은 이름의 기존 노트({})가 업무일지가 아니라 덮어쓰지 않았습니다. \
                         outputs.obsidian.subdir 를 데일리노트와 다른 폴더로 지정하세요.",
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                );
            }
        }
        let frontmatter = format!("---\ndate: {}\n{MARKER}\n---\n\n", worklog.target_date);
        match fs::write(&path, format!("{frontmatter}{}", worklog.full_markdown)) {
            Ok(()) => SinkResult::success(self.name(), path.to_string_lossy()),
            Err(e) => SinkResult::failure(self.name(), e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DailyData;
    use chrono::NaiveDate;

    fn wl() -> WorkLog {
        let date = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        WorkLog {
            target_date: date,
            facts_markdown: String::new(),
            full_markdown: "업무일지 본문".into(),
            data: DailyData::new(date, "Asia/Seoul"),
            summary_markdown: None,
        }
    }

    #[test]
    fn refuses_to_overwrite_foreign_note_then_writes_ours() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join("업무일지")).unwrap();
        let existing = vault.join("업무일지").join("2026-07-06.md");
        fs::write(&existing, "# 내 데일리노트\n중요한 메모").unwrap();

        let cfg = ObsidianOutputConfig {
            enabled: true,
            vault_dir: vault.to_string_lossy().into_owned(),
            subdir: "업무일지".into(),
        };
        let res = ObsidianSink::new(cfg.clone()).write(&wl());
        assert!(!res.ok);
        assert!(res.error.unwrap().contains("덮어쓰지 않았습니다"));
        assert!(
            fs::read_to_string(&existing)
                .unwrap()
                .contains("중요한 메모")
        );

        fs::write(
            &existing,
            "---\ndate: 2026-07-06\ntags: [업무일지]\n---\n\n이전본",
        )
        .unwrap();
        let res2 = ObsidianSink::new(cfg.clone()).write(&wl());
        assert!(res2.ok, "{:?}", res2.error);
        let body = fs::read_to_string(&existing).unwrap();
        assert!(body.starts_with("---\ndate: 2026-07-06\ntags: [업무일지]\n---\n\n업무일지 본문"));

        // 새 파일도 정상
        let cfg2 = ObsidianOutputConfig {
            subdir: "다른폴더".into(),
            ..cfg.clone()
        };
        assert!(ObsidianSink::new(cfg2).write(&wl()).ok);
        assert!(vault.join("다른폴더").join("2026-07-06.md").exists());
    }

    #[test]
    fn connection_test_and_missing_vault() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        let ok = test_connection(&ObsidianOutputConfig {
            enabled: true,
            vault_dir: vault.to_string_lossy().into_owned(),
            subdir: "업무일지".into(),
        });
        assert!(ok.0, "{}", ok.1);
        assert!(ok.1.contains("vault/업무일지"));
        assert!(!vault.join("업무일지").join(".worklog_write_test").exists());
        let bad = test_connection(&ObsidianOutputConfig {
            enabled: true,
            vault_dir: dir.path().join("nope").to_string_lossy().into_owned(),
            subdir: "x".into(),
        });
        assert!(!bad.0);
        assert!(!test_connection(&ObsidianOutputConfig::default()).0);

        let res = ObsidianSink::new(ObsidianOutputConfig {
            enabled: true,
            vault_dir: dir.path().join("nope").to_string_lossy().into_owned(),
            subdir: String::new(),
        })
        .write(&wl());
        assert!(!res.ok);
        assert!(
            !ObsidianSink::new(ObsidianOutputConfig::default())
                .write(&wl())
                .ok
        );
    }
}
