//! 시간대·날짜 처리: 하루 경계 계산, ISO 파싱, 표시 포맷.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum TimeError {
    #[error("날짜 형식 오류: {0:?} (YYYY-MM-DD | today | yesterday)")]
    BadDate(String),
}

/// 시간대 이름 → `Tz`. 모르면 경고 후 UTC.
pub fn get_tz(name: &str) -> Tz {
    match name.trim().parse::<Tz>() {
        Ok(tz) => tz,
        Err(_) => {
            tracing::warn!("알 수 없는 시간대 {name:?} → UTC 로 대체합니다.");
            Tz::UTC
        }
    }
}

/// 대상 하루의 경계. `start` 는 그날 00:00, `end` 는 다음날 00:00 (exclusive).
#[derive(Debug, Clone, PartialEq)]
pub struct DayBounds {
    pub date: NaiveDate,
    pub start: DateTime<Tz>,
    pub end: DateTime<Tz>,
}

impl DayBounds {
    pub fn for_date(date: NaiveDate, tz: Tz) -> Self {
        Self {
            date,
            start: local_midnight(date, tz),
            end: local_midnight(date + Duration::days(1), tz),
        }
    }

    pub fn tz(&self) -> Tz {
        self.start.timezone()
    }

    /// UTC 시각이 이 하루 안에 있는지 `[start, end)`.
    pub fn contains(&self, t: &DateTime<Utc>) -> bool {
        let t = t.with_timezone(&self.tz());
        t >= self.start && t < self.end
    }
}

/// 그날 로컬 00:00. DST 갭에 자정이 걸리면(서울은 없음) 존재하는 첫 로컬 시각(보통 01:00)으로 —
/// Python zoneinfo(fold=0)가 갭 시각을 전환 전 오프셋으로 해석해 얻는 것과 같은 순간이다.
fn local_midnight(date: NaiveDate, tz: Tz) -> DateTime<Tz> {
    let naive = date.and_hms_opt(0, 0, 0).expect("00:00 은 항상 유효");
    for minutes in [0i64, 30, 60, 90, 120, 180] {
        if let Some(d) = tz
            .from_local_datetime(&(naive + Duration::minutes(minutes)))
            .earliest()
        {
            return d;
        }
    }
    tz.from_utc_datetime(&naive)
}

/// `spec`: "YYYY-MM-DD" | "today" | "yesterday" | None(=today) → 하루 경계.
pub fn resolve_day(spec: Option<&str>, tz: Tz) -> Result<DayBounds, TimeError> {
    let today = Utc::now().with_timezone(&tz).date_naive();
    let date = match spec.map(str::trim) {
        None | Some("") | Some("today") => today,
        Some("yesterday") => today - Duration::days(1),
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| TimeError::BadDate(s.to_string()))?,
    };
    Ok(DayBounds::for_date(date, tz))
}

/// ISO8601/RFC3339(끝의 `Z` 또는 오프셋, 소수 초 허용) → UTC.
/// 오프셋이 없는 문자열은 UTC 로 간주한다. 실패 시 None.
pub fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
    ] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&n));
        }
    }
    None
}

/// [`parse_iso`] 와 같되, 오프셋이 없는 문자열은 `tz` 의 로컬 시각으로 해석한다.
/// NaverWorks 캘린더처럼 로컬 시각을 오프셋 없이 주는 소스에 쓴다(v1 은 시스템 로컬로 해석했다).
pub fn parse_iso_in(s: &str, tz: Tz) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
    ] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return tz
                .from_local_datetime(&n)
                .earliest()
                .map(|d| d.with_timezone(&Utc));
        }
    }
    None
}

/// 타임스탬프 문자열 → 시간대 기준 로컬 날짜.
pub fn local_date_of(s: &str, tz: Tz) -> Option<NaiveDate> {
    parse_iso(s).map(|d| d.with_timezone(&tz).date_naive())
}

/// UTC 시각 → 로컬 "HH:MM". None 이면 "--:--".
pub fn fmt_time(dt: Option<&DateTime<Utc>>, tz: Tz) -> String {
    match dt {
        Some(d) => d.with_timezone(&tz).format("%H:%M").to_string(),
        None => "--:--".to_string(),
    }
}

