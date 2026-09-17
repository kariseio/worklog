//! 라이브 수집 엔진 스레드 — [`Live`] 를 독점하고 파일 감시·주기 보정·정해진 시각 동작을 돌린다.
//!
//! 요청은 [`WorkerMsg`] 채널로 받고, 결과는 `feed:changed` / `feed:refreshing` / `reminder:fired`
//! 이벤트와 [`AppState::set_feed`] 스냅샷으로 내보낸다. 감시기의 변경 배치는 작은 전달 스레드가
//! 같은 채널로 넣어 준다(std mpsc 에는 select 가 없다).
//!
//! 오늘 피드는 갱신될 때마다 SQLite `day_feeds` 에 스냅샷으로 남긴다(원본이 사라진 뒤에도
//! 지난 날짜 타임라인을 그릴 수 있게). 틱·감시 폭주에는 [`PERSIST_MIN`] 간격으로 묶어 쓴다.
//!
//! 저장소 목록은 SQLite `repos` 캐시를 쓴다: 시작 때 캐시로 먼저 빠르게 채우고, 이어서 디스크를
//! 한 번 탐색해 캐시를 갱신한다. 주기 전체 수집은 캐시(+세션 cwd)만 다시 읽는다. 디스크 재탐색은
//! 시작 때와 '저장소 다시 찾기'([`WorkerMsg::RescanRepos`]), git 설정 변경 때만.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use worklog_core::{
    config::{Config, ScheduleMode},
    feed::{self, Feed, FeedDelta},
    live::Live,
    paths, schedule,
    store::{RepoEntry, Store, run_kind},
    time::get_tz,
    watch::{Changed, FileWatcher, WatchHandle},
};

use crate::{
    generate, shell,
    state::{AppState, Reminder, WorkerMsg},
};

/// 요청 대기 최대 시간 — 이 주기로 타이머(틱·폴링·스케줄)를 점검한다.
const POLL: Duration = Duration::from_secs(10);
/// 진행 중 표시 재평가 · 메모 재조회 · 날짜 넘김 · 생존 기록 주기.
const TICK: Duration = Duration::from_secs(60);
/// 파일 변경 디바운스.
const DEBOUNCE: Duration = Duration::from_millis(700);
/// 앱이 마지막으로 살아 있던 시각(놓친 스케줄 판단용).
const KV_LAST_ALIVE: &str = "app.last_alive";
/// 마지막으로 처리한 정해진 시각 동작.
const KV_LAST_FIRED: &str = "schedule.last_fired";
/// 오늘 스냅샷 저장 최소 간격(틱·파일 변경 폭주로 SQLite 를 두들기지 않게).
/// 전체 수집·설정 변경·날짜 넘김은 이 간격과 무관하게 항상 저장한다.
const PERSIST_MIN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub struct FeedChanged {
    /// startup | scan | rescan | manual | periodic | config | rollover | watch | notes | calendar | tick
    pub reason: &'static str,
    pub delta: FeedDelta,
    pub feed: Feed,
}

pub fn spawn(app: AppHandle, tx: mpsc::Sender<WorkerMsg>, rx: mpsc::Receiver<WorkerMsg>) {
    thread::Builder::new()
        .name("worklog-engine".into())
        .spawn(move || run(app, tx, rx))
        .expect("엔진 스레드 시작");
}

