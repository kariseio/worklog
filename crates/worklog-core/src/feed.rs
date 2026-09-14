//! 오늘 피드 — 자동 수집(세션·커밋·회의)과 메모를 한 줄기 시간축으로.
//!
//! 앱의 '오늘' 화면이 그대로 그리는 형태이고, 실시간 갱신은 이전 피드와의 델타로 보낸다.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::{
    collect::SourceStatus,
    model::{DailyData, Session},
    render::is_meta_session,
    time::{fmt_time, parse_iso_in},
};

/// 마지막 활동이 이 안이면 세션을 '진행 중'으로 본다.
pub const ACTIVE_WINDOW_MIN: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedKind {
    Session,
    Commit,
    Meeting,
    Note,
}

impl FeedKind {
    fn order(self) -> u8 {
        match self {
            FeedKind::Meeting => 0,
            FeedKind::Session => 1,
            FeedKind::Commit => 2,
            FeedKind::Note => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedItem {
    /// 안정적인 식별자(델타 계산용).
    pub id: String,
    pub kind: FeedKind,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    /// 표시용 HH:MM (로컬).
    pub time: String,
    pub end_time: Option<String>,
    pub project: Option<String>,
    pub label: String,
    pub detail: Option<String>,
    /// claude | codex (세션만)
    pub agent: Option<String>,
    pub files: u32,
    pub insertions: u32,
    pub deletions: u32,
    /// 세션이 아직 진행 중(마지막 활동이 최근).
    pub active: bool,
    pub note_id: Option<i64>,
    pub tags: Vec<String>,
    pub mentions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedKpis {
    pub commits: u32,
    pub sessions: u32,
    pub meetings: u32,
    pub notes: u32,
    pub tokens: u64,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feed {
    pub date: chrono::NaiveDate,
    pub tz_name: String,
    pub items: Vec<FeedItem>,
    pub kpis: FeedKpis,
    pub statuses: Vec<SourceStatus>,
    pub warnings: Vec<String>,
    pub built_at: DateTime<Utc>,
    /// 가장 최근 이벤트 시각(세션 마지막 활동·커밋·메모 중 최대).
    pub last_event_at: Option<DateTime<Utc>>,
}

fn session_item(s: &Session, tz: Tz, now: DateTime<Utc>) -> FeedItem {
    let id = s
        .session_id
        .clone()
        .or_else(|| {
            s.cwd
                .clone()
                .map(|c| format!("{c}@{}", s.first_ts.map(|t| t.timestamp()).unwrap_or(0)))
        })
        .unwrap_or_else(|| "?".into());
    let active = s.last_ts.is_some_and(|l| {
        now - l <= Duration::minutes(ACTIVE_WINDOW_MIN) && l <= now + Duration::minutes(1)
    });
    FeedItem {
        id: format!("s:{id}"),
        kind: FeedKind::Session,
        start: s.first_ts,
        end: s.last_ts,
        time: fmt_time(s.first_ts.as_ref(), tz),
        end_time: s.last_ts.map(|t| fmt_time(Some(&t), tz)),
        project: s.project.clone(),
        label: s.display_title(),
        detail: s.git_branch.clone(),
        agent: Some(match s.agent {
            crate::model::Agent::Claude => "claude".into(),
            crate::model::Agent::Codex => "codex".into(),
        }),
        files: s.files_edited.len() as u32,
        insertions: 0,
        deletions: 0,
        active,
        note_id: None,
        tags: Vec::new(),
        mentions: Vec::new(),
    }
}

/// 수집 데이터 → 피드. `now` 는 '진행 중' 판정 기준.
pub fn build(data: &DailyData, statuses: &[SourceStatus], tz: Tz, now: DateTime<Utc>) -> Feed {
    let mut items: Vec<FeedItem> = Vec::new();
    let mut kpis = FeedKpis::default();
    let mut last_event: Option<DateTime<Utc>> = None;
    let mut bump = |t: Option<DateTime<Utc>>| {
        if let Some(t) = t
            && last_event.is_none_or(|l| t > l)
        {
            last_event = Some(t);
        }
    };

    for s in data.all_sessions() {
        if is_meta_session(s) {
            continue;
        }
        kpis.sessions += 1;
        kpis.tokens += s.output_tokens;
        bump(s.last_ts);
        items.push(session_item(s, tz, now));
    }
    if let Some(git) = &data.git {
        for c in &git.commits {
            kpis.commits += 1;
            kpis.insertions += c.insertions as u64;
            kpis.deletions += c.deletions as u64;
            bump(Some(c.when));
            let key = if c.repo_path.is_empty() {
                c.repo.clone()
            } else {
                c.repo_path.clone()
            };
            items.push(FeedItem {
                id: format!("c:{key}:{}", c.hash),
                kind: FeedKind::Commit,
                start: Some(c.when),
                end: None,
                time: fmt_time(Some(&c.when), tz),
                end_time: None,
                project: Some(c.repo.clone()),
                label: c.subject.clone(),
                detail: Some(c.short_hash().to_string()),
                agent: None,
                files: c.files_changed,
                insertions: c.insertions,
                deletions: c.deletions,
                active: false,
                note_id: None,
                tags: Vec::new(),
                mentions: Vec::new(),
            });
        }
    }
    if let Some(cal) = &data.calendar {
        for e in &cal.events {
            kpis.meetings += 1;
            let title = e
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "회의".into());
            let (start, end, time, end_time) = if e.all_day {
                (None, None, "종일".to_string(), None)
            } else {
                let s = e.start.as_deref().and_then(|t| parse_iso_in(t, tz));
                let en = e.end.as_deref().and_then(|t| parse_iso_in(t, tz));
                (
                    s,
                    en,
                    fmt_time(s.as_ref(), tz),
                    en.map(|t| fmt_time(Some(&t), tz)),
                )
            };
            items.push(FeedItem {
                id: format!("m:{}:{title}", e.start.clone().unwrap_or_default()),
                kind: FeedKind::Meeting,
                start,
                end,
                time,
                end_time,
                project: None,
                label: title,
                detail: e.location.clone().filter(|l| !l.is_empty()),
                agent: None,
                files: e.attendees.len() as u32,
                insertions: 0,
                deletions: 0,
                active: false,
                note_id: None,
                tags: Vec::new(),
                mentions: Vec::new(),
            });
        }
    }
    for n in &data.notes {
        kpis.notes += 1;
        bump(Some(n.ts));
        items.push(FeedItem {
            id: format!("n:{}", n.id),
            kind: FeedKind::Note,
            start: Some(n.ts),
            end: None,
            time: fmt_time(Some(&n.ts), tz),
            end_time: None,
            project: None,
            label: n.text.clone(),
            detail: Some(n.source.clone()),
            agent: None,
            files: 0,
            insertions: 0,
            deletions: 0,
            active: false,
            note_id: Some(n.id),
            tags: n.tags.clone(),
            mentions: n.mentions.clone(),
        });
    }

    // 시간순(시각 없는 종일 회의는 맨 앞), 같은 시각이면 회의 → 세션 → 커밋 → 메모.
    items.sort_by_key(|i| (i.start.is_some(), i.start, i.kind.order()));
    Feed {
        date: data.target_date,
        tz_name: data.tz_name.clone(),
        items,
        kpis,
        statuses: statuses.to_vec(),
        warnings: data.warnings.clone(),
        built_at: now,
        last_event_at: last_event,
    }
}

/// 두 피드의 차이. UI 는 델타만 받아 갱신한다.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedDelta {
    pub added: Vec<FeedItem>,
    pub updated: Vec<FeedItem>,
    pub removed: Vec<String>,
    pub kpis: FeedKpis,
    pub last_event_at: Option<DateTime<Utc>>,
}

impl FeedDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

pub fn delta(old: &Feed, new: &Feed) -> FeedDelta {
    let mut d = FeedDelta {
        kpis: new.kpis.clone(),
        last_event_at: new.last_event_at,
        ..Default::default()
    };
    for item in &new.items {
        match old.items.iter().find(|o| o.id == item.id) {
            None => d.added.push(item.clone()),
            Some(o) if o != item => d.updated.push(item.clone()),
            Some(_) => {}
        }
    }
    for o in &old.items {
        if !new.items.iter().any(|n| n.id == o.id) {
            d.removed.push(o.id.clone());
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CalendarData, CalendarEvent, GitCommit, GitData, NoteItem, SessionData};
    use crate::time::{get_tz, parse_iso};
    use chrono::NaiveDate;

    fn data() -> DailyData {
        let mut d = DailyData::new(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(), "Asia/Seoul");
        d.claude = Some(SessionData {
            sessions: vec![
                Session {
                    session_id: Some("s1".into()),
                    project: Some("kms".into()),
                    title: Some("로그인 배지".into()),
                    first_ts: parse_iso("2026-09-04T04:30:00Z"),
                    last_ts: parse_iso("2026-09-04T05:25:00Z"),
                    files_edited: vec!["a".into(), "b".into()],
                    output_tokens: 10,
                    ..Default::default()
                },
                Session {
                    intent: Some(format!("{} x", crate::render::WORKLOG_SENTINEL)),
                    first_ts: parse_iso("2026-09-04T06:00:00Z"),
                    ..Default::default()
                },
            ],
        });
        d.git = Some(GitData {
            commits: vec![GitCommit {
                repo: "kms".into(),
                hash: "abcdef1234567890".into(),
                author: "me".into(),
                when: parse_iso("2026-09-04T02:02:00Z").unwrap(),
                subject: "fix(api): 타임아웃".into(),
                files_changed: 1,
                insertions: 12,
                deletions: 4,
                repo_path: "D:/kms/.git".into(),
            }],
        });
        d.calendar = Some(CalendarData {
            events: vec![
                CalendarEvent {
                    title: Some("스프린트".into()),
                    start: Some("2026-09-04T10:00:00".into()),
                    end: Some("2026-09-04T10:30:00".into()),
                    ..Default::default()
                },
                CalendarEvent {
                    title: Some("휴가".into()),
                    start: Some("2026-09-04".into()),
                    all_day: true,
                    ..Default::default()
                },
            ],
        });
        d.notes.push(NoteItem {
            id: 7,
            ts: parse_iso("2026-09-04T01:35:00Z").unwrap(),
            text: "#요청 @김팀장 타임아웃".into(),
            tags: vec!["요청".into()],
            mentions: vec!["김팀장".into()],
            source: "app".into(),
        });
        d
    }

    #[test]
    fn builds_sorted_feed_with_kpis_and_active_flag() {
        let tz = get_tz("Asia/Seoul");
        let now = parse_iso("2026-09-04T05:30:00Z").unwrap(); // 세션 마지막 활동 5분 뒤
        let f = build(&data(), &[], tz, now);
        let ids: Vec<&str> = f.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "m:2026-09-04:휴가",
                "m:2026-09-04T10:00:00:스프린트",
                "n:7",
                "c:D:/kms/.git:abcdef1234567890",
                "s:s1"
            ]
        );
        assert_eq!(f.items[0].time, "종일");
        assert_eq!(f.items[1].time, "10:00");
        assert_eq!(f.items[1].end_time.as_deref(), Some("10:30"));
        assert_eq!(f.items[2].tags, vec!["요청"]);
        assert_eq!(f.items[3].insertions, 12);
        assert_eq!(f.items[3].detail.as_deref(), Some("abcdef12"));
        let s = &f.items[4];
        assert!(s.active);
        assert_eq!(s.agent.as_deref(), Some("claude"));
        assert_eq!(s.files, 2);
        assert_eq!(
            f.kpis,
            FeedKpis {
                commits: 1,
                sessions: 1,
                meetings: 2,
                notes: 1,
                tokens: 10,
                insertions: 12,
                deletions: 4
            }
        );
        assert_eq!(f.last_event_at, parse_iso("2026-09-04T05:25:00Z"));
        // 한참 뒤에 보면 진행 중이 아님
        let later = build(&data(), &[], tz, parse_iso("2026-09-04T09:00:00Z").unwrap());
        assert!(!later.items[4].active);
    }

    #[test]
    fn delta_detects_added_updated_removed() {
        let tz = get_tz("Asia/Seoul");
        let now = parse_iso("2026-09-04T05:30:00Z").unwrap();
        let old = build(&data(), &[], tz, now);
        let mut d2 = data();
        d2.notes.push(NoteItem {
            id: 8,
            ts: parse_iso("2026-09-04T05:20:00Z").unwrap(),
            text: "새 메모".into(),
            tags: vec![],
            mentions: vec![],
            source: "tray".into(),
        });
        d2.claude.as_mut().unwrap().sessions[0].last_ts = parse_iso("2026-09-04T05:29:00Z");
        d2.git.as_mut().unwrap().commits.clear();
        let new = build(&d2, &[], tz, now);
        let dl = delta(&old, &new);
        assert_eq!(
            dl.added.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["n:8"]
        );
        assert_eq!(
            dl.updated.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["s:s1"]
        );
        assert_eq!(dl.removed, vec!["c:D:/kms/.git:abcdef1234567890"]);
        assert_eq!(dl.kpis.notes, 2);
        assert!(!dl.is_empty());
        assert!(delta(&new, &new).is_empty());
    }
}
