//! 系統匣常駐：右鍵選單（顯示/結束）、左鍵開主視窗、雙語選單重建。
//! 0.2.8 移除 rules 子系統後精簡為 timer 常駐所需的最小選單。

use std::sync::Arc;

use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::AppState;

const ID_SHOW: &str = "fa_show";
const ID_ABOUT: &str = "fa_about";
const ID_QUIT: &str = "fa_quit";
const TRAY_ID: &str = "main";

struct TrayStrings {
    show: &'static str,
    about: &'static str,
    quit: &'static str,
}

fn strings(lang: &str) -> TrayStrings {
    if lang.starts_with("en") {
        TrayStrings {
            show: "Show PaceDock",
            about: "About PaceDock",
            quit: "Quit PaceDock",
        }
    } else {
        TrayStrings {
            show: "顯示 PaceDock",
            about: "關於 PaceDock",
            quit: "結束 PaceDock",
        }
    }
}

pub fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let lang = current_lang(app);
    let menu = build_menu(app, &lang)?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .menu(&menu)
        .tooltip(format!("PaceDock v{}", app.package_info().version))
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| handle_menu_event(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn build_menu(app: &AppHandle, lang: &str) -> tauri::Result<Menu<Wry>> {
    let s = strings(lang);
    let show = MenuItemBuilder::with_id(ID_SHOW, s.show).build(app)?;
    let about = MenuItemBuilder::with_id(ID_ABOUT, s.about).build(app)?;
    let quit = MenuItemBuilder::with_id(ID_QUIT, s.quit).build(app)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    Menu::with_items(app, &[&show, &sep, &about, &sep2, &quit])
}

/// 語言切換時重建整個選單（save_settings 呼叫）
pub fn rebuild_menu(app: &AppHandle) {
    let lang = current_lang(app);
    if let Ok(menu) = build_menu(app, &lang) {
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn current_lang(app: &AppHandle) -> String {
    app.state::<Arc<AppState>>()
        .config
        .read()
        .map(|c| c.settings.language.clone())
        .unwrap_or_else(|_| "zh-TW".to_string())
}

fn handle_menu_event(app: &AppHandle, id: &str) {
    match id {
        ID_SHOW => crate::show_main_window(app),
        ID_ABOUT => {
            crate::show_main_window(app);
            // 前端 AboutDialog 監聽；WebView 在 close-to-tray 下仍存活
            let _ = app.emit("show-about", ());
        }
        ID_QUIT => {
            let state = app.state::<Arc<AppState>>();
            // 基準測試執行中：拒絕退出，讓 backend runner 完成或安全取消/還原
            if state.benchmark.refuse_exit_if_running().is_err() {
                log::warn!("基準測試執行中，拒絕結束 PaceDock");
                return;
            }
            app.exit(0);
        }
        _ => {}
    }
}