fn run(app: AppHandle, tx: mpsc::Sender<WorkerMsg>, rx: mpsc::Receiver<WorkerMsg>) {
    let store = match Store::open(&paths::db_path()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("엔진용 저장소 연결 실패(메모·저장소 캐시 없이 진행): {e}");
            None
        }
    };
    let cfg = app.state::<AppState>().config();
    let live = match Live::new(cfg, None) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("라이브 엔진 초기화 실패: {e}");
            return;
        }
    };
    let mut eng = Engine {
        app,
        tx,
        live,
        watcher: None,
        store,
        last_full: None,
        last_meeting: None,
        last_tick: Instant::now(),
        // 부팅 직후(자동 시작)에도 안전하게 — 뺄 수 없으면 그냥 지금.
        last_persist: Instant::now()
            .checked_sub(PERSIST_MIN)
            .unwrap_or_else(Instant::now),
        next_fire: None,
    };
    if let Err(p) = catch_unwind(AssertUnwindSafe(|| eng.startup())) {
        eng.report_panic(&p, "시작 수집");
    }

    loop {
        let first = match rx.recv_timeout(POLL) {
            Ok(msg) => Some(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let batch = match first {
            Some(m) => coalesce(drain(&rx, m)),
            None => Vec::new(),
        };
        let res = catch_unwind(AssertUnwindSafe(|| {
            for m in batch {
                eng.handle(m);
            }
            eng.timers();
        }));
        if let Err(p) = res {
            eng.report_panic(&p, "엔진 작업");
        }
    }
}

/// 첫 메시지 뒤에 쌓여 있는 것들까지 한 번에 꺼낸다.
fn drain(rx: &mpsc::Receiver<WorkerMsg>, first: WorkerMsg) -> Vec<WorkerMsg> {
    let mut msgs = vec![first];
    while let Ok(m) = rx.try_recv() {
        msgs.push(m);
    }
    msgs
}

/// 쌓인 요청을 합친다: 설정 변경은 마지막 것만, 전체 수집이 있으면 부분 갱신은 생략(전체가 포함),
/// 변경 배치는 하나로 이어 붙인다. 엔진이 바쁜 동안 같은 전체 수집이 여러 번 돌지 않게.
fn coalesce(msgs: Vec<WorkerMsg>) -> Vec<WorkerMsg> {
    let mut config: Option<Box<Config>> = None;
    let (mut full, mut rescan, mut notes, mut cal) = (false, false, false, false);
    let mut changes: Vec<Changed> = Vec::new();
    for m in msgs {
        match m {
            WorkerMsg::ConfigChanged(c) => config = Some(c),
            WorkerMsg::FullRefresh => full = true,
            WorkerMsg::RescanRepos => rescan = true,
            WorkerMsg::NotesChanged => notes = true,
            WorkerMsg::CalendarRefresh => cal = true,
            WorkerMsg::Changes(b) => changes.extend(b),
        }
    }
    let heavy = config.is_some() || rescan || full;
    let mut out = Vec::new();
    if let Some(c) = config {
        out.push(WorkerMsg::ConfigChanged(c)); // 설정 변경은 그 자체로 전체 수집
    }
    if rescan {
        out.push(WorkerMsg::RescanRepos);
    } else if full && out.is_empty() {
        out.push(WorkerMsg::FullRefresh);
    }
    if !heavy {
        if !changes.is_empty() {
            changes.sort();
            changes.dedup();
            out.push(WorkerMsg::Changes(changes));
        }
        if notes {
            out.push(WorkerMsg::NotesChanged);
        }
        if cal {
            out.push(WorkerMsg::CalendarRefresh);
        }
    }
    out
}

/// 간격과 무관하게 항상 스냅샷을 남기는 이유들(전체 수집·설정 변경·날짜 넘김).
/// 나머지(watch · notes · calendar · tick)는 [`PERSIST_MIN`] 간격으로 묶는다.
fn always_persists(reason: &str) -> bool {
    matches!(
        reason,
        "startup" | "scan" | "rescan" | "manual" | "periodic" | "config" | "rollover"
    )
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "알 수 없는 패닉".into())
}

struct Engine {
    app: AppHandle,
    tx: mpsc::Sender<WorkerMsg>,
    live: Live,
    watcher: Option<WatchHandle>,
    store: Option<Store>,
    last_full: Option<DateTime<Utc>>,
    last_meeting: Option<DateTime<Utc>>,
    last_tick: Instant,
    /// 오늘 스냅샷을 마지막으로 저장한 시각.
    last_persist: Instant,
    next_fire: Option<DateTime<Utc>>,
}

