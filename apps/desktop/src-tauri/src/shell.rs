//! 셸 잡동사니 — 창 보이기/숨기기 · 트레이 · Windows 알림 · 전역 단축키 · 자동 시작.

use std::{thread, time::Duration};

use tauri::{
    AppHandle, Emitter, Manager, WebviewWindow, WebviewWindowBuilder,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::GlobalShortcutExt as _;
use tauri_plugin_notification::NotificationExt as _;

use crate::state::AppState;

pub const MAIN: &str = "main";
pub const QUICK: &str = "quick";

/// 메인 창. 닫혀서(파괴돼서) 없으면 설정 파일의 정의로 다시 만든다 — 트레이 상주 중에는
/// WebView 를 내려 두는 게 원칙(계획 §4).
fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    if let Some(w) = app.get_webview_window(MAIN) {
        return Some(w);
    }
    let cfg = app.config().app.windows.iter().find(|w| w.label == MAIN)?.clone();
    match WebviewWindowBuilder::from_config(app, &cfg).and_then(|b| b.build()) {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::error!("메인 창 생성 실패: {e}");
            None
        }
    }
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = main_window(app) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 빠른 메모 창을 띄우고 입력칸에 포커스를 주라고 알린다.
pub fn show_quick(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(QUICK) {
        let _ = w.center();
        let _ = w.show();
        let _ = w.set_focus();
        let _ = app.emit_to(QUICK, "quick:show", ());
    }
}

pub fn hide_quick(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(QUICK) {
        let _ = w.hide();
    }
}

/// 포커스를 잃은 뒤 잠깐 기다렸다가 여전히 포커스가 없으면 숨긴다 — WebView2 는 창 안에서의
/// 일시적 포커스 이동에도 Focused(false) 를 올리므로 바로 숨기면 깜빡인다.
pub fn hide_quick_if_unfocused(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        if let Some(w) = app.get_webview_window(QUICK)
            && w.is_visible().unwrap_or(false)
            && !w.is_focused().unwrap_or(true)
        {
            let _ = w.hide();
        }
    });
}

/// 빠른 메모 창 토글(단축키).
pub fn toggle_quick(app: &AppHandle) {
    match app.get_webview_window(QUICK) {
        Some(w) if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) => {
            let _ = w.hide();
        }
        _ => show_quick(app),
    }
}

// ---- 트레이 --------------------------------------------------------------- //

pub fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "열기", true, None::<&str>)?;
    let quick = MenuItem::with_id(app, "quick", "빠른 메모", true, None::<&str>)?;
    let generate = MenuItem::with_id(app, "generate", "지금 일지 만들기", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quick, &generate, &sep, &quit])?;

    TrayIconBuilder::with_id(MAIN)
        .icon(app.default_window_icon().cloned().expect("기본 아이콘"))
        .tooltip("업무일지")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "quick" => show_quick(app),
            "generate" => {
                let cfg = app.state::<AppState>().config();
                let tz = worklog_core::time::get_tz(&cfg.timezone);
                let today = chrono::Utc::now().with_timezone(&tz).date_naive();
                match crate::generate::start(
                    app,
                    today,
                    worklog_core::store::run_kind::MANUAL,
                    false,
                ) {
                    Ok(_) => show_main(app),
                    Err(e) => {
                        // 편집된 일지가 있으면 창에서 확인 후 다시 만들게 한다.
                        show_main(app);
                        notify(app, "일지 생성", &e);
                    }
                }
            }
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
}

// ---- 알림 ----------------------------------------------------------------- //

/// Windows 알림. 설정에서 알림을 껐으면 조용히 넘어간다.
pub fn notify(app: &AppHandle, title: &str, body: &str) {
    let enabled = app.state::<AppState>().config().automation.notify;
    if !enabled {
        return;
    }
    if let Err(e) = app
        .notification()
        .builder()
        .title(format!("업무일지 · {title}"))
        .body(body)
        .show()
    {
        tracing::warn!("알림 표시 실패: {e}");
    }
}

// ---- 전역 단축키 ------------------------------------------------------------- //

/// 단축키를 `old` 에서 `new` 로 바꿔 등록한다. 결과는 [`AppState::set_shortcut_error`] 에도 남긴다.
///
/// `unregister_all` 은 플러그인 내부 뮤텍스를 쥔 채 메인 스레드를 기다려 핫키 이벤트 핸들러와
/// 잠금 순서가 엇갈릴 수 있으므로 쓰지 않는다. `unregister`/`register` 는 메인 스레드 호출을
/// 먼저 끝내고 잠근다.
pub fn apply_shortcut(app: &AppHandle, old: &str, new: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    let (old, new) = (old.trim(), new.trim());
    if !old.is_empty()
        && let Err(e) = gs.unregister(old)
    {
        tracing::debug!("이전 단축키 '{old}' 해제 실패(무시): {e}");
    }
    let result = if new.is_empty() {
        Ok(())
    } else {
        gs.register(new)
            .map_err(|e| format!("단축키 '{new}' 등록 실패: {e}"))
    };
    app.state::<AppState>()
        .set_shortcut_error(result.as_ref().err().cloned());
    result
}

// ---- 자동 시작 -------------------------------------------------------------- //

pub fn apply_autostart(app: &AppHandle, on: bool) -> Result<(), String> {
    let al = app.autolaunch();
    let cur = al.is_enabled().map_err(|e| e.to_string())?;
    if cur == on {
        return Ok(());
    }
    if on {
        al.enable().map_err(|e| e.to_string())
    } else {
        al.disable().map_err(|e| e.to_string())
    }
}

pub fn autostart_enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}
