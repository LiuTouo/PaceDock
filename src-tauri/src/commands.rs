//! IPC commands（PLAN §8）。錯誤回傳穩定代碼字串，前端查 i18n 顯示。

use std::sync::Arc;

use tauri::{AppHandle, Emitter, State};

use crate::model::Settings;
use crate::topology::Topology;
use crate::update::{self, UpdateState, UpdateStatus};
use crate::{config, AppState};

#[tauri::command]
pub fn get_topology(state: State<Arc<AppState>>) -> Topology {
    state.topology.clone()
}

#[tauri::command]
pub fn get_settings(state: State<Arc<AppState>>) -> Settings {
    state
        .config
        .read()
        .map(|c| c.settings.clone())
        .unwrap_or_default()
}

#[tauri::command]
pub fn save_settings(
    state: State<Arc<AppState>>,
    app: AppHandle,
    settings: Settings,
) -> Result<(), String> {
    let mut cfg = state.config.write().map_err(|e| e.to_string())?;
    let timer_changed = cfg.settings.high_precision_timer != settings.high_precision_timer;
    let enable_timer = settings.high_precision_timer;
    let lang_changed = cfg.settings.language != settings.language;
    let autostart_changed = cfg.settings.start_with_windows != settings.start_with_windows;
    let enable_autostart = settings.start_with_windows;
    let mut candidate = cfg.clone();
    candidate.settings = settings;
    config::save(&candidate)?;
    *cfg = candidate;
    // 套用失敗時回 Err：前端既有 rollback 會還原 checkbox；下次啟動會再嘗試
    if timer_changed {
        crate::timer::apply(enable_timer)?;
    }
    if autostart_changed {
        crate::autostart::set_autostart(enable_autostart)?;
    }
    if lang_changed {
        // 託管在 cfg lock 釋放後重建（此處 lock 仍在 scope，先 drop）
        drop(cfg);
        crate::tray::rebuild_menu(&app);
    }
    Ok(())
}

/// 高精度計時器狀態（開關 + 實際生效解析回讀，含支援區間 — 對應 Clockres 三值）
#[tauri::command]
pub fn get_timer_status() -> crate::model::TimerStatus {
    let intervals = crate::timer::intervals();
    crate::model::TimerStatus {
        enabled: crate::timer::enabled(),
        current_resolution_ms: intervals.map(|(_, _, cur)| cur as f64 / 10_000.0),
        // NtQueryTimerResolution 的 min = 最細值、max = 最粗值
        min_interval_ms: intervals.map(|(min, _, _)| min as f64 / 10_000.0),
        max_interval_ms: intervals.map(|(_, max, _)| max as f64 / 10_000.0),
    }
}

/// 全域請求政策登錄值狀態（None = 未設定 = 系統預設 per-process 語意）
#[tauri::command]
pub fn get_timer_global_enabled() -> Result<Option<bool>, String> {
    crate::timer::global_requests_enabled()
}

/// 一鍵寫入/刪除全域請求政策登錄值（寫入後需重開機生效）
#[tauri::command]
pub fn set_timer_global_enabled(enabled: bool) -> Result<(), String> {
    crate::timer::set_global_requests(enabled)
}

/// 對程式（exe 檔名為鍵）開啟/關閉持久化 timer 節流豁免：寫入 config，
/// 並立即對執行中同名行程施加/還原；遊戲重啟、FrameAnchor 重啟後自動重套。
#[tauri::command]
pub fn set_timer_exempt(
    state: State<Arc<AppState>>,
    exe_name: String,
    enabled: bool,
) -> Result<(), String> {
    // 先施加 runtime（黑名單擋在此失敗，不會寫入 config）
    crate::timer::set_exempt_program(&exe_name, enabled)?;
    let exe = exe_name.to_lowercase();
    let mut cfg = state.config.write().map_err(|e| e.to_string())?;
    let mut candidate = cfg.clone();
    if enabled {
        if !candidate.settings.timer_exempt_programs.contains(&exe) {
            candidate.settings.timer_exempt_programs.push(exe);
        }
    } else {
        candidate
            .settings
            .timer_exempt_programs
            .retain(|p| *p != exe);
    }
    config::save(&candidate)?;
    *cfg = candidate;
    Ok(())
}

/// 目前豁免清單（附存活狀態）
#[tauri::command]
pub fn list_timer_exempts() -> Vec<crate::timer::TimerExemptEntry> {
    crate::timer::list_exempts()
}

