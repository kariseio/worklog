//! 메모 — 구두 요청·결정·할 일을 채팅처럼 한 줄씩 남긴다.
//!
//! 본문에서 `#태그` 와 `@이름` 을 뽑아 함께 저장한다(본문은 그대로 둔다). 저장은 [`Store`],
//! 날짜는 설정 시간대 기준 로컬 날짜다. 앱 창·트레이 빠른 메모·CLI(`worklog note`) 가 공유한다.

use std::sync::LazyLock;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use regex::Regex;

use crate::{
    model::NoteItem,
    store::{NewNote, Note, Result, Store},
};

static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#([\p{L}\p{N}_][\p{L}\p{N}_\-]*)").expect("regex"));
static MENTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@([\p{L}\p{N}_][\p{L}\p{N}_.\-]*)").expect("regex"));

/// 본문에서 뽑은 태그·멘션(등장 순서, 중복 제거).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    pub text: String,
    pub tags: Vec<String>,
    pub mentions: Vec<String>,
}

fn unique_captures(re: &Regex, text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for cap in re.captures_iter(text) {
        let v = cap[1].trim_end_matches(['.', '-']).to_string();
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// `#요청 @김팀장 결제 API 타임아웃 늘려달라` → 태그 ["요청"], 멘션 ["김팀장"], 본문은 앞뒤 공백만 정리.
pub fn parse(text: &str) -> Parsed {
    let text = text.trim();
    Parsed {
        text: text.to_string(),
        tags: unique_captures(&TAG_RE, text),
        mentions: unique_captures(&MENTION_RE, text),
    }
}

/// 메모 한 줄 저장. `at` 이 없으면 지금. 날짜는 `tz` 기준 로컬 날짜.
pub fn add_note(
    store: &Store,
    tz: Tz,
    text: &str,
    source: &str,
    at: Option<DateTime<Utc>>,
) -> Result<Option<Note>> {
    let parsed = parse(text);
    if parsed.text.is_empty() {
        return Ok(None);
    }
    let ts = at.unwrap_or_else(Utc::now);
    let date: NaiveDate = ts.with_timezone(&tz).date_naive();
    let note = store.note_add(&NewNote {
        date,
        ts,
        text: &parsed.text,
        tags: &parsed.tags,
        mentions: &parsed.mentions,
        source,
    })?;
    Ok(Some(note))
}

/// 본문 수정(태그·멘션 재추출).
pub fn edit_note(store: &Store, id: i64, text: &str) -> Result<bool> {
    let parsed = parse(text);
    if parsed.text.is_empty() {
        return Ok(false);
    }
    store.note_update(id, &parsed.text, &parsed.tags, &parsed.mentions)
}

/// 저장소 행 → 파이프라인 모델.
pub fn to_item(n: &Note) -> NoteItem {
    NoteItem {
        id: n.id,
        ts: n.ts,
        text: n.text.clone(),
        tags: n.tags.clone(),
        mentions: n.mentions.clone(),
        source: n.source.clone(),
    }
}

/// 그날의 메모(시간순)를 파이프라인 모델로. 저장소가 없거나 실패하면 빈 목록.
pub fn items_for(store: Option<&Store>, date: NaiveDate) -> Vec<NoteItem> {
    let Some(store) = store else {
        return Vec::new();
    };
    match store.notes_for(date) {
        Ok(v) => v.iter().map(to_item).collect(),
        Err(e) => {
            tracing::warn!("메모 조회 실패({date}): {e}");
            Vec::new()
        }
    }
}

/// 메모 한 줄의 표시용 꼬리표: 태그·멘션을 `#요청 @김팀장` 형태로. 본문에 이미 있으면 생략.
pub fn trailer(n: &NoteItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    for t in &n.tags {
        let tok = format!("#{t}");
        if !n.text.contains(&tok) {
            parts.push(tok);
        }
    }
    for m in &n.mentions {
        let tok = format!("@{m}");
        if !n.text.contains(&tok) {
            parts.push(tok);
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{get_tz, parse_iso};

    #[test]
    fn parses_tags_and_mentions() {
        let p = parse("  #요청 @김팀장 결제 API 타임아웃 3초→10초 #요청 #할일, @박PM. ");
        assert_eq!(
            p.text,
            "#요청 @김팀장 결제 API 타임아웃 3초→10초 #요청 #할일, @박PM."
        );
        assert_eq!(p.tags, vec!["요청", "할일"]);
        assert_eq!(p.mentions, vec!["김팀장", "박PM"]);
        let p = parse("태그 없음");
        assert!(p.tags.is_empty() && p.mentions.is_empty());
        let p = parse("이메일 me@x.com 은 멘션이 아님? @lee-j.k 는 멘션");
        assert_eq!(p.mentions, vec!["x.com", "lee-j.k"]); // '@' 앞 문자를 보지 않는 단순 규칙(문서화)
        assert!(parse("   ").text.is_empty());
        assert_eq!(parse("#a-b #c_1 #-").tags, vec!["a-b", "c_1"]);
    }

    #[test]
    fn add_and_edit_use_local_date() {
        let store = Store::open_in_memory().unwrap();
        let tz = get_tz("Asia/Seoul");
        // 2026-09-04 15:30Z = 09-05 00:30 KST → 날짜는 09-05
        let at = parse_iso("2026-09-04T15:30:00Z");
        let n = add_note(&store, tz, "#결정 배포는 목요일", "tray", at)
            .unwrap()
            .unwrap();
        assert_eq!(n.date, NaiveDate::from_ymd_opt(2026, 9, 5).unwrap());
        assert_eq!(n.tags, vec!["결정"]);
        assert_eq!(n.source, "tray");
        assert!(add_note(&store, tz, "  ", "app", None).unwrap().is_none());
        assert!(edit_note(&store, n.id, "#할일 @김팀장 QA 요청").unwrap());
        let e = store.note_get(n.id).unwrap().unwrap();
        assert_eq!(e.tags, vec!["할일"]);
        assert_eq!(e.mentions, vec!["김팀장"]);
        assert!(!edit_note(&store, n.id, "   ").unwrap());
        assert_eq!(store.notes_for(n.date).unwrap().len(), 1);

        let items = items_for(Some(&store), n.date);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "#할일 @김팀장 QA 요청");
        assert_eq!(items[0].tags, vec!["할일"]);
        assert_eq!(trailer(&items[0]), ""); // 본문에 이미 있으면 꼬리표 없음
        assert!(items_for(None, n.date).is_empty());
        assert!(items_for(Some(&store), NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()).is_empty());
        let mut it = items[0].clone();
        it.text = "QA 요청".into();
        assert_eq!(trailer(&it), " [#할일 @김팀장]");
    }
}
