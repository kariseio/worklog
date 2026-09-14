//! 로컬 Markdown 파일 출력: `<dir>/YYYY-MM-DD.md`.

use std::fs;

use super::{Sink, SinkResult};
use crate::{config::MarkdownOutputConfig, model::WorkLog};

pub struct MarkdownSink {
    cfg: MarkdownOutputConfig,
}

impl MarkdownSink {
    pub fn new(cfg: MarkdownOutputConfig) -> Self {
        Self { cfg }
    }
}

impl Sink for MarkdownSink {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn write(&self, worklog: &WorkLog) -> SinkResult {
        let out_dir = self.cfg.resolved_dir();
        if let Err(e) = fs::create_dir_all(&out_dir) {
            return SinkResult::failure(self.name(), e.to_string());
        }
        let path = out_dir.join(format!("{}.md", worklog.target_date));
        match fs::write(&path, &worklog.full_markdown) {
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

    #[test]
    fn writes_dated_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("nested").join("logs");
        let sink = MarkdownSink::new(MarkdownOutputConfig {
            enabled: true,
            dir: out.to_string_lossy().into_owned(),
        });
        let date = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        let wl = WorkLog {
            target_date: date,
            facts_markdown: String::new(),
            full_markdown: "# 업무일지".into(),
            data: DailyData::new(date, "Asia/Seoul"),
            summary_markdown: None,
        };
        let r = sink.write(&wl);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            fs::read_to_string(out.join("2026-07-06.md")).unwrap(),
            "# 업무일지"
        );
        assert!(r.location.unwrap().ends_with("2026-07-06.md"));
    }
}