impl Engine {
    fn state(&self) -> tauri::State<'_, AppState> {
        self.app.state::<AppState>()
    }

    /// 시작 순서: 감시 먼저(수집 중 생기는 변경을 놓치지 않게) → 캐시로 빠른 첫 수집 →
    /// 디스크 탐색으로 캐시 갱신 → 놓친 스케줄 → 다음 발동 시각.
    fn startup(&mut self) {
        self.restart_watcher();
        let cached: Vec<PathBuf> = self
            .store
            .as_ref()
            .and_then(|s| s.repos_all().ok())
            .map(|v| v.into_iter().map(|r| PathBuf::from(r.path)).collect())
            .unwrap_or_default();
        if cached.is_empty() {
            self.full_refresh("startup");
        } else {
            tracing::info!("저장소 캐시 {}개로 먼저 채웁니다.", cached.len());
            self.live.set_known_repos(Some(cached));
            self.full_refresh("startup");
            self.live.set_known_repos(None);
            self.full_refresh("scan");
        }
        self.persist_repos(true);
        self.freeze_repos();
        self.check_missed_schedule();
        self.reschedule(Utc::now());
    }

    fn report_panic(&self, p: &(dyn std::any::Any + Send), what: &str) {
        let msg = panic_message(p);
        tracing::error!("{what} 중 패닉(엔진은 계속 돕니다): {msg}");
        self.set_refreshing(false);
        let _ = self
            .app
            .emit("engine:error", format!("{what} 중 오류: {msg}"));
    }

    /// 스냅샷 갱신 + 저장 + 이벤트. 틱에서 아무것도 안 바뀌었으면 저장·이벤트 모두 생략.
    fn publish(&mut self, reason: &'static str, delta: FeedDelta) {
        let feed = self.live.feed().clone();
        self.state().set_feed(feed.clone());
        if reason == "tick" && delta.is_empty() {
            return;
        }
        self.persist_feed(reason);
        let _ = self.app.emit(
            "feed:changed",
            FeedChanged {
                reason,
                delta,
                feed,
            },
        );
    }

    /// 오늘 피드를 `day_feeds` 에 남긴다. 이미 저장된 스냅샷이 있으면 이번 수집에 없는 예전
    /// 항목(사라진 세션 로그 등)을 `archived` 로 남기며 합친다. 실패해도 엔진은 계속 돈다.
    fn persist_feed(&mut self, reason: &'static str) {
        if !always_persists(reason) && self.last_persist.elapsed() < PERSIST_MIN {
            return;
        }
        self.last_persist = Instant::now();
        let Some(store) = &self.store else { return };
        let live = self.live.feed();
        let date = live.date;
        let stored = match store.day_feed_get(date) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("{date} 저장된 피드 조회 실패: {e}");
                None
            }
        };
        let old: Option<Feed> = stored.and_then(|(json, _)| match serde_json::from_str(&json) {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!("{date} 저장된 피드가 깨져 새로 씁니다: {e}");
                None
            }
        });
        let mut merged = match &old {
            Some(o) => feed::merge_keep_missing(o, live),
            None => live.clone(),
        };
        merged.source = "stored".into();
        merged.stored_at = Some(Utc::now());
        let json = match serde_json::to_string(&merged) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("{date} 피드 직렬화 실패: {e}");
                return;
            }
        };
        match store.day_feed_put(date, &json) {
            Ok(()) => tracing::debug!(
                "{date} 피드 스냅샷 저장({reason}) — 항목 {}개",
                merged.items.len()
            ),
            Err(e) => tracing::warn!("{date} 피드 스냅샷 저장 실패: {e}"),
        }
    }

    fn set_refreshing(&self, on: bool) {
        self.state()
            .refreshing
            .store(on, std::sync::atomic::Ordering::Relaxed);
        let _ = self.app.emit("feed:refreshing", on);
    }

    fn full_refresh(&mut self, reason: &'static str) {
        self.set_refreshing(true);
        let t = Instant::now();
        let now = Utc::now();
        let delta = self.live.full_refresh(self.store.as_ref(), now);
        tracing::info!(
            "전체 수집({reason}) {:.1}s — 항목 {}개, 저장소 {}개",
            t.elapsed().as_secs_f32(),
            self.live.feed().items.len(),
            self.live.repo_summaries().len()
        );
        self.last_full = Some(now);
        self.last_meeting = Some(now); // 전체 수집에 회의 조회가 포함된다.
        self.sync_git_dirs();
        self.publish(reason, delta);
        self.set_refreshing(false);
    }

    /// 지금 알고 있는 저장소를 캐시(SQLite `repos`)에 쓴다. `prune` 이면 이번에 못 본 항목은 지운다.
    fn persist_repos(&self, prune: bool) {
        let Some(store) = &self.store else { return };
        let now = Utc::now();
        let summaries = self.live.repo_summaries();
        for (ident, last_commit) in &summaries {
            let entry = RepoEntry {
                common_dir: ident.common_dir.clone(),
                path: ident.path.display().to_string(),
                name: ident.name.clone(),
                source: "scan".into(),
                first_seen: now,
                last_seen: now,
                last_commit_ts: *last_commit,
            };
            if let Err(e) = store.repo_upsert(&entry) {
                tracing::debug!("저장소 캐시 저장 실패({}): {e}", ident.name);
            }
        }
        if prune && let Ok(all) = store.repos_all() {
            for r in all {
                if !summaries.iter().any(|(i, _)| i.common_dir == r.common_dir) {
                    let _ = store.repo_delete(&r.common_dir);
                }
            }
        }
    }

    /// 다음 전체 수집부터 디스크 탐색 없이 현재 저장소 목록을 쓴다.
    fn freeze_repos(&mut self) {
        let paths: Vec<PathBuf> = self
            .live
            .repo_summaries()
            .into_iter()
            .map(|(i, _)| i.path)
            .collect();
        self.live.set_known_repos(Some(paths));
    }

    fn sync_git_dirs(&mut self) {
        if let Some(w) = &mut self.watcher {
            w.set_git_dirs(self.live.watch_targets().git_common_dirs);
        }
    }

    /// 감시기를 (다시) 시작한다. 실시간 수집이 꺼져 있으면 없앤다.
    fn restart_watcher(&mut self) {
        self.watcher = None; // 이전 감시기·전달 스레드는 여기서 정리된다.
        if !self.live.config().automation.realtime.enabled {
            tracing::info!("실시간 수집 꺼짐 — 파일 감시 없이 수동/주기 갱신만");
            return;
        }
        match FileWatcher::start(self.live.watch_targets(), DEBOUNCE) {
            Ok(w) => {
                let (handle, rx) = w.into_parts();
                let tx = self.tx.clone();
                let spawned = thread::Builder::new()
                    .name("worklog-watch-forward".into())
                    .spawn(move || {
                        for batch in rx {
                            if tx.send(WorkerMsg::Changes(batch)).is_err() {
                                break;
                            }
                        }
                    });
                if let Err(e) = spawned {
                    tracing::warn!("감시 전달 스레드 시작 실패: {e}");
                    return;
                }
                tracing::info!(
                    "파일 감시 시작 — 저장소 {}개",
                    handle.targets().git_common_dirs.len()
                );
                self.watcher = Some(handle);
            }
            Err(e) => tracing::warn!("파일 감시 시작 실패: {e}"),
        }
    }

    fn handle(&mut self, msg: WorkerMsg) {
        let now = Utc::now();
        match msg {
            WorkerMsg::FullRefresh => self.full_refresh("manual"),
            WorkerMsg::RescanRepos => {
                self.live.set_known_repos(None);
                self.full_refresh("rescan");
                self.persist_repos(true);
                self.freeze_repos();
            }
            WorkerMsg::NotesChanged => {
                let d = self.live.refresh_notes(self.store.as_ref(), now);
                self.publish("notes", d);
            }
            WorkerMsg::CalendarRefresh => {
                let d = self.live.refresh_calendar(now);
                self.last_meeting = Some(now);
                self.publish("calendar", d);
            }
            WorkerMsg::ConfigChanged(cfg) => self.apply_config(*cfg),
            WorkerMsg::Changes(batch) => {
                let t = Instant::now();
                let d = self.live.apply_changes(&batch, now);
                tracing::debug!(
                    "변경 {}건 반영 {:?} — +{} ~{} -{}",
                    batch.len(),
                    t.elapsed(),
                    d.added.len(),
                    d.updated.len(),
                    d.removed.len()
                );
                self.sync_git_dirs();
                self.publish("watch", d);
            }
        }
    }

    /// 설정 변경: 시간대가 바뀌면 하루 경계가 달라지므로 엔진을 새로 만든다. git 설정이 바뀌면
    /// 저장소를 다시 찾는다. 그 외는 감시 재시작 + 전체 수집 + 스케줄 재계산.
    fn apply_config(&mut self, cfg: Config) {
        let old = self.live.config().clone();
        let tz_changed = old.timezone != cfg.timezone;
        let git_changed = old.sources.git != cfg.sources.git;
        let known = self.live.known_repos().map(|k| k.to_vec());
        if tz_changed {
            match Live::new(cfg.clone(), None) {
                Ok(l) => self.live = l,
                Err(e) => tracing::warn!("시간대 변경 적용 실패: {e}"),
            }
        }
        self.live.set_config(cfg);
        self.live
            .set_known_repos(if git_changed { None } else { known });
        self.restart_watcher();
        self.full_refresh("config");
        if git_changed {
            self.persist_repos(true);
            self.freeze_repos();
        }
        self.reschedule(Utc::now());
    }

    fn timers(&mut self) {
        let now = Utc::now();
        if self.last_tick.elapsed() >= TICK {
            self.last_tick = Instant::now();
            self.heartbeat(now);
            if self.live.is_stale(now) {
                self.rollover();
            } else {
                // 메모는 CLI(`worklog note`)로도 들어오므로 틱마다 다시 읽는다(SQLite 조회 한 번).
                // 재조립되면서 세션 '진행 중' 표시도 다시 평가된다.
                let d = self.live.refresh_notes(self.store.as_ref(), now);
                self.publish("tick", d);
            }
        }
        // 회의 폴링·전체 재수집은 실시간(파일 감시) 설정과 무관하게 돈다(계획 §4.4).
        let rt = self.live.config().automation.realtime.clone();
        let nw_on = self.live.config().sources.naverworks.enabled;
        if nw_on && schedule::interval_due(self.last_meeting, now, rt.meeting_poll_min) {
            let d = self.live.refresh_calendar(now);
            self.last_meeting = Some(now);
            self.publish("calendar", d);
        }
        if schedule::interval_due(self.last_full, now, rt.full_rescan_min) {
            self.full_refresh("periodic");
        }
        if let Some(f) = self.next_fire
            && now >= f
        {
            self.fire(f, false);
            self.reschedule(now);
        }
    }

    fn heartbeat(&self, now: DateTime<Utc>) {
        if let Some(s) = &self.store
            && let Err(e) = s.kv_set(KV_LAST_ALIVE, &now.to_rfc3339())
        {
            tracing::debug!("생존 기록 실패: {e}");
        }
    }

    fn rollover(&mut self) {
        tracing::info!("날짜가 바뀌어 오늘 피드를 새로 만듭니다.");
        self.persist_feed("rollover"); // Live 를 갈아 끼우기 전에 그날의 마지막 스냅샷을 남긴다.
        let known = self.live.known_repos().map(|k| k.to_vec());
        match Live::new(self.live.config().clone(), None) {
            Ok(l) => self.live = l,
            Err(e) => {
                tracing::warn!("날짜 넘김 실패: {e}");
                return;
            }
        }
        self.live.set_known_repos(known);
        self.restart_watcher();
        self.full_refresh("rollover");
    }

    fn reschedule(&mut self, now: DateTime<Utc>) {
        let cfg = self.live.config();
        let tz = get_tz(&cfg.timezone);
        self.next_fire = schedule::next_fire_after(&cfg.automation.schedule, now, tz);
        match self.next_fire {
            Some(f) => tracing::info!(
                "다음 정해진 시각 동작: {} ({:?})",
                f.with_timezone(&tz).format("%m-%d %H:%M"),
                cfg.automation.schedule.mode
            ),
            None => tracing::info!("정해진 시각 동작 없음(꺼짐)"),
        }
    }

    fn kv_time(&self, key: &str) -> Option<DateTime<Utc>> {
        self.store
            .as_ref()?
            .kv_get(key)
            .ok()
            .flatten()
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&Utc))
    }

    /// 앱이 꺼져 있던 동안 놓친 발동이 있으면(24시간 안) 한 번만 처리한다.
    fn check_missed_schedule(&mut self) {
        let now = Utc::now();
        let Some(last_alive) = self.kv_time(KV_LAST_ALIVE) else {
            self.heartbeat(now); // 첫 실행 — 기준점만 남긴다.
            return;
        };
        let last_fired = self.kv_time(KV_LAST_FIRED);
        let cfg = self.live.config().clone();
        let tz = get_tz(&cfg.timezone);
        if let Some(f) = schedule::due_between(&cfg.automation.schedule, last_alive, now, tz)
            && last_fired.is_none_or(|lf| f > lf)
            && now - f < chrono::Duration::hours(24)
        {
            tracing::info!(
                "꺼져 있던 동안 놓친 발동 처리: {}",
                f.with_timezone(&tz).format("%m-%d %H:%M")
            );
            self.fire(f, true);
        }
        self.heartbeat(now);
    }

    fn fire(&mut self, at: DateTime<Utc>, missed: bool) {
        if let Some(s) = &self.store
            && let Err(e) = s.kv_set(KV_LAST_FIRED, &at.to_rfc3339())
        {
            tracing::debug!("발동 기록 실패: {e}");
        }
        let cfg = self.live.config().clone();
        let tz = get_tz(&cfg.timezone);
        let date = at.with_timezone(&tz).date_naive();
        let reminder = match cfg.automation.schedule.mode {
            ScheduleMode::Notify => {
                shell::notify(
                    &self.app,
                    "일지 만들 시간",
                    &format!("{date} 일지를 만들까요? 클릭해서 열기"),
                );
                Reminder {
                    date,
                    mode: "notify",
                    run_id: None,
                    missed,
                    at,
                }
            }
            ScheduleMode::Generate => {
                // 자동 생성은 편집된 일지를 절대 덮어쓰지 않는다.
                match generate::start(&self.app, date, run_kind::AUTO, false) {
                    Ok(id) => Reminder {
                        date,
                        mode: "generate",
                        run_id: Some(id),
                        missed,
                        at,
                    },
                    Err(e) => {
                        tracing::warn!("정해진 시각 자동 생성 건너뜀: {e}");
                        shell::notify(&self.app, "자동 생성 건너뜀", &e);
                        Reminder {
                            date,
                            mode: "generate",
                            run_id: None,
                            missed,
                            at,
                        }
                    }
                }
            }
        };
        self.state().set_last_reminder(reminder.clone());
        let _ = self.app.emit("reminder:fired", reminder);
    }
}
