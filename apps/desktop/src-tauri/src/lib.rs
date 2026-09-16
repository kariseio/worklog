//! 데스크톱 앱 셸(Tauri 2).
//!
//! - 창 두 개: `main`(오늘·일지·설정) · `quick`(빠른 메모 팝업, 전역 단축키)
//! - 트레이 상주: 메인 창 닫기 = 창 파괴(WebView 해제), 다시 열면 재생성. 종료는 트레이 메뉴.
//!   빠른 메모 창은 즉시 떠야 하므로 숨겨 둔 채 상주한다.
//! - 단일 인스턴스: 두 번째 실행(알림 클릭 포함) → 기존 창 앞으로
//! - 엔진 스레드([`worker`]): 파일 감시 · 주기 보정 · 정해진 시각 동작
//! - 생성 작업([`generate`]): 동시 1개, 진행 이벤트, 취소
//! - 플러그인: 알림 · 자동 시작(`--minimized`) · 전역 단축키 · 업데이터 · 대화상자 · 열기

mod commands;
mod generate;
mod shell;
mod state;
mod worker;

use std::{sync::mpsc, thread, time::Duration};

use tauri::{Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_global_shortcut::ShortcutState;
use worklog_core::{
    config::{Config, LoadStatus},
    paths,
    store::Store,
};

use state::AppState;

/// 시작 후 이만큼 지나 조용히 새 버전을 확인한다(있으면 `update:available`).
const UPDATE_CHECK_DELAY: Duration = Duration::from_secs(20);

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .without_time()
        .init();

    let minimized = std::env::args().any(|a| a == "--minimized");

    let app = tauri::Builder::default()
        // 반드시 첫 플러그인: 두 번째 인스턴스는 여기서 기존 창을 띄우고 바로 끝난다.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            shell::show_main(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args(["--minimized"])
                .build(),
        )
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        shell::toggle_quick(app);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::app_version,
            commands::app_quit,
            commands::feed_today,
            commands::refresh_now,
            commands::refresh_calendar,
            commands::rescan_repos,
            commands::note_add,
            commands::note_edit,
            commands::note_delete,
            commands::notes_for,
            commands::generate_start,
            commands::generate_cancel,
            commands::generate_status,
            commands::runs_recent,
            commands::document_get,
            commands::document_save,
            commands::document_dates,
            commands::document_calendar,
            commands::settings_get,
            commands::settings_set,
            commands::test_connection,
            commands::naverworks_calendars,
            commands::open_path,
            commands::open_url,
            commands::pick_path,
            commands::drives,
            commands::update_check,
            commands::update_install,
            commands::show_main,
            commands::quick_show,
            commands::quick_hide,
        ])
        .setup(move |app| {
            // 설정 · 저장소
            let loaded = Config::load_with_status();
            match &loaded.status {
                LoadStatus::Loaded => {}
                LoadStatus::Missing => tracing::info!(
                    "설정 파일 없음 — 기본값으로 시작: {}",
                    paths::settings_path().display()
                ),
                LoadStatus::CorruptedBackedUp(e) => {
                    tracing::warn!("설정 파일 손상(.json.bak 으로 보관) — 기본값으로 시작: {e}")
                }
                LoadStatus::ReadFailed(e) => {
                    tracing::error!("설정 파일 읽기 실패 — 저장을 막습니다: {e}")
                }
            }
            let mut cfg = loaded.config;
            cfg.normalize();
            let store = Store::open(&paths::db_path()).map_err(|e| e.to_string());
            match &store {
                Ok(s) => abandon_stale_runs(s),
                Err(e) => tracing::error!(
                    "메모 저장소 열기 실패({}): {e}",
                    paths::db_path().display()
                ),
            }

            let (tx, rx) = mpsc::channel();
            app.manage(AppState::new(cfg.clone(), loaded.status, store, tx.clone()));

            shell::build_tray(app)?;
            let handle = app.handle().clone();
            if let Err(e) = shell::apply_shortcut(&handle, "", &cfg.automation.global_shortcut) {
                tracing::warn!("{e}");
            }
            if let Err(e) = shell::apply_autostart(&handle, cfg.automation.autostart) {
                tracing::warn!("자동 시작 설정 동기화 실패: {e}");
            }
            worker::spawn(handle.clone(), tx, rx);

            if minimized {
                tracing::info!("--minimized: 트레이로 시작");
                // 설정 파일에서 만들어진 메인 창은 숨김 상태 — WebView 를 붙들지 않게 닫는다.
                if let Some(w) = app.get_webview_window(shell::MAIN) {
                    let _ = w.destroy();
                }
            } else {
                shell::show_main(&handle);
            }
            spawn_update_check(handle);
            Ok(())
        })
        .on_window_event(|window, event| match (window.label(), event) {
            // 빠른 메모 팝업: 닫기 = 숨김(상주), 포커스를 잃으면 잠시 뒤 숨김.
            (shell::QUICK, WindowEvent::CloseRequested { api, .. }) => {
                api.prevent_close();
                let _ = window.hide();
            }
            (shell::QUICK, WindowEvent::Focused(false)) => {
                shell::hide_quick_if_unfocused(window.app_handle());
            }
            // 메인 창: 닫기를 그대로 두어 창(WebView)을 파괴한다. 앱은 트레이에 남는다(아래 ExitRequested).
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("업무일지 앱 빌드 실패");

    app.run(|_app, event| {
        // 마지막 창이 닫혀도 프로세스는 남긴다. 트레이 '종료'(app.exit)는 code 가 Some 이라 통과.
        if let RunEvent::ExitRequested { code: None, api, .. } = event {
            api.prevent_exit();
        }
    });
}

/// 앱이 죽어 `running` 으로 남은 실행 기록을 정리한다.
fn abandon_stale_runs(store: &Store) {
    match store.runs_running() {
        Ok(runs) => {
            for r in runs {
                if let Err(e) = store.run_abandon(r.id, Some("앱 종료로 중단됨")) {
                    tracing::warn!("실행 기록 정리 실패(run {}): {e}", r.id);
                }
            }
        }
        Err(e) => tracing::warn!("실행 기록 조회 실패: {e}"),
    }
}

fn spawn_update_check(app: tauri::AppHandle) {
    let _ = thread::Builder::new()
        .name("worklog-update-check".into())
        .spawn(move || {
            thread::sleep(UPDATE_CHECK_DELAY);
            use tauri_plugin_updater::UpdaterExt as _;
            let Ok(updater) = app.updater() else {
                tracing::debug!("업데이트 endpoint 미설정 — 확인 생략");
                return;
            };
            match tauri::async_runtime::block_on(updater.check()) {
                Ok(Some(u)) => {
                    tracing::info!("새 버전 {} (현재 {})", u.version, u.current_version);
                    let _ = app.emit(
                        "update:available",
                        commands::UpdateInfo {
                            version: u.version.clone(),
                            current: u.current_version.clone(),
                            notes: u.body.clone(),
                            date: u.date.map(|d| d.to_string()),
                        },
                    );
                }
                Ok(None) => tracing::debug!("최신 버전입니다."),
                Err(e) => tracing::debug!("업데이트 확인 실패(무시): {e}"),
            }
        });
}