/// 초 → "2h 15m" / "45m" / "30s".
pub fn human_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        return format!("{s}s");
    }
    let minutes = s / 60;
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// 지금(UTC) RFC3339 밀리초 문자열. 저장소 타임스탬프 표준 형식.
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEOUL: Tz = chrono_tz::Asia::Seoul;

    #[test]
    fn day_bounds_seoul() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        let b = DayBounds::for_date(d, SEOUL);
        assert_eq!(b.start.to_rfc3339(), "2026-07-06T00:00:00+09:00");
        assert_eq!(b.end.to_rfc3339(), "2026-07-07T00:00:00+09:00");
        // 시작(그날 00:00 = 전날 15:00Z)은 포함, 그 1초 전은 제외.
        assert!(b.contains(&parse_iso("2026-07-05T15:00:00Z").unwrap()));
        assert!(!b.contains(&parse_iso("2026-07-05T14:59:59Z").unwrap()));
        // 전날 23:30 UTC = 그날 08:30 KST → 포함. 그날 15:00 UTC = 다음날 00:00 KST → 제외.
        assert!(b.contains(&parse_iso("2026-07-05T23:30:00Z").unwrap()));
        assert!(!b.contains(&parse_iso("2026-07-06T15:00:00Z").unwrap()));
    }

    #[test]
    fn day_bounds_dst_gap_uses_first_existing_local_time() {
        // 베이루트 2026-03-29: 00:00 → 01:00 으로 건너뜀. 그날은 01:00+03:00(= 전날 22:00Z)부터.
        let d = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        let b = DayBounds::for_date(d, chrono_tz::Asia::Beirut);
        assert_eq!(b.start.to_rfc3339(), "2026-03-29T01:00:00+03:00");
        assert!(b.contains(&parse_iso("2026-03-28T23:00:00Z").unwrap()));
        assert!(!b.contains(&parse_iso("2026-03-28T21:59:59Z").unwrap()));
        // 전날의 끝은 그 순간과 같다(빈틈·겹침 없음).
        let prev = DayBounds::for_date(d - Duration::days(1), chrono_tz::Asia::Beirut);
        assert_eq!(prev.end, b.start);
    }

    #[test]
    fn resolve_day_specs() {
        let explicit = resolve_day(Some("2026-07-05"), SEOUL).unwrap();
        assert_eq!(explicit.date, NaiveDate::from_ymd_opt(2026, 7, 5).unwrap());
        let today = resolve_day(None, SEOUL).unwrap();
        let yesterday = resolve_day(Some("yesterday"), SEOUL).unwrap();
        assert_eq!(today.date - Duration::days(1), yesterday.date);
        assert_eq!(resolve_day(Some("today"), SEOUL).unwrap().date, today.date);
        assert_eq!(
            resolve_day(Some("07/05"), SEOUL),
            Err(TimeError::BadDate("07/05".into()))
        );
    }

    #[test]
    fn parse_iso_variants() {
        let z = parse_iso("2026-07-06T01:02:03.456Z").unwrap();
        assert_eq!(z.to_rfc3339(), "2026-07-06T01:02:03.456+00:00");
        let off = parse_iso("2026-07-06T10:02:03+09:00").unwrap();
        assert_eq!(off, parse_iso("2026-07-06T01:02:03Z").unwrap());
        let naive = parse_iso("2026-07-06T01:02:03").unwrap();
        assert_eq!(naive, parse_iso("2026-07-06T01:02:03Z").unwrap());
        assert!(parse_iso("").is_none());
        assert!(parse_iso("not a date").is_none());
        // 오프셋 없는 로컬 시각(NaverWorks) → 지정 시간대로 해석
        assert_eq!(
            parse_iso_in("2026-09-14T09:30:00", SEOUL),
            parse_iso("2026-09-14T00:30:00Z")
        );
        assert_eq!(
            parse_iso_in("2026-09-14T09:30:00+09:00", SEOUL),
            parse_iso("2026-09-14T00:30:00Z")
        );
        assert_eq!(
            parse_iso_in("2026-09-14T00:30:00Z", SEOUL),
            parse_iso("2026-09-14T00:30:00Z")
        );
        assert!(parse_iso_in("x", SEOUL).is_none());
        assert_eq!(
            local_date_of("2026-07-05T23:30:00Z", SEOUL),
            NaiveDate::from_ymd_opt(2026, 7, 6)
        );
    }

    #[test]
    fn formatting() {
        let t = parse_iso("2026-07-06T01:02:03Z").unwrap();
        assert_eq!(fmt_time(Some(&t), SEOUL), "10:02");
        assert_eq!(fmt_time(None, SEOUL), "--:--");
        assert_eq!(human_duration(30), "30s");
        assert_eq!(human_duration(45 * 60), "45m");
        assert_eq!(human_duration(2 * 3600 + 15 * 60 + 5), "2h 15m");
        assert_eq!(human_duration(-5), "0s");
    }

    #[test]
    fn unknown_tz_falls_back_to_utc() {
        assert_eq!(get_tz("Mars/Olympus"), Tz::UTC);
        assert_eq!(get_tz(" Asia/Seoul "), SEOUL);
    }
}
