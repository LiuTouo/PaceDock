//! 基準測試與 GPU 控制的 IPC commands。錯誤一律回傳穩定代碼字串（查 i18n）。

use std::sync::Arc;

use tauri::{AppHandle, State};

use crate::error::codes;
use crate::gpu::{AffinityPolicy, GpuDevice};
use crate::AppState;

use super::storage;
use super::{BenchmarkConfig, BenchmarkState, SessionDetail, SessionSummary, StorageInfo};

/// 列舉目前使用的顯示配接器
#[tauri::command]
pub fn enumerate_gpus(state: State<Arc<AppState>>) -> Result<Vec<GpuDevice>, String> {
    state
        .benchmark
        .backend
        .enumerate_present_adapters()
        .map_err(|e| e.code().to_string())
}

/// 目前基準測試狀態（含 recoveryRequired）
#[tauri::command]
pub fn get_benchmark_state(state: State<Arc<AppState>>) -> BenchmarkState {
    state.benchmark.state_snapshot()
}

/// 歷史 session 摘要列表
#[tauri::command]
pub fn list_benchmark_sessions() -> Result<Vec<SessionSummary>, String> {
    storage::list()
}

/// 讀單一 session 完整內容
#[tauri::command]
pub fn get_benchmark_session(id: String) -> Result<SessionDetail, String> {
    storage::get(&id)
}

/// 刪除單一 session（嚴謹 id 驗證；永不自動刪除）
#[tauri::command]
pub fn delete_benchmark_session(id: String) -> Result<(), String> {
    storage::delete(&id)
}

/// 儲存體總位元組數與 session 數
#[tauri::command]
pub fn get_benchmark_storage_info() -> Result<StorageInfo, String> {
    let total_bytes = storage::total_bytes();
    let session_count = storage::list()?.len();
    Ok(StorageInfo {
        total_bytes,
        session_count,
    })
}

/// 查詢目前 GPU 中斷親和性策略
#[tauri::command]
pub fn get_gpu_affinity_policy(
    state: State<Arc<AppState>>,
    instance_id: String,
) -> Result<AffinityPolicy, String> {
    state
        .benchmark
        .backend
        .read_affinity_policy(&instance_id)
        .map_err(|e| e.code().to_string())
}

#[tauri::command]
pub fn get_core_candidates(state: State<Arc<AppState>>) -> Vec<super::physical::CoreTarget> {
    super::physical::candidates(&state.topology)
}

#[tauri::command]
pub fn get_quick_schedule(
    state: State<Arc<AppState>>,
    mut config: BenchmarkConfig,
) -> Result<super::physical::QuickSchedule, String> {
    config.method_version = super::physical::METHOD_VERSION;
    super::runner::validate_config(&config, &state.topology)?;
    let targets = super::physical::select(&state.topology, &config.candidate_core_ids)?;
    Ok(super::physical::schedule(&config, targets.len()))
}

#[tauri::command]
pub async fn apply_gpu_core(
    state: State<'_, Arc<AppState>>,
    instance_id: String,
    core_id: u32,
    session_id: Option<String>,
) -> Result<(), String> {
    let manager = state.benchmark.clone();
    let guard = manager.reserve_mutation()?;
    tauri::async_runtime::spawn_blocking(move || {
        let topo = crate::topology::enumerate_topology().map_err(|e| e.to_string())?;
        manager.apply_core(guard, &topo, &instance_id, core_id, session_id.as_deref())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn restore_previous_gpu_affinity(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let manager = state.benchmark.clone();
    let guard = manager.reserve_mutation()?;
    tauri::async_runtime::spawn_blocking(move || manager.restore_reserved(guard))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn start_gpu_benchmark(
    app: AppHandle,
    state: State<Arc<AppState>>,
    mut config: BenchmarkConfig,
) -> Result<(), String> {
    if !config.candidate_lps.is_empty() {
        return Err(codes::BENCHMARK_INVALID_CONFIG.into());
    }
    config.method_version = super::physical::METHOD_VERSION;
    let topo = crate::topology::enumerate_topology().map_err(|e| e.to_string())?;
    state.benchmark.start(&app, &topo, config)
}

#[tauri::command]
pub fn cancel_benchmark(state: State<Arc<AppState>>) -> Result<(), String> {
    if !state.benchmark.is_running() {
        return Err(codes::BENCHMARK_NOT_ACTIVE.into());
    }
    state.benchmark.request_cancel();
    Ok(())
}

// ── 實際遊戲量測（capture.rs）──

/// 列舉目前可量測的遊戲視窗
#[tauri::command]
pub fn list_game_windows() -> Result<Vec<super::capture::GameWindow>, String> {
    super::capture::list_game_windows()
}

/// 對執行中遊戲做一次 N 秒 PresentMon 量測（duration clamp 5..=600）
#[tauri::command]
pub async fn start_game_capture(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    pid: u32,
    title: String,
    duration_secs: u32,
    gpu_instance_id: Option<String>,
) -> Result<super::capture::GameCaptureRecord, String> {
    let duration_secs = duration_secs.clamp(5, 600);
    let manager = state.benchmark.clone();
    // 共用 GPU 排他鎖：capture 期間 benchmark/apply/其他 capture 全被擋
    let guard = manager
        .reserve_capture()
        .map_err(|_| codes::CAPTURE_ALREADY_RUNNING.to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        super::capture::run_game_capture(
            &app,
            &manager,
            guard,
            pid,
            &title,
            duration_secs,
            gpu_instance_id,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 請求取消進行中的遊戲量測（冪等）
#[tauri::command]
pub fn cancel_game_capture() -> Result<(), String> {
    super::capture::request_cancel();
    Ok(())
}

/// 歷史遊戲量測列表（startedAt 降冪）
#[tauri::command]
pub fn list_game_captures() -> Result<Vec<super::capture::GameCaptureRecord>, String> {
    super::capture::list_captures()
}

/// 刪除單一遊戲量測（json + csv）
#[tauri::command]
pub fn delete_game_capture(id: String) -> Result<(), String> {
    super::capture::delete_capture(&id)
}
