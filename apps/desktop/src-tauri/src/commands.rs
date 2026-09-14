//! IPC 커맨드 — 화면(SolidJS)이 `invoke` 로 부르는 함수들.
//!
//! I/O 가 있는 커맨드는 `async` 로 두어 메인(UI) 스레드를 막지 않는다. 오래 걸리는 네트워크·
//! 대화상자는 `spawn_blocking` 으로 뺀다. 오류는 사용자에게 그대로 보여줄 한국어 문자열.

use chrono::NaiveDate;
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt as _;
use tauri_plugin_opener::OpenerExt as _;
use tauri_plugin_updater::UpdaterExt as _;
use worklog_core::{
    collect::naverworks::NaverWorksCollector,
    config::{CalendarInfo, Config, LoadStatus, SecretsPresence},
    drives::{DriveInfo, drives_info},
    feed::Feed,
    model::{DailyData, WorkLog},
    notes,
    output::{SinkResult, notion::NotionSink, obsidian},
    paths, service,
    store::{DocStatus, Document, Note, Run, run_kind},
    summarize::claude_exe,
    time::{get_tz, resolve_day},
};

use crate::{
    generate, shell,
    state::{AppState, GenStatus, Reminder, WorkerMsg},
};

type Res<T> = Result<T, String>;

fn parse_date(cfg: &Config, spec: Option<&str>) -> Res<NaiveDate> {
    resolve_day(spec, get_tz(&cfg.timezone))
        .map(|d| d.date)
        .map_err(|e| e.to_string())
}

/// 화면이 보낸 설정(비밀 칸이 비어 있을 수 있음)을 현재 설정과 합쳐 쓸 수 있는 형태로.
fn effective(state: &AppState, provided: Option<Config>) -> Config {
    match provided {
        Some(mut c) => {
            c.merge_blank_from(&state.config());
            c.normalize();
            c
        }
        None => state.config(),
    }
}

// ---- 앱 ------------------------------------------------------------------- //

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub version: String,
    pub settings_path: String,
    pub db_path: String,
    pub markdown_dir: String,
    pub store_ok: bool,
    pub store_error: Option<String>,
    pub refreshing: bool,
    pub generating: Option<GenStatus>,
    /// 마지막 정해진 시각 동작(창이 닫혀 있을 때 발동한 것을 새 창이 보여주기 위해).
    pub last_reminder: Option<Reminder>,
    pub shortcut_error: Option<String>,
}

#[tauri::command]
pub fn app_info(state: State<'_, AppState>) -> AppInfo {
    let cfg = state.config();
    AppInfo {
        version: worklog_core::version().to_string(),
        settings_path: paths::settings_path().display().to_string(),
        db_path: paths::db_path().display().to_string(),
        markdown_dir: cfg.outputs.markdown.resolved_dir().display().to_string(),
        store_ok: state.store_error.is_none(),
        store_error: state.store_error.clone(),
        refreshing: state.refreshing.load(std::sync::atomic::Ordering::Relaxed),
        generating: state.gen_status(),
        last_reminder: state.last_reminder(),
        shortcut_error: state.shortcut_error(),
    }
}

#[tauri::command]
pub fn app_version() -> &'static str {
    worklog_core::version()
}

// ---- 피드 ----------------------------------------------------------------- //

/// 오늘 피드 스냅샷. 첫 수집이 끝나기 전에는 None(곧 `feed:changed` 가 온다).
#[tauri::command]
pub fn feed_today(state: State<'_, AppState>) -> Option<Feed> {
    state.feed()
}

/// 모든 소스를 지금 다시 수집한다(비동기 — 결과는 `feed:changed`).
#[tauri::command]
pub fn refresh_now(state: State<'_, AppState>) -> Res<()> {
    state.send(WorkerMsg::FullRefresh)
}

/// 회의만 다시 조회한다(비동기 — 결과는 `feed:changed`).
#[tauri::command]
pub fn refresh_calendar(state: State<'_, AppState>) -> Res<()> {
    state.send(WorkerMsg::CalendarRefresh)
}

/// 디스크를 다시 탐색해 저장소 캐시를 새로 만든다(비동기 — 결과는 `feed:changed`).
#[tauri::command]
pub fn rescan_repos(state: State<'_, AppState>) -> Res<()> {
    state.send(WorkerMsg::RescanRepos)
}

// ---- 메모 ----------------------------------------------------------------- //

#[tauri::command]
pub async fn note_add(
    state: State<'_, AppState>,
    text: String,
    source: Option<String>,
) -> Res<Note> {
    let cfg = state.config();
    let tz = get_tz(&cfg.timezone);
    let source = source.unwrap_or_else(|| "app".into());
    let note = state
        .with_store(|s| notes::add_note(s, tz, &text, &source, None).map_err(|e| e.to_string()))?
        .ok_or_else(|| "빈 메모는 저장하지 않습니다.".to_string())?;
    state.send(WorkerMsg::NotesChanged)?;
    Ok(note)
}

