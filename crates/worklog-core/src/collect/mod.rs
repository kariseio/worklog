//! 수집기 공통 인터페이스.
//!
//! 모든 수집기는 [`Collector`] 를 구현하고 `collect(ctx)` 로 [`CollectorResult`] 를 돌려준다.
//! 수집기는 패닉하거나 에러를 전파하지 않고, 실패·건너뜀을 결과로 표현한다
//! (오케스트레이터도 방어적으로 감싼다).

pub mod claude;
pub mod codex;
pub mod git;
pub mod naverworks;
pub mod qa;
pub mod scan;

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::time::DayBounds;

/// 수집 대상 하루의 경계 정보.
#[derive(Debug, Clone)]
pub struct CollectContext {
    pub day: DayBounds,
    pub tz_name: String,
}

impl CollectContext {
    pub fn new(day: DayBounds, tz_name: impl Into<String>) -> Self {
        Self {
            day,
            tz_name: tz_name.into(),
        }
    }

    pub fn tz(&self) -> Tz {
        self.day.tz()
    }

    pub fn target_date(&self) -> chrono::NaiveDate {
        self.day.date
    }
}

/// 수집 결과. `data` 가 있으면 성공, `skipped` 면 조용히 건너뜀(미연동 등), 그 외는 실패.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectorResult<T> {
    pub name: &'static str,
    pub data: Option<T>,
    pub warnings: Vec<String>,
    pub ok: bool,
    pub skipped: bool,
    pub skip_reason: Option<String>,
}

impl<T> CollectorResult<T> {
    pub fn ok(name: &'static str, data: T) -> Self {
        Self {
            name,
            data: Some(data),
            warnings: Vec::new(),
            ok: true,
            skipped: false,
            skip_reason: None,
        }
    }

    pub fn ok_with_warnings(name: &'static str, data: T, warnings: Vec<String>) -> Self {
        Self {
            warnings,
            ..Self::ok(name, data)
        }
    }

    pub fn skip(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            data: None,
            warnings: Vec::new(),
            ok: true,
            skipped: true,
            skip_reason: Some(reason.into()),
        }
    }

    pub fn fail(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            data: None,
            warnings: vec![reason.into()],
            ok: false,
            skipped: false,
            skip_reason: None,
        }
    }
}

/// 수집기 베이스.
pub trait Collector {
    type Data;
    const NAME: &'static str;

    fn collect(&self, ctx: &CollectContext) -> CollectorResult<Self::Data>;
}

/// 앱 화면 칩·가용성 라벨용 소스 상태.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceState {
    Ok,
    Skipped,
    Error,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub name: String,
    pub state: SourceState,
    pub count: usize,
    pub note: Option<String>,
}

impl SourceStatus {
    pub fn disabled(name: &str) -> Self {
        Self {
            name: name.to_string(),
            state: SourceState::Disabled,
            count: 0,
            note: None,
        }
    }

    /// 수집 결과 → 상태. `count` 는 성공했을 때의 항목 수.
    pub fn from_result<T>(res: &CollectorResult<T>, count: impl FnOnce(&T) -> usize) -> Self {
        if res.skipped {
            return Self {
                name: res.name.to_string(),
                state: SourceState::Skipped,
                count: 0,
                note: res.skip_reason.clone(),
            };
        }
        if !res.ok {
            return Self {
                name: res.name.to_string(),
                state: SourceState::Error,
                count: 0,
                note: Some(
                    res.warnings
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "실패".into()),
                ),
            };
        }
        Self {
            name: res.name.to_string(),
            state: SourceState::Ok,
            count: res.data.as_ref().map(count).unwrap_or(0),
            note: res.warnings.first().cloned(),
        }
    }
}

/// 파일 수정 시각이 `since`(UTC) 이후인지. 메타데이터를 못 읽으면 false.
pub(crate) fn modified_since(
    path: &std::path::Path,
    since: &chrono::DateTime<chrono::Utc>,
) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let since_sys = std::time::UNIX_EPOCH
        + std::time::Duration::from_millis(since.timestamp_millis().max(0) as u64);
    modified >= since_sys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_constructors_and_status() {
        let ok: CollectorResult<Vec<u8>> = CollectorResult::ok("git", vec![1, 2, 3]);
        let st = SourceStatus::from_result(&ok, |v| v.len());
        assert_eq!(st.state, SourceState::Ok);
        assert_eq!(st.count, 3);
        assert_eq!(st.note, None);

        let sk: CollectorResult<Vec<u8>> = CollectorResult::skip("codex", "폴더 없음");
        let st = SourceStatus::from_result(&sk, |v| v.len());
        assert_eq!(st.state, SourceState::Skipped);
        assert_eq!(st.note.as_deref(), Some("폴더 없음"));

        let f: CollectorResult<Vec<u8>> = CollectorResult::fail("naverworks", "토큰 실패");
        assert!(!f.ok);
        let st = SourceStatus::from_result(&f, |v| v.len());
        assert_eq!(st.state, SourceState::Error);
        assert_eq!(st.note.as_deref(), Some("토큰 실패"));

        let w: CollectorResult<Vec<u8>> =
            CollectorResult::ok_with_warnings("claude", vec![], vec!["경고".into()]);
        let st = SourceStatus::from_result(&w, |v| v.len());
        assert_eq!(st.state, SourceState::Ok);
        assert_eq!(st.note.as_deref(), Some("경고"));
        assert_eq!(SourceStatus::disabled("git").state, SourceState::Disabled);
    }

    #[test]
    fn modified_since_checks_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "x").unwrap();
        let past = chrono::Utc::now() - chrono::Duration::days(1);
        let future = chrono::Utc::now() + chrono::Duration::days(1);
        assert!(modified_since(&p, &past));
        assert!(!modified_since(&p, &future));
        assert!(!modified_since(&dir.path().join("missing"), &past));
    }
}
