//! PaceDock 主程式（composition root：拓撲/設定載入、計時器、恢復、Tauri 插件）。
//! 單一 exe、requireAdministrator；關閉視窗預設隱藏到系統匣（高精度計時器常駐），
//! GPU 測試／套用／還原期間阻止退出。
//! Release 用 GUI subsystem 避免 CMD 閃爍；debug 保留 console。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod benchmark;
mod commands;
mod config;
mod drift;
mod error;
mod gpu;
mod gpu_interrupts;
mod health;
mod model;
mod power;
mod process;
mod state_auth;
mod syspath;
mod timer;
mod topology;
mod tray;
mod update;

use std::sync::{Arc, RwLock};

use tauri::{Emitter, Manager};
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

use model::Config;
use topology::Topology;

/// 全域共享狀態（PLAN §4）
pub struct AppState {
    pub config: RwLock<Config>,
    pub topology: Topology,
    pub benchmark: Arc<benchmark::manager::BenchmarkManager>,
}

fn main() {
    // 強殺殘留的 WebView2 孤兒（鎖 user-data 目錄會導致白畫面）
    process::kill_orphan_webviews();
    // SeDebugPrivilege：對 ACL 保護的進程有幫助（無法繞過反作弊 kernel callback）

    // GUI subsystem 看不到 panic 輸出，寫到暫存檔方便診斷
    std::panic::set_hook(Box::new(|info| {
        let path = std::env::temp_dir().join("pacedock-panic.log");
        let _ = std::fs::write(&path, format!("{info}\n"));
    }));

    let topology = match topology::enumerate_topology() {
        Ok(t) => {
            if t.total_lp > 64 {
                log::warn!(
                    "偵測到 {} 個邏輯處理器：v1 只支援 group 0（前 64 個）",
                    t.total_lp
                );
            }
            log::info!(
                "拓撲：{} LP / {} 核心, SMT={}, Hybrid={}",
                t.total_lp,
                t.physical_cores.len(),
                t.has_smt,
                t.has_hybrid
            );
            t
        }
        Err(e) => {
            log::error!("拓撲列舉失敗: {e}");
            Topology::default()
        }
    };

    let cfg = match config::load() {
        Ok(cfg) => cfg,
        Err(error) => {
            show_startup_error(&error);
            std::process::exit(1);
        }
    };

    // 高精度計時器：依設定套用（失敗僅 log，不阻啟動）；並註冊喚醒重發
    if let Err(e) = timer::apply(cfg.settings.high_precision_timer) {
        log::warn!("高精度計時器啟動套用失敗: {e}");
    }
    timer::init_power_watch();
    // 遊戲節流豁免：灌入持久化名單並啟動輪詢（首輪立即套用執行中遊戲，
    // 之後每 3 秒對新啟動的遊戲自動重套）
    timer::init_programs(cfg.settings.timer_exempt_programs.clone());
    timer::init_exempt_watch();

    // 基準測試管理者：GPU 控制一律透過注入的 backend（啟動時嘗試 pending 還原）
    let backend: Arc<dyn gpu::GpuBackend> = Arc::new(gpu::RealGpuBackend::new());
    let benchmark = Arc::new(benchmark::manager::BenchmarkManager::new(backend));

    let state = Arc::new(AppState {
        config: RwLock::new(cfg),
        topology,
        benchmark,
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 第二個實例啟動 → 喚醒既有視窗後退出
            show_main_window(app);
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .manage(state.clone())
        .setup(move |app| {
            // 單一實例 plugin 已完成檢查後，才可讀取並還原共用 GPU 日誌。
            state.benchmark.attempt_startup_recovery();
            let handle = app.handle().clone();
            tray::build_tray(&handle)?;
            drift::start(handle.clone());

            // --minimized（或設定 start_minimized）→ 不開主視窗，常駐系統匣
            let minimized = std::env::args().any(|a| a == "--minimized");
            let start_min = state
                .config
                .read()
                .map(|c| c.settings.start_minimized)
                .unwrap_or(false);
            if !minimized && !start_min {
                show_main_window(&handle);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_topology,
            commands::begin_update,
            commands::end_update,
            commands::get_settings,
            commands::save_settings,
            commands::get_timer_status,
            commands::get_timer_global_enabled,
            commands::set_timer_global_enabled,
            commands::set_timer_exempt,
            commands::list_timer_exempts,
            commands::open_data_folder,
            commands::get_update_info,
            commands::check_portable_update,
            commands::perform_portable_update,
            benchmark::ipc::enumerate_gpus,
            benchmark::ipc::get_core_candidates,
            benchmark::ipc::get_quick_schedule,
            benchmark::ipc::apply_gpu_core,
            benchmark::ipc::get_benchmark_state,
            benchmark::ipc::list_benchmark_sessions,
            benchmark::ipc::get_benchmark_session,
            benchmark::ipc::delete_benchmark_session,
            benchmark::ipc::get_benchmark_storage_info,
            benchmark::ipc::get_gpu_affinity_policy,
            gpu_interrupts::sample_gpu_interrupts,
            gpu_interrupts::verify_interrupt_affinity,
            gpu_interrupts::scan_dpc_offenders,
            health::get_system_health,
            benchmark::ipc::get_msi_status,
            benchmark::ipc::apply_msi,
            benchmark::ipc::restore_msi,
            power::get_power_tweaks,
            power::apply_power_tweak,
            power::restore_power_tweak,
            benchmark::ipc::restore_previous_gpu_affinity,
            benchmark::ipc::start_gpu_benchmark,
            benchmark::ipc::cancel_benchmark,
            benchmark::ipc::list_game_windows,
            benchmark::ipc::start_game_capture,
            benchmark::ipc::cancel_game_capture,
            benchmark::ipc::list_game_captures,
            benchmark::ipc::delete_game_capture,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let state = app.state::<Arc<AppState>>();
                if state.benchmark.refuse_exit_if_running().is_err() {
                    api.prevent_close();
                    let _ = window.emit("gpu-exit-blocked", ());
                    return;
                }
                // close_to_tray：關閉 = 隱藏到系統匣，timer 常駐不中斷
                let close_to_tray = state
                    .config
                    .read()
                    .map(|c| c.settings.close_to_tray)
                    .unwrap_or(true);
                if close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building PaceDock")
        .run(|app, event| {
            match event {
                tauri::RunEvent::ExitRequested { api, .. } => {
                    if !app.state::<Arc<AppState>>().benchmark.can_exit() {
                        api.prevent_exit();
                        let _ = app.emit("gpu-exit-blocked", ());
                    }
                }
                // 禮貌性釋放：OS 在行程結束時會自動復原 timer resolution 請求；
                // 豁免無此保證（作用在別的行程上），正常退出時批次還原
                tauri::RunEvent::Exit => {
                    tray::shutdown(app);
                    timer::revert_all_exempts();
                    if timer::enabled() {
                        if let Err(e) = timer::release() {
                            log::warn!("退出釋放高精度計時器失敗: {e}");
                        }
                    }
                }
                _ => {}
            }
        });
}

fn show_startup_error(error: &str) {
    use std::os::windows::ffi::OsStrExt;

    let message = format!(
        "無法讀取 PaceDock 設定，程式將結束。\n請確認設定檔未被防毒軟體或同步程式鎖定。\n\nPaceDock cannot read its configuration and will exit.\nCheck whether antivirus or sync software has locked the file.\n\n{}\n\n{}",
        config::config_path().display(),
        error
    );
    let wide = |text: &str| {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let title = wide("PaceDock — CONFIG_FAILED");
    let message = wide(&message);
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