#[tauri::command]
pub async fn note_edit(state: State<'_, AppState>, id: i64, text: String) -> Res<bool> {
    let ok = state.with_store(|s| notes::edit_note(s, id, &text).map_err(|e| e.to_string()))?;
    if ok {
        state.send(WorkerMsg::NotesChanged)?;
    }
    Ok(ok)
}

#[tauri::command]
pub async fn note_delete(state: State<'_, AppState>, id: i64) -> Res<bool> {
    let ok = state.with_store(|s| s.note_delete(id).map_err(|e| e.to_string()))?;
    if ok {
        state.send(WorkerMsg::NotesChanged)?;
    }
    Ok(ok)
}

#[tauri::command]
pub async fn notes_for(state: State<'_, AppState>, date: Option<String>) -> Res<Vec<Note>> {
    let date = parse_date(&state.config(), date.as_deref())?;
    state.with_store(|s| s.notes_for(date).map_err(|e| e.to_string()))
}

// ---- 생성 ----------------------------------------------------------------- //

/// 생성 시작. 편집된 일지가 있으면 `overwrite_edited` 없이는 거절한다(오류 문구가
/// [`generate::EDITED_PREFIX`] 로 시작하므로 화면은 확인 후 true 로 다시 부른다).
#[tauri::command]
pub async fn generate_start(
    app: AppHandle,
    state: State<'_, AppState>,
    date: Option<String>,
    overwrite_edited: Option<bool>,
) -> Res<i64> {
    let date = parse_date(&state.config(), date.as_deref())?;
    generate::start(
        &app,
        date,
        run_kind::MANUAL,
        overwrite_edited.unwrap_or(false),
    )
}

#[tauri::command]
pub fn generate_cancel(app: AppHandle) -> bool {
    generate::cancel(&app)
}

#[tauri::command]
pub fn generate_status(state: State<'_, AppState>) -> Option<GenStatus> {
    state.gen_status()
}

#[tauri::command]
pub async fn runs_recent(state: State<'_, AppState>, limit: Option<usize>) -> Res<Vec<Run>> {
    state.with_store(|s| s.runs_recent(limit.unwrap_or(20)).map_err(|e| e.to_string()))
}

// ---- 문서 ----------------------------------------------------------------- //

#[tauri::command]
pub async fn document_get(state: State<'_, AppState>, date: String) -> Res<Option<Document>> {
    let date = parse_date(&state.config(), Some(&date))?;
    state.with_store(|s| s.document_get(date).map_err(|e| e.to_string()))
}

/// 사용자가 편집한 본문 저장. `export` 면 markdown/obsidian 파일도 다시 쓴다(Notion 은 새 페이지가
/// 생기므로 제외).
#[tauri::command]
pub async fn document_save(
    state: State<'_, AppState>,
    date: String,
    full_md: String,
    export: Option<bool>,
) -> Res<Vec<SinkResult>> {
    let cfg = state.config();
    let date = parse_date(&cfg, Some(&date))?;
    let ok = state.with_store(|s| {
        s.document_mark_edited(date, &full_md, None)
            .map_err(|e| e.to_string())
    })?;
    if !ok {
        return Err(format!("{date} 문서가 없습니다. 먼저 생성하세요."));
    }
    if !export.unwrap_or(false) {
        return Ok(Vec::new());
    }
    let worklog = WorkLog {
        target_date: date,
        facts_markdown: String::new(),
        full_markdown: full_md,
        data: DailyData::new(date, cfg.timezone.clone()),
        summary_markdown: None,
    };
    let targets: Vec<String> = ["markdown", "obsidian"]
        .into_iter()
        .filter(|t| match *t {
            "markdown" => cfg.outputs.markdown.enabled,
            "obsidian" => cfg.outputs.obsidian.enabled,
            _ => false,
        })
        .map(String::from)
        .collect();
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    Ok(service::save(&cfg, &worklog, Some(&targets)))
}

