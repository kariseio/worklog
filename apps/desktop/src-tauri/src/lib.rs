//! 데스크톱 앱 셸(Tauri 2).
//!
//! 0단계: 창 하나 + 트레이(열기/종료) + 닫기→트레이 + `app_version` 커맨드.
//! 5단계에서 단일 인스턴스·알림·자동 시작·전역 단축키·업데이터와 IPC 커맨드를 채운다.

use tauri::{
    Manager, WindowEvent,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

/// 코어 라이브러리 버전 (UI 가 IPC 연결 확인용으로 호출).
#[tauri::command]
fn app_version() -> &'static str {
    worklog_core::version()
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .without_time()
        .init();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_version])
        .setup(|app| {
            let open = MenuItem::with_id(app, "open", "열기", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;

            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().expect("기본 아이콘"))
                .tooltip("업무일지")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        // 창 닫기 = 트레이로 숨김. 종료는 트레이 메뉴에서만.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("업무일지 앱 실행 실패");
}
