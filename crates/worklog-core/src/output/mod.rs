//! 출력(sink) — 만들어진 업무일지를 어디에 쓸지.
//!
//! - markdown: 로컬 `<dir>/YYYY-MM-DD.md`
//! - obsidian: vault 안 폴더 (frontmatter 표식으로 남의 노트 보호)
//! - notion : 새 페이지(페이지 하위 또는 DB 행)

pub mod markdown;
pub mod notion;
pub mod obsidian;

use serde::{Deserialize, Serialize};

use crate::model::WorkLog;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SinkResult {
    pub name: String,
    pub ok: bool,
    /// 저장 위치/URL
    pub location: Option<String>,
    pub error: Option<String>,
}

impl SinkResult {
    pub fn success(name: &str, location: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            location: Some(location.into()),
            error: None,
        }
    }

    pub fn failure(name: &str, error: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: false,
            location: None,
            error: Some(error.into()),
        }
    }
}

pub trait Sink {
    fn name(&self) -> &'static str;
    fn write(&self, worklog: &WorkLog) -> SinkResult;
}