#[tauri::command]
pub async fn document_dates(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> Res<Vec<NaiveDate>> {
    state.with_store(|s| {
        s.document_dates(limit.unwrap_or(400))
            .map_err(|e| e.to_string())
    })
}

#[tauri::command]
pub async fn document_calendar(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Res<Vec<DocStatus>> {
    let cfg = state.config();
    let from = parse_date(&cfg, Some(&from))?;
    let to = parse_date(&cfg, Some(&to))?;
    state.with_store(|s| s.document_statuses(from, to).map_err(|e| e.to_string()))
}

// ---- 설정 ----------------------------------------------------------------- //

#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    /// 비밀 값은 비운 사본. `secrets` 로 '설정됨' 여부만 알린다.
    pub config: Config,
    pub secrets: SecretsPresence,
    pub path: String,
    /// loaded | missing | corrupted | read_failed
    pub status: &'static str,
    pub status_detail: Option<String>,
    pub safe_to_save: bool,
    pub autostart_enabled: bool,
    pub shortcut_error: Option<String>,
    /// 찾은 `claude` CLI 경로(요약 provider auto/claude_cli 안내용).
    pub claude_cli: Option<String>,
}

fn settings_view(app: &AppHandle, state: &AppState) -> SettingsView {
    let cfg = state.config();
    let (status, detail) = match state.config_status() {
        LoadStatus::Loaded => ("loaded", None),
        LoadStatus::Missing => ("missing", None),
        LoadStatus::CorruptedBackedUp(e) => ("corrupted", Some(e)),
        LoadStatus::ReadFailed(e) => ("read_failed", Some(e)),
    };
    SettingsView {
        secrets: cfg.secrets_presence(),
        config: cfg.redacted(),
        path: paths::settings_path().display().to_string(),
        status,
        status_detail: detail,
        safe_to_save: status != "read_failed",
        autostart_enabled: shell::autostart_enabled(app),
        shortcut_error: state.shortcut_error(),
        claude_cli: claude_exe().map(|p| p.display().to_string()),
    }
}

#[tauri::command]
pub async fn settings_get(app: AppHandle, state: State<'_, AppState>) -> Res<SettingsView> {
    Ok(settings_view(&app, &state))
}

/// 설정 저장. 빈 비밀 칸은 기존 값 유지. 저장 후 단축키·자동 시작·엔진에 즉시 반영.
#[tauri::command]
pub async fn settings_set(
    app: AppHandle,
    state: State<'_, AppState>,
    config: Config,
) -> Res<SettingsView> {
    if let LoadStatus::ReadFailed(reason) = state.config_status() {
        // 시작 때 못 읽었던 파일을 한 번 더 읽어 본다. 화면이 들고 있는 값은 기본값에서 출발한
        // 것이라 그대로 저장하면 실제 설정을 기본값으로 덮어쓴다 — 다시 읽혔으면 화면을 새로
        // 고치게 하고, 여전히 못 읽으면 저장을 막는다.
        let again = Config::load_with_status();
        if again.safe_to_save() {
            let mut c = again.config;
            c.normalize();
            state.set_config(c);
            state.set_config_status(again.status);
            return Err(
                "설정 파일을 다시 읽었습니다. 화면을 새로 고친 뒤 다시 저장하세요.".into(),
            );
        }
        return Err(format!(
            "설정 파일을 읽지 못해 저장을 막았습니다(기존 설정을 지우지 않기 위해): {reason}"
        ));
    }
    let old = state.config();
    let mut new = config;
    new.merge_blank_from(&old);
    new.normalize();
    new.save().map_err(|e| format!("설정 저장 실패: {e}"))?;
    state.set_config(new.clone());
    state.set_config_status(LoadStatus::Loaded);

    // 단축키는 매번 다시 등록한다 — 시작 때 실패했던 것도 저장으로 다시 시도할 수 있게.
    if let Err(e) = shell::apply_shortcut(
        &app,
        &old.automation.global_shortcut,
        &new.automation.global_shortcut,
    ) {
        tracing::warn!("{e}");
    }
    if let Err(e) = shell::apply_autostart(&app, new.automation.autostart) {
        tracing::warn!("자동 시작 설정 실패: {e}");
    }
    state.send(WorkerMsg::ConfigChanged(Box::new(new)))?;
    Ok(settings_view(&app, &state))
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub ok: bool,
    pub message: String,
}

/// 연동 확인. `kind`: naverworks | notion | obsidian | markdown | claude_cli.
/// `config` 를 주면 저장 전 화면 값으로 확인한다(빈 비밀 칸은 저장된 값 사용).
#[tauri::command]
pub async fn test_connection(
    state: State<'_, AppState>,
    kind: String,
    config: Option<Config>,
) -> Res<Check> {
    let cfg = effective(&state, config);
    let (ok, message) = match kind.as_str() {
        "naverworks" => {
            let c = cfg.sources.naverworks.clone();
            tauri::async_runtime::spawn_blocking(move || {
                NaverWorksCollector::new(c).test_connection()
            })
            .await
            .map_err(|e| e.to_string())?
        }
        "notion" => {
            let c = cfg.outputs.notion.clone();
            tauri::async_runtime::spawn_blocking(move || NotionSink::new(c).test_connection())
                .await
                .map_err(|e| e.to_string())?
        }
        "obsidian" => obsidian::test_connection(&cfg.outputs.obsidian),
        "markdown" => {
            let dir = cfg.outputs.markdown.resolved_dir();
            let probe = dir.join(".worklog_write_test");
            match std::fs::create_dir_all(&dir)
                .and_then(|_| std::fs::write(&probe, "ok"))
                .and_then(|_| std::fs::remove_file(&probe))
            {
                Ok(()) => (true, format!("쓰기 가능 · {}", dir.display())),
                Err(e) => (false, format!("쓰기 실패({}): {e}", dir.display())),
            }
        }
        "claude_cli" => match claude_exe() {
            Some(p) => (true, format!("찾음 · {}", p.display())),
            None => (
                false,
                "PATH 에서 claude CLI 를 찾지 못했습니다. Claude Code 를 설치하거나 provider 를 바꾸세요."
                    .into(),
            ),
        },
        other => return Err(format!("알 수 없는 연동 종류: {other}")),
    };
    Ok(Check { ok, message })
}

/// NaverWorks 캘린더 목록(설정 화면 다중 선택용).
#[tauri::command]
pub async fn naverworks_calendars(
    state: State<'_, AppState>,
    config: Option<Config>,
) -> Res<Vec<CalendarInfo>> {
    let c = effective(&state, config).sources.naverworks;
    tauri::async_runtime::spawn_blocking(move || {
        NaverWorksCollector::new(c)
            .list_calendars()
            .map_err(|e| e.0)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- 파일·경로 --------------------------------------------------------------- //

/// 폴더·파일을 기본 앱(탐색기)으로 연다. 없는 폴더면 만들고 연다.
#[tauri::command]
pub async fn open_path(app: AppHandle, path: String) -> Res<()> {
    let p = paths::expand_user(&path);
    if !p.exists() {
        if p.extension().is_some() {
            return Err(format!("경로가 없습니다: {}", p.display()));
        }
        std::fs::create_dir_all(&p).map_err(|e| format!("폴더 생성 실패: {e}"))?;
    }
    app.opener()
        .open_path(p.display().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// 폴더/파일 선택 대화상자. `kind`: folder | file. 취소하면 None.
#[tauri::command]
pub async fn pick_path(app: AppHandle, kind: String, start: Option<String>) -> Res<Option<String>> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut b = app.dialog().file();
        if let Some(s) = start.filter(|s| !s.trim().is_empty()) {
            let p = paths::expand_user(&s);
            if p.is_dir() {
                b = b.set_directory(p);
            }
        }
        let picked = if kind == "file" {
            b.blocking_pick_file()
        } else {
            b.blocking_pick_folder()
        };
        picked
            .and_then(|f| f.into_path().ok())
            .map(|p| p.display().to_string())
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn drives() -> Res<Vec<DriveInfo>> {
    tauri::async_runtime::spawn_blocking(drives_info)
        .await
        .map_err(|e| e.to_string())
}

// ---- 업데이트 --------------------------------------------------------------- //

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
    pub date: Option<String>,
}

/// 새 버전 확인. 없으면 None. 업데이트 서버가 설정되지 않았거나 실패하면 Err.
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Res<Option<UpdateInfo>> {
    let updater = app
        .updater()
        .map_err(|e| format!("업데이트 확인을 사용할 수 없습니다: {e}"))?;
    match updater.check().await {
        Ok(Some(u)) => Ok(Some(UpdateInfo {
            version: u.version.clone(),
            current: u.current_version.clone(),
            notes: u.body.clone(),
            date: u.date.map(|d| d.to_string()),
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("업데이트 확인 실패: {e}")),
    }
}

/// 새 버전 내려받아 설치. Windows 에서는 설치기가 뜨면서 앱이 종료된다.
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Res<()> {
    let updater = app
        .updater()
        .map_err(|e| format!("업데이트를 사용할 수 없습니다: {e}"))?;
    let Some(u) = updater
        .check()
        .await
        .map_err(|e| format!("업데이트 확인 실패: {e}"))?
    else {
        return Err("새 버전이 없습니다.".into());
    };
    u.download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| format!("업데이트 설치 실패: {e}"))
}

// ---- 창 ------------------------------------------------------------------- //

#[tauri::command]
pub fn show_main(app: AppHandle) {
    shell::show_main(&app);
}

#[tauri::command]
pub fn quick_show(app: AppHandle) {
    shell::show_quick(&app);
}

#[tauri::command]
pub fn quick_hide(app: AppHandle) {
    shell::hide_quick(&app);
}

/// 앱 종료(트레이 '종료'와 동일).
#[tauri::command]
pub fn app_quit(app: AppHandle) {
    app.exit(0);
}

