//! 앱 전역 상태 — 설정 · 메모 저장소 · 피드 스냅샷 · 엔진 스레드 채널 · 생성 작업.
//!
//! 라이브 수집 엔진([`crate::worker`])은 자기 스레드가 `Live` 를 독점하고, 커맨드는
//! 여기 있는 스냅샷([`AppState::feed`])만 읽는다. 저장소는 커맨드용 연결 하나를 뮤텍스로
//! 공유하고, 엔진은 자기 연결을 따로 연다(SQLite WAL 이라 동시 읽기·쓰기가 된다).

use std::sync::{
    Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
    atomic::AtomicBool,
    mpsc,
};

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use worklog_core::{
    config::{Config, LoadStatus},
    feed::Feed,
    store::Store,
};

/// 엔진 스레드에 보내는 요청.
pub enum WorkerMsg {
    /// 모든 소스 다시 수집('지금 갱신' · 주기 보정). 저장소는 캐시 목록을 쓴다.
    FullRefresh,
    /// 디스크를 다시 탐색해 저장소 캐시를 새로 만들고 전체 수집.
    RescanRepos,
    /// 메모가 바뀌었다 — 저장소에서 다시 읽어 피드에 반영.
    NotesChanged,
    /// 회의 다시 조회.
    CalendarRefresh,
    /// 설정이 바뀌었다 — 감시 대상·주기·스케줄 재계산 후 전체 수집.
    ConfigChanged(Box<Config>),
    /// 감시기가 보낸 변경 배치.
    Changes(Vec<worklog_core::watch::Changed>),
}

/// 진행 중인 생성 작업의 화면 표시용 상태.
#[derive(Debug, Clone, Serialize)]
pub struct GenStatus {
    pub run_id: i64,
    pub date: NaiveDate,
    /// manual | auto
    pub kind: String,
    /// 이번 실행에 쓰는 일지 템플릿 id(standard | report | retro).
    pub template: String,
    pub step: String,
    pub detail: String,
    pub started: DateTime<Utc>,
}

pub struct GenJob {
    pub status: GenStatus,
    pub cancel: Arc<AtomicBool>,
}

/// 정해진 시각 동작이 발동했을 때의 정보(`reminder:fired` 이벤트 · 마지막 것은 `app_info` 로도).
#[derive(Debug, Clone, Serialize)]
pub struct Reminder {
    pub date: NaiveDate,
    /// notify | generate
    pub mode: &'static str,
    pub run_id: Option<i64>,
    /// 앱이 꺼져 있던 동안 놓친 발동을 뒤늦게 처리했는가.
    pub missed: bool,
    pub at: DateTime<Utc>,
}

pub struct AppState {
    cfg: RwLock<Config>,
    /// 설정 파일을 읽은 결과. `ReadFailed` 면 저장을 막는다(실제 파일을 덮어쓰지 않게).
    cfg_status: Mutex<LoadStatus>,
    store: Mutex<Option<Store>>,
    pub store_error: Option<String>,
    /// 엔진이 갱신하는 오늘 피드 스냅샷. 첫 수집 전에는 None.
    feed: RwLock<Option<Feed>>,
    worker: mpsc::Sender<WorkerMsg>,
    job: Mutex<Option<GenJob>>,
    /// 전체 수집이 돌고 있는가(화면 스피너용).
    pub refreshing: AtomicBool,
    /// 마지막 전역 단축키 등록 오류(설정 화면 안내용).
    shortcut_error: Mutex<Option<String>>,
    /// 마지막 정해진 시각 동작(창이 없을 때 발동한 것을 나중에 보여주기 위해).
    last_reminder: Mutex<Option<Reminder>>,
}

/// 뮤텍스 오염(다른 스레드 패닉)은 데이터 자체가 깨진 게 아니므로 그대로 쓴다.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

impl AppState {
    pub fn new(
        cfg: Config,
        cfg_status: LoadStatus,
        store: Result<Store, String>,
        worker: mpsc::Sender<WorkerMsg>,
    ) -> Self {
        let (store, store_error) = match store {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            cfg: RwLock::new(cfg),
            cfg_status: Mutex::new(cfg_status),
            store: Mutex::new(store),
            store_error,
            feed: RwLock::new(None),
            worker,
            job: Mutex::new(None),
            refreshing: AtomicBool::new(false),
            shortcut_error: Mutex::new(None),
            last_reminder: Mutex::new(None),
        }
    }

    // ---- 설정 ----------------------------------------------------------- //

    pub fn config(&self) -> Config {
        read(&self.cfg).clone()
    }

    pub fn set_config(&self, cfg: Config) {
        *write(&self.cfg) = cfg;
    }

    pub fn config_status(&self) -> LoadStatus {
        lock(&self.cfg_status).clone()
    }

    pub fn set_config_status(&self, st: LoadStatus) {
        *lock(&self.cfg_status) = st;
    }

    pub fn shortcut_error(&self) -> Option<String> {
        lock(&self.shortcut_error).clone()
    }

    pub fn set_shortcut_error(&self, e: Option<String>) {
        *lock(&self.shortcut_error) = e;
    }

    // ---- 저장소 --------------------------------------------------------- //

    /// 커맨드용 저장소 연결로 작업. 못 열었으면 이유를 돌려준다.
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> Result<T, String>) -> Result<T, String> {
        let guard = lock(&self.store);
        match guard.as_ref() {
            Some(s) => f(s),
            None => Err(format!(
                "메모 저장소를 열 수 없습니다: {}",
                self.store_error.as_deref().unwrap_or("알 수 없는 오류")
            )),
        }
    }

    // ---- 피드 ----------------------------------------------------------- //

    pub fn feed(&self) -> Option<Feed> {
        read(&self.feed).clone()
    }

    pub fn set_feed(&self, feed: Feed) {
        *write(&self.feed) = Some(feed);
    }

    // ---- 생성 작업 · 알림 -------------------------------------------------- //

    pub fn job(&self) -> MutexGuard<'_, Option<GenJob>> {
        lock(&self.job)
    }

    pub fn gen_status(&self) -> Option<GenStatus> {
        self.job().as_ref().map(|j| j.status.clone())
    }

    pub fn last_reminder(&self) -> Option<Reminder> {
        lock(&self.last_reminder).clone()
    }

    pub fn set_last_reminder(&self, r: Reminder) {
        *lock(&self.last_reminder) = Some(r);
    }

    // ---- 엔진 ----------------------------------------------------------- //

    /// 엔진 스레드에 요청. 스레드가 죽어 있으면 오류.
    pub fn send(&self, msg: WorkerMsg) -> Result<(), String> {
        self.worker.send(msg).map_err(|_| {
            "수집 엔진이 멈춰 있습니다. 앱을 다시 시작하세요.".to_string()
        })
    }
}
