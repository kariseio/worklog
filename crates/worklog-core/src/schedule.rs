//! 정해진 시각 동작(알림만 / 자동 생성)과 주기 작업의 시각 계산 — 순수 함수.
//! 실제 루프(타이머·알림·생성 실행)는 앱 셸이 돌리고, 여기서는 "언제 울려야 하는가"만 답한다.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use chrono_tz::Tz;

use crate::config::ScheduleConfig;

/// `after` 보다 엄격히 뒤인, 다음 발동 시각(UTC). 꺼져 있거나 요일·시각이 비었으면 None.
pub fn next_fire_after(
    cfg: &ScheduleConfig,
    after: DateTime<Utc>,
    tz: Tz,
) -> Option<DateTime<Utc>> {
    if !cfg.enabled || cfg.weekdays.is_empty() {
        return None;
    }
    let (h, m) = cfg.time_hm()?;
    let local_after = after.with_timezone(&tz);
    let mut day = local_after.date_naive();
    for _ in 0..8 {
        if cfg.on_weekday(day.weekday()) {
            let naive = day.and_hms_opt(h, m, 0)?;
            if let Some(fire) = tz.from_local_datetime(&naive).earliest() {
                let fire = fire.with_timezone(&Utc);
                if fire > after {
                    return Some(fire);
                }
            }
        }
        day += Duration::days(1);
    }
    None
}

/// `(since, until]` 안에 발동 시각이 있으면 그 중 **가장 늦은** 것. 앱이 꺼져 있던 동안 놓친
/// 발동을 한 번만 처리할 때 쓴다(여러 번 놓쳤어도 한 번).
pub fn due_between(
    cfg: &ScheduleConfig,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    tz: Tz,
) -> Option<DateTime<Utc>> {
    let mut cursor = since;
    let mut last = None;
    for _ in 0..64 {
        match next_fire_after(cfg, cursor, tz) {
            Some(f) if f <= until => {
                last = Some(f);
                cursor = f;
            }
            _ => break,
        }
    }
    last
}

/// 주기 작업(회의 폴링·전체 재수집)이 지금 돌아야 하는지. `last` 가 없으면 항상 true.
pub fn interval_due(last: Option<DateTime<Utc>>, now: DateTime<Utc>, every_min: u32) -> bool {
    match last {
        None => true,
        Some(l) => now - l >= Duration::minutes(every_min.max(1) as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScheduleMode;
    use crate::time::{get_tz, parse_iso};

    fn cfg() -> ScheduleConfig {
        ScheduleConfig {
            enabled: true,
            time: "18:30".into(),
            weekdays: vec![1, 2, 3, 4, 5],
            mode: ScheduleMode::Notify,
        }
    }

    #[test]
    fn next_fire_respects_time_and_weekdays() {
        let tz = get_tz("Asia/Seoul");
        // 2026-09-11 은 금요일. 금 18:00 KST → 그날 18:30
        let after = parse_iso("2026-09-11T09:00:00Z").unwrap(); // 18:00 KST
        assert_eq!(
            next_fire_after(&cfg(), after, tz),
            parse_iso("2026-09-11T09:30:00Z")
        );
        // 금 19:00 KST → 다음 월요일(9/14) 18:30
        let after = parse_iso("2026-09-11T10:00:00Z").unwrap();
        assert_eq!(
            next_fire_after(&cfg(), after, tz),
            parse_iso("2026-09-14T09:30:00Z")
        );
        // 정확히 발동 시각이면 '뒤'가 아니므로 다음 날
        let at = parse_iso("2026-09-14T09:30:00Z").unwrap();
        assert_eq!(
            next_fire_after(&cfg(), at, tz),
            parse_iso("2026-09-15T09:30:00Z")
        );
        // 꺼짐 / 요일 없음 / 잘못된 시각
        let mut off = cfg();
        off.enabled = false;
        assert_eq!(next_fire_after(&off, after, tz), None);
        let mut none = cfg();
        none.weekdays.clear();
        assert_eq!(next_fire_after(&none, after, tz), None);
        let mut bad = cfg();
        bad.time = "25:00".into();
        assert_eq!(next_fire_after(&bad, after, tz), None);
    }

    #[test]
    fn due_between_returns_latest_missed_fire() {
        let tz = get_tz("Asia/Seoul");
        // 목(9/10) 12:00 KST 에 껐다가 월(9/14) 09:00 KST 에 켬 → 놓친 발동은 목·금 18:30, 마지막은 금
        let since = parse_iso("2026-09-10T03:00:00Z").unwrap();
        let until = parse_iso("2026-09-14T00:00:00Z").unwrap();
        assert_eq!(
            due_between(&cfg(), since, until, tz),
            parse_iso("2026-09-11T09:30:00Z")
        );
        // 그 사이에 발동이 없으면 None
        let since = parse_iso("2026-09-12T03:00:00Z").unwrap(); // 토
        assert_eq!(due_between(&cfg(), since, until, tz), None);
        // 경계: until 이 정확히 발동 시각이면 포함
        let until = parse_iso("2026-09-14T09:30:00Z").unwrap();
        assert_eq!(due_between(&cfg(), since, until, tz), Some(until));
    }

    #[test]
    fn interval_due_basic() {
        let now = parse_iso("2026-09-14T09:00:00Z").unwrap();
        assert!(interval_due(None, now, 15));
        assert!(!interval_due(Some(now - Duration::minutes(14)), now, 15));
        assert!(interval_due(Some(now - Duration::minutes(15)), now, 15));
        assert!(interval_due(Some(now - Duration::minutes(1)), now, 0)); // 0 은 1분으로
    }
}
