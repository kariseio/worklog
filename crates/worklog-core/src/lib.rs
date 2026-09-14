//! worklog-core — 업무일지 v2 핵심 라이브러리.
//!
//! 수집(git · Claude Code · Codex · NaverWorks) → 분석 → 렌더 → 요약 → 저장의
//! 파이프라인과, 메모·파일 감시·스케줄·SQLite 저장소를 담는다.
//! CLI(`worklog`)와 데스크톱 앱(Tauri)이 이 크레이트 하나를 공유한다.
//!
//! 모듈은 `docs/v2-plan.md` 2절의 매핑 순서대로 채운다.

/// 크레이트 버전 (Cargo.toml `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 현재 버전 문자열.
pub fn version() -> &'static str {
    VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }
}