#[tauri::command]
pub fn open_data_folder() -> Result<(), String> {
    let dir = config::config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // explorer 即使成功開窗也常回非零結束碼，用 spawn 不等候；
    // 以 %SystemRoot% 絕對路徑啟動，避免依賴 PATH 搜尋
    std::process::Command::new(crate::syspath::explorer_exe()?)
        .arg(dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ── 更新相關 commands ──

/// 回傳目前版本、是否為可攜版
#[tauri::command]
pub fn get_update_info(app: AppHandle) -> serde_json::Value {
    let version = update::current_version(&app);
    let portable = update::is_portable();
    serde_json::json!({
        "version": version,
        "portable": portable,
    })
}

/// 可攜版：檢查 GitHub 有無新版，emit update-state 事件
#[tauri::command]
pub async fn check_portable_update(app: AppHandle) -> Result<(), String> {
    let version = update::current_version(&app);

    // 狀態：檢查中
    let _ = app.emit(
        "update-state",
        UpdateState {
            status: UpdateStatus::Checking,
            latest_version: None,
            current_version: version.clone(),
            progress: None,
            error: None,
        },
    );

    let version_for_check = version.clone();
    let version_for_up_to_date = version.clone();
    let version_for_available = version.clone();
    let app_for_error = app.clone();

    let result = tokio::task::spawn_blocking(move || {
        let release = update::fetch_portable_release()?;
        let latest_str = release.version.to_string();

        if !update::is_update_available(&version_for_check, &release.version) {
            let _ = app.emit(
                "update-state",
                UpdateState {
                    status: UpdateStatus::UpToDate,
                    latest_version: Some(latest_str),
                    current_version: version_for_up_to_date,
                    progress: None,
                    error: None,
                },
            );
            return Ok::<_, String>(());
        }

        // 有新版本
        let _ = app.emit(
            "update-state",
            UpdateState {
                status: UpdateStatus::Available,
                latest_version: Some(latest_str),
                current_version: version_for_available,
                progress: None,
                error: None,
            },
        );
        Ok::<_, String>(())
    })
    .await
    .map_err(|e| format!("檢查更新失敗: {e}"))?;

    // 若 result 為 Err，emit Error 狀態
    if let Err(ref err) = result {
        let _ = app_for_error.emit(
            "update-state",
            UpdateState {
                status: UpdateStatus::Error,
                latest_version: None,
                current_version: version,
                progress: None,
                error: Some(err.clone()),
            },
        );
    }

    result
}

/// 可攜版：下載並安裝更新
#[tauri::command]
pub async fn perform_portable_update(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    // 基準測試執行中：拒絕替換/退出，避免 runner 中斷未還原
    let _update_guard = state.benchmark.reserve_update()?;
    let version = update::current_version(&app);

    // 狀態：下載中
    let _ = app.emit(
        "update-state",
        UpdateState {
            status: UpdateStatus::Downloading,
            latest_version: None,
            current_version: version.clone(),
            progress: Some(0),
            error: None,
        },
    );

    let version_for_download = version.clone();

    // 階段 1：查詢 + 下載 + 校驗
    let (zip_data, latest_str) = tokio::task::spawn_blocking(move || {
        let release = update::fetch_portable_release()?;

        if !update::is_update_available(&version_for_download, &release.version) {
            return Err("已經是最新版本".to_string());
        }

        let latest_str = release.version.to_string();

        let zip_data = update::download_portable_zip(&release, |_pct| {
            // 下載完成時由 download_portable_zip 回報 100%
        })?;

        Ok::<_, String>((zip_data, latest_str))
    })
    .await
    .map_err(|e| format!("更新執行失敗: {e}"))??;

    // emit 下載完成進度
    let _ = app.emit(
        "update-state",
        UpdateState {
            status: UpdateStatus::Downloading,
            latest_version: Some(latest_str.clone()),
            current_version: version.clone(),
            progress: Some(100),
            error: None,
        },
    );

    // 階段 2：解壓縮（含基準測試資源）
    let (new_exe, marker_path, new_resources) =
        tokio::task::spawn_blocking(move || update::extract_portable_exe(&zip_data))
            .await
            .map_err(|e| format!("解壓縮失敗: {e}"))??;

    let old_exe = update::current_exe_path().ok_or("無法取得目前執行檔路徑".to_string())?;
    let pid = std::process::id();

    // 狀態：安裝中
    let _ = app.emit(
        "update-state",
        UpdateState {
            status: UpdateStatus::Installing,
            latest_version: None,
            current_version: version,
            progress: None,
            error: None,
        },
    );

    // 執行可攜版替換輔助腳本（同步更新基準測試資源）
    update::execute_portable_replacement(&old_exe, &new_exe, &marker_path, &new_resources, pid)?;

    // 設定 quitting flag，繞過 close-to-tray，真正結束程序
    app.exit(0);
    Ok(())
}

#[tauri::command]
pub fn begin_update(state: State<Arc<AppState>>) -> Result<(), String> {
    state.benchmark.begin_update()
}
#[tauri::command]
pub fn end_update(state: State<Arc<AppState>>) {
    state.benchmark.end_update();
}
