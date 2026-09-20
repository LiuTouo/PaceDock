//! 系統匣常駐：右鍵選單（顯示/結束）、左鍵開主視窗、雙語選單重建。
//! 0.2.8 移除 rules 子系統後精簡為 timer 常駐所需的最小選單。

use std::sync::Arc;
mod native;

use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem};
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::AppState;

const ID_SHOW: &str = "fa_show";
const ID_ABOUT: &str = "fa_about";
const ID_QUIT: &str = "fa_quit";

/// 在原始圖示右下角繪製高對比紅底白色驚嘆號。
fn status_icon(warning: bool) -> tauri::Result<tauri::image::Image<'static>> {
    let base = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
    if !warning {
        return Ok(base);
    }
    let mut rgba = base.rgba().to_vec();
    let width = base.width();
    let height = base.height();
    for y in 16..height {
        for x in 16..width {
            let dx = x as f32 - 23.5;
            let dy = y as f32 - 23.5;
            let distance = dx * dx + dy * dy;
            if distance <= 64.0 {
                let mark =
                    (22..=25).contains(&x) && ((19..=25).contains(&y) || (27..=29).contains(&y));
                let color = if mark || distance > 49.0 {
                    [255, 255, 255, 255]
                } else {
                    [200, 30, 40, 255]
                };
                let offset = ((y * width + x) * 4) as usize;
                rgba[offset..offset + 4].copy_from_slice(&color);
            }
        }
    }
    Ok(tauri::image::Image::new_owned(rgba, width, height))
}

pub(crate) fn set_drift_warning(
    app: &AppHandle,
    warning: bool,
    tooltip: &str,
) -> tauri::Result<()> {
    native::update(app, status_icon(warning)?, tooltip.to_string())
}

pub(crate) use native::{notify, shutdown};

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
    app.on_menu_event(|app, event| handle_menu_event(app, event.id().as_ref()));
    native::build(app, status_icon(false)?)
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
pub fn rebuild_menu(_app: &AppHandle) {
    // 原生系統匣每次右鍵都依最新語言建立選單，不持有舊語言快取。
}

fn show_context_menu(app: &AppHandle) {
    if let (Some(window), Ok(menu)) = (
        app.get_webview_window("main"),
        build_menu(app, &current_lang(app)),
    ) {
        if let Err(e) = window.popup_menu(&menu) {
            log::warn!("顯示系統匣選單失敗: {e}");
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

#[cfg(test)]
mod drift_icon_tests {
    use super::*;

    #[test]
    fn warning_badge_only_changes_bottom_right_and_can_be_cleared() {
        let normal = status_icon(false).unwrap();
        let warning = status_icon(true).unwrap();
        assert_ne!(normal.rgba(), warning.rgba());
        for y in 0..32 {
            for x in 0..32 {
                if x < 16 || y < 16 {
                    let offset = (y * 32 + x) * 4;
                    assert_eq!(
                        &normal.rgba()[offset..offset + 4],
                        &warning.rgba()[offset..offset + 4]
                    );
                }
            }
        }
        assert_eq!(normal.rgba(), status_icon(false).unwrap().rgba());
    }
}
