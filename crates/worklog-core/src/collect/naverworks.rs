//! NaverWorks(네이버웍스) 캘린더 수집기.
//!
//! 서비스 계정(JWT-bearer, RS256)으로 액세스 토큰을 발급받아 그날의 일정을 가져온다.
//! 자격증명이 없으면 조용히 건너뛴다.
//!
//! 인증:  POST https://auth.worksmobile.com/oauth2/v2.0/token
//!        grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer
//!        (JWT claims: iss=client_id, sub=service_account, iat, exp<=+3600, RS256)
//! 캘린더: GET {API}/users/{userId}/calendar/events?fromDateTime=..&untilDateTime=..  (RFC3339, 31일 이내)
//!        선택 캘린더: GET {API}/users/{userId}/calendars/{calendarId}/events
//!        응답: events[].eventComponents[] (summary/start/end/location/attendees ...) + responseMetaData.nextCursor
//! 목록:  GET {API}/users/{userId}/calendar-personals  (내 캘린더들의 속성)

use std::{fs, time::Duration};

use chrono::Utc;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use super::{CollectContext, Collector, CollectorResult};
use crate::{
    config::{CalendarInfo, NaverWorksConfig},
    model::{CalendarData, CalendarEvent},
    paths,
};

pub const TOKEN_URL: &str = "https://auth.worksmobile.com/oauth2/v2.0/token";
pub const API_BASE: &str = "https://www.worksapis.com/v1.0";
/// 이벤트 페이지네이션 안전 상한.
const MAX_EVENT_PAGES: usize = 50;
const MAX_CALENDAR_PAGES: usize = 10;

/// Python `urllib.parse.quote` 기본과 동일: 영숫자와 `-._~/` 만 그대로.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'/');

fn quote(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

#[derive(Debug, Error, Clone, PartialEq)]
#[error("{0}")]
pub struct NwError(pub String);

impl From<reqwest::Error> for NwError {
    fn from(e: reqwest::Error) -> Self {
        NwError(e.to_string())
    }
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    sub: &'a str,
    iat: i64,
    exp: i64,
}

fn snippet(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 응답에서 캘린더 배열을 찾아낸다(키 이름이 문서마다 달라 방어적으로).
pub(crate) fn extract_list(data: &Value) -> Vec<Value> {
    match data {
        Value::Array(a) => a.clone(),
        Value::Object(map) => {
            for key in [
                "calendarPersonals",
                "calendars",
                "calendarList",
                "list",
                "elements",
                "items",
            ] {
                if let Some(Value::Array(a)) = map.get(key) {
                    return a.clone();
                }
            }
            map.values()
                .find_map(|v| v.as_array().cloned())
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn next_cursor(payload: &Value) -> Option<String> {
    payload
        .get("responseMetaData")
        .and_then(|m| m.get("nextCursor"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

/// events[].eventComponents[] → CalendarEvent (시작 시각순).
pub(crate) fn normalize_events(payload: &Value) -> Vec<CalendarEvent> {
    let mut out = Vec::new();
    let events = payload.get("events").and_then(Value::as_array);
    for ev in events.into_iter().flatten() {
        let comps = ev.get("eventComponents").and_then(Value::as_array);
        for comp in comps.into_iter().flatten() {
            let start = comp.get("start").cloned().unwrap_or(Value::Null);
            let end = comp.get("end").cloned().unwrap_or(Value::Null);
            let all_day = start.get("date").is_some(); // all-day 는 date, timed 는 dateTime
            let pick = |v: &Value| {
                v.get(if all_day { "date" } else { "dateTime" })
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            let attendees = comp
                .get("attendees")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            x.get("displayName")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    x.get("email")
                                        .and_then(Value::as_str)
                                        .filter(|s| !s.is_empty())
                                })
                        })
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let s = |k: &str| comp.get(k).and_then(Value::as_str).map(str::to_string);
            out.push(CalendarEvent {
                title: s("summary"),
                start: pick(&start),
                end: pick(&end),
                all_day,
                location: s("location"),
                description: s("description"),
                attendees,
            });
        }
    }
    sort_by_start(&mut out);
    out
}

/// 시작 시각 순 정렬(문자열 비교로 충분: RFC3339). 없는 것은 앞으로.
fn sort_by_start(events: &mut [CalendarEvent]) {
    events.sort_by(|a, b| {
        a.start
            .as_deref()
            .unwrap_or("")
            .cmp(b.start.as_deref().unwrap_or(""))
    });
}

/// 선택된 캘린더들(없으면 기본 캘린더 1회)을 `fetch` 로 조회해 합치고 정렬한다.
pub(crate) fn merge_calendars<F>(
    ids: &[String],
    mut fetch: F,
) -> Result<Vec<CalendarEvent>, NwError>
where
    F: FnMut(Option<&str>) -> Result<Vec<CalendarEvent>, NwError>,
{
    let mut events = Vec::new();
    if ids.is_empty() {
        events = fetch(None)?;
    } else {
        for id in ids {
            events.extend(fetch(Some(id))?);
        }
    }
    sort_by_start(&mut events);
    Ok(events)
}

pub struct NaverWorksCollector {
    cfg: NaverWorksConfig,
    client: Client,
    token_url: String,
    api_base: String,
}

impl NaverWorksCollector {
    pub fn new(cfg: NaverWorksConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            cfg,
            client,
            token_url: TOKEN_URL.into(),
            api_base: API_BASE.into(),
        }
    }

    /// 테스트·프록시용: 엔드포인트 교체.
    pub fn with_endpoints(
        mut self,
        token_url: impl Into<String>,
        api_base: impl Into<String>,
    ) -> Self {
        self.token_url = token_url.into();
        self.api_base = api_base.into();
        self
    }

    fn private_key(&self) -> Result<String, NwError> {
        if !self.cfg.private_key.trim().is_empty() {
            return Ok(self.cfg.private_key.clone());
        }
        let p = paths::expand_user(&self.cfg.private_key_path);
        fs::read_to_string(&p)
            .map_err(|e| NwError(format!("private key 파일을 읽을 수 없습니다: {e}")))
    }

    fn build_assertion(&self) -> Result<String, NwError> {
        use jsonwebtoken::{Algorithm, EncodingKey, Header};
        let now = Utc::now().timestamp();
        let claims = Claims {
            iss: &self.cfg.client_id,
            sub: &self.cfg.service_account,
            iat: now,
            exp: now + 3600,
        };
        let key = EncodingKey::from_rsa_pem(self.private_key()?.as_bytes())
            .map_err(|e| NwError(format!("JWT 서명 실패(키 형식 확인): {e}")))?;
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
            .map_err(|e| NwError(format!("JWT 서명 실패(키 형식 확인): {e}")))
    }

    /// 액세스 토큰 발급.
    pub fn get_token(&self) -> Result<String, NwError> {
        let assertion = self.build_assertion()?;
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
            ("client_id", self.cfg.client_id.as_str()),
            ("client_secret", self.cfg.client_secret.as_str()),
            ("scope", self.cfg.scope.as_str()),
        ];
        let resp = self
            .client
            .post(&self.token_url)
            .timeout(Duration::from_secs(15))
            .form(&form)
            .send()?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if status.as_u16() != 200 {
            let hint = if body.contains("invalid_scope") {
                format!(
                    " ⟶ Developer Console 앱의 'OAuth Scope' 에 '{}' 가 등록/승인돼 있는지 확인하세요. \
                     (읽기전용은 calendar.read, 읽기/쓰기는 calendar)",
                    self.cfg.scope
                )
            } else {
                String::new()
            };
            return Err(NwError(format!(
                "HTTP {}: {}{hint}",
                status.as_u16(),
                snippet(&body, 300)
            )));
        }
        let json: Value = serde_json::from_str(&body).map_err(|_| {
            NwError(format!(
                "토큰 응답 파싱 실패(JSON 아님): {}",
                snippet(&body, 200)
            ))
        })?;
        json.get("access_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .ok_or_else(|| NwError(format!("access_token 없음: {}", snippet(&body, 300))))
    }

    fn get_json(&self, url: &str, token: &str, query: &[(&str, &str)]) -> Result<Value, NwError> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(token)
            .query(query)
            .send()?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if status.as_u16() != 200 {
            return Err(NwError(format!(
                "HTTP {}: {}",
                status.as_u16(),
                snippet(&body, 300)
            )));
        }
        serde_json::from_str(&body).map_err(|_| {
            NwError(format!(
                "응답 파싱 실패(JSON 아님): {}",
                snippet(&body, 200)
            ))
        })
    }

    /// 캘린더 하나의 그날 이벤트(커서 페이지네이션 포함).
    pub(crate) fn fetch_calendar(
        &self,
        token: &str,
        ctx: &CollectContext,
        calendar_id: Option<&str>,
    ) -> Result<Vec<CalendarEvent>, NwError> {
        let user = quote(&self.cfg.user_id);
        let url = match calendar_id {
            Some(cid) => format!(
                "{}/users/{user}/calendars/{}/events",
                self.api_base,
                quote(cid)
            ),
            None => format!("{}/users/{user}/calendar/events", self.api_base),
        };
        let from = ctx.day.start.to_rfc3339();
        let until = ctx.day.end.to_rfc3339();
        let mut events = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_EVENT_PAGES {
            let mut q: Vec<(&str, &str)> = vec![("fromDateTime", &from), ("untilDateTime", &until)];
            if let Some(c) = &cursor {
                q.push(("cursor", c));
            }
            let payload = self.get_json(&url, token, &q)?;
            events.extend(normalize_events(&payload));
            cursor = next_cursor(&payload);
            if cursor.is_none() {
                break;
            }
        }
        Ok(events)
    }

    /// 선택된 캘린더들(복수)의 이벤트. 없으면 기본 캘린더.
    pub fn get_events(
        &self,
        token: &str,
        ctx: &CollectContext,
    ) -> Result<Vec<CalendarEvent>, NwError> {
        let ids = self.cfg.effective_calendar_ids();
        merge_calendars(&ids, |cid| self.fetch_calendar(token, ctx, cid))
    }

    /// 사용자의 캘린더 목록.
    pub fn list_calendars(&self) -> Result<Vec<CalendarInfo>, NwError> {
        if self.cfg.user_id.trim().is_empty() {
            return Err(NwError("사용자 ID 를 먼저 입력하세요.".into()));
        }
        let token = self
            .get_token()
            .map_err(|e| NwError(format!("토큰 발급 실패: {e}")))?;
        let url = format!(
            "{}/users/{}/calendar-personals",
            self.api_base,
            quote(&self.cfg.user_id)
        );
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_CALENDAR_PAGES {
            let q: Vec<(&str, &str)> = cursor
                .as_deref()
                .map(|c| vec![("cursor", c)])
                .unwrap_or_default();
            let data = self.get_json(&url, &token, &q)?;
            for c in extract_list(&data) {
                let Some(obj) = c.as_object() else { continue };
                let id = obj
                    .get("calendarId")
                    .or_else(|| obj.get("id"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let name = ["calendarName", "name", "summary", "title", "subject"]
                    .iter()
                    .find_map(|k| {
                        obj.get(*k)
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or("(이름 없음)");
                if let Some(id) = id {
                    out.push(CalendarInfo {
                        calendar_id: id.to_string(),
                        name: name.to_string(),
                    });
                }
            }
            cursor = next_cursor(&data);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// 실제로 액세스 토큰 발급을 시도해 자격증명이 유효한지 확인.
    pub fn test_connection(&self) -> (bool, String) {
        let mut missing: Vec<&str> = Vec::new();
        if self.cfg.client_id.trim().is_empty() {
            missing.push("Client ID");
        }
        if self.cfg.client_secret.trim().is_empty() {
            missing.push("Client Secret");
        }
        if self.cfg.service_account.trim().is_empty() {
            missing.push("Service Account");
        }
        if !self.cfg.has_private_key() {
            missing.push("Private Key");
        }
        if !missing.is_empty() {
            return (
                false,
                format!("필요한 값이 비어있습니다: {}", missing.join(", ")),
            );
        }
        match self.get_token() {
            Ok(_) => (true, "연결됨 · 액세스 토큰 발급 성공".into()),
            Err(e) => (false, format!("토큰 발급 실패: {e}")),
        }
    }
}

impl Collector for NaverWorksCollector {
    type Data = CalendarData;
    const NAME: &'static str = "naverworks";

    fn collect(&self, ctx: &CollectContext) -> CollectorResult<CalendarData> {
        let missing = self.cfg.missing_credentials();
        if !missing.is_empty() {
            return CollectorResult::skip(
                Self::NAME,
                format!("NaverWorks 자격증명 미설정: {}", missing.join(", ")),
            );
        }
        let token = match self.get_token() {
            Ok(t) => t,
            Err(e) => return CollectorResult::fail(Self::NAME, format!("토큰 발급 실패: {e}")),
        };
        match self.get_events(&token, ctx) {
            Ok(events) => CollectorResult::ok(Self::NAME, CalendarData { events }),
            Err(e) => CollectorResult::fail(Self::NAME, format!("캘린더 조회 실패: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{DayBounds, get_tz};
    use chrono::NaiveDate;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn ctx() -> CollectContext {
        let tz = get_tz("Asia/Seoul");
        CollectContext::new(
            DayBounds::for_date(NaiveDate::from_ymd_opt(2026, 7, 8).unwrap(), tz),
            "Asia/Seoul",
        )
    }

    #[test]
    fn extract_list_variants() {
        assert_eq!(
            extract_list(&json!({"calendarPersonals":[{"a":1}]})),
            vec![json!({"a":1})]
        );
        assert_eq!(extract_list(&json!([{"a":1}])), vec![json!({"a":1})]);
        assert_eq!(
            extract_list(&json!({"foo":[{"a":1}]})),
            vec![json!({"a":1})]
        );
        assert!(extract_list(&json!({})).is_empty());
        assert!(extract_list(&json!("x")).is_empty());
    }

    #[test]
    fn normalize_timed_and_all_day() {
        let payload = json!({"events":[{"eventComponents":[
            {"summary":"스프린트","start":{"dateTime":"2026-07-06T10:00:00+09:00"},"end":{"dateTime":"2026-07-06T11:00:00+09:00"},
             "location":"회의실 A","attendees":[{"displayName":"김"},{"email":"lee@x"},{"displayName":""}]},
            {"summary":"휴가","start":{"date":"2026-07-06"},"end":{"date":"2026-07-07"}}
        ]}]});
        let evs = normalize_events(&payload);
        assert_eq!(evs.len(), 2);
        assert!(evs[0].all_day); // "2026-07-06" < "2026-07-06T10..." → 종일이 먼저
        assert_eq!(evs[0].start.as_deref(), Some("2026-07-06"));
        assert_eq!(evs[1].title.as_deref(), Some("스프린트"));
        assert_eq!(evs[1].location.as_deref(), Some("회의실 A"));
        assert_eq!(evs[1].attendees, vec!["김", "lee@x"]);
        assert!(normalize_events(&json!({})).is_empty());
    }

    #[test]
    fn merge_selected_calendars_or_default() {
        let mut calls = Vec::new();
        let evs = merge_calendars(&["a".to_string(), "b".to_string()], |cid| {
            calls.push(cid.map(str::to_string));
            let t = if cid == Some("a") {
                "2026-07-06T09:00:00+09:00"
            } else {
                "2026-07-06T08:00:00+09:00"
            };
            Ok(vec![CalendarEvent {
                title: Some(format!("ev-{}", cid.unwrap())),
                start: Some(t.into()),
                ..Default::default()
            }])
        })
        .unwrap();
        assert_eq!(calls, vec![Some("a".into()), Some("b".into())]);
        assert_eq!(
            evs.iter()
                .map(|e| e.title.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["ev-b", "ev-a"]
        );
        let mut calls = Vec::new();
        merge_calendars(&[], |cid| {
            calls.push(cid.map(str::to_string));
            Ok(vec![])
        })
        .unwrap();
        assert_eq!(calls, vec![None]);
    }

    #[test]
    fn missing_credentials_skip_and_test_connection_message() {
        let c = NaverWorksCollector::new(NaverWorksConfig::default());
        let res = c.collect(&ctx());
        assert!(res.skipped);
        assert!(res.skip_reason.unwrap().contains("client_id"));
        let (ok, msg) = c.test_connection();
        assert!(!ok);
        assert!(msg.contains("Client ID") && msg.contains("Private Key"));
        assert_eq!(quote("me@hecto.co.kr"), "me%40hecto.co.kr");
        assert_eq!(quote("c_1/x-y.z~"), "c_1/x-y.z~");
    }

    /// 로컬 가짜 HTTP 서버로 이벤트 페이지네이션(cursor)을 검증한다.
    #[test]
    fn event_pagination_follows_cursor() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let first_line = req.lines().next().unwrap_or("").to_string();
                let has_cursor = first_line.contains("cursor=PAGE2");
                seen.push(first_line);
                let body = if has_cursor {
                    json!({"events":[{"eventComponents":[{"summary":"두번째","start":{"dateTime":"2026-07-08T11:00:00+09:00"},"end":{"dateTime":"2026-07-08T12:00:00+09:00"}}]}],
                           "responseMetaData":{}})
                } else {
                    json!({"events":[{"eventComponents":[{"summary":"첫번째","start":{"dateTime":"2026-07-08T09:00:00+09:00"},"end":{"dateTime":"2026-07-08T10:00:00+09:00"}}]}],
                           "responseMetaData":{"nextCursor":"PAGE2"}})
                }
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(resp.as_bytes()).unwrap();
            }
            seen
        });

        let cfg = NaverWorksConfig {
            user_id: "me@x".into(),
            ..Default::default()
        };
        let c = NaverWorksCollector::new(cfg).with_endpoints(
            format!("http://127.0.0.1:{port}/token"),
            format!("http://127.0.0.1:{port}"),
        );
        let evs = c.fetch_calendar("tok", &ctx(), Some("cal1")).unwrap();
        let seen = handle.join().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].contains("/users/me%40x/calendars/cal1/events?"));
        assert!(seen[0].contains("fromDateTime=2026-07-08T00%3A00%3A00%2B09%3A00"));
        assert!(!seen[0].contains("cursor="));
        assert!(seen[1].contains("cursor=PAGE2"));
        assert_eq!(
            evs.iter()
                .map(|e| e.title.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["첫번째", "두번째"]
        );
    }
}
