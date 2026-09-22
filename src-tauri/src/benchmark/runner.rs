//! GPU 實體核心快速測試：共用策略交易、PresentMon 擷取與回復保護。
//! 舊 LP 排程僅供既有回歸測試使用。
//!
//! 所有會動真實系統的依賴（backend / sleeper / process / cancel）都是注入的
//! trait，測試用 fake 跑完整流程，不碰真實 GPU 驅動重啟或真實子程序。

#[path = "quick_runner.rs"]
pub mod quick;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::codes;
use crate::gpu::{
    restore_snapshot, single_lp_mask_bytes, AffinityPolicy, GpuBackend, RegistryValueSnapshot,
    Sleep, DEVICE_POLICY_SINGLE_PROCESSOR,
};

use super::assets::{self, BenchmarkAssets};
use super::env::{self, EnvironmentProbe};
use super::metrics::{
    attach_display_metrics, compute_lp_result, merge_display_rounds, merge_rounds,
    parse_presentmon_csv, parse_presentmon_csv_full, parse_presentmon_series, DisplaySeries,
    FullSeries,
};
use super::recovery::{self, RecoveryStage};
use super::storage;
use super::window_layout::{
    logical_to_physical, prepare_window_layout, verify_workload_fits, LayoutPlan,
    MainWindowController,
};
use super::window_win::{integrity_ok, Rect, WindowIntegritySnapshot, WorkloadWindow};
use super::{
    cpu_fingerprint_with, BenchmarkConfig, BenchmarkPhase, BenchmarkProgress, CaptureQuality,
    CpuIdentity, FpsCapPolicy, LpResult, ReliabilitySummary, SessionDetail, SessionStatus,
    SessionSummary, WindowIntegrity, WorkloadKind,
};
#[cfg(test)]
use super::{
    metrics::{
        competitive_score, confirmation_effect, is_competitive_eligible, median, robust_candidates,
        round_medians, severe_lps,
    },
    EnvironmentStability, EquivalentSafetyStatus, ReliabilityStatus,
};

/// restart 後額外穩定時間（毫秒）：驅動中斷重新分配後等 GPU 安定
pub const RESTART_STABILIZE_MS: u64 = 5000;
/// workload 啟動後、開始收集前的固定等待（毫秒）
pub const WORKLOAD_STARTUP_MS: u64 = 5000;
/// PresentMon 等待其退出（`-timed` 自停）時的 margin（秒）：
/// 超過 `sample_secs + CAPTURE_WAIT_MARGIN_S` 仍未退出 → 視為卡住、穩定代碼失敗
pub const CAPTURE_WAIT_MARGIN_S: u64 = 15;
/// Vulkan/PresentMon 偶發會在 workload 仍正常運作時漏掉整段 Present 事件。
/// 每次都建立新的 workload 與 display-device generation，最多嘗試三次。
pub const MAX_CAPTURE_ATTEMPTS: u32 = 3;
/// 無 FPS 上限 workload 可能產生每秒上萬個 Present；v2.5.1 預設 2048，
/// 提升到 8192 降低 consumer 短暫落後時遺失事件的機率。Adaptive 校準會依
/// 選定 cap 重新計算更大的 buffer；此值為 legacy/Fixed 模式的預設。
pub const PRESENTMON_CIRCULAR_BUFFER_SIZE: u32 = 8192;
/// 長 sleep / capture wait 的取消輪詢間隔（毫秒）。runner 以這個粒度檢查
/// cancel，避免被 5s 穩定、warmup 或 PresentMon 等待長時間阻塞。
pub const CANCEL_POLL_MS: u64 = 100;
/// workload spawn 後，等待其 top-level window 出現的上限（毫秒）。
/// 期間以 [`CANCEL_POLL_MS`] 輪詢，可被取消中斷。
pub const WORKLOAD_WINDOW_WAIT_MS: u64 = 3000;
/// 快速篩選 round 數：所有選定 LP 只跑一輪短 capture，先淘汰明顯落後者。
/// 篩選證據獨立保存，絕不混入確認推論證據。
pub const SCREENING_ROUNDS: u32 = 1;
/// refinement round 數：第一輪測 Top 5（中等 capture），第二輪只測 Top 3（正式
/// capture），再由三輪 screening/refinement 證據選出最終 Top 2。
pub const REFINEMENT_ROUNDS: u32 = 2;
/// racing refinement 第一輪最多保留的候選數（Top 5）。
#[cfg(test)]
pub const MAX_SELECTION_CANDIDATES: usize = 5;
/// 正式 refinement 最多保留的候選數（Top 3）。
#[cfg(test)]
pub const MAX_REFINEMENT_CANDIDATES: usize = 3;
/// 快速篩選 capture 秒數上限；使用者設定更短時尊重原設定。
#[cfg(test)]
pub const QUICK_SCREEN_SAMPLE_SECS: u32 = 10;
/// 快速篩選 warm-up 秒數上限。
#[cfg(test)]
pub const QUICK_SCREEN_WARMUP_SECS: u32 = 3;
/// racing refinement capture 秒數上限。
#[cfg(test)]
pub const RACING_SAMPLE_SECS: u32 = 20;
/// 確認（confirmation）最少 round 數：前 2 名 finalists 先各測 3 個配對 round。
#[cfg(test)]
pub const CONFIRMATION_MIN_ROUNDS: u32 = 3;
/// 確認（confirmation）最多 round 數：證據不足時最多擴充到 7 個配對 round。
#[cfg(test)]
pub const CONFIRMATION_MAX_ROUNDS: u32 = 7;
/// 前向 + 反向確認的總配對 round 預算（上限 10）。
#[cfg(test)]
pub const TOTAL_PAIR_BUDGET: u32 = 10;
/// 反向驗證最多 round 數；實際上限再受 [`TOTAL_PAIR_BUDGET`] 扣除前向 round 限制。
#[cfg(test)]
pub const REVERSE_MAX_ROUNDS: u32 = 5;
/// 等效判定最低 round 數（至少 5 輪才判定 Equivalent）。
#[cfg(test)]
pub const EQUIVALENT_MIN_ROUNDS: u32 = 5;
/// 前向確認 round 的起始編號（與篩選/refinement 的 round 編號隔離，防止證據重用）。
pub const CONFIRMATION_ROUND_BASE: u32 = 100;
/// 反向驗證 round 的起始編號（獨立 namespace，避免與前向確認共用檔案/round）。
pub const REVERSE_ROUND_BASE: u32 = 200;
/// 等效安全驗證的 AB/BA 配對 round 數（3 組 = 6 captures）。
#[cfg(test)]
pub const EQUIVALENT_VALIDATION_ROUNDS: u32 = 3;
/// 等效安全驗證 round 的起始編號（獨立 namespace，不混入原確認資料）。
pub const EQUIVALENT_VALIDATION_ROUND_BASE: u32 = 400;
/// 最多 finalists 數（使用者決策：Top 2 only）。
#[cfg(test)]
pub const MAX_FINALISTS: usize = 2;
/// bootstrap 穩定性區間的下百分位（第 5 百分位）；小型樣本決策啟發式，非信賴區間。
#[cfg(test)]
pub const INTERVAL_LOW_PERCENTILE: f64 = 0.05;
/// bootstrap 穩定性區間的上百分位（第 95 百分位）；小型樣本決策啟發式，非信賴區間。
#[cfg(test)]
pub const INTERVAL_HIGH_PERCENTILE: f64 = 0.95;
/// capture 完整性：觀測時長（TimeInSeconds 跨度）須 ≥ sample_secs 的此比例。
pub const CAPTURE_DURATION_MIN_RATIO: f64 = 0.95;
/// capture overflow 重試時 circular buffer 的加倍倍數。
pub const CAPTURE_OVERFLOW_BUFFER_MULT: u32 = 2;
/// 校準 tier 序列（最高安全遞增 cap）。
pub const CALIBRATION_TIERS: [u32; 5] = [240, 500, 1000, 2000, 4000];
/// 校準每個 tier 的 warmup 秒數。
pub const CALIBRATION_WARMUP_SECS: u32 = 5;
/// 校準每個 tier 的 capture 秒數。
pub const CALIBRATION_SAMPLE_SECS: u32 = 10;
/// 校準：下一 tier 相對目前 clean tier 的 FPS 增益低於此值即停（選目前 clean tier）。
pub const CALIBRATION_GAIN_MIN_PCT: f64 = 10.0;

/// 子程序控制邊界。`spawn` 回傳 pid（owned handle），終結時由 runner 統一 `kill`。
pub trait ProcessRunner: Send + Sync {
    fn spawn(&self, exe: &Path, args: &[String]) -> Result<u32, String>;
    fn is_alive(&self, pid: u32) -> bool;
    fn kill(&self, pid: u32) -> Result<(), String>;
    /// 等 pid 退出，最多 timeout_ms；true=已退出，false=逾時
    fn wait_exit(&self, pid: u32, timeout_ms: u64) -> Result<bool, String>;
    /// 已退出程序的 exit code（None = 仍在執行 / 未知）。診斷用，無副作用。
    fn exit_code(&self, _pid: u32) -> Option<i32> {
        None
    }
    /// 該 pid 的 stdout/stderr bounded tail（None = 未擷取）。診斷用。
    fn output_tail(&self, _pid: u32, _max_chars: usize) -> Option<ProcessOutput> {
        None
    }
}

/// 子程序的 bounded stdout/stderr tail（診斷用）。內容有長度上限，不洩漏環境資料。
#[derive(Clone, Debug, Default)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
}

/// 單一 capture 的診斷資料，寫入 `session_dir/diag/capture-round-<r>-lp-<lp>.json`。
/// 即使 session 失敗也保留，供 restart 後診斷「第二次 capture 無 CSV」。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct CaptureDiagnostics {
    pub round: u32,
    pub lp: u32,
    /// capture attempt（1=首次，2=retry）。用於區分診斷檔名。
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    pub workload_pid: u32,
    /// PresentMon 啟動前 workload 是否還活著
    pub workload_alive_before_pm: bool,
    /// capture 結束後（PresentMon 退出後）workload 是否還活著
    pub workload_alive_after_capture: bool,
    /// workload 提早退出時的 exit code
    #[serde(default)]
    pub workload_exit_code: Option<i32>,
    #[serde(default)]
    pub workload_stdout: String,
    #[serde(default)]
    pub workload_stderr: String,
    pub presentmon_pid: u32,
    /// PresentMon 正常退出時的 exit code
    #[serde(default)]
    pub presentmon_exit_code: Option<i32>,
    /// wait_exit 是否回傳 Ok（沒 Error；timeout 也屬 Ok）
    pub wait_completed: bool,
    /// wait_exit 回傳 Ok(false)：PresentMon 逾時未退出
    pub wait_timed_out: bool,
    #[serde(default)]
    pub wait_error: Option<String>,
    #[serde(default)]
    pub presentmon_stdout: String,
    #[serde(default)]
    pub presentmon_stderr: String,
    pub csv_path: String,
    pub csv_exists: bool,
    pub csv_size_bytes: u64,
    /// PresentMon 的處理序篩選方式（固定 "process_id"）
    #[serde(default)]
    pub capture_filter_kind: String,
    /// `-process_id` 傳入的精確十進位 PID（runner 持有的 spawned workload PID）
    #[serde(default)]
    pub capture_filter_value: String,
    /// 每次 capture 的獨立 ETW session，避免與其他 PresentMon 工具互相終止。
    #[serde(default)]
    pub presentmon_session_name: String,
    /// PresentMon stderr 是否回報 ETW events lost（擷取負載過高訊號）
    #[serde(default)]
    pub etw_events_lost: bool,
    /// overflowed present events 數量（0 = 無溢位）。
    #[serde(default)]
    pub overflowed_present_events: u64,
    /// ETW events/buffers lost 數量（0 = 無遺失）。
    #[serde(default)]
    pub etw_events_lost_count: u64,
    /// 觀測到的 capture 時長（秒，來自 CSV `TimeInSeconds` 跨度；無該欄位 = None）。
    #[serde(default)]
    pub capture_duration_secs: Option<f64>,
    /// 本次 capture 是否通過完整性閘（CSV 有效且無 overflow/ETW lost）。
    #[serde(default)]
    pub valid: bool,
    /// 拒絕原因（overflow/etw_lost/duration/monotonic/empty/missing；成功 None）。
    #[serde(default)]
    pub rejection_reason: Option<String>,
    /// 本次 capture 採用的有效 FPS cap（校準鎖定值）。
    #[serde(default)]
    pub effective_fps_cap: u32,
    /// 本次 capture 採用的 circular buffer size（含 overflow 重試的加倍值）。
    #[serde(default)]
    pub circular_buffer_size: u32,
    /// 本次 capture 的穩定錯誤代碼（成功為 None）
    #[serde(default)]
    pub error: Option<String>,
}

/// 每次 capture 的診斷輸出 tail 上限（bytes/stream）；避免診斷檔無限成長
pub const DIAG_OUTPUT_TAIL_CAP: usize = 4096;

fn default_attempt() -> u32 {
    1
}

/// 取消訊號：runner 只在安全階段邊界檢查
pub trait CancelSignal: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

/// 一次執行所需的全部注入依賴與狀態
pub struct RunContext {
    pub backend: Arc<dyn GpuBackend>,
    pub sleeper: Arc<dyn Sleep>,
    pub processes: Arc<dyn ProcessRunner>,
    pub cancel: Arc<dyn CancelSignal>,
    /// 環境穩定度探針（AC/電池/CPU）；生產用 Real，測試注入 fake。
    pub env: Arc<dyn EnvironmentProbe>,
    pub topo: crate::topology::Topology,
    pub cpu_identity: CpuIdentity,
    pub assets: BenchmarkAssets,
    pub storage_root: PathBuf,
    pub journal_path: PathBuf,
    pub session_id: String,
    pub config: BenchmarkConfig,
    /// 每個 progress 事件都會呼叫（manager 拿來更新 state + emit 事件）
    pub on_progress: Box<dyn FnMut(&BenchmarkProgress) + Send>,
    /// pre-flight 取得的原始策略快照（終結時還原）
    pub baseline: Option<AffinityPolicy>,
    /// 本 session 擁有的子程序 pid（終結時全部終止）
    pub owned_processes: Vec<u32>,
    /// workload 視窗調整（僅 Vulkan windowed 使用；測試注入 fake）
    pub window: Arc<dyn WorkloadWindow>,
    /// capture 完整性累計器（run_capture 每次 attempt 更新；終結時寫入 summary）。
    pub capture_quality: CaptureQuality,
    /// 主視窗 compact/還原（production 用 Tauri+Win32；測試注入 fake）。
    pub window_control: Arc<dyn MainWindowController>,
    /// 本次佈局規劃（prepare_window_layout 成功後填入；workload 定位/空間複檢用）。
    pub layout: Option<LayoutPlan>,
    /// 視窗完整性快照回報（manager 拿來更新 state.window_integrity）。
    pub on_integrity: Box<dyn FnMut(&WindowIntegrity) + Send>,
    /// 累計視窗完整性重跑次數（回報用）。
    pub window_retries: u32,
    /// 上次回報的完整性（變更偵測，避免每 100ms 重複 emit）。
    pub last_integrity: Option<WindowIntegrity>,
}

/// runner 的最終結果
pub struct RunResult {
    pub status: SessionStatus,
    pub detail: SessionDetail,
    /// 錯誤代碼（Failed 時）
    pub error: Option<String>,
    pub best_lp: Option<u32>,
    pub severe_lps: Vec<u32>,
    pub recommended_cores: Vec<u32>,
    /// 終結還原失敗 → 需封鎖新的 test/apply（啟動時會重試還原）
    pub recovery_required: bool,
}

enum TerminalReason {
    Cancelled,
    Error(String),
}

/// capture wait 的結果
enum CaptureWaitOutcome {
    /// PresentMon 自行退出
    Exited,
    /// 逾時未退出
    TimedOut,
    /// 等待期間收到取消
    Cancelled,
    /// wait 本身回報錯誤
    Failed(String),
    /// workload 視窗完整性破壞（前景/位置/遮擋等）
    IntegrityBroken,
}

/// 可中斷 sleep：以 [`CANCEL_POLL_MS`] 分段睡，每段檢查 cancel。
/// 回傳 true = 已取消（sleep 被提前中斷）。
fn sleep_interruptible(ctx: &RunContext, ms: u64) -> bool {
    let mut remaining = ms;
    while remaining > 0 {
        if ctx.cancel.is_cancelled() {
            return true;
        }
        let step = remaining.min(CANCEL_POLL_MS);
        ctx.sleeper.sleep(step);
        remaining -= step;
    }
    ctx.cancel.is_cancelled()
}

/// 可中斷地等待 PresentMon 退出。以單一等待預算 `timeout_ms` 計算：每輪先
/// 非阻塞輪詢 `wait_exit(pid, 0)`（已退出即回 Exited），再以 [`CANCEL_POLL_MS`]
/// 分段睡兼作取消輪詢節奏。預算完全由累計 sleep 時間扣減，production 的
/// `wait_exit(0)` 立即回、sleeper 真的睡，故實際逾時 == `timeout_ms`（不再因
/// `wait_exit` 阻塞 + sleep 而 double）；fake sleeper（不真的睡）在有限輪數內
/// 收斂，測試不空轉。取消輪詢粒度維持 ≤ [`CANCEL_POLL_MS`]。
fn wait_capture(
    ctx: &RunContext,
    pid: u32,
    timeout_ms: u64,
    integrity: &dyn Fn() -> bool,
) -> CaptureWaitOutcome {
    let mut remaining = timeout_ms;
    loop {
        if ctx.cancel.is_cancelled() {
            return CaptureWaitOutcome::Cancelled;
        }
        // cancel 優先於完整性：點擊取消不誤報成視窗完整性失敗。
        if !integrity() {
            return CaptureWaitOutcome::IntegrityBroken;
        }
        // timeout 0 = 非阻塞輪詢：只檢查一次是否已退出，不做任何等待。
        match ctx.processes.wait_exit(pid, 0) {
            Ok(true) => return CaptureWaitOutcome::Exited,
            Ok(false) => {}
            Err(e) => return CaptureWaitOutcome::Failed(e),
        }
        if remaining == 0 {
            return CaptureWaitOutcome::TimedOut;
        }
        let step = remaining.min(CANCEL_POLL_MS);
        ctx.sleeper.sleep(step);
        remaining -= step;
    }
}

/// 是否對 Vulkan workload 安裝關閉防護：僅內建 lava-triangle
/// （Vulkan workload 一律為內建資源）。D3D9 自行編譯,不保護。
fn should_guard_close(config: &BenchmarkConfig) -> bool {
    config.workload == WorkloadKind::Vulkan
}

/// 以 [`CANCEL_POLL_MS`] 輪詢 `op` 直到它回 `Ok(true)`；`Ok(false)` 表示視窗尚未
/// 建立，在 [`WORKLOAD_WINDOW_WAIT_MS`] 預算內重試；`Err` 或預算用盡只 log warn，
/// 不影響 benchmark（保留預設狀態）。等待可被 cancel 中斷。
fn poll_window_ready(
    ctx: &RunContext,
    wl_pid: u32,
    what: &str,
    mut op: impl FnMut() -> Result<bool, String>,
) {
    let mut remaining = WORKLOAD_WINDOW_WAIT_MS;
    loop {
        match op() {
            Ok(true) => return,
            Ok(false) => {
                if remaining == 0 || ctx.cancel.is_cancelled() {
                    log::warn!("{what}：找不到 workload pid {wl_pid} 的 top-level visible window");
                    return;
                }
                let step = remaining.min(CANCEL_POLL_MS);
                ctx.sleeper.sleep(step);
                remaining -= step;
            }
            Err(e) => {
                log::warn!("{what} 失敗（pid={wl_pid}）: {e}");
                return;
            }
        }
    }
}

/// 同 [`poll_window_ready`] 但強制成功：找不到視窗（預算用盡）或操作失敗回 Err；
/// cancel 中斷回 `Err("cancelled")`。定位/resize/空間複檢等「必須成功」的步驟使用。
fn require_window_ready(
    ctx: &RunContext,
    wl_pid: u32,
    what: &str,
    mut op: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    let mut remaining = WORKLOAD_WINDOW_WAIT_MS;
    loop {
        if ctx.cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        match op() {
            Ok(true) => return Ok(()),
            Ok(false) => {
                if remaining == 0 {
                    return Err(codes::BENCHMARK_WORKLOAD_FAILED.to_string());
                }
                let step = remaining.min(CANCEL_POLL_MS);
                ctx.sleeper.sleep(step);
                remaining -= step;
            }
            Err(e) => {
                log::warn!("{what} 失敗（pid={wl_pid}）: {e}");
                return Err(e);
            }
        }
    }
}

/// 停用 workload 視窗的關閉能力（SC_CLOSE），防使用者誤關（best-effort，沿用舊語意）。
fn guard_workload_window(ctx: &RunContext, wl_pid: u32) {
    poll_window_ready(ctx, wl_pid, "停用 workload 關閉鈕", || {
        ctx.window.guard_close(wl_pid)
    });
}

/// 定位 workload：`ShowWindow(SW_RESTORE)` + `HWND_TOPMOST` 至 rcWork 左上 +
/// `SetForegroundWindow`。適用所有 spawned workload（Vulkan/D3D9/自訂）。
fn position_workload_window(
    ctx: &RunContext,
    wl_pid: u32,
    plan: &LayoutPlan,
) -> Result<(), String> {
    require_window_ready(ctx, wl_pid, "定位 workload 視窗", || {
        ctx.window
            .position_topmost(wl_pid, plan.workload_rect.left, plan.workload_rect.top)
    })
}

/// Vulkan：把 client area 精確調成實體像素目標（config.width/height 換算）。
fn resize_workload_window_exact(
    ctx: &RunContext,
    wl_pid: u32,
    client_w: u32,
    client_h: u32,
) -> Result<(), String> {
    require_window_ready(ctx, wl_pid, "調整 workload 視窗", || {
        ctx.window.find_and_resize(wl_pid, client_w, client_h)
    })
}

/// 讀 workload 實際外框矩形，並以 rcWork + compact 複檢空間（不重疊/在界內）。
fn verify_workload_actual_rect(
    ctx: &RunContext,
    wl_pid: u32,
    plan: &LayoutPlan,
) -> Result<Rect, String> {
    let mut actual: Option<Rect> = None;
    require_window_ready(ctx, wl_pid, "複檢 workload 外框", || {
        match ctx.window.outer_rect(wl_pid) {
            Ok(Some(r)) => {
                actual = Some(r);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(e),
        }
    })?;
    let rect = actual.ok_or_else(|| codes::BENCHMARK_WORKLOAD_FAILED.to_string())?;
    verify_workload_fits(rect, plan.compact_rect, plan.rc_work)?;
    Ok(rect)
}

/// 準備 workload 視窗（強制視窗模式）：
/// 1. 內建 Vulkan 安裝關閉防護（best-effort）。
/// 2. 定位到 rcWork 左上（SW_RESTORE + topmost + foreground）。
/// 3. Vulkan 精確 client resize；D3D9/自訂不改尺寸。
/// 4. 以實際 outer rect 複檢空間（與 compact 不重疊）。
///
/// 回傳實際外框矩形（供完整性比對）。
fn prepare_workload_window(ctx: &RunContext, wl_pid: u32) -> Result<Rect, String> {
    let plan = ctx
        .layout
        .as_ref()
        .ok_or_else(|| codes::BENCHMARK_WINDOW_SPACE_INSUFFICIENT.to_string())?;
    if should_guard_close(&ctx.config) {
        guard_workload_window(ctx, wl_pid);
    }
    position_workload_window(ctx, wl_pid, plan)?;
    // 僅內建 Vulkan（exe_path 未覆寫）精確 client resize；D3D9 用自身 config 尺寸、
    // 自訂 exe 不可擅自 resize。
    if should_guard_close(&ctx.config) {
        let w = logical_to_physical(ctx.config.width, plan.scale).max(1) as u32;
        let h = logical_to_physical(ctx.config.height, plan.scale).max(1) as u32;
        resize_workload_window_exact(ctx, wl_pid, w, h)?;
    }
    verify_workload_actual_rect(ctx, wl_pid, plan)
}

/// 可中斷 + 完整性輪詢的 warmup：以 [`CANCEL_POLL_MS`] 分段睡，每段先查 cancel
/// （優先）再查視窗完整性；任一異常回 Err。回傳 `Err("cancelled")` 或
/// `Err(BENCHMARK_WINDOW_INTEGRITY)`。
fn warmup_with_integrity(
    ctx: &mut RunContext,
    wl_pid: u32,
    expected: Rect,
    ms: u64,
) -> Result<(), String> {
    let mut remaining = ms;
    while remaining > 0 {
        if ctx.cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let step = remaining.min(CANCEL_POLL_MS);
        let snap = ctx.window.integrity(wl_pid, expected);
        report_integrity(ctx, &snap, None);
        if !integrity_ok(&snap) {
            return Err(codes::BENCHMARK_WINDOW_INTEGRITY.to_string());
        }
        ctx.sleeper.sleep(step);
        remaining -= step;
    }
    if ctx.cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    let snap = ctx.window.integrity(wl_pid, expected);
    report_integrity(ctx, &snap, None);
    if !integrity_ok(&snap) {
        return Err(codes::BENCHMARK_WINDOW_INTEGRITY.to_string());
    }
    Ok(())
}

/// 視窗完整性快照 → runtime `WindowIntegrity`（變更偵測後回報）。
fn report_integrity(ctx: &mut RunContext, snap: &WindowIntegritySnapshot, error: Option<String>) {
    // 失去前景（workload 被其他視窗搶走）→ 本次 benchmark 結束後主視窗置中還原。
    // 僅 foreground==false 觸發；minimized/position/topmost/visible/occlusion 失敗不觸發。
    if !snap.foreground {
        ctx.window_control.request_center_restore();
    }
    let integrity = WindowIntegrity {
        foreground: snap.foreground,
        minimized: snap.minimized,
        position: snap.position_ok && snap.topmost && snap.visible,
        occlusion: snap.occluded,
        retries: ctx.window_retries,
        error,
    };
    if ctx.last_integrity.as_ref() == Some(&integrity) {
        return;
    }
    ctx.last_integrity = Some(integrity.clone());
    (ctx.on_integrity)(&integrity);
}

/// 回報 retry 數遞增（保持上次 booleans，僅更新 retries/error）。
fn report_retries(ctx: &mut RunContext, error: Option<String>) {
    let mut integrity = ctx
        .last_integrity
        .clone()
        .unwrap_or_else(|| WindowIntegrity {
            foreground: true,
            position: true,
            ..Default::default()
        });
    integrity.retries = ctx.window_retries;
    integrity.error = error;
    if ctx.last_integrity.as_ref() == Some(&integrity) {
        return;
    }
    ctx.last_integrity = Some(integrity.clone());
    (ctx.on_integrity)(&integrity);
}

/// 單一 (round, lp) capture 的結果控制（供 run_benchmark 兩階段排程使用）。
enum StepOutcome {
    /// capture 成功（CSV 已寫入）；繼續下一項
    Continue,
    /// 該 LP 因 MISSING/EMPTY 且已有成功 capture 而被隔離（記錄錯誤碼，session 終將 Failed）
    Isolated(String),
    /// 需立即終止（取消或錯誤）
    Break(TerminalReason),
}

/// 執行單一 (round, lp) 的完整 capture：套用單 LP 策略 → 重啟 GPU → spawn workload →
/// PresentMon 收集（含 retry）。成功回傳 Continue；MISSING/EMPTY 且已有成功 capture
/// 回傳 Isolated（隔離該 LP 並繼續）；其餘錯誤/取消回傳 Break。
#[allow(clippy::too_many_arguments)]
fn capture_step(
    ctx: &mut RunContext,
    instance: &str,
    round: u32,
    lp: u32,
    session_dir: &Path,
    round_csvs: &mut RoundCsvs,
    done: u32,
    total_tests: u32,
    detail: &SessionDetail,
    fps_cap: u32,
    buffer: u32,
) -> StepOutcome {
    let pct = (done * 100 / total_tests.max(1)).min(100);
    let eta = detail
        .summary
        .quick
        .as_ref()
        .map(|q| super::physical::remaining_secs(&q.schedule, q.candidates.len(), done))
        .or_else(|| eta_secs(&ctx.config, total_tests, done));
    emit(
        ctx,
        detail,
        "applying",
        Some(round),
        Some(lp),
        pct,
        eta,
        None,
    );

    // 1) 日誌 baseline（第一次變更前）+ 寫入單 LP 策略
    if let Err(e) = recovery::begin_at(&ctx.journal_path, ctx.baseline.as_ref().unwrap()) {
        return StepOutcome::Break(TerminalReason::Error(e));
    }
    let new_policy = AffinityPolicy {
        instance_id: instance.to_string(),
        device_policy: RegistryValueSnapshot::dword(DEVICE_POLICY_SINGLE_PROCESSOR),
        assignment_set_override: RegistryValueSnapshot::binary(
            if let Some(quick) = &detail.summary.quick {
                let Some(target) = quick
                    .candidates
                    .iter()
                    .find(|t| t.lp_indices.first() == Some(&lp))
                else {
                    return StepOutcome::Break(TerminalReason::Error(
                        codes::BENCHMARK_SESSION_INCOMPATIBLE.into(),
                    ));
                };
                match super::physical::mask(target, &ctx.topo) {
                    Ok(bytes) => bytes,
                    Err(e) => return StepOutcome::Break(TerminalReason::Error(e)),
                }
            } else {
                single_lp_mask_bytes(lp)
            },
        ),
    };
    if let Err(_e) = ctx.backend.write_affinity_policy(&new_policy) {
        return StepOutcome::Break(TerminalReason::Error(codes::GPU_APPLY_FAILED.to_string()));
    }
    if let Err(e) = require_journal(&ctx.journal_path)
        .and_then(|j| recovery::advance_to_at(&ctx.journal_path, &j, RecoveryStage::PolicyApplied))
    {
        return StepOutcome::Break(TerminalReason::Error(e));
    }

    // 2) 重啟 GPU（2s/2s 由 backend 內含）+ 5s 穩定
    if let Err(_e) = ctx.backend.restart_device(instance, ctx.sleeper.as_ref()) {
        return StepOutcome::Break(TerminalReason::Error(codes::GPU_RESTART_FAILED.to_string()));
    }
    if sleep_interruptible(ctx, RESTART_STABILIZE_MS) {
        return StepOutcome::Break(TerminalReason::Cancelled);
    }
    if let Err(e) = require_journal(&ctx.journal_path).and_then(|j| {
        recovery::advance_to_at(&ctx.journal_path, &j, RecoveryStage::DeviceRestarted)
    }) {
        return StepOutcome::Break(TerminalReason::Error(e));
    }

    if detail.summary.quick.is_some() {
        match ctx.backend.read_affinity_policy(instance) {
            Ok(actual) if crate::gpu::policy_matches(&new_policy, &actual) => {}
            _ => return StepOutcome::Break(TerminalReason::Error(codes::GPU_APPLY_FAILED.into())),
        }
    }

    // 3) 啟動 workload + prepare + warmup + capture：統一 attempt loop。
    // prepare/warmup 的視窗完整性失敗與 capture 相同，最多重試 2 次（共 3 次嘗試），
    // 污染嘗試不留下 CSV/frametime；cancel 於每輪優先檢查。
    emit(
        ctx,
        detail,
        "launching",
        Some(round),
        Some(lp),
        pct,
        eta,
        None,
    );
    // sync_workload_affinity 已棄用：production runner 絕不把 workload
    // process affinity 繫結到被測 LP。測量變因只能是 GPU interrupt affinity，
    // 將 workload 鎖在單一 LP 會導致 Vulkan present 事件全無、PresentMon
    // 無法產生 CSV（BENCHMARK_CAPTURE_MISSING）。
    let csv = session_dir.join(format!("round-{round}-lp-{lp}.csv"));
    let mut capture_attempt: u32 = 0;
    let mut capture_buffer = buffer;
    let mut capture_result: Result<CapturedCsv, String>;
    loop {
        // attempt >= 2：完整 GPU restart，讓 retry 建立在新的 display device
        // generation 上（單純重啟 workload 無法修復 driver restart 後卡住的
        // Vulkan device/swapchain）。
        if capture_attempt >= 1 {
            if let Err(e) = ctx.backend.restart_device(instance, ctx.sleeper.as_ref()) {
                log::warn!("capture retry 前 GPU 重啟失敗: {e}");
                return StepOutcome::Break(TerminalReason::Error(
                    codes::GPU_RESTART_FAILED.to_string(),
                ));
            }
            if sleep_interruptible(ctx, RESTART_STABILIZE_MS) {
                return StepOutcome::Break(TerminalReason::Cancelled);
            }
        }

        // 啟動 workload（絕不設 CPU affinity）
        let (wl_exe, wl_args) = workload_command(&ctx.assets, &ctx.config, fps_cap);
        let wl_pid = match ctx.processes.spawn(&wl_exe, &wl_args) {
            Ok(pid) => {
                ctx.owned_processes.push(pid);
                pid
            }
            Err(e) => {
                log::warn!("workload 啟動失敗: {e}");
                return StepOutcome::Break(TerminalReason::Error(
                    codes::BENCHMARK_WORKLOAD_FAILED.to_string(),
                ));
            }
        };

        // 強制視窗模式：定位 workload 至 rcWork 左上、Vulkan 精確 resize、空間複檢。
        // 失敗屬確定性問題（找不到視窗/空間不足），非完整性瞬時破壞，不重試。
        let expected = match prepare_workload_window(ctx, wl_pid) {
            Ok(rect) => rect,
            Err(e) => {
                let _ = ctx.processes.kill(wl_pid);
                ctx.owned_processes.clear();
                return StepOutcome::Break(if e == "cancelled" {
                    TerminalReason::Cancelled
                } else {
                    TerminalReason::Error(e)
                });
            }
        };

        // 啟動固定等待 + warm-up（期間輪詢視窗完整性）。完整性失敗 → 污染本次
        // attempt（無 CSV），交由下方與 capture 相同的 retry 決策。
        let warmup_err = warmup_with_integrity(
            ctx,
            wl_pid,
            expected,
            WORKLOAD_STARTUP_MS + (ctx.config.warm_up_secs as u64) * 1000,
        )
        .err();
        capture_attempt += 1;
        capture_result = match warmup_err {
            Some(e) => {
                let _ = ctx.processes.kill(wl_pid);
                ctx.owned_processes.clear();
                if e == "cancelled" {
                    return StepOutcome::Break(TerminalReason::Cancelled);
                }
                Err(e)
            }
            None => {
                // PresentMon 收集 sample_secs（含 stale session 清理、逾時/輸出驗證）
                emit(
                    ctx,
                    detail,
                    "collecting",
                    Some(round),
                    Some(lp),
                    pct,
                    eta,
                    None,
                );
                run_capture(
                    ctx,
                    round,
                    lp,
                    wl_pid,
                    &csv,
                    capture_attempt,
                    fps_cap,
                    capture_buffer,
                    ctx.config.sample_secs,
                    expected,
                )
            }
        };
        if ctx.cancel.is_cancelled() {
            return StepOutcome::Break(TerminalReason::Cancelled);
        }

        // retry 決策：overflow → 重試一次並加倍 buffer；MISSING/EMPTY/WINDOW_INTEGRITY → 重試（同 buffer）。
        let is_window_integrity =
            matches!(&capture_result, Err(e) if e == codes::BENCHMARK_WINDOW_INTEGRITY);
        let double_buffer = match &capture_result {
            Err(e) if e == codes::BENCHMARK_CAPTURE_OVERFLOW && capture_attempt == 1 => Some(true),
            Err(e)
                if (e == codes::BENCHMARK_CAPTURE_MISSING
                    || e == codes::BENCHMARK_CAPTURE_EMPTY
                    || e == codes::BENCHMARK_WINDOW_INTEGRITY)
                    && capture_attempt < MAX_CAPTURE_ATTEMPTS =>
            {
                Some(false)
            }
            _ => None,
        };
        let Some(double) = double_buffer else { break };
        if is_window_integrity {
            ctx.window_retries += 1;
            ctx.capture_quality.window_retry_captures += 1;
            log::warn!(
                "capture round-{round}-lp-{lp} workload 視窗完整性失敗，重跑第 {capture_attempt} 次"
            );
            report_retries(ctx, None);
        } else if double {
            capture_buffer = capture_buffer.saturating_mul(CAPTURE_OVERFLOW_BUFFER_MULT);
            log::warn!(
                "capture round-{round}-lp-{lp} overflowed present events，buffer 加倍至 {capture_buffer} 重試一次"
            );
        } else {
            log::warn!(
                "capture round-{round}-lp-{lp} attempt {capture_attempt} 失敗（{:?}），進行 retry",
                capture_result.as_ref().err()
            );
        }
        if ctx.cancel.is_cancelled() {
            return StepOutcome::Break(TerminalReason::Cancelled);
        }
    }
    if ctx.cancel.is_cancelled() {
        return StepOutcome::Break(TerminalReason::Cancelled);
    }
    let captured = match capture_result {
        Ok(c) => c,
        Err(e) => {
            if e == codes::BENCHMARK_WINDOW_INTEGRITY {
                report_retries(ctx, Some(e.clone()));
            }
            if e == codes::BENCHMARK_CAPTURE_MISSING || e == codes::BENCHMARK_CAPTURE_EMPTY {
                if round_csvs.is_empty() {
                    // 尚無任何成功 capture：第一個候選 LP 就 MISSING/EMPTY，代表
                    // PresentMon/ETW 環境根本無法建立 CSV。繼續跑剩餘 LP/round 只會
                    // 反覆失敗，立即終止並交由 terminal 統一 cleanup/restore。
                    log::error!(
                        "capture round-{round}-lp-{lp} 經 {capture_attempt} 次嘗試仍失敗 \
                         （{e}），且尚無任何成功 capture；中止 session"
                    );
                    return StepOutcome::Break(TerminalReason::Error(e));
                }
                log::error!(
                    "capture round-{round}-lp-{lp} 經 {capture_attempt} 次嘗試仍失敗；隔離此 LP 並繼續"
                );
                return StepOutcome::Isolated(e);
            }
            return StepOutcome::Break(TerminalReason::Error(e));
        }
    };
    round_csvs.entry(lp).or_default().insert(round, captured);
    emit(
        ctx,
        detail,
        "collected",
        Some(round),
        Some(lp),
        pct,
        eta,
        None,
    );
    StepOutcome::Continue
}

/// 校準 capture 的 round 編號（與正式 round namespace 隔離；diagnostic 用）。
const CALIBRATION_ROUND_BASE: u32 = 300;

fn next_power_of_two(v: u64) -> u64 {
    if v == 0 {
        return 1;
    }
    let mut p = 1u64;
    while p < v {
        p <<= 1;
    }
    p
}

/// 依 cap 決定 circular buffer：`max(8192, next_power_of_two(cap*8))`。
pub fn calibration_buffer(cap: u32) -> u32 {
    let needed = (cap as u64).saturating_mul(8);
    let pow2 = next_power_of_two(needed);
    (pow2.max(8192)).min(u32::MAX as u64) as u32
}

/// 校準 tier 決策：剛測試的新 tier 相對前一 clean tier 的 FPS 增益 < 門檻 → 停
/// （選剛測試的新 tier，其 clean 且仍 ≥ 前一 tier）。
fn calibration_stop(clean_fps: f64, next_fps: f64) -> bool {
    if clean_fps <= 0.0 || !clean_fps.is_finite() || !next_fps.is_finite() {
        return true;
    }
    ((next_fps - clean_fps) / clean_fps * 100.0) < CALIBRATION_GAIN_MIN_PCT
}

/// 純決策：剛測試的新 tier clean 時，決定「選新 tier 停」或「繼續下一 tier」。
/// 回傳 `Some(cap)` = 選定、`None` = 繼續。規則：
/// - 最後一個 tier clean → 選它（不管增益）。
/// - 相對前一 clean tier 增益 < [`CALIBRATION_GAIN_MIN_PCT`] → 選新 tier（非前一 tier）。
/// - 首 tier（無前一 clean FPS）→ 繼續。
fn calibration_clean_decision(
    clean_fps: Option<f64>,
    tier: u32,
    fps: f64,
    is_last_tier: bool,
) -> Option<u32> {
    if is_last_tier {
        return Some(tier);
    }
    match clean_fps {
        Some(cf) if calibration_stop(cf, fps) => Some(tier),
        _ => None,
    }
}

/// 單一校準 capture 的結果。
enum CalibrationCapture {
    Clean(f64),
    Overflow,
}

/// 校準：依 tier 序列選定最高安全 cap + buffer。Adaptive 才執行；Fixed 沿用 fps_cap。
/// 全程不變更 GPU affinity（workload 在既有策略下執行）。
fn calibrate(
    ctx: &mut RunContext,
    session_dir: &Path,
    detail: &SessionDetail,
) -> Result<(u32, u32), String> {
    if ctx.config.fps_cap_policy != FpsCapPolicy::Adaptive {
        return Ok((ctx.config.fps_cap, PRESENTMON_CIRCULAR_BUFFER_SIZE));
    }
    let mut clean_tier: Option<u32> = None;
    let mut clean_fps: Option<f64> = None;
    let n_tiers = CALIBRATION_TIERS.len();
    for (i, &tier) in CALIBRATION_TIERS.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let buffer = calibration_buffer(tier);
        match calibration_capture(ctx, tier, buffer, session_dir, detail)? {
            CalibrationCapture::Overflow => match clean_tier {
                Some(prev) => return Ok((prev, calibration_buffer(prev))),
                None => return Err(codes::BENCHMARK_CAPTURE_OVERFLOW.to_string()),
            },
            CalibrationCapture::Clean(fps) => {
                if let Some(selected) =
                    calibration_clean_decision(clean_fps, tier, fps, i + 1 == n_tiers)
                {
                    return Ok((selected, calibration_buffer(selected)));
                }
                clean_tier = Some(tier);
                clean_fps = Some(fps);
            }
        }
    }
    match clean_tier {
        Some(ct) => Ok((ct, calibration_buffer(ct))),
        None => Err(codes::BENCHMARK_CAPTURE_OVERFLOW.to_string()),
    }
}

/// 執行單一 tier 的校準 capture（無 GPU mutation），回傳 Clean(avg_fps) 或 Overflow。
fn calibration_capture(
    ctx: &mut RunContext,
    tier: u32,
    buffer: u32,
    session_dir: &Path,
    detail: &SessionDetail,
) -> Result<CalibrationCapture, String> {
    emit(ctx, detail, "calibrating", None, Some(tier), 0, None, None);
    let (wl_exe, wl_args) = workload_command(&ctx.assets, &ctx.config, tier);
    let wl_pid = match ctx.processes.spawn(&wl_exe, &wl_args) {
        Ok(pid) => {
            ctx.owned_processes.push(pid);
            pid
        }
        Err(_) => return Err(codes::BENCHMARK_WORKLOAD_FAILED.to_string()),
    };
    let expected = prepare_workload_window(ctx, wl_pid)?;
    warmup_with_integrity(
        ctx,
        wl_pid,
        expected,
        WORKLOAD_STARTUP_MS + (CALIBRATION_WARMUP_SECS as u64) * 1000,
    )?;
    let csv = session_dir.join(format!("calib-tier-{tier}.csv"));
    let result = run_capture(
        ctx,
        CALIBRATION_ROUND_BASE,
        tier,
        wl_pid,
        &csv,
        1,
        tier,
        buffer,
        CALIBRATION_SAMPLE_SECS,
        expected,
    );
    if ctx.cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    match result {
        Err(e) if e == codes::BENCHMARK_CAPTURE_OVERFLOW => Ok(CalibrationCapture::Overflow),
        Err(e) => Err(e),
        Ok(captured) => {
            let frames = captured.read()?;
            let res = compute_lp_result(tier, &frames)?;
            match res.avg_fps {
                Some(fps) if fps.is_finite() && fps > 0.0 => Ok(CalibrationCapture::Clean(fps)),
                _ => Err(codes::BENCHMARK_CSV_INVALID.to_string()),
            }
        }
    }
}

/// 原始策略基線 capture 的 round 編號 namespace（與正式 round 隔離）
pub const BASELINE_ROUND_BASE: u32 = 900;

/// 執行單次「原始策略基線」capture：不套用任何 GPU policy，workload 在
/// 既有（OS 預設或先前套用）策略下執行。形狀同 [`calibration_capture`]，
/// 但 warmup/取樣秒數由呼叫端指定（與 retest 同條件）。
#[allow(clippy::too_many_arguments)]
fn baseline_capture(
    ctx: &mut RunContext,
    round: u32,
    fps_cap: u32,
    buffer: u32,
    warmup_secs: u32,
    sample_secs: u32,
    session_dir: &Path,
    detail: &SessionDetail,
) -> Result<CapturedCsv, String> {
    emit(ctx, detail, "calibrating", Some(round), None, 0, None, None);
    if ctx.cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    let (wl_exe, wl_args) = workload_command(&ctx.assets, &ctx.config, fps_cap);
    let wl_pid = match ctx.processes.spawn(&wl_exe, &wl_args) {
        Ok(pid) => {
            ctx.owned_processes.push(pid);
            pid
        }
        Err(_) => return Err(codes::BENCHMARK_WORKLOAD_FAILED.to_string()),
    };
    let expected = prepare_workload_window(ctx, wl_pid)?;
    warmup_with_integrity(
        ctx,
        wl_pid,
        expected,
        WORKLOAD_STARTUP_MS + (warmup_secs as u64) * 1000,
    )?;
    let csv = session_dir.join(format!("round-{round}-lp-0.csv"));
    let result = run_capture(
        ctx,
        round,
        0,
        wl_pid,
        &csv,
        1,
        fps_cap,
        buffer,
        sample_secs,
        expected,
    );
    if ctx.cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    result
}

/// 某 round 所有 LP 的 frametime 中位數（漂移偵測用）。
#[cfg(test)]
fn round_median_frametime(round_csvs: &RoundCsvs, round: u32) -> Option<f64> {
    let mut all: Vec<f64> = Vec::new();
    for rounds in round_csvs.values() {
        if let Some(csv) = rounds.get(&round) {
            if let Ok(frames) = csv.read() {
                all.extend(frames);
            }
        }
    }
    if all.is_empty() {
        None
    } else {
        Some(median(&all))
    }
}

/// 主要入口：執行整個基準測試並回傳最終結果。
/// 固定調適排程：短 capture 篩選全 LP → Top 5 中等 capture → Top 3 正式 capture → Top 2 →
/// 3..=7 前向確認 →（若 RunnerUpReversal）反向驗證（min 3、max min(5,10−forward)）。
/// 成功 / 取消 / 失敗都會：停止 owned 子程序 → 還原原始策略並重啟 GPU →
/// 還原驗證成功才清除日誌 → 原子寫入最終 session。
#[cfg(not(test))]
pub fn run_benchmark(ctx: &mut RunContext) -> RunResult {
    if ctx.config.method_version != super::physical::METHOD_VERSION {
        return abort(ctx, codes::BENCHMARK_INVALID_CONFIG.into());
    }
    quick::run(ctx)
}

// 舊排程僅保留作為原有 capture、取消與回復回歸測試的測試驅動器。
#[cfg(test)]
pub fn run_benchmark(ctx: &mut RunContext) -> RunResult {
    if ctx.config.method_version == super::physical::METHOD_VERSION {
        return quick::run(ctx);
    }
    // 前置驗證
    let (instance, gpu_name) = match pre_flight(ctx) {
        Ok(v) => v,
        Err(e) => return abort(ctx, e),
    };
    let lps = effective_lps(&ctx.config, &ctx.topo);
    if lps.is_empty() {
        return abort(ctx, codes::BENCHMARK_INVALID_CONFIG.to_string());
    }

    // 空間預檢 + 快照主視窗 + 切 compact（RAII guard 於函式結束/panic unwind 還原）
    let _layout_guard = match prepare_window_layout(
        ctx.window_control.clone(),
        (ctx.config.width, ctx.config.height),
    ) {
        Ok(g) => {
            ctx.layout = Some(g.plan);
            g
        }
        Err(e) => return abort(ctx, e),
    };

    // 建立 session（Running）並原子寫入
    let mut detail = SessionDetail {
        summary: SessionSummary {
            id: ctx.session_id.clone(),
            status: SessionStatus::Running,
            started_at: chrono::Local::now().to_rfc3339(),
            finished_at: None,
            gpu_name,
            gpu_instance_id: instance.clone(),
            cpu_fingerprint: cpu_fingerprint_with(&ctx.topo, &ctx.cpu_identity),
            best_lp: None,
            reliability: ReliabilitySummary::default(),
            severe_lps: Vec::new(),
            sample_count: 0,
            total_bytes: 0,
            config: ctx.config.clone(),
            error: None,
            ..Default::default()
        },
        results: Vec::new(),
        samples: Vec::new(),
        ..Default::default()
    };
    let _ = storage::save_session_at(&ctx.storage_root, &detail);
    emit(ctx, &detail, "starting", None, None, 0, None, None);

    // 環境閘（AC/電池節能/CPU idle）。不滿足 → fail closed（Failed，無可套用結果）。
    if let Err(e) = env::environment_gate(ctx.env.as_ref()) {
        detail.summary.environment_stability = EnvironmentStability {
            passed: false,
            drift_reruns: 0,
            error: Some(e.clone()),
        };
        return terminal(
            ctx,
            detail,
            SessionStatus::Failed,
            Some(e),
            None,
            Vec::new(),
            Vec::new(),
        );
    }
    if let Err(e) = env::wait_for_cpu_idle(ctx.env.as_ref(), ctx.sleeper.as_ref(), &|| {
        ctx.cancel.is_cancelled()
    }) {
        if e == "cancelled" {
            return terminal(
                ctx,
                detail,
                SessionStatus::Cancelled,
                None,
                None,
                Vec::new(),
                Vec::new(),
            );
        }
        detail.summary.environment_stability = EnvironmentStability {
            passed: false,
            drift_reruns: 0,
            error: Some(e.clone()),
        };
        return terminal(
            ctx,
            detail,
            SessionStatus::Failed,
            Some(e),
            None,
            Vec::new(),
            Vec::new(),
        );
    }

    let session_dir = ctx.storage_root.join(&ctx.session_id);
    // LP → round → CSV 路徑（每個 (lp, round) 最多擷取一次）
    let mut round_csvs: RoundCsvs = HashMap::new();

    // 校準（Adaptive）鎖定 cap + buffer；Fixed 沿用 fps_cap。
    let (fps_cap, buffer) = match calibrate(ctx, &session_dir, &detail) {
        Ok((cap, buf)) => (cap, buf),
        Err(e) if e == "cancelled" => {
            return terminal(
                ctx,
                detail,
                SessionStatus::Cancelled,
                None,
                None,
                Vec::new(),
                Vec::new(),
            );
        }
        Err(e) => {
            return terminal(
                ctx,
                detail,
                SessionStatus::Failed,
                Some(e),
                None,
                Vec::new(),
                Vec::new(),
            );
        }
    };
    detail.summary.capture_quality.effective_fps_cap = fps_cap;
    detail.summary.capture_quality.circular_buffer_size = buffer;

    // 進度分母取「最多」capture 數（含 refinement 與前向+反向確認的總配對預算上限）。
    let n = lps.len() as u32;
    let racing_count = n.min(MAX_SELECTION_CANDIDATES as u32);
    let refinement_count = n.min(MAX_REFINEMENT_CANDIDATES as u32);
    let total_tests = n * SCREENING_ROUNDS
        + racing_count
        + refinement_count
        + (MAX_FINALISTS as u32) * TOTAL_PAIR_BUDGET;
    let mut done = 0u32;
    let mut reason: Option<TerminalReason> = None;
    // MISSING/EMPTY 的處置分兩類（見 capture_step 分支）：
    // - 尚無任何成功 capture → fail-fast，立即終止（PresentMon 根本無法建立 CSV）。
    // - 已有成功 capture → 單一 LP 屬可隔離的擷取故障，記錄後繼續收集其他 LP。
    let mut isolated_capture_error: Option<String> = None;
    let mut drift_reruns = 0u32;

    let configured_sample_secs = ctx.config.sample_secs;
    let configured_warmup_secs = ctx.config.warm_up_secs;

    // ── 快速篩選：短 capture 測全部候選 LP（含漂移重跑）。 ──
    ctx.config.sample_secs = configured_sample_secs.min(QUICK_SCREEN_SAMPLE_SECS);
    ctx.config.warm_up_secs = configured_warmup_secs.min(QUICK_SCREEN_WARMUP_SECS);
    let mut screening_ref: Option<f64> = None;
    'screening: for round in 0..SCREENING_ROUNDS {
        match capture_round_with_drift(
            ctx,
            &instance,
            round,
            &lps,
            &session_dir,
            &mut round_csvs,
            &mut done,
            total_tests,
            &detail,
            fps_cap,
            buffer,
            &mut screening_ref,
            &mut drift_reruns,
        ) {
            StepOutcome::Continue => {}
            StepOutcome::Isolated(e) => {
                isolated_capture_error.get_or_insert(e);
            }
            StepOutcome::Break(r) => {
                reason = Some(r);
                break 'screening;
            }
        }
    }

    // ── racing refinement：短篩結果 Top 5 再跑一輪中等 capture。 ──
    let racing_target = lps.len().min(MAX_SELECTION_CANDIDATES);
    let mut racing: Vec<u32> = Vec::new();
    if reason.is_none() && isolated_capture_error.is_none() {
        racing = select_top_candidates(&round_csvs, SCREENING_ROUNDS, racing_target);
    }
    if reason.is_none()
        && isolated_capture_error.is_none()
        && lps.len() >= MAX_FINALISTS
        && !racing.is_empty()
        && racing.len() == racing_target
    {
        let racing_round = SCREENING_ROUNDS;
        ctx.config.sample_secs = configured_sample_secs.min(RACING_SAMPLE_SECS);
        ctx.config.warm_up_secs = configured_warmup_secs.min(QUICK_SCREEN_WARMUP_SECS);
        for &lp in round_order(racing_round, &racing).iter() {
            if ctx.cancel.is_cancelled() {
                reason = Some(TerminalReason::Cancelled);
                break;
            }
            done += 1;
            match capture_step(
                ctx,
                &instance,
                racing_round,
                lp,
                &session_dir,
                &mut round_csvs,
                done,
                total_tests,
                &detail,
                fps_cap,
                buffer,
            ) {
                StepOutcome::Continue => {}
                StepOutcome::Isolated(e) => {
                    isolated_capture_error.get_or_insert(e);
                }
                StepOutcome::Break(r) => {
                    reason = Some(r);
                    break;
                }
            }
        }

        // 前兩輪仍有完整證據者中保留 Top 3；差距接近者透過較大的保留池避免
        // 被單次短 capture 過早淘汰。
        let refinement_target = racing_target.min(MAX_REFINEMENT_CANDIDATES);
        let top3 = if reason.is_none() && isolated_capture_error.is_none() {
            select_top_candidates(&round_csvs, SCREENING_ROUNDS + 1, refinement_target)
        } else {
            Vec::new()
        };
        if top3.len() == refinement_target && !top3.is_empty() {
            let refinement_round = SCREENING_ROUNDS + 1;
            ctx.config.sample_secs = configured_sample_secs;
            ctx.config.warm_up_secs = configured_warmup_secs;
            for &lp in round_order(refinement_round, &top3).iter() {
                if ctx.cancel.is_cancelled() {
                    reason = Some(TerminalReason::Cancelled);
                    break;
                }
                done += 1;
                match capture_step(
                    ctx,
                    &instance,
                    refinement_round,
                    lp,
                    &session_dir,
                    &mut round_csvs,
                    done,
                    total_tests,
                    &detail,
                    fps_cap,
                    buffer,
                ) {
                    StepOutcome::Continue => {}
                    StepOutcome::Isolated(e) => {
                        isolated_capture_error.get_or_insert(e);
                    }
                    StepOutcome::Break(r) => {
                        reason = Some(r);
                        break;
                    }
                }
            }
        }
    }

    // 後續 confirmation 一律使用使用者設定的正式時長。
    ctx.config.sample_secs = configured_sample_secs;
    ctx.config.warm_up_secs = configured_warmup_secs;

    // ── 選 Top 2：三個 screening/refinement round 的完整證據。 ──
    let mut finalists: Vec<u32> = Vec::new();
    if reason.is_none() && isolated_capture_error.is_none() {
        finalists = select_top_candidates(
            &round_csvs,
            SCREENING_ROUNDS + REFINEMENT_ROUNDS,
            MAX_FINALISTS,
        );
    }

    // ── 前向確認：Top 2，rounds CONFIRMATION_ROUND_BASE..（含 pair 漂移重跑）。 ──
    let mut confirmation_rounds_done: u32 = 0;
    let mut forward_verdict: Option<ForwardVerdict> = None;
    let mut confirm_ref: Option<f64> = None;
    if reason.is_none() && isolated_capture_error.is_none() && finalists.len() == MAX_FINALISTS {
        'confirmation: for round in
            CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + CONFIRMATION_MAX_ROUNDS)
        {
            match capture_round_with_drift(
                ctx,
                &instance,
                round,
                &finalists,
                &session_dir,
                &mut round_csvs,
                &mut done,
                total_tests,
                &detail,
                fps_cap,
                buffer,
                &mut confirm_ref,
                &mut drift_reruns,
            ) {
                StepOutcome::Continue => {}
                StepOutcome::Isolated(e) => {
                    isolated_capture_error.get_or_insert(e);
                }
                StepOutcome::Break(r) => {
                    reason = Some(r);
                    break 'confirmation;
                }
            }
            if reason.is_some() || isolated_capture_error.is_some() {
                break 'confirmation;
            }
            confirmation_rounds_done += 1;
            if confirmation_rounds_done >= CONFIRMATION_MIN_ROUNDS {
                let verdict = evaluate_forward(
                    &round_csvs,
                    finalists[0],
                    finalists[1],
                    confirmation_rounds_done,
                    CONFIRMATION_ROUND_BASE,
                );
                if matches!(
                    verdict,
                    ForwardVerdict::CandidatePassed
                        | ForwardVerdict::RunnerUpReversal
                        | ForwardVerdict::Equivalent
                ) {
                    forward_verdict = Some(verdict);
                    break 'confirmation;
                }
            }
        }
    }

    // ── 反向驗證（RunnerUpReversal 觸發）：fresh round namespace。 ──
    let mut reverse_ran = false;
    let mut reverse_passed = false;
    let mut reverse_rounds = 0u32;
    if reason.is_none()
        && isolated_capture_error.is_none()
        && matches!(forward_verdict, Some(ForwardVerdict::RunnerUpReversal))
        && finalists.len() == MAX_FINALISTS
    {
        reverse_ran = true;
        let predeclared = finalists[1];
        let challenger = finalists[0];
        let pair = [predeclared, challenger];
        let reverse_max = reverse_max_rounds(confirmation_rounds_done);
        let mut reverse_ref: Option<f64> = None;
        'reverse: for round in REVERSE_ROUND_BASE..(REVERSE_ROUND_BASE + reverse_max) {
            match capture_round_with_drift(
                ctx,
                &instance,
                round,
                &pair,
                &session_dir,
                &mut round_csvs,
                &mut done,
                total_tests,
                &detail,
                fps_cap,
                buffer,
                &mut reverse_ref,
                &mut drift_reruns,
            ) {
                StepOutcome::Continue => {}
                StepOutcome::Isolated(e) => {
                    isolated_capture_error.get_or_insert(e);
                }
                StepOutcome::Break(r) => {
                    reason = Some(r);
                    break 'reverse;
                }
            }
            if reason.is_some() || isolated_capture_error.is_some() {
                break 'reverse;
            }
            reverse_rounds += 1;
            if reverse_rounds >= CONFIRMATION_MIN_ROUNDS {
                let verdict = evaluate_forward(
                    &round_csvs,
                    predeclared,
                    challenger,
                    reverse_rounds,
                    REVERSE_ROUND_BASE,
                );
                match verdict {
                    ForwardVerdict::CandidatePassed => {
                        reverse_passed = true;
                        break 'reverse;
                    }
                    ForwardVerdict::Equivalent | ForwardVerdict::RunnerUpReversal => {
                        break 'reverse;
                    }
                    ForwardVerdict::Continue => {}
                }
            }
        }
    }

    if reason.is_none() {
        if let Some(e) = isolated_capture_error {
            reason = Some(TerminalReason::Error(e));
        }
    }

    match reason {
        Some(TerminalReason::Cancelled) => {
            // 保留已收集到的部分結果
            let (partial, _) = compute_session_results(&round_csvs);
            detail.results = partial;
            detail.summary.sample_count = detail.results.iter().map(|r| r.sample_count).sum();
            terminal(
                ctx,
                detail,
                SessionStatus::Cancelled,
                None,
                None,
                Vec::new(),
                Vec::new(),
            )
        }
        Some(TerminalReason::Error(e)) => {
            // 保留已收集到的部分結果，無推薦
            let (partial, _) = compute_session_results(&round_csvs);
            detail.results = partial;
            detail.summary.sample_count = detail.results.iter().map(|r| r.sample_count).sum();
            if e == codes::BENCHMARK_ENV_UNSTABLE {
                detail.summary.environment_stability.error = Some(e.clone());
            }
            terminal(
                ctx,
                detail,
                SessionStatus::Failed,
                Some(e),
                None,
                Vec::new(),
                Vec::new(),
            )
        }
        None => {
            // 成功：合併各 round frametime → 統計 → 最佳/嚴重 LP
            let (results, compute_err) = compute_session_results(&round_csvs);
            detail.results = results.clone();
            detail.summary.sample_count = results.iter().map(|r| r.sample_count).sum();
            if let Some(e) = compute_err {
                // 任一 LP 的 CSV 缺失/空/無效 → 失敗，保留已算出的部分結果、無推薦
                return terminal(
                    ctx,
                    detail,
                    SessionStatus::Failed,
                    Some(e),
                    None,
                    Vec::new(),
                    Vec::new(),
                );
            }
            // 分相結果（獨立保存，供稽核 round 數與隔離證據）。
            detail.screening_results = compute_phase_results(&round_csvs, 0, SCREENING_ROUNDS);
            detail.refinement_results = compute_phase_results(
                &round_csvs,
                SCREENING_ROUNDS,
                SCREENING_ROUNDS + REFINEMENT_ROUNDS,
            );
            detail.confirmation_results = compute_phase_results(
                &round_csvs,
                CONFIRMATION_ROUND_BASE,
                CONFIRMATION_ROUND_BASE + confirmation_rounds_done,
            );

            // 判定 verified best。
            let mut verified_best: Option<u32> = None;
            let mut confirmation_winner: Option<u32> = None;
            if finalists.len() == MAX_FINALISTS {
                match forward_verdict {
                    Some(ForwardVerdict::CandidatePassed) => {
                        verified_best = Some(finalists[0]);
                        confirmation_winner = Some(finalists[0]);
                    }
                    Some(ForwardVerdict::RunnerUpReversal) if reverse_passed => {
                        verified_best = Some(finalists[1]);
                        confirmation_winner = Some(finalists[1]);
                    }
                    _ => {}
                }
            }

            let severe = severe_lps(&results);
            let reliability = compute_reliability(
                &round_csvs,
                &results,
                &finalists,
                confirmation_rounds_done,
                forward_verdict,
                reverse_ran,
                reverse_passed,
                reverse_rounds,
            );
            let is_equivalent = reliability.status == ReliabilityStatus::Equivalent;
            let best = verified_best;
            let recommended = best.map(|b| vec![b]).unwrap_or_default();
            detail.summary.reliability = reliability;
            detail.summary.screening_candidate_lp = finalists.first().copied();
            detail.summary.screening_runner_up_lp = finalists.get(1).copied();
            detail.summary.confirmation_winner_lp = confirmation_winner;
            detail.summary.verified_best_lp = verified_best;
            // Equivalent 不設 best/verified/winner，只記錄等效 finalists（[candidate, runner]）。
            detail.summary.equivalent_finalist_lps = if is_equivalent {
                finalists.clone()
            } else {
                Vec::new()
            };
            detail.summary.environment_stability = EnvironmentStability {
                passed: true,
                drift_reruns,
                error: None,
            };
            terminal(
                ctx,
                detail,
                SessionStatus::Completed,
                None,
                best,
                severe,
                recommended,
            )
        }
    }
}

/// 執行一 round 的所有 capture（`lps` 為該 round 的 LP 集合），並做漂移重跑：
/// 該 round 的 frametime 中位數相對跨 round 參考偏離 >5% → 覆寫重跑（最多
/// [`env::MAX_DRIFT_RETRIES`] 次）；重跑上限用盡 → BENCHMARK_ENV_UNSTABLE（fail closed）。
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn capture_round_with_drift(
    ctx: &mut RunContext,
    instance: &str,
    round: u32,
    lps: &[u32],
    session_dir: &Path,
    round_csvs: &mut RoundCsvs,
    done: &mut u32,
    total_tests: u32,
    detail: &SessionDetail,
    fps_cap: u32,
    buffer: u32,
    reference: &mut Option<f64>,
    drift_reruns: &mut u32,
) -> StepOutcome {
    let mut reruns = 0u32;
    loop {
        if ctx.cancel.is_cancelled() {
            return StepOutcome::Break(TerminalReason::Cancelled);
        }
        let mut break_reason: Option<TerminalReason> = None;
        let mut isolated: Option<String> = None;
        for &lp in round_order(round, lps).iter() {
            if ctx.cancel.is_cancelled() {
                break_reason = Some(TerminalReason::Cancelled);
                break;
            }
            *done += 1;
            match capture_step(
                ctx,
                instance,
                round,
                lp,
                session_dir,
                round_csvs,
                *done,
                total_tests,
                detail,
                fps_cap,
                buffer,
            ) {
                StepOutcome::Continue => {}
                StepOutcome::Isolated(e) => {
                    isolated.get_or_insert(e);
                }
                StepOutcome::Break(r) => {
                    break_reason = Some(r);
                    break;
                }
            }
        }
        if let Some(r) = break_reason {
            return StepOutcome::Break(r);
        }
        if let Some(e) = isolated {
            return StepOutcome::Isolated(e);
        }
        // 漂移檢查：round 中位數相對參考偏離 >5% → 重跑；參考由首 round 建立。
        let current = round_median_frametime(round_csvs, round);
        if let Some(cur) = current {
            if let Some(r) = *reference {
                if env::drift_pct(r, cur) > env::DRIFT_THRESHOLD_PCT {
                    if reruns < env::MAX_DRIFT_RETRIES {
                        reruns += 1;
                        *drift_reruns += 1;
                        log::warn!(
                            "round {round} 中位數漂移 >5%，重跑（{reruns}/{}）",
                            env::MAX_DRIFT_RETRIES
                        );
                        continue;
                    }
                    log::error!("round {round} 漂移重跑上限用盡，環境不穩定 → 失敗關閉");
                    return StepOutcome::Break(TerminalReason::Error(
                        codes::BENCHMARK_ENV_UNSTABLE.to_string(),
                    ));
                }
            } else {
                *reference = Some(cur);
            }
        }
        return StepOutcome::Continue;
    }
}

/// 前置驗證：資源 hash、GPU 存在、BasicDisplay 可用、設定有效。
/// 成功回傳 (gpu_instance_id, gpu_name)，並把原始策略快照存進 ctx.baseline。
fn pre_flight(ctx: &mut RunContext) -> Result<(String, String), String> {
    assets::verify(&ctx.assets).map_err(|e| {
        log::error!("基準測試資源驗證失敗: {e}");
        e.code().to_string()
    })?;

    let instance = ctx
        .config
        .gpu_instance_id
        .clone()
        .ok_or_else(|| codes::BENCHMARK_INVALID_CONFIG.to_string())?;

    let gpu_name = {
        let adapters = ctx
            .backend
            .enumerate_present_adapters()
            .map_err(|e| e.code().to_string())?;
        adapters
            .iter()
            .find(|d| d.instance_id.eq_ignore_ascii_case(&instance))
            .map(|d| d.friendly_name.clone())
            .ok_or_else(|| codes::GPU_NOT_FOUND.to_string())?
    };

    if !ctx
        .backend
        .basic_display_enabled()
        .map_err(|e| e.code().to_string())?
    {
        return Err(codes::GPU_BASIC_DISPLAY_DISABLED.to_string());
    }

    validate_config(&ctx.config, &ctx.topo)?;

    let baseline = ctx
        .backend
        .read_affinity_policy(&instance)
        .map_err(|e| e.code().to_string())?;
    ctx.baseline = Some(baseline);
    Ok((instance, gpu_name))
}

/// 設定驗證（獨立函式方便測試）
pub fn validate_config(
    config: &BenchmarkConfig,
    topo: &crate::topology::Topology,
) -> Result<(), String> {
    if config.gpu_instance_id.is_none() {
        return Err(codes::BENCHMARK_INVALID_CONFIG.to_string());
    }
    if config.fps_cap > 10000
        || config.sample_secs == 0
        || config.sample_secs > 120
        || config.warm_up_secs > 60
        || config.retest_sample_secs == 0
        || config.retest_sample_secs > 120
        || config.retest_warm_up_secs > 60
    {
        return Err(codes::BENCHMARK_INVALID_CONFIG.to_string());
    }
    // 強制視窗模式：所有 benchmark 與 Equivalent validation 皆拒絕 fullscreen=true。
    if config.fullscreen {
        return Err(codes::BENCHMARK_INVALID_CONFIG.to_string());
    }
    // 新排程固定為 1 篩選 + 2 refinement + 3..=7 前向確認，與 `repetitions` 欄位無關；
    // 該欄位保留供舊 session 反序列化，新 run 一律忽略。
    if (config.method_version == super::physical::METHOD_VERSION
        && super::physical::select(topo, &config.candidate_core_ids).is_err())
        || (config.method_version != super::physical::METHOD_VERSION
            && effective_lps(config, topo).is_empty())
    {
        return Err(codes::BENCHMARK_INVALID_CONFIG.to_string());
    }
    if config.workload == WorkloadKind::Vulkan && config.vulkan_args.is_empty() {
        return Err(codes::BENCHMARK_INVALID_CONFIG.to_string());
    }
    if config.width == 0 || config.height == 0 {
        return Err(codes::BENCHMARK_INVALID_CONFIG.to_string());
    }
    Ok(())
}

/// 有效 LP：候選清單 ∩ [0, min(total_lp,64))。
/// 空清單 = 預設候選：P-core 的 primary LP（非 SMT sibling），排除 physical Core 0 —
/// GPU 中斷目標只需最高時脈實體核心；sibling 與 primary 是同一顆物理核心（GPU policy
/// 綁單一 LP），成對測試是重複測量。P-core 全落在 core 0 的奇異拓撲才 fallback 到
/// 「所有 primary − core 0」。明確清單照單全收（僅範圍與 core 0 檢查），保留除錯/實驗
/// （例如驗證 sibling 表現）的完全控制。
pub fn effective_lps(config: &BenchmarkConfig, topo: &crate::topology::Topology) -> Vec<u32> {
    let max_lp = topo.total_lp.min(64);
    let source: Vec<u32> = if config.candidate_lps.is_empty() {
        // 預設：P-core primary，排除 core 0；P-core 全落在 core 0 時 fallback 所有 primary
        let primaries = |p_only: bool| -> Vec<u32> {
            topo.logical_processors
                .iter()
                .filter(|lp| {
                    lp.index < max_lp
                        && lp.core_id != 0
                        && !lp.is_smt_sibling
                        && topo
                            .physical_cores
                            .get(lp.core_id as usize)
                            .is_some_and(|c| c.is_p_core || !p_only)
                })
                .map(|lp| lp.index)
                .collect()
        };
        let p = primaries(true);
        if p.is_empty() {
            primaries(false)
        } else {
            p
        }
    } else {
        config.candidate_lps.clone()
    };
    // physical Core 0 的 LP index 集合（core_id 由拓撲列舉時依 index 順序指派）
    let core0_lps: Vec<u32> = topo
        .logical_processors
        .iter()
        .filter(|lp| lp.core_id == 0)
        .map(|lp| lp.index)
        .collect();
    let mut lps: Vec<u32> = source
        .into_iter()
        .filter(|&lp| lp < max_lp)
        .filter(|&lp| !core0_lps.contains(&lp))
        .collect();
    lps.sort_unstable();
    lps.dedup();
    lps
}

/// 每 round 的 LP 順序：確定性「旋轉 + 反轉」平衡排程。
/// round r 的起始位置 = `r % n`（n = LP 數），方向 = r 為奇數時遞減、偶數遞增。
/// 每個 round 都是完整排列（不省略、不重複），且相鄰 round 的起始位置與方向
/// 皆改變，避免固定順序造成的系統性偏誤。
#[cfg(test)]
pub fn round_order(round: u32, lps: &[u32]) -> Vec<u32> {
    let mut v = lps.to_vec();
    v.sort_unstable();
    let n = v.len();
    if n == 0 {
        return v;
    }
    let start = (round as usize) % n;
    let reverse = round % 2 == 1;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let idx = if reverse {
            (start + n - i) % n
        } else {
            (start + i) % n
        };
        out.push(v[idx]);
    }
    out
}

/// 依 workload 種類決定要啟動的 exe + args。`fps_cap` 為有效 cap（校準鎖定值 /
/// legacy `config.fps_cap`），會覆寫 Vulkan args 中既有的 `--fps_cap=`。
fn workload_command(
    assets: &BenchmarkAssets,
    config: &BenchmarkConfig,
    fps_cap: u32,
) -> (PathBuf, Vec<String>) {
    match config.workload {
        WorkloadKind::Vulkan => (
            assets.vulkan_workload.clone(),
            with_fps_cap(&config.vulkan_args, fps_cap),
        ),
        WorkloadKind::D3D9 => (
            assets.d3d9_workload.clone(),
            // D3D9 workload 的 CLI 為 `<0|1>` 布林格式（false 字串會被解析成 true）
            vec![
                format!("--fullscreen={}", if config.fullscreen { 1 } else { 0 }),
                format!("--width={}", config.width),
                format!("--height={}", config.height),
                format!("--fps-cap={fps_cap}"),
                format!(
                    "--triple-buffer={}",
                    if config.triple_buffer { 1 } else { 0 }
                ),
            ],
        ),
    }
}

/// 覆寫 args 中的 `--fps_cap=` 為 `cap`（既有者移除後重插於尾端）。
fn with_fps_cap(args: &[String], cap: u32) -> Vec<String> {
    let mut out: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--fps_cap="))
        .cloned()
        .collect();
    out.push(format!("--fps_cap={cap}"));
    out
}

/// PresentMon 2.5.1 命令：以已 spawn 的 workload PID 精確篩選。每次使用獨立
/// ETW session；`--v1_metrics` 固定既有 MsBetweenPresents 統計語意。
///
/// 使用低負載追蹤：固定停用 `--no_track_gpu` 與 `--no_track_input`（統計只讀
/// v1_metrics 的 MsBetweenPresents，不需要 GPU/input 事件），以降低高 FPS 下
/// ETW events lost（circular buffer 溢位、導致 capture 無效）的機率。
/// `display` 追蹤必須保留：PresentMon 需要 display present 事件作為 frame 來源，
/// 加 `--no_track_display` 會使 CSV 完全無法建立。校準與正式 capture 共用此命令。
pub(crate) fn presentmon_command(
    sample_secs: u32,
    buffer: u32,
    pid: u32,
    csv: &Path,
    session_name: &str,
) -> Vec<String> {
    vec![
        "--session_name".to_string(),
        session_name.to_string(),
        "--stop_existing_session".to_string(),
        "--no_console_stats".to_string(),
        "--no_track_gpu".to_string(),
        "--no_track_input".to_string(),
        "--process_id".to_string(),
        pid.to_string(),
        "--output_file".to_string(),
        csv.to_string_lossy().to_string(),
        "--timed".to_string(),
        sample_secs.to_string(),
        "--terminate_after_timed".to_string(),
        "--v1_metrics".to_string(),
        "--set_circular_buffer_size".to_string(),
        buffer.to_string(),
    ]
}

/// 執行一次 PresentMon capture 並驗證輸出。成功 → Ok(())；失敗 → Err(穩定錯誤代碼)。
///
/// 保證：
/// 1. 先刪除目標 CSV（stale 檔絕不能被當成當前輸出）。
/// 2. 等待 PresentMon 在 `sample_secs + CAPTURE_WAIT_MARGIN_S` 內自行退出
///    （`-timed` + `-terminate_after_timed`）；逾時 = 卡住 → 穩定代碼失敗，不靜默繼續。
/// 3. 任何路徑都終止 PresentMon 與 workload（確定性清理），再由 terminal 統一 reaping。
/// 4. 退出後驗證 CSV 存在且含有效 frametime；缺檔/空檔各自有穩定代碼。
/// 5. 每個路徑都把本次 capture 的診斷資料寫進 session 目錄
///    （`diag/capture-round-<r>-lp-<lp>.json`），成功與失敗都保留。
#[allow(clippy::too_many_arguments)]
fn run_capture(
    ctx: &mut RunContext,
    round: u32,
    lp: u32,
    wl_pid: u32,
    csv: &Path,
    attempt: u32,
    fps_cap: u32,
    buffer: u32,
    sample_secs: u32,
    expected: Rect,
) -> Result<CapturedCsv, String> {
    let started_at = chrono::Local::now().to_rfc3339();
    let pm_session_name = format!("PaceDock-{}-{round}-{lp}-{attempt}", ctx.session_id);
    // PresentMon 啟動前確認 workload 是否還活著（第二次 capture 無 CSV 的關鍵判據）
    let wl_alive_before_pm = ctx.processes.is_alive(wl_pid);
    // 1) stale 輸出清除：刪除失敗 = 可能留下既有 shaped CSV 被當本次 capture → fail closed
    if let Err(e) = std::fs::remove_file(csv) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::error!(
                "capture 前清除舊 CSV 失敗 {}（{e}）——可能遭鎖定，拒絕 capture",
                csv.display()
            );
            return Err(codes::BENCHMARK_CAPTURE_MISSING.to_string());
        }
    }
    // 2) 啟動 PresentMon（以 -process_id 配合已 spawn 的 workload PID）
    let pm_pid = match ctx.processes.spawn(
        &ctx.assets.presentmon,
        &presentmon_command(sample_secs, buffer, wl_pid, csv, &pm_session_name),
    ) {
        Ok(pid) => {
            ctx.owned_processes.push(pid);
            pid
        }
        Err(e) => {
            log::warn!("PresentMon 啟動失敗: {e}");
            // PresentMon 沒起來也要留診斷（workload 存活、CSV 狀態）
            let (csv_exists, csv_size) = csv_meta(csv);
            let diag = CaptureDiagnostics {
                round,
                lp,
                attempt,
                started_at: started_at.clone(),
                finished_at: Some(chrono::Local::now().to_rfc3339()),
                workload_pid: wl_pid,
                workload_alive_before_pm: wl_alive_before_pm,
                workload_alive_after_capture: ctx.processes.is_alive(wl_pid),
                csv_path: csv.to_string_lossy().to_string(),
                csv_exists,
                csv_size_bytes: csv_size,
                capture_filter_kind: "process_id".to_string(),
                capture_filter_value: wl_pid.to_string(),
                presentmon_session_name: pm_session_name,
                error: Some(codes::BENCHMARK_PRESENTMON_FAILED.to_string()),
                ..Default::default()
            };
            // PresentMon 未啟動亦算一次 invalid attempt（無 overflow/ETW 可計）。
            ctx.capture_quality.total_captures += 1;
            ctx.capture_quality.invalid_captures += 1;
            persist_capture_diagnostics(csv, round, lp, &diag);
            return Err(codes::BENCHMARK_PRESENTMON_FAILED.to_string());
        }
    };
    // 3) 等待 PresentMon 自停（可中斷；期間輪詢 workload 視窗完整性）
    let integrity_probe = || {
        let snap = ctx.window.integrity(wl_pid, expected);
        // 失去前景 → 本次 benchmark 結束後主視窗置中還原（與 report_integrity 一致）。
        if !snap.foreground {
            ctx.window_control.request_center_restore();
        }
        integrity_ok(&snap)
    };
    let wait = wait_capture(
        ctx,
        pm_pid,
        (sample_secs as u64 + CAPTURE_WAIT_MARGIN_S) * 1000,
        &integrity_probe,
    );
    let (wait_completed, wait_timed_out, wait_error) = match &wait {
        CaptureWaitOutcome::Exited => (true, false, None),
        CaptureWaitOutcome::TimedOut => (true, true, None),
        CaptureWaitOutcome::Cancelled => (true, false, None),
        CaptureWaitOutcome::Failed(e) => (false, false, Some(e.clone())),
        CaptureWaitOutcome::IntegrityBroken => (false, false, None),
    };
    // 4) 終止前先取 exit code / output tail（kill 會 reap 並丟掉 pipe 內容）
    let pm_exit_code = if wait_completed && !wait_timed_out {
        ctx.processes.exit_code(pm_pid)
    } else {
        None
    };
    let pm_out = ctx.processes.output_tail(pm_pid, DIAG_OUTPUT_TAIL_CAP);
    let wl_alive_after_capture = ctx.processes.is_alive(wl_pid);
    let wl_exit_code = if wl_alive_after_capture {
        None
    } else {
        ctx.processes.exit_code(wl_pid)
    };
    let wl_out = ctx.processes.output_tail(wl_pid, DIAG_OUTPUT_TAIL_CAP);
    // 5) 任何路徑都先終止再判結果（避免漏殺）
    let _ = ctx.processes.kill(pm_pid);
    let _ = ctx.processes.kill(wl_pid);
    ctx.owned_processes.clear();
    let pm_stderr = pm_out
        .as_ref()
        .map(|o| o.stderr.clone())
        .unwrap_or_default();
    let overflowed = parse_overflowed_present_events(&pm_stderr).unwrap_or(0);
    let etw_lost_count = parse_etw_events_lost(&pm_stderr).unwrap_or(0);
    let etw_events_lost = stderr_has_etw_loss(&pm_stderr);
    let integ = assess_capture_integrity(csv, sample_secs, overflowed, etw_events_lost);
    let (result, rejection_reason) = match &wait {
        CaptureWaitOutcome::Failed(e) => {
            log::warn!("PresentMon wait 失敗: {e}");
            (
                Err(codes::BENCHMARK_PRESENTMON_FAILED.to_string()),
                Some("presentmon_failed".to_string()),
            )
        }
        CaptureWaitOutcome::TimedOut => {
            // PresentMon 卡住：不靜默繼續，回穩定代碼；已驗證的部分結果由呼叫端保留
            log::error!(
                "PresentMon 逾時未退出（sample={}s + margin={}s），round-{round}-lp-{lp} 失敗",
                sample_secs,
                CAPTURE_WAIT_MARGIN_S
            );
            (
                Err(codes::BENCHMARK_PRESENTMON_TIMEOUT.to_string()),
                Some("timeout".to_string()),
            )
        }
        CaptureWaitOutcome::Cancelled => {
            log::info!("capture round-{round}-lp-{lp} 收到取消，提前終止 PresentMon 與 workload");
            (Err("cancelled".to_string()), Some("cancelled".to_string()))
        }
        CaptureWaitOutcome::IntegrityBroken => {
            log::warn!(
                "capture round-{round}-lp-{lp} workload 視窗完整性破壞，該 capture 不進統計"
            );
            (
                Err(codes::BENCHMARK_WINDOW_INTEGRITY.to_string()),
                Some("window_integrity".to_string()),
            )
        }
        CaptureWaitOutcome::Exited => match &integ.code {
            Some(c) => {
                log::error!(
                    "capture round-{round}-lp-{lp} 完整性失敗（{:?}），overflowed={overflowed} etw_lost={etw_lost_count}",
                    integ.reason
                );
                (Err(c.clone()), integ.reason.clone())
            }
            None => match pm_exit_code {
                // PresentMon 非零結束 = capture 生產者未成功；即使 CSV 存在也不採信
                Some(code) if code != 0 => {
                    log::error!(
                        "capture round-{round}-lp-{lp} PresentMon 異常結束（exit={code}），
                         CSV 內容不予採信"
                    );
                    (
                        Err(codes::BENCHMARK_PRESENTMON_FAILED.to_string()),
                        Some(format!("presentmon_exit_{code}")),
                    )
                }
                _ => (Ok(()), None),
            },
        },
    };
    // 6) 記錄診斷（成功與失敗都寫）
    let (csv_exists, csv_size) = csv_meta(csv);
    let diag = CaptureDiagnostics {
        round,
        lp,
        attempt,
        started_at,
        finished_at: Some(chrono::Local::now().to_rfc3339()),
        workload_pid: wl_pid,
        workload_alive_before_pm: wl_alive_before_pm,
        workload_alive_after_capture: wl_alive_after_capture,
        workload_exit_code: wl_exit_code,
        workload_stdout: wl_out
            .as_ref()
            .map(|o| o.stdout.clone())
            .unwrap_or_default(),
        workload_stderr: wl_out
            .as_ref()
            .map(|o| o.stderr.clone())
            .unwrap_or_default(),
        presentmon_pid: pm_pid,
        presentmon_exit_code: pm_exit_code,
        wait_completed,
        wait_timed_out,
        wait_error,
        presentmon_stdout: pm_out
            .as_ref()
            .map(|o| o.stdout.clone())
            .unwrap_or_default(),
        presentmon_stderr: pm_out
            .as_ref()
            .map(|o| o.stderr.clone())
            .unwrap_or_default(),
        csv_path: csv.to_string_lossy().to_string(),
        csv_exists,
        csv_size_bytes: csv_size,
        capture_filter_kind: "process_id".to_string(),
        capture_filter_value: wl_pid.to_string(),
        presentmon_session_name: pm_session_name,
        etw_events_lost,
        overflowed_present_events: overflowed,
        etw_events_lost_count: etw_lost_count,
        capture_duration_secs: integ.duration_secs,
        valid: result.is_ok(),
        rejection_reason,
        effective_fps_cap: fps_cap,
        circular_buffer_size: buffer,
        error: result.as_ref().err().cloned(),
    };
    // 累計 capture 完整性（每次 attempt 都計，含校準/overflow retry/drift rerun）。
    ctx.capture_quality.total_captures += 1;
    if result.is_ok() {
        ctx.capture_quality.valid_captures += 1;
    } else {
        ctx.capture_quality.invalid_captures += 1;
    }
    if matches!(result.as_ref().err(), Some(e) if e == codes::BENCHMARK_WINDOW_INTEGRITY) {
        ctx.capture_quality.window_invalid_captures += 1;
    }
    ctx.capture_quality.overflowed_present_events += overflowed;
    ctx.capture_quality.etw_events_lost += etw_lost_count;
    persist_capture_diagnostics(csv, round, lp, &diag);
    // 成功的 capture 立即讀取/解析並記錄 digest — 之後任何讀取都綁定此刻內容
    match result {
        Ok(()) => CapturedCsv::capture(csv),
        Err(e) => Err(e),
    }
}

/// 目標 CSV 的存在與大小（診斷用）
fn csv_meta(csv: &Path) -> (bool, u64) {
    match std::fs::metadata(csv) {
        Ok(m) => (true, m.len()),
        Err(_) => (false, 0),
    }
}

/// 把 capture 診斷寫到 `session_dir/diag/capture-round-<r>-lp-<lp>.json`。
/// best-effort：寫失敗只 log，不影響 capture 結果。
fn persist_capture_diagnostics(csv: &Path, round: u32, lp: u32, diag: &CaptureDiagnostics) {
    let Some(session_dir) = csv.parent() else {
        log::warn!("capture 診斷路徑無法解析（csv={}）", csv.display());
        return;
    };
    let diag_dir = session_dir.join("diag");
    if let Err(e) = std::fs::create_dir_all(&diag_dir) {
        log::warn!("建立診斷目錄失敗 {}: {e}", diag_dir.display());
        return;
    }
    let attempt = diag.attempt.max(1);
    let path = if attempt <= 1 {
        diag_dir.join(format!("capture-round-{round}-lp-{lp}.json"))
    } else {
        diag_dir.join(format!(
            "capture-round-{round}-lp-{lp}-attempt-{attempt}.json"
        ))
    };
    match serde_json::to_string_pretty(diag) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                log::warn!("寫入 capture 診斷失敗 {}: {e}", path.display());
            }
        }
        Err(e) => log::warn!("序列化 capture 診斷失敗: {e}"),
    }
}

/// 偵測 PresentMon stderr 是否回報 ETW events lost（circular buffer 溢位 /
/// 擷取負載過高）。關鍵字不分大小寫；需含 "lost" 且帶 "event"/"etw" 脈絡，
/// 涵蓋 "123 ETW events lost"、"Lost 123 ETW events"、"ETW events were lost"
/// 等變體。排除否定/零值表述（"no/not/0/without … lost"、"lost 0 …"），
/// 避免 "0 ETW events lost"、"no events were lost" 誤判為溢失。
pub(crate) fn stderr_has_etw_loss(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    if !s.contains("lost") || !(s.contains("event") || s.contains("etw")) {
        return false;
    }
    // 逐子句判斷：否定標記只在該子句內生效，避免前一子句的 "no/0"（或 "error code 0"
    // 之類無關 "0"）跨子句抑制真正 loss。任一子句含非零、非否定的 lost 即回報。
    s.split(['.', ';', ':', ',', '!', '?', '\n', '\r'])
        .any(clause_has_etw_loss)
}

/// 單一子句內是否回報非零、非否定的 ETW events lost。
fn clause_has_etw_loss(clause: &str) -> bool {
    if !clause.contains("lost") || !(clause.contains("event") || clause.contains("etw")) {
        return false;
    }
    const NEG: [&str; 6] = ["0", "no", "not", "none", "zero", "without"];
    let tokens: Vec<&str> = clause
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    for (i, t) in tokens.iter().enumerate() {
        if *t != "lost" {
            continue;
        }
        // 往前最多看 4 個 token：否定前綴（"no/0 ETW events lost"）即排除此 "lost"
        let negated = tokens[..i].iter().rev().take(4).any(|w| NEG.contains(w));
        if negated {
            continue;
        }
        // 後方緊接 "0"（"lost 0 events"）也是零遺失，排除
        if matches!(tokens.get(i + 1), Some(next) if *next == "0") {
            continue;
        }
        return true;
    }
    false
}

/// 從子句擷取第一個非零整數（供 count 解析）。
fn first_nonzero_integer(clause: &str) -> Option<u64> {
    clause
        .split(|c: char| !c.is_ascii_digit())
        .find_map(|t| t.parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// 解析 PresentMon stderr 回報的 ETW events/buffers lost 數量。
/// 涵蓋實測措辭 "warning: %lu ETW events were lost."、"warning: %lu ETW buffers were lost."
/// 等變體；零/否定表述回 None。
pub fn parse_etw_events_lost(stderr: &str) -> Option<u64> {
    let s = stderr.to_ascii_lowercase();
    if !s.contains("lost") || !(s.contains("event") || s.contains("etw")) {
        return None;
    }
    s.split(['.', ';', ':', ',', '!', '?', '\n', '\r'])
        .find_map(|clause| {
            clause_has_etw_loss(clause)
                .then(|| first_nonzero_integer(clause))
                .flatten()
        })
}

/// 解析 PresentMon stderr 回報的 overflowed present events 數量。
/// 涵蓋實測措辭 "warning: %lu overflowed present events detected. ..."；零/否定回 None。
pub fn parse_overflowed_present_events(stderr: &str) -> Option<u64> {
    let s = stderr.to_ascii_lowercase();
    if !s.contains("overflow") || !s.contains("present") {
        return None;
    }
    s.split(['.', ';', ':', ',', '!', '?', '\n', '\r'])
        .find_map(|clause| {
            if !(clause.contains("overflow") && clause.contains("present")) {
                return None;
            }
            // 排除否定/零（"0 overflowed present events"）。
            const NEG: [&str; 4] = ["0", "no", "none", "zero"];
            let tokens: Vec<&str> = clause
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| !t.is_empty())
                .collect();
            if let Some(pos) = tokens.iter().position(|t| t.starts_with("overflow")) {
                let negated = tokens[..pos].iter().rev().take(4).any(|w| NEG.contains(w));
                if negated {
                    return None;
                }
            }
            first_nonzero_integer(clause)
        })
}

/// 完整性評估結果。
pub(crate) struct CaptureIntegrity {
    /// 穩定錯誤代碼（成功 = None）。
    pub(crate) code: Option<String>,
    /// 拒絕原因短標籤（"overflowed_present_events"/"etw_events_lost"/"missing"/
    /// "empty"/"duration"/"monotonic"；成功 = None）。
    reason: Option<String>,
    /// 觀測 capture 時長（秒，來自 CSV `TimeInSeconds`；無欄位 = None）。
    duration_secs: Option<f64>,
}

/// 最佳努力讀取 CSV 的觀測時長（秒）。任何錯誤 → None（不影響 capture 判定）。
fn csv_duration(csv: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(csv).ok()?;
    parse_presentmon_csv_full(&text)
        .ok()?
        .observed_duration_secs()
}

/// 完整性驗證：CSV 有列、finite positive frametime、單調 capture 時間（有欄位才判）、
/// 時長 ≥95% sample_secs（有秒數欄位才判）。回傳 `Ok(observed_duration_secs)` 或
/// `Err((穩定代碼, 原因))`。
fn validate_capture_integrity(
    csv: &Path,
    sample_secs: u32,
) -> Result<Option<f64>, (String, String)> {
    let text = match std::fs::read_to_string(csv) {
        Ok(t) => t,
        Err(_) => {
            log::warn!("capture 無輸出檔案: {}", csv.display());
            return Err((
                codes::BENCHMARK_CAPTURE_MISSING.to_string(),
                "missing".to_string(),
            ));
        }
    };
    let parsed = match parse_presentmon_csv_full(&text) {
        Ok(p) => p,
        Err(_) => {
            log::warn!("capture 無有效 frametime 資料: {}", csv.display());
            return Err((
                codes::BENCHMARK_CAPTURE_EMPTY.to_string(),
                "empty".to_string(),
            ));
        }
    };
    if !parsed.monotonic {
        return Err((
            codes::BENCHMARK_CSV_INVALID.to_string(),
            "monotonic".to_string(),
        ));
    }
    if let Some(dur) = parsed.observed_duration_secs() {
        if dur < sample_secs as f64 * CAPTURE_DURATION_MIN_RATIO {
            return Err((
                codes::BENCHMARK_CSV_INVALID.to_string(),
                "duration".to_string(),
            ));
        }
        Ok(Some(dur))
    } else {
        Ok(None)
    }
}

/// 綜合完整性評估：overflow/ETW lost 優先（capture 無效，與 CSV 內容無關），
/// 其次 CSV 完整性。純函式，供 capture 結果判定與診斷共用。
pub(crate) fn assess_capture_integrity(
    csv: &Path,
    sample_secs: u32,
    overflowed: u64,
    etw_loss_detected: bool,
) -> CaptureIntegrity {
    let duration_secs = csv_duration(csv);
    if overflowed > 0 {
        return CaptureIntegrity {
            code: Some(codes::BENCHMARK_CAPTURE_OVERFLOW.to_string()),
            reason: Some("overflowed_present_events".to_string()),
            duration_secs,
        };
    }
    if etw_loss_detected {
        return CaptureIntegrity {
            code: Some(codes::BENCHMARK_CAPTURE_ETW_LOST.to_string()),
            reason: Some("etw_events_lost".to_string()),
            duration_secs,
        };
    }
    match validate_capture_integrity(csv, sample_secs) {
        Ok(d) => CaptureIntegrity {
            code: None,
            reason: None,
            duration_secs: d,
        },
        Err((code, reason)) => CaptureIntegrity {
            code: Some(code),
            reason: Some(reason),
            duration_secs,
        },
    }
}

/// 預估剩餘秒數（per-test：sample + warmup + 啟動 5s + 重啟 ~14s + 緩衝）
fn eta_secs(config: &BenchmarkConfig, total_tests: u32, done: u32) -> Option<u64> {
    if total_tests == 0 {
        return None;
    }
    let per_test = config.sample_secs as u64 + config.warm_up_secs as u64 + 19;
    Some((total_tests.saturating_sub(done)) as u64 * per_test)
}

/// 合併各 round CSV 並計算每 LP 指標。
/// 任一 LP 的 CSV 缺失/空/無效 → 回傳 (已算出的部分結果, Some(錯誤碼))。
#[cfg(test)]
fn compute_session_results(round_csvs: &RoundCsvs) -> (Vec<LpResult>, Option<String>) {
    let mut lps: Vec<u32> = round_csvs.keys().copied().collect();
    lps.sort_unstable();
    let mut out = Vec::new();
    for lp in lps {
        match compute_lp_all_rounds(lp, &round_csvs[&lp]) {
            Ok(r) => out.push(r),
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

#[cfg(test)]
fn compute_lp_all_rounds(lp: u32, rounds: &HashMap<u32, CapturedCsv>) -> Result<LpResult, String> {
    let mut per_round: Vec<Vec<f64>> = Vec::new();
    let mut round_nums: Vec<u32> = rounds.keys().copied().collect();
    round_nums.sort_unstable();
    for round in round_nums {
        let csv = &rounds[&round];
        let frames = csv.read()?;
        per_round.push(frames);
    }
    let merged = merge_rounds(&per_round);
    compute_lp_result(lp, &merged).map_err(|e| {
        log::warn!("LP {lp} 統計失敗: {e}");
        codes::BENCHMARK_CSV_INVALID.to_string()
    })
}

/// 計算某 phase（round 編號 [start_round, end_round)）的逐 LP 聚合結果。
/// 只納入該範圍內實際存在的 round；無資料的 LP 略過。供 SessionDetail 的
/// screening/refinement/confirmation 分相結果（證據獨立保存）。
/// 顯示端指標（present→display 延遲、display-change 間隔、丟幀）一併聚合。
fn compute_phase_results(
    round_csvs: &RoundCsvs,
    start_round: u32,
    end_round: u32,
) -> Vec<LpResult> {
    let mut lps: Vec<u32> = round_csvs.keys().copied().collect();
    lps.sort_unstable();
    let mut out = Vec::new();
    for lp in lps {
        let rounds = &round_csvs[&lp];
        let mut round_nums: Vec<u32> = rounds
            .keys()
            .copied()
            .filter(|&r| r >= start_round && r < end_round)
            .collect();
        round_nums.sort_unstable();
        let mut per_round: Vec<Vec<f64>> = Vec::new();
        let mut per_round_display: Vec<DisplaySeries> = Vec::new();
        for round in round_nums {
            if let Ok(series) = rounds[&round].read_full() {
                per_round.push(series.frames);
                per_round_display.push(series.display);
            }
        }
        if per_round.is_empty() {
            continue;
        }
        let merged = merge_rounds(&per_round);
        if let Ok(mut r) = compute_lp_result(lp, &merged) {
            attach_display_metrics(&mut r, &merge_display_rounds(&per_round_display));
            out.push(r);
        }
    }
    out
}

/// 已驗證的 capture 產物：路徑 + capture 完成當下的內容 digest。
/// capture CSV 位於 same-user 可寫的 APPDATA，驗證後仍可能被置換；
/// 下游每次讀取都重新驗 digest，不一致即 fail closed。
#[derive(Clone, Debug)]
struct CapturedCsv {
    path: PathBuf,
    sha256: String,
}

impl CapturedCsv {
    /// capture 完成當下讀取 + 解析 + 記錄 digest（不可讀/不可解析即 Err）
    fn capture(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            log::warn!("capture CSV 讀取失敗 {}: {e}", path.display());
            codes::BENCHMARK_CSV_INVALID.to_string()
        })?;
        parse_presentmon_csv(&text).map_err(|e| {
            log::warn!("capture CSV 解析失敗 {}: {e}", path.display());
            codes::BENCHMARK_CSV_INVALID.to_string()
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            sha256: sha256_hex(text.as_bytes()),
        })
    }

    /// 下游讀取：重新讀檔並驗證 digest 與 capture 當下一致
    fn read(&self) -> Result<Vec<f64>, String> {
        self.read_text().and_then(|text| {
            parse_presentmon_csv(&text).map_err(|e| {
                log::warn!("CSV 解析失敗 {}: {e}", self.path.display());
                codes::BENCHMARK_CSV_INVALID.to_string()
            })
        })
    }

    /// 下游讀取（完整序列）：驗 digest 後回傳 frametime + 顯示端序列
    /// （present→display 延遲、display-change 間隔、丟幀）。
    fn read_full(&self) -> Result<FullSeries, String> {
        self.read_text().and_then(|text| {
            parse_presentmon_series(&text).map_err(|e| {
                log::warn!("CSV 解析失敗 {}: {e}", self.path.display());
                codes::BENCHMARK_CSV_INVALID.to_string()
            })
        })
    }

    /// 重新讀檔並驗證 digest 與 capture 當下一致（共用檢查）。
    fn read_text(&self) -> Result<String, String> {
        let text = std::fs::read_to_string(&self.path).map_err(|e| {
            log::warn!("CSV 讀取失敗 {}: {e}", self.path.display());
            codes::BENCHMARK_CSV_INVALID.to_string()
        })?;
        if sha256_hex(text.as_bytes()) != self.sha256 {
            log::error!(
                "capture CSV 內容與 capture 完成時不符（可能遭置換）: {}",
                self.path.display()
            );
            return Err(codes::BENCHMARK_CSV_INVALID.to_string());
        }
        Ok(text)
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// LP → round → 已驗證 capture 產物
type RoundCsvs = HashMap<u32, HashMap<u32, CapturedCsv>>;

/// 單一 (round, lp) 的 LpResult（供逐 round 勝者計算；CSV 不可讀 → None）
#[cfg(test)]
fn compute_lp_single_round(lp: u32, csv: &CapturedCsv) -> Option<LpResult> {
    csv.read()
        .ok()
        .and_then(|frames| compute_lp_result(lp, &frames).ok())
}

/// 改善百分比：`(candidate - runner_up) / runner_up * 100`。
/// 亞軍不可得、非有限、或 `runner_up <= 0` → None（視為未達門檻）。
#[cfg(test)]
fn improvement_pct(candidate: Option<f64>, runner_up: Option<f64>) -> Option<f64> {
    let c = candidate?;
    let r = runner_up?;
    if !c.is_finite() || !r.is_finite() || r <= 0.0 {
        return None;
    }
    let pct = (c - r) / r * 100.0;
    if pct.is_finite() {
        Some(pct)
    } else {
        None
    }
}

/// 複合分數優勢門檻（%）：穩健候選的跨 round 中位分數須高於亞軍至少此值才算 Passed。
#[cfg(test)]
pub const COMPOSITE_ADVANTAGE_MIN_PCT: f64 = 0.5;
/// 護欄：候選 Avg FPS / 1% low 相較亞軍最多允許落後（%，負值 = 允許小幅落後）。
#[cfg(test)]
pub const GUARDRAIL_MAX_DEFICIT_PCT: f64 = -0.5;
/// 護欄：候選 spike rate 相較亞軍最多允許超出（絕對百分點）。
#[cfg(test)]
pub const SPIKE_GUARD_PP: f64 = 0.5;

/// 由 per-round LpResult map 取某指標的逐 round 中位數（缺/非有限 → 略過）。
#[cfg(test)]
fn median_of_metric(
    per_round: &HashMap<u32, LpResult>,
    pick: fn(&LpResult) -> Option<f64>,
) -> Option<f64> {
    let vals: Vec<f64> = per_round.values().filter_map(pick).collect();
    if vals.is_empty() {
        None
    } else {
        Some(median(&vals))
    }
}

/// 逐 LP、逐 round 的 competitive-eligible 單 round 結果（僅納入 rounds 0..round_count）。
#[cfg(test)]
fn build_per_round(
    round_csvs: &RoundCsvs,
    round_count: u32,
) -> HashMap<u32, HashMap<u32, LpResult>> {
    let mut per_round: HashMap<u32, HashMap<u32, LpResult>> = HashMap::new();
    for (&lp, rounds) in round_csvs {
        for round in 0..round_count {
            if let Some(csv) = rounds.get(&round) {
                if let Some(r) = compute_lp_single_round(lp, csv) {
                    if is_competitive_eligible(&r) {
                        per_round.entry(lp).or_default().insert(round, r);
                    }
                }
            }
        }
    }
    per_round
}

/// 由 rounds 0..round_count 的完整（competitive-eligible）分數選出前 k 名候選。
/// 僅納入該 round 數皆完整的 LP；少於 k 個完整候選 → 回傳空（呼叫端跳過後續階段）。
#[cfg(test)]
fn select_top_candidates(round_csvs: &RoundCsvs, round_count: u32, k: usize) -> Vec<u32> {
    let per_round = build_per_round(round_csvs, round_count);
    let mut eligible: Vec<u32> = per_round
        .iter()
        .filter(|(_, rounds)| rounds.len() as u32 == round_count)
        .map(|(lp, _)| *lp)
        .collect();
    eligible.sort_unstable();
    if eligible.len() < k {
        return Vec::new();
    }
    let mut per_lp_scores: HashMap<u32, Vec<f64>> = HashMap::new();
    for round in 0..round_count {
        let round_results: Vec<LpResult> = eligible
            .iter()
            .filter_map(|lp| per_round.get(lp).and_then(|m| m.get(&round)).cloned())
            .collect();
        let med = round_medians(&round_results);
        for lp in &eligible {
            if let Some(r) = per_round.get(lp).and_then(|m| m.get(&round)) {
                if let Some(s) = competitive_score(r, &med) {
                    per_lp_scores.entry(*lp).or_default().push(s);
                }
            }
        }
    }
    let ranked = robust_candidates(
        &per_lp_scores
            .iter()
            .map(|(lp, s)| (*lp, s.clone()))
            .collect::<Vec<_>>(),
    );
    ranked.into_iter().take(k).map(|c| c.lp).collect()
}

/// 確認階段逐 round 的 (candidate, runner) 單 round 結果（round 編號由 `base_round`
/// 起算，前向與反向驗證各自獨立 namespace）。任一 round 缺 CSV/不可算 → None。
#[cfg(test)]
fn confirmation_pairs(
    round_csvs: &RoundCsvs,
    candidate: u32,
    runner: u32,
    confirmation_rounds: u32,
    base_round: u32,
) -> Option<Vec<(LpResult, LpResult)>> {
    let mut out = Vec::with_capacity(confirmation_rounds as usize);
    for round in base_round..(base_round + confirmation_rounds) {
        let c = round_csvs
            .get(&candidate)
            .and_then(|m| m.get(&round))
            .and_then(|csv| compute_lp_single_round(candidate, csv))?;
        let r = round_csvs
            .get(&runner)
            .and_then(|m| m.get(&round))
            .and_then(|csv| compute_lp_single_round(runner, csv))?;
        out.push((c, r));
    }
    Some(out)
}

/// 逐確認 round 的配對效應：候選相較亞軍的「有界 log-ratio 確認複合分數」。
/// 任一 round 缺完整（非有限 / ≤0）指標 → None（fail closed，非中性分）。
#[cfg(test)]
fn confirmation_effects(pairs: &[(LpResult, LpResult)]) -> Option<Vec<f64>> {
    pairs
        .iter()
        .map(|(c, r)| confirmation_effect(c, r))
        .collect()
}

/// 確定性、無相依的配對 bootstrap 穩定性區間（bootstrap stability interval）。
/// 對 K 個配對效應值，窮舉所有 K^K 個「放回抽樣」組合的平均（等同完整 bootstrap
/// 分布），取第 5 與第 95 百分位為區間下/上界。無隨機種子、無外部相依；
/// K ≤ 7 時組合數 ≤ 7^7（823543）。回傳 (點估計 = 樣本平均, 下界, 上界)。
///
/// 這**不是**統計信賴區間、也不宣稱 90% 覆蓋率：n=3..=7 太小，無法支持任何
/// 形式化顯著性宣稱。它是小型樣本的決策啟發式，只提供「超越 / 可忽略 / 未定」
/// 三分類用的穩定性量度。
#[cfg(test)]
pub fn paired_bootstrap_interval(effects: &[f64]) -> (f64, f64, f64) {
    let k = effects.len();
    assert!(k > 0, "paired_bootstrap_interval 需要至少一個效應值");
    let total = k.pow(k as u32);
    let mut means = Vec::with_capacity(total);
    let mut idx = vec![0usize; k];
    loop {
        let sum: f64 = idx.iter().map(|&i| effects[i]).sum();
        means.push(sum / k as f64);
        let mut pos = 0;
        while pos < k {
            idx[pos] += 1;
            if idx[pos] < k {
                break;
            }
            idx[pos] = 0;
            pos += 1;
        }
        if pos == k {
            break;
        }
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = means.len();
    let point = effects.iter().sum::<f64>() / k as f64;
    let lo = means[((INTERVAL_LOW_PERCENTILE * (n as f64 - 1.0)).floor() as usize).min(n - 1)];
    let hi = means[((INTERVAL_HIGH_PERCENTILE * (n as f64 - 1.0)).floor() as usize).min(n - 1)];
    (point, lo, hi)
}

/// 確認階段的逐 round 護欄（候選 vs 亞軍，跨確認 round 中位數）。
/// 回傳 (Avg FPS 優勢 %, 1% low 優勢 %, spike rate 差 pp)。
#[cfg(test)]
fn confirmation_guardrails(
    pairs: &[(LpResult, LpResult)],
    base_round: u32,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mut candidate_map: HashMap<u32, LpResult> = HashMap::new();
    let mut runner_map: HashMap<u32, LpResult> = HashMap::new();
    for (i, (c, r)) in pairs.iter().enumerate() {
        let round = base_round + i as u32;
        candidate_map.insert(round, c.clone());
        runner_map.insert(round, r.clone());
    }
    let candidate_avg = median_of_metric(&candidate_map, |r| r.avg_fps);
    let runner_avg = median_of_metric(&runner_map, |r| r.avg_fps);
    let candidate_p1 = median_of_metric(&candidate_map, |r| r.p1_low);
    let runner_p1 = median_of_metric(&runner_map, |r| r.p1_low);
    let candidate_spike = median_of_metric(&candidate_map, |r| r.spike_rate_pct);
    let runner_spike = median_of_metric(&runner_map, |r| r.spike_rate_pct);
    (
        improvement_pct(candidate_avg, runner_avg),
        improvement_pct(candidate_p1, runner_p1),
        match (candidate_spike, runner_spike) {
            (Some(c), Some(r)) => Some(c - r),
            _ => None,
        },
    )
}

/// 護欄是否通過（候選不得在 Avg FPS / 1% low 明顯倒退，spike 不得明顯變差）。
#[cfg(test)]
fn guardrails_ok(avg_adv: Option<f64>, p1_adv: Option<f64>, spike_delta: Option<f64>) -> bool {
    avg_adv.is_some_and(|v| v >= GUARDRAIL_MAX_DEFICIT_PCT)
        && p1_adv.is_some_and(|v| v >= GUARDRAIL_MAX_DEFICIT_PCT)
        && spike_delta.is_some_and(|d| d <= SPIKE_GUARD_PP)
}

/// 前向確認的判定訊號。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
enum ForwardVerdict {
    /// 證據尚未達決定性，繼續下一確認 round。
    Continue,
    /// 候選決定性勝出（通過一致性 + 區間下界 + 護欄）。
    CandidatePassed,
    /// 亞軍以相同（反相）criteria 決定性勝出 → 觸發反向驗證。
    RunnerUpReversal,
    /// 實質等效（可忽略差異）。
    Equivalent,
}

/// 保守一致性規則（決定 Passed；適用於最多 7 個配對效應樣本）。
///
/// 逐 round 配對複合效應的一致性要求：
/// - K=3：三個效應**全部** > [`COMPOSITE_ADVANTAGE_MIN_PCT`]（同向為正）。
/// - K=4：四個效應**全部** > 門檻。
/// - K=5：**至少 4/5**、K=6：**至少 5/6**、K=7：**至少 6/7** 個效應 > 門檻。
///
/// 任何 K 都另需 bootstrap 穩定性區間下界 > 門檻，且護欄未倒退。
/// 這是小型樣本的決策啟發式（decision heuristic），**不**宣稱形式化顯著性；
/// 區間下界只是必要條件之一，不能單獨觸發 Passed。
#[cfg(test)]
fn confirmation_passed(effects: &[f64], interval_lower: f64, rails_ok: bool) -> bool {
    if !rails_ok {
        return false;
    }
    let k = effects.len();
    let above = effects
        .iter()
        .filter(|&&e| e > COMPOSITE_ADVANTAGE_MIN_PCT)
        .count();
    let consistent = match k {
        3 => above == 3,
        4 => above == 4,
        5 => above >= 4,
        6 => above >= 5,
        7 => above >= 6,
        _ => above >= k.saturating_sub(1),
    };
    consistent && interval_lower > COMPOSITE_ADVANTAGE_MIN_PCT
}

/// 單一 (a vs b) 配對的完整證據：(effects, avg_adv, p1_adv, spike_delta)。
#[cfg(test)]
type PairEvidence = (Vec<f64>, Option<f64>, Option<f64>, Option<f64>);

/// 任一 round 缺完整 competitive-eligible 分數 → None。
#[cfg(test)]
fn pair_evidence(
    round_csvs: &RoundCsvs,
    a: u32,
    b: u32,
    confirmation_rounds: u32,
    base_round: u32,
) -> Option<PairEvidence> {
    let pairs = confirmation_pairs(round_csvs, a, b, confirmation_rounds, base_round)?;
    let effects = confirmation_effects(&pairs)?;
    let (avg, p1, spike) = confirmation_guardrails(&pairs, base_round);
    Some((effects, avg, p1, spike))
}

/// 等效判定所需的 raw median evidence（% 或 pp）。由逐 round raw metrics 的中位數
/// 差異計算（**不**使用 screening/refinement 的 competitive_score，也不使用 bootstrap）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg(test)]
struct EquivalentEvidence {
    avg_improvement_pct: Option<f64>,
    p1_improvement_pct: Option<f64>,
    p01_improvement_pct: Option<f64>,
    mad_delta_pp: Option<f64>,
    spike_delta_pp: Option<f64>,
}

/// 由 (candidate, runner) 逐 round raw metrics 計算某指標的兩側中位數。
/// 任一側缺值 → (None, None)。
#[cfg(test)]
fn pair_metric_medians(
    pairs: &[(LpResult, LpResult)],
    pick: fn(&LpResult) -> Option<f64>,
) -> (Option<f64>, Option<f64>) {
    let c: Vec<f64> = pairs.iter().filter_map(|(c, _)| pick(c)).collect();
    let r: Vec<f64> = pairs.iter().filter_map(|(_, r)| pick(r)).collect();
    if c.is_empty() || r.is_empty() {
        (None, None)
    } else {
        (Some(median(&c)), Some(median(&r)))
    }
}

/// 絕對百分點差（兩側皆有限才有值）。
#[cfg(test)]
fn delta_pp(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Some(x - y),
        _ => None,
    }
}

/// 計算等效判定的 raw median evidence。
#[cfg(test)]
fn equivalent_evidence(pairs: &[(LpResult, LpResult)]) -> EquivalentEvidence {
    let (c_avg, r_avg) = pair_metric_medians(pairs, |r| r.avg_fps);
    let (c_p1, r_p1) = pair_metric_medians(pairs, |r| r.p1_low);
    let (c_p01, r_p01) = pair_metric_medians(pairs, |r| r.p01_low);
    let (c_mad, r_mad) = pair_metric_medians(pairs, |r| r.frametime_mad_pct);
    let (c_spike, r_spike) = pair_metric_medians(pairs, |r| r.spike_rate_pct);
    EquivalentEvidence {
        avg_improvement_pct: improvement_pct(c_avg, r_avg),
        p1_improvement_pct: improvement_pct(c_p1, r_p1),
        p01_improvement_pct: improvement_pct(c_p01, r_p01),
        mad_delta_pp: delta_pp(c_mad, r_mad),
        spike_delta_pp: delta_pp(c_spike, r_spike),
    }
}

/// 等效判定的中位數差異門檻：avg ≤0.5%、p1 ≤1.5%、p01 ≤2.0%、
/// MAD ≤0.5pp、spike ≤0.10pp（全部取絕對值）。
#[cfg(test)]
fn equivalent_medians_ok(ev: &EquivalentEvidence) -> bool {
    ev.avg_improvement_pct.is_some_and(|v| v.abs() <= 0.5)
        && ev.p1_improvement_pct.is_some_and(|v| v.abs() <= 1.5)
        && ev.p01_improvement_pct.is_some_and(|v| v.abs() <= 2.0)
        && ev.mad_delta_pp.is_some_and(|v| v.abs() <= 0.5)
        && ev.spike_delta_pp.is_some_and(|v| v.abs() <= 0.10)
}

/// 相對差（任一方向）是否超過 threshold_pct。
#[cfg(test)]
fn relative_diverges_pct(a: Option<f64>, b: Option<f64>, threshold_pct: f64) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => {
            improvement_pct(Some(x), Some(y)).is_some_and(|v| v.abs() > threshold_pct)
                || improvement_pct(Some(y), Some(x)).is_some_and(|v| v.abs() > threshold_pct)
        }
        _ => false,
    }
}

/// spike rate 絕對差（pp）是否超過 threshold_pp。
#[cfg(test)]
fn spike_diverges_pp(a: Option<f64>, b: Option<f64>, threshold_pp: f64) -> bool {
    match (a, b) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => (x - y).abs() > threshold_pp,
        _ => false,
    }
}

/// 單輪嚴重退步禁制：任何單輪（任一方向）出現 avg 絕對差 >3%、p1 絕對差 >5%、
/// 或 spike 絕對差 >0.5pp → 禁止 Equivalent。
#[cfg(test)]
fn equivalent_per_round_forbidden(pairs: &[(LpResult, LpResult)]) -> bool {
    pairs.iter().any(|(c, r)| {
        relative_diverges_pct(c.avg_fps, r.avg_fps, 3.0)
            || relative_diverges_pct(c.p1_low, r.p1_low, 5.0)
            || spike_diverges_pp(c.spike_rate_pct, r.spike_rate_pct, 0.5)
    })
}

/// 單向退步禁制（safety validation 專用）：任一單輪 `selected` 相對 `reference`
/// 出現 avg 改善 < -3%、p1 改善 < -5%、或 spike 增加 > 0.5pp → true（明顯退步，拒絕）。
/// 與 [`equivalent_per_round_forbidden`]（前向 Equivalent 的雙向 abs）分開：
/// 這裡只拒絕「更差」方向，單輪明顯改善不誤拒。
#[cfg(test)]
fn equivalent_validation_regressed(pairs: &[(LpResult, LpResult)]) -> bool {
    pairs.iter().any(|(selected, reference)| {
        let avg_worse =
            improvement_pct(selected.avg_fps, reference.avg_fps).is_some_and(|v| v < -3.0);
        let p1_worse = improvement_pct(selected.p1_low, reference.p1_low).is_some_and(|v| v < -5.0);
        let spike_worse =
            delta_pp(selected.spike_rate_pct, reference.spike_rate_pct).is_some_and(|d| d > 0.5);
        avg_worse || p1_worse || spike_worse
    })
}

/// 是否判定為 Equivalent：中位數差異全落在門檻內，且無任何單輪嚴重退步。
#[cfg(test)]
fn equivalent_finalists(pairs: &[(LpResult, LpResult)]) -> bool {
    equivalent_medians_ok(&equivalent_evidence(pairs)) && !equivalent_per_round_forbidden(pairs)
}

/// 等效安全驗證的中位數門檻（單側：selected 不得比 reference 明顯更差）。
/// avg ≥ −0.5%、p1 ≥ −1%、p01 ≥ −2%、MAD 增加 ≤0.5pp、spike 增加 ≤0.10pp。
#[cfg(test)]
fn equivalent_validation_medians_ok(ev: &EquivalentEvidence) -> bool {
    ev.avg_improvement_pct.is_some_and(|v| v >= -0.5)
        && ev.p1_improvement_pct.is_some_and(|v| v >= -1.0)
        && ev.p01_improvement_pct.is_some_and(|v| v >= -2.0)
        && ev.mad_delta_pp.is_some_and(|v| v <= 0.5)
        && ev.spike_delta_pp.is_some_and(|v| v <= 0.10)
}

/// Equivalent 在完成至少 [`EQUIVALENT_MIN_ROUNDS`] 輪（K=5、6、7）且雙向 decisive win
/// 皆未通過時才評估；K<5 只做 decisive win 判定。
#[cfg(test)]
fn equivalent_eligible(rounds: u32) -> bool {
    rounds >= EQUIVALENT_MIN_ROUNDS
}

/// 反向驗證 round 上限 = min([`REVERSE_MAX_ROUNDS`], [`TOTAL_PAIR_BUDGET`] − forward_rounds)。
/// 前向 ≤7 → 至少剩 3 個 pair；總 pair（前向 + 反向）≤ 10。
#[cfg(test)]
fn reverse_max_rounds(forward_rounds: u32) -> u32 {
    REVERSE_MAX_ROUNDS.min(TOTAL_PAIR_BUDGET.saturating_sub(forward_rounds))
}

/// 前向確認的判定（適用於前向與反向驗證，差在 base_round namespace）：
/// - CandidatePassed：候選通過一致性 + 區間下界 + 護欄（decisive win）。
/// - RunnerUpReversal：亞軍以「相同、反相」criteria 決定性勝出（不直接驗證亞軍）。
/// - Equivalent：至少 5 輪且雙方皆未 decisive win，且 raw median 差異落在等效門檻內。
/// - 其餘 → Continue（續跑至上限後 Inconclusive）。
#[cfg(test)]
fn evaluate_forward(
    round_csvs: &RoundCsvs,
    candidate: u32,
    runner: u32,
    confirmation_rounds: u32,
    base_round: u32,
) -> ForwardVerdict {
    let Some((effects, avg, p1, spike)) = pair_evidence(
        round_csvs,
        candidate,
        runner,
        confirmation_rounds,
        base_round,
    ) else {
        return ForwardVerdict::Continue;
    };
    let (_, lo, _) = paired_bootstrap_interval(&effects);
    let rails_ok = guardrails_ok(avg, p1, spike);
    if confirmation_passed(&effects, lo, rails_ok) {
        return ForwardVerdict::CandidatePassed;
    }
    if let Some((inv_effects, inv_avg, inv_p1, inv_spike)) = pair_evidence(
        round_csvs,
        runner,
        candidate,
        confirmation_rounds,
        base_round,
    ) {
        let (_, inv_lo, _) = paired_bootstrap_interval(&inv_effects);
        let inv_rails_ok = guardrails_ok(inv_avg, inv_p1, inv_spike);
        if confirmation_passed(&inv_effects, inv_lo, inv_rails_ok) {
            return ForwardVerdict::RunnerUpReversal;
        }
    }
    if equivalent_eligible(confirmation_rounds) {
        if let Some(pairs) = confirmation_pairs(
            round_csvs,
            candidate,
            runner,
            confirmation_rounds,
            base_round,
        ) {
            if equivalent_finalists(&pairs) {
                return ForwardVerdict::Equivalent;
            }
        }
    }
    ForwardVerdict::Continue
}

/// 計算可靠性摘要：只使用確認階段的獨立配對測量（篩選/refinement 資料絕不混入推論）。
/// `results` 為聚合結果（僅供舊版改善欄位）；`finalists` 為 [候選, 亞軍]；
/// `confirmation_rounds` 為前向確認完成的 round 數（3..=7）。
/// `forward_verdict`/reverse 參數記錄前向與反向 phase 的判定與證據（不重用資料）。
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn compute_reliability(
    round_csvs: &RoundCsvs,
    results: &[LpResult],
    finalists: &[u32],
    confirmation_rounds: u32,
    forward_verdict: Option<ForwardVerdict>,
    reverse_ran: bool,
    reverse_passed: bool,
    reverse_rounds: u32,
) -> ReliabilitySummary {
    let forward_str = forward_verdict_str(forward_verdict);
    let reverse_str = if !reverse_ran {
        String::new()
    } else if reverse_passed {
        "passed".to_string()
    } else {
        "inconclusive".to_string()
    };

    // 少於兩個 finalists（如 N=1）→ Inconclusive，無推薦。
    if finalists.len() < 2 {
        return ReliabilitySummary {
            status: ReliabilityStatus::Inconclusive,
            screening_rounds: SCREENING_ROUNDS,
            confirmation_rounds,
            forward_verdict: forward_str,
            reverse_ran,
            reverse_verdict: reverse_str,
            reverse_candidate_lp: None,
            reverse_rounds,
            stopping_reason: "inconclusive".to_string(),
            algorithm_version: 2,
            ..Default::default()
        };
    }
    let candidate = finalists[0];
    let runner = finalists[1];

    let pairs = confirmation_pairs(
        round_csvs,
        candidate,
        runner,
        confirmation_rounds,
        CONFIRMATION_ROUND_BASE,
    );
    let effects = pairs.as_ref().and_then(|p| confirmation_effects(p));
    // 等效判定的 raw median evidence（逐 round raw metrics 中位數差，供 Equivalent 稽核）。
    let equiv = pairs.as_ref().map(|p| equivalent_evidence(p));
    let (avg_adv, p1_adv, spike_delta) = match pairs.as_ref() {
        Some(p) => confirmation_guardrails(p, CONFIRMATION_ROUND_BASE),
        None => (None, None, None),
    };

    // 逐前向確認 round 勝者（僅供顯示；推論不依賴勝場門檻）。
    let mut round_winners: Vec<Option<u32>> = Vec::with_capacity(confirmation_rounds as usize);
    let mut candidate_wins = 0u32;
    if let Some(p) = &pairs {
        for (c, r) in p {
            let med = round_medians(&[c.clone(), r.clone()]);
            match (competitive_score(c, &med), competitive_score(r, &med)) {
                (Some(a), Some(b)) => {
                    let w = if a > b {
                        candidate
                    } else if b > a {
                        runner
                    } else {
                        candidate.min(runner)
                    };
                    if w == candidate {
                        candidate_wins += 1;
                    }
                    round_winners.push(Some(w));
                }
                _ => round_winners.push(None),
            }
        }
    }

    // 效應點估計 + bootstrap 穩定性區間（非信賴區間）。
    let (effect_estimate, interval_bounds) = match &effects {
        Some(e) => {
            let (point, lo, hi) = paired_bootstrap_interval(e);
            (Some(point), Some((lo, hi)))
        }
        None => (None, None),
    };

    // 舊版改善欄位（聚合結果）。
    let candidate_res = results.iter().find(|res| res.lp == candidate);
    let runner_up_res = results.iter().find(|res| res.lp == runner);
    let avg_fps_pct = improvement_pct(
        candidate_res.and_then(|r| r.avg_fps),
        runner_up_res.and_then(|r| r.avg_fps),
    );
    let p1_low_pct = improvement_pct(
        candidate_res.and_then(|r| r.p1_low),
        runner_up_res.and_then(|r| r.p1_low),
    );
    let p01_low_pct = improvement_pct(
        candidate_res.and_then(|r| r.p01_low),
        runner_up_res.and_then(|r| r.p01_low),
    );

    // 最終狀態：由前向判定 + 反向驗證決定（僅 reverse Passed 與 forward CandidatePassed
    // 產生 Passed → 套用閘開放）。
    let (status, stopping_reason) = match forward_verdict {
        Some(ForwardVerdict::CandidatePassed) => (ReliabilityStatus::Passed, "passed".to_string()),
        Some(ForwardVerdict::RunnerUpReversal) if reverse_passed => {
            (ReliabilityStatus::Passed, "reverse_passed".to_string())
        }
        Some(ForwardVerdict::Equivalent) => {
            (ReliabilityStatus::Equivalent, "equivalent".to_string())
        }
        _ => (ReliabilityStatus::Inconclusive, "inconclusive".to_string()),
    };

    ReliabilitySummary {
        status,
        per_round_winners: round_winners,
        candidate_lp: Some(candidate),
        runner_up_lp: Some(runner),
        candidate_wins,
        avg_fps_pct,
        p1_low_pct,
        p01_low_pct,
        evaluated_rounds: confirmation_rounds,
        required_wins: 0,
        composite_advantage_pct: effect_estimate,
        avg_fps_advantage_pct: avg_adv,
        p1_low_advantage_pct: p1_adv,
        spike_rate_delta_pp: spike_delta,
        screening_rounds: SCREENING_ROUNDS,
        confirmation_rounds,
        ci_lower_pct: interval_bounds.map(|(lo, _)| lo),
        ci_upper_pct: interval_bounds.map(|(_, hi)| hi),
        stopping_reason,
        forward_verdict: forward_str,
        reverse_ran,
        reverse_verdict: reverse_str,
        reverse_candidate_lp: reverse_ran.then_some(runner),
        reverse_rounds,
        algorithm_version: 2,
        equivalent_avg_improvement_pct: equiv.as_ref().and_then(|e| e.avg_improvement_pct),
        equivalent_p1_improvement_pct: equiv.as_ref().and_then(|e| e.p1_improvement_pct),
        equivalent_p01_improvement_pct: equiv.as_ref().and_then(|e| e.p01_improvement_pct),
        equivalent_mad_delta_pp: equiv.as_ref().and_then(|e| e.mad_delta_pp),
        equivalent_spike_delta_pp: equiv.as_ref().and_then(|e| e.spike_delta_pp),
    }
}

/// ForwardVerdict → 字串（ReliabilitySummary.forward_verdict）。
#[cfg(test)]
fn forward_verdict_str(v: Option<ForwardVerdict>) -> String {
    match v {
        Some(ForwardVerdict::CandidatePassed) => "passed".to_string(),
        Some(ForwardVerdict::RunnerUpReversal) => "reversal".to_string(),
        Some(ForwardVerdict::Equivalent) => "equivalent".to_string(),
        Some(ForwardVerdict::Continue) | None => "inconclusive".to_string(),
    }
}

/// 前置失敗（尚未寫入任何策略）：直接以 Failed 收尾，不需還原
fn abort(ctx: &mut RunContext, error: String) -> RunResult {
    let detail = SessionDetail {
        summary: SessionSummary {
            id: ctx.session_id.clone(),
            status: SessionStatus::Failed,
            started_at: chrono::Local::now().to_rfc3339(),
            finished_at: Some(chrono::Local::now().to_rfc3339()),
            gpu_name: String::new(),
            gpu_instance_id: ctx.config.gpu_instance_id.clone().unwrap_or_default(),
            cpu_fingerprint: cpu_fingerprint_with(&ctx.topo, &ctx.cpu_identity),
            best_lp: None,
            reliability: ReliabilitySummary::default(),
            severe_lps: Vec::new(),
            sample_count: 0,
            total_bytes: 0,
            config: ctx.config.clone(),
            error: Some(error.clone()),
            ..Default::default()
        },
        results: Vec::new(),
        samples: Vec::new(),
        ..Default::default()
    };
    let _ = storage::save_session_at(&ctx.storage_root, &detail);
    emit(
        ctx,
        &detail,
        "finalizing",
        None,
        None,
        100,
        None,
        Some(error.clone()),
    );
    RunResult {
        status: SessionStatus::Failed,
        detail,
        error: Some(error),
        best_lp: None,
        severe_lps: Vec::new(),
        recommended_cores: Vec::new(),
        recovery_required: false,
    }
}

/// 終結路徑（成功/取消/失敗共用）：
/// 停止 owned 子程序 → 還原原始策略並重啟 GPU → 驗證成功才清日誌 → 原子寫入。
/// 終結清理（run_benchmark 與等效安全驗證共用）：
/// 停止 owned 子程序 → 還原 baseline → 還原成功才清日誌。回傳是否還原成功。
fn cleanup_run(ctx: &mut RunContext) -> bool {
    // 取消清理階段：僅在 cancel signal 為 true 時依序回報，讓前端顯示可見的
    // 取消進度（停止子程序 → 還原 GPU → 完成紀錄），而非只有靜態「取消中」。
    let cancelling = ctx.cancel.is_cancelled();
    // 1) 停止所有 owned 子程序
    if cancelling {
        emit_cancel(ctx, "stopping", 20);
    }
    for pid in ctx.owned_processes.drain(..) {
        let _ = ctx.processes.kill(pid);
    }
    // 2) 還原原始策略（若有 baseline）+ 重啟 GPU
    if cancelling {
        emit_cancel(ctx, "restoring", 60);
    }
    let mut restored = true;
    if let Some(baseline) = &ctx.baseline {
        if recovery::mark_restore_needed_at(&ctx.journal_path, baseline).is_err() {
            return false;
        }
        if let Err(e) = restore_snapshot(ctx.backend.as_ref(), ctx.sleeper.as_ref(), baseline) {
            restored = false;
            log::error!("基準測試終結還原失敗: {e}；保留還原日誌供啟動重試");
        }
    }
    // 3) 還原驗證成功才清除日誌（Task 1 崩潰還原保證不被削弱）
    if restored {
        restored = recovery::clear_at(&ctx.journal_path).is_ok();
    }
    restored
}

/// panic 終結路徑：即使 cleanup 本身再次 panic，也把 session 寫成 Failed 並要求復原。
pub fn panic_failure(ctx: &mut RunContext) -> RunResult {
    let error = codes::BENCHMARK_RUNNER_PANIC.to_string();
    let restored = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cleanup_run(ctx)))
        .unwrap_or(false);
    let mut detail =
        storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap_or_else(|_| SessionDetail {
            summary: SessionSummary {
                id: ctx.session_id.clone(),
                started_at: chrono::Local::now().to_rfc3339(),
                gpu_instance_id: ctx.config.gpu_instance_id.clone().unwrap_or_default(),
                cpu_fingerprint: cpu_fingerprint_with(&ctx.topo, &ctx.cpu_identity),
                config: ctx.config.clone(),
                ..Default::default()
            },
            ..Default::default()
        });
    detail.summary.status = SessionStatus::Failed;
    detail.summary.finished_at = Some(chrono::Local::now().to_rfc3339());
    detail.summary.best_lp = None;
    detail.summary.reliability = ReliabilitySummary::default();
    detail.summary.error = Some(error.clone());
    let _ = storage::save_session_at(&ctx.storage_root, &detail);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        emit(
            ctx,
            &detail,
            "finalizing",
            None,
            None,
            100,
            None,
            Some(error.clone()),
        )
    }));
    RunResult {
        status: SessionStatus::Failed,
        detail,
        error: Some(error),
        best_lp: None,
        severe_lps: Vec::new(),
        recommended_cores: Vec::new(),
        recovery_required: !restored,
    }
}

fn terminal(
    ctx: &mut RunContext,
    mut detail: SessionDetail,
    status: SessionStatus,
    error: Option<String>,
    best: Option<u32>,
    severe: Vec<u32>,
    recommended: Vec<u32>,
) -> RunResult {
    let restored = cleanup_run(ctx);
    let status = if !restored {
        SessionStatus::Failed
    } else {
        status
    };
    let mut error = if !restored {
        Some(codes::BENCHMARK_RECOVERY_REQUIRED.to_string())
    } else {
        error
    };
    // 4) 組最終 session 並原子寫入
    detail.summary.status = status;
    detail.summary.finished_at = Some(chrono::Local::now().to_rfc3339());
    detail.summary.best_lp = best;
    detail.summary.severe_lps = severe.clone();
    detail.summary.error = error.clone();
    // capture 完整性：從累計器複製（含校準/overflow retry/drift rerun），不在此硬設。
    // effective_fps_cap / circular_buffer_size 已於校準後寫入 summary，保留不覆蓋。
    detail.summary.capture_quality.total_captures = ctx.capture_quality.total_captures;
    detail.summary.capture_quality.valid_captures = ctx.capture_quality.valid_captures;
    detail.summary.capture_quality.invalid_captures = ctx.capture_quality.invalid_captures;
    detail.summary.capture_quality.window_invalid_captures =
        ctx.capture_quality.window_invalid_captures;
    detail.summary.capture_quality.window_retry_captures =
        ctx.capture_quality.window_retry_captures;
    detail.summary.capture_quality.overflowed_present_events =
        ctx.capture_quality.overflowed_present_events;
    detail.summary.capture_quality.etw_events_lost = ctx.capture_quality.etw_events_lost;
    detail.summary.capture_quality.integrity_passed = status == SessionStatus::Completed;
    let mut status = status;
    if storage::save_session_at(&ctx.storage_root, &detail).is_err() {
        status = SessionStatus::Failed;
        error = Some(codes::BENCHMARK_STORAGE_FAILED.into());
        detail.summary.status = status;
        detail.summary.error = error.clone();
        detail.summary.capture_quality.integrity_passed = false;
    }
    // 5) 最終 progress。取消時以取消專用階段收尾（完成紀錄），讓前端取消進度
    // 推到 100；其餘維持原 benchmark finalizing 事件（不改既有語意）。
    if status == SessionStatus::Cancelled {
        emit_cancel(ctx, "finalizing", 100);
    } else {
        emit(
            ctx,
            &detail,
            "finalizing",
            None,
            None,
            100,
            None,
            error.clone(),
        );
    }
    RunResult {
        status,
        detail,
        error,
        best_lp: best,
        severe_lps: severe,
        recommended_cores: recommended,
        recovery_required: !restored,
    }
}

fn require_journal(path: &Path) -> Result<recovery::RecoveryJournal, String> {
    recovery::load_from(path)?.ok_or_else(|| codes::GPU_APPLY_FAILED.to_string())
}

/// 等效安全驗證的最終結果（供 manager 寫入 `EquivalentSafetyValidation`）。
#[cfg(test)]
pub struct EquivalentValidationOutcome {
    pub status: EquivalentSafetyStatus,
    pub rounds: u32,
    pub avg_improvement_pct: Option<f64>,
    pub p1_improvement_pct: Option<f64>,
    pub p01_improvement_pct: Option<f64>,
    pub mad_delta_pp: Option<f64>,
    pub spike_delta_pp: Option<f64>,
    pub reason: Option<String>,
    pub recovery_required: bool,
    pub drift_reruns: u32,
    pub capture_quality: CaptureQuality,
}

#[cfg(test)]
impl EquivalentValidationOutcome {
    #[allow(clippy::too_many_arguments)]
    fn from_evidence(
        status: EquivalentSafetyStatus,
        ev: Option<EquivalentEvidence>,
        rounds: u32,
        reason: Option<String>,
        recovery_required: bool,
        drift_reruns: u32,
        capture_quality: CaptureQuality,
    ) -> Self {
        Self {
            status,
            rounds,
            avg_improvement_pct: ev.as_ref().and_then(|e| e.avg_improvement_pct),
            p1_improvement_pct: ev.as_ref().and_then(|e| e.p1_improvement_pct),
            p01_improvement_pct: ev.as_ref().and_then(|e| e.p01_improvement_pct),
            mad_delta_pp: ev.as_ref().and_then(|e| e.mad_delta_pp),
            spike_delta_pp: ev.as_ref().and_then(|e| e.spike_delta_pp),
            reason,
            recovery_required,
            drift_reruns,
            capture_quality,
        }
    }
}

/// 等效驗證 panic 的清理與穩定終態。
#[cfg(test)]
pub fn equivalent_panic_failure(ctx: &mut RunContext) -> EquivalentValidationOutcome {
    let restored = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cleanup_run(ctx)))
        .unwrap_or(false);
    EquivalentValidationOutcome::from_evidence(
        EquivalentSafetyStatus::Failed,
        None,
        0,
        Some(codes::BENCHMARK_RUNNER_PANIC.to_string()),
        !restored,
        0,
        ctx.capture_quality.clone(),
    )
}

/// 等效安全驗證：3 組新鮮 AB/BA 配對（共 6 captures，round 400..402）比較
/// `selected_lp` 與 `reference_lp`（目前鎖定核心），沿用既有的 capture 完整性、
/// 環境穩定性、漂移重跑、取消、清理與策略還原。不混入原確認資料，也不寫 session。
#[cfg(test)]
pub fn run_equivalent_validation(
    ctx: &mut RunContext,
    selected_lp: u32,
    reference_lp: u32,
    fps_cap: u32,
    buffer: u32,
) -> EquivalentValidationOutcome {
    // 前置驗證 + baseline（= 目前 reference policy 快照）。
    let (instance, _gpu_name) = match pre_flight(ctx) {
        Ok(v) => v,
        Err(e) => {
            return EquivalentValidationOutcome::from_evidence(
                EquivalentSafetyStatus::Failed,
                None,
                0,
                Some(e),
                false,
                0,
                ctx.capture_quality.clone(),
            );
        }
    };

    // 進度事件契約：一進 compact 就 emit「starting」，讓前端立即讀到
    // windowLayout=CompactProgress（不等 CPU idle 最長 60s）。
    let detail = SessionDetail {
        summary: SessionSummary {
            id: ctx.session_id.clone(),
            started_at: chrono::Local::now().to_rfc3339(),
            ..Default::default()
        },
        ..Default::default()
    };

    // 空間預檢 + 快照主視窗 + 切 compact（RAII 還原；失敗立即回 Failed）
    let _layout_guard = match prepare_window_layout(
        ctx.window_control.clone(),
        (ctx.config.width, ctx.config.height),
    ) {
        Ok(g) => {
            ctx.layout = Some(g.plan);
            g
        }
        Err(e) => {
            return EquivalentValidationOutcome::from_evidence(
                EquivalentSafetyStatus::Failed,
                None,
                0,
                Some(e),
                false,
                0,
                ctx.capture_quality.clone(),
            );
        }
    };
    emit(ctx, &detail, "starting", None, None, 0, None, None);

    // 環境閘（AC/電池節能/CPU idle）。
    if let Err(e) = env::environment_gate(ctx.env.as_ref()) {
        let restored = cleanup_run(ctx);
        return EquivalentValidationOutcome::from_evidence(
            EquivalentSafetyStatus::Failed,
            None,
            0,
            Some(e),
            !restored,
            0,
            ctx.capture_quality.clone(),
        );
    }
    if let Err(e) = env::wait_for_cpu_idle(ctx.env.as_ref(), ctx.sleeper.as_ref(), &|| {
        ctx.cancel.is_cancelled()
    }) {
        let status = if e == "cancelled" {
            EquivalentSafetyStatus::Cancelled
        } else {
            EquivalentSafetyStatus::Failed
        };
        let restored = cleanup_run(ctx);
        return EquivalentValidationOutcome::from_evidence(
            status,
            None,
            0,
            Some(e),
            !restored,
            0,
            ctx.capture_quality.clone(),
        );
    }

    // 3 組 AB/BA（獨立 round namespace 400..402）。
    let session_dir = ctx.storage_root.join(&ctx.session_id);
    let lps = [selected_lp, reference_lp];
    let mut round_csvs: RoundCsvs = HashMap::new();
    let total_tests = (lps.len() as u32) * EQUIVALENT_VALIDATION_ROUNDS;
    let mut done = 0u32;
    let mut reference_med: Option<f64> = None;
    let mut drift_reruns = 0u32;

    let mut reason: Option<TerminalReason> = None;
    for round in EQUIVALENT_VALIDATION_ROUND_BASE
        ..(EQUIVALENT_VALIDATION_ROUND_BASE + EQUIVALENT_VALIDATION_ROUNDS)
    {
        match capture_round_with_drift(
            ctx,
            &instance,
            round,
            &lps,
            &session_dir,
            &mut round_csvs,
            &mut done,
            total_tests,
            &detail,
            fps_cap,
            buffer,
            &mut reference_med,
            &mut drift_reruns,
        ) {
            StepOutcome::Continue => {}
            StepOutcome::Isolated(e) => {
                reason = Some(TerminalReason::Error(e));
                break;
            }
            StepOutcome::Break(r) => {
                reason = Some(r);
                break;
            }
        }
    }

    let restored = cleanup_run(ctx);
    let recovery_required = !restored;
    let capture_quality = ctx.capture_quality.clone();

    if let Some(r) = reason {
        let (status, reason_str) = match r {
            TerminalReason::Cancelled => {
                (EquivalentSafetyStatus::Cancelled, "cancelled".to_string())
            }
            TerminalReason::Error(e) => (EquivalentSafetyStatus::Failed, e),
        };
        return EquivalentValidationOutcome::from_evidence(
            status,
            None,
            EQUIVALENT_VALIDATION_ROUNDS,
            Some(reason_str),
            recovery_required,
            drift_reruns,
            capture_quality,
        );
    }

    // 計算 selected 相對 reference 的 raw paired median evidence。
    let Some(pairs) = confirmation_pairs(
        &round_csvs,
        selected_lp,
        reference_lp,
        EQUIVALENT_VALIDATION_ROUNDS,
        EQUIVALENT_VALIDATION_ROUND_BASE,
    ) else {
        return EquivalentValidationOutcome::from_evidence(
            EquivalentSafetyStatus::Failed,
            None,
            EQUIVALENT_VALIDATION_ROUNDS,
            Some(codes::BENCHMARK_CSV_INVALID.to_string()),
            recovery_required,
            drift_reruns,
            capture_quality,
        );
    };
    let ev = equivalent_evidence(&pairs);
    let passed = equivalent_validation_medians_ok(&ev) && !equivalent_validation_regressed(&pairs);
    let status = if passed {
        EquivalentSafetyStatus::Passed
    } else {
        EquivalentSafetyStatus::Failed
    };
    EquivalentValidationOutcome::from_evidence(
        status,
        Some(ev),
        EQUIVALENT_VALIDATION_ROUNDS,
        None,
        recovery_required,
        drift_reruns,
        capture_quality,
    )
}

#[allow(clippy::too_many_arguments)]
fn emit(
    ctx: &mut RunContext,
    detail: &SessionDetail,
    stage: &str,
    round: Option<u32>,
    lp: Option<u32>,
    percentage: u32,
    eta: Option<u64>,
    error: Option<String>,
) {
    let (phase, phase_round) = progress_phase(stage, round);
    let progress = BenchmarkProgress {
        target: if stage == "calibrating" {
            None
        } else {
            detail
                .summary
                .quick
                .as_ref()
                .and_then(|q| {
                    q.candidates
                        .iter()
                        .find(|t| t.lp_indices.first().copied() == lp)
                })
                .cloned()
        },
        session_id: detail.summary.id.clone(),
        stage: stage.to_string(),
        round,
        phase,
        phase_round,
        lp,
        percentage,
        eta_secs: eta,
        error,
        window_integrity: None,
        cancel_stage: None,
        cancel_progress: None,
    };
    (ctx.on_progress)(&progress);
}

/// 取消專用 progress 事件：以 `cancel_stage`/`cancel_progress` 承載取消階段與百分比，
/// 不碰 `percentage`（維持 benchmark 進度既有單調語意）。百分比 0..100 單調。
fn emit_cancel(ctx: &mut RunContext, stage: &str, progress: u32) {
    let p = BenchmarkProgress {
        target: None,
        session_id: ctx.session_id.clone(),
        stage: "cancelling".to_string(),
        round: None,
        phase: None,
        phase_round: None,
        lp: None,
        percentage: 0,
        eta_secs: None,
        error: None,
        window_integrity: None,
        cancel_stage: Some(stage.to_string()),
        cancel_progress: Some(progress),
    };
    (ctx.on_progress)(&p);
}

fn progress_phase(stage: &str, round: Option<u32>) -> (Option<BenchmarkPhase>, Option<u32>) {
    if stage == "calibrating" {
        return (None, None);
    }
    let Some(round) = round else {
        return (None, None);
    };
    let phase = if round < SCREENING_ROUNDS {
        (BenchmarkPhase::Screening, round + 1)
    } else if round < SCREENING_ROUNDS + REFINEMENT_ROUNDS {
        (BenchmarkPhase::Refinement, round - SCREENING_ROUNDS + 1)
    } else if (CONFIRMATION_ROUND_BASE..REVERSE_ROUND_BASE).contains(&round) {
        (
            BenchmarkPhase::Confirmation,
            round - CONFIRMATION_ROUND_BASE + 1,
        )
    } else if (REVERSE_ROUND_BASE..EQUIVALENT_VALIDATION_ROUND_BASE).contains(&round) {
        (
            BenchmarkPhase::ReverseConfirmation,
            round - REVERSE_ROUND_BASE + 1,
        )
    } else if round >= EQUIVALENT_VALIDATION_ROUND_BASE {
        (
            BenchmarkPhase::EquivalentValidation,
            round - EQUIVALENT_VALIDATION_ROUND_BASE + 1,
        )
    } else {
        return (None, None);
    };
    (Some(phase.0), Some(phase.1))
}

// ── 測試用 fake ─────────────────────────────────────────────────────────

#[cfg(test)]
pub mod fake {
    use super::*;
    use crate::benchmark::window_layout::{MainWindowSnapshot, MonitorInfo};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use windows::Win32::UI::WindowsAndMessaging::WINDOWPLACEMENT;

    /// 測試用環境探針：預設桌上機（無電池、接 AC、無電池節能、CPU 0%），環境閘立即通過。
    pub struct FakeEnvironmentProbe;

    impl FakeEnvironmentProbe {
        pub fn new() -> Self {
            Self
        }
    }

    impl super::EnvironmentProbe for FakeEnvironmentProbe {
        fn battery_present(&self) -> bool {
            false
        }
        fn on_ac_power(&self) -> bool {
            true
        }
        fn battery_saver_on(&self) -> bool {
            false
        }
        fn sample_total_cpu(&self) -> f64 {
            0.0
        }
    }

    /// 從 `round-<r>-lp-<lp>.csv` 檔名取出 (round, LP)
    fn round_lp_from_csv_path(path: &str) -> Option<(u32, u32)> {
        let stem = std::path::Path::new(path).file_stem()?.to_str()?;
        let (head, lp_str) = stem.rsplit_once("-lp-")?;
        let lp: u32 = lp_str.parse().ok()?;
        let round_str = head.rsplit_once("round-")?.1;
        let round: u32 = round_str.parse().ok()?;
        Some((round, lp))
    }

    /// 從 `round-<r>-lp-<lp>.csv` 檔名取出 LP
    fn lp_from_csv_path(path: &str) -> Option<u32> {
        let stem = std::path::Path::new(path).file_stem()?.to_str()?;
        let lp_part = stem.rsplit_once("-lp-")?.1;
        lp_part.parse().ok()
    }

    /// 記憶體模擬的 ProcessRunner：記錄 spawn/kill，
    /// 並在「PresentMon」spawn 時寫出指定的 CSV（依 program 名判斷）。
    pub struct FakeProcessRunner {
        /// spawn 紀錄：(exe name, pid, args)
        pub spawned: Mutex<Vec<(String, u32, Vec<String>)>>,
        pub killed: Mutex<Vec<u32>>,
        /// PresentMon 的 CSV 內容（寫到 -output_file）
        pub presentmon_csv: Mutex<String>,
        /// 依 LP 覆寫 CSV 內容（優先於 `presentmon_csv`）；供決定性勝負測試用。
        pub presentmon_csv_by_lp: Mutex<std::collections::HashMap<u32, String>>,
        /// PresentMon spawn 時是否失敗（對所有 LP）
        pub fail_presentmon: AtomicBool,
        /// 指定哪些 LP 的 PresentMon spawn 失敗（依 -output_file 檔名內 -lp-<n> 判斷）
        pub fail_presentmon_lps: Mutex<std::collections::HashSet<u32>>,
        /// 指定哪些 round 的 PresentMon spawn 失敗（依 -output_file 檔名內 round-<n> 判斷）
        pub fail_presentmon_rounds: Mutex<std::collections::HashSet<u32>>,
        /// workload spawn 時是否失敗
        pub fail_workload: AtomicBool,
        /// PresentMon 的 wait_exit 是否回傳「卡住」（逾時）
        pub presentmon_timeout: AtomicBool,
        /// PresentMon spawn 時是否真的寫出 CSV（false = 不寫 → 缺檔）
        pub presentmon_write_csv: AtomicBool,
        /// 模擬 PresentMon 回報 ETW events lost：spawn 時不寫 CSV，且 stderr
        /// 含 "Lost ... ETW events"（擷取負載過高 → 應觸發不可重試的 fail-fast）
        pub presentmon_etw_loss: AtomicBool,
        /// pid → 程式名（wait_exit 需要判斷 pid 是不是 PresentMon）
        pid_name: Mutex<HashMap<u32, String>>,
        next_pid: AtomicU32,
        pub alive: Mutex<HashMap<u32, bool>>,
        /// 已退出程序的 exit code（spawn 時預設 0；診斷測試用）
        exit_codes: Mutex<HashMap<u32, i32>>,
        /// spawn 時記錄的 bounded output tail（診斷測試用）
        outputs: Mutex<HashMap<u32, ProcessOutput>>,
        /// 特定 LP 的第一個 capture attempt 行為：Missing（不寫 CSV）
        pub first_attempt_missing: Mutex<std::collections::HashSet<u32>>,
        /// 特定 LP 的第一個 capture attempt 行為：Empty（僅 header）
        pub first_attempt_empty: Mutex<std::collections::HashSet<u32>>,
        /// 特定 LP 的第一個 capture attempt 回報 overflowed present events
        /// （stderr 帶 count、CSV 仍有效）；第二個 attempt 乾淨 → 測 overflow retry。
        pub first_attempt_overflow: Mutex<std::collections::HashSet<u32>>,
        /// 特定 LP 的所有 retry attempt 也 Missing（全部嘗試都失敗）
        pub second_attempt_also_missing: Mutex<std::collections::HashSet<u32>>,
        /// per-(round, lp) → PresentMon spawn 次數（判斷第幾次 attempt）
        capture_call_count: Mutex<std::collections::HashMap<(u32, u32), u32>>,
    }

    impl FakeProcessRunner {
        pub fn new() -> Self {
            Self {
                spawned: Mutex::new(Vec::new()),
                killed: Mutex::new(Vec::new()),
                presentmon_csv: Mutex::new(String::new()),
                presentmon_csv_by_lp: Mutex::new(std::collections::HashMap::new()),
                fail_presentmon: AtomicBool::new(false),
                fail_presentmon_lps: Mutex::new(std::collections::HashSet::new()),
                fail_presentmon_rounds: Mutex::new(std::collections::HashSet::new()),
                fail_workload: AtomicBool::new(false),
                presentmon_timeout: AtomicBool::new(false),
                presentmon_write_csv: AtomicBool::new(true),
                presentmon_etw_loss: AtomicBool::new(false),
                pid_name: Mutex::new(HashMap::new()),
                next_pid: AtomicU32::new(1000),
                alive: Mutex::new(HashMap::new()),
                exit_codes: Mutex::new(HashMap::new()),
                outputs: Mutex::new(HashMap::new()),
                first_attempt_missing: Mutex::new(std::collections::HashSet::new()),
                first_attempt_empty: Mutex::new(std::collections::HashSet::new()),
                first_attempt_overflow: Mutex::new(std::collections::HashSet::new()),
                second_attempt_also_missing: Mutex::new(std::collections::HashSet::new()),
                capture_call_count: Mutex::new(std::collections::HashMap::new()),
            }
        }

        pub fn spawn_log(&self) -> Vec<(String, u32, Vec<String>)> {
            self.spawned.lock().unwrap().clone()
        }
        pub fn killed_log(&self) -> Vec<u32> {
            self.killed.lock().unwrap().clone()
        }
    }

    impl FakeProcessRunner {
        pub fn fail_lp(&self, lp: u32) {
            self.fail_presentmon_lps.lock().unwrap().insert(lp);
        }
    }

    impl ProcessRunner for FakeProcessRunner {
        fn spawn(&self, exe: &Path, args: &[String]) -> Result<u32, String> {
            let name = exe
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
            self.spawned
                .lock()
                .unwrap()
                .push((name.clone(), pid, args.to_vec()));
            let out = args
                .iter()
                .position(|a| a == "--output_file")
                .and_then(|i| args.get(i + 1))
                .cloned();
            if name.contains("PresentMon") {
                // 依 -output_file 檔名判 (round, lp)，若在失敗清單則 spawn 失敗
                let lp = out
                    .as_deref()
                    .and_then(lp_from_csv_path)
                    .unwrap_or(u32::MAX);
                let round = out
                    .as_deref()
                    .and_then(round_lp_from_csv_path)
                    .map(|(r, _)| r);
                let round_fail = round
                    .map(|r| self.fail_presentmon_rounds.lock().unwrap().contains(&r))
                    .unwrap_or(false);
                if self.fail_presentmon.load(Ordering::SeqCst)
                    || self.fail_presentmon_lps.lock().unwrap().contains(&lp)
                    || round_fail
                {
                    return Err("fake: presentmon fail".into());
                }
            }
            if !name.contains("PresentMon") && self.fail_workload.load(Ordering::SeqCst) {
                return Err("fake: workload fail".into());
            }
            self.alive.lock().unwrap().insert(pid, true);
            self.pid_name.lock().unwrap().insert(pid, name.clone());
            self.exit_codes.lock().unwrap().insert(pid, 0);
            // 先決定 attempt/lp（供 stderr 與 CSV 內容共用）。
            let rl = out.as_deref().and_then(round_lp_from_csv_path);
            let (attempt, lp_opt) = if let Some((round, lp)) = rl {
                let mut counts = self.capture_call_count.lock().unwrap();
                let entry = counts.entry((round, lp)).or_insert(0);
                *entry += 1;
                (*entry, Some(lp))
            } else {
                (1, None)
            };
            // 診斷測試用：依程式名 + attempt 記錄可預期的 output tail。
            let etw_loss = self.presentmon_etw_loss.load(Ordering::SeqCst);
            let first_overflow = lp_opt
                .map(|lp| attempt == 1 && self.first_attempt_overflow.lock().unwrap().contains(&lp))
                .unwrap_or(false);
            let output = if name.contains("PresentMon") {
                if etw_loss {
                    ProcessOutput {
                        stdout: "fake-presentmon-stdout".into(),
                        stderr: "Lost 9000000 ETW events".into(),
                    }
                } else if first_overflow {
                    ProcessOutput {
                        stdout: "fake-presentmon-stdout".into(),
                        stderr: "warning: 47123 overflowed present events detected. This could be due to a high-fps application.".into(),
                    }
                } else {
                    ProcessOutput {
                        stdout: "fake-presentmon-stdout".into(),
                        stderr: "fake-presentmon-stderr".into(),
                    }
                }
            } else {
                ProcessOutput {
                    stdout: "fake-workload-stdout".into(),
                    stderr: "fake-workload-stderr".into(),
                }
            };
            self.outputs.lock().unwrap().insert(pid, output);
            if name.contains("PresentMon") {
                // 模擬 PresentMon：支援 per-attempt 行為（first_attempt_missing/empty/
                // first_attempt_overflow/second_attempt_also_missing）。先決定內容再寫入。
                let write_full = self.presentmon_write_csv.load(Ordering::SeqCst);
                let csv_content: Option<String> = match lp_opt {
                    _ if etw_loss => None,
                    Some(lp)
                        if attempt == 1
                            && self.first_attempt_missing.lock().unwrap().contains(&lp) =>
                    {
                        None
                    }
                    Some(lp)
                        if attempt == 1
                            && self.first_attempt_empty.lock().unwrap().contains(&lp) =>
                    {
                        Some("Application,ProcessID,msBetweenPresents\n".to_string())
                    }
                    Some(lp)
                        if attempt >= 2
                            && self
                                .second_attempt_also_missing
                                .lock()
                                .unwrap()
                                .contains(&lp) =>
                    {
                        None
                    }
                    _ if !write_full => None,
                    _ => Some(
                        lp_opt
                            .and_then(|lp| {
                                self.presentmon_csv_by_lp.lock().unwrap().get(&lp).cloned()
                            })
                            .unwrap_or_else(|| self.presentmon_csv.lock().unwrap().clone()),
                    ),
                };
                if let (Some(path), Some(content)) = (out, csv_content) {
                    let _ = std::fs::write(&path, content);
                }
            }
            Ok(pid)
        }

        fn is_alive(&self, pid: u32) -> bool {
            self.alive
                .lock()
                .unwrap()
                .get(&pid)
                .copied()
                .unwrap_or(false)
        }

        fn kill(&self, pid: u32) -> Result<(), String> {
            self.killed.lock().unwrap().push(pid);
            self.alive.lock().unwrap().insert(pid, false);
            Ok(())
        }

        fn wait_exit(&self, pid: u32, _timeout_ms: u64) -> Result<bool, String> {
            // 模擬 PresentMon 卡住（逾時）：回傳「未退出」
            let is_presentmon = self
                .pid_name
                .lock()
                .unwrap()
                .get(&pid)
                .map(|n| n.contains("PresentMon"))
                .unwrap_or(false);
            if is_presentmon && self.presentmon_timeout.load(Ordering::SeqCst) {
                return Ok(false);
            }
            Ok(true) // fake 立即退出
        }

        fn exit_code(&self, pid: u32) -> Option<i32> {
            self.exit_codes.lock().unwrap().get(&pid).copied()
        }

        fn output_tail(&self, pid: u32, _max_chars: usize) -> Option<ProcessOutput> {
            self.outputs.lock().unwrap().get(&pid).cloned()
        }
    }

    /// 可程式化取消的 fake
    pub struct FakeCancel {
        pub cancelled: AtomicBool,
    }

    impl FakeCancel {
        pub fn new() -> Self {
            Self {
                cancelled: AtomicBool::new(false),
            }
        }
        pub fn set(&self, v: bool) {
            self.cancelled.store(v, Ordering::SeqCst);
        }
    }

    impl CancelSignal for FakeCancel {
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    /// 累計 sleep 達 `trigger_after_ms` 毫秒時觸發取消（測試「執行中取消」用）。
    /// 累計毫秒與 runner 的 sleep_interruptible / wait_capture 同源，因此
    /// 可精準落在 startup/warmup/capture wait 的某一時點。
    pub struct CancelAfterSleeper {
        pub cancel: Arc<FakeCancel>,
        pub trigger_after_ms: u64,
        elapsed: Mutex<u64>,
    }

    impl CancelAfterSleeper {
        pub fn new(cancel: Arc<FakeCancel>, trigger_after_ms: u64) -> Self {
            Self {
                cancel,
                trigger_after_ms,
                elapsed: Mutex::new(0),
            }
        }

        /// 目前累計 sleep 毫秒（測試斷言「取消是否提前中斷」用）
        pub fn elapsed_ms(&self) -> u64 {
            *self.elapsed.lock().unwrap()
        }
    }

    impl Sleep for CancelAfterSleeper {
        fn sleep(&self, ms: u64) {
            let mut e = self.elapsed.lock().unwrap();
            *e += ms;
            if *e >= self.trigger_after_ms {
                self.cancel.set(true);
            }
        }
    }

    /// 記錄 resize 呼叫的 fake window（不碰真實 Win32）
    pub struct FakeWindow {
        pub calls: Mutex<Vec<(u32, u32, u32)>>,
        /// find_and_resize 回傳值：Some(Ok(true))=找到、Some(Ok(false))=未找到、
        /// Some(Err)=resize 失敗、None=預設 Ok(true)
        pub result: Mutex<Option<Result<bool, String>>>,
        /// guard_close 呼叫紀錄（pid）
        pub guard_calls: Mutex<Vec<u32>>,
        /// guard_close 回傳值：Some(Ok(true))=找到、Some(Ok(false))=未找到、
        /// Some(Err)=guard 失敗、None=預設 Ok(true)
        pub guard_result: Mutex<Option<Result<bool, String>>>,
        /// position_topmost 呼叫紀錄（pid, x, y）
        pub position_calls: Mutex<Vec<(u32, i32, i32)>>,
        /// outer_rect 回傳值：None = Ok(None)（未找到）；Some(rect) = Ok(Some(rect))
        pub outer_rect_rect: Mutex<Option<Rect>>,
        /// integrity 是否回報「良好」快照（true=全良好、false=前景失敗）
        pub integrity_ok: AtomicBool,
        /// 指定 integrity() 直接回傳的完整快照（Some 優先於 integrity_ok；None=沿用 bool 邏輯）
        pub integrity_snapshot: Mutex<Option<WindowIntegritySnapshot>>,
    }

    impl FakeWindow {
        pub fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: Mutex::new(None),
                guard_calls: Mutex::new(Vec::new()),
                guard_result: Mutex::new(None),
                position_calls: Mutex::new(Vec::new()),
                // 預設回報一個落在 rcWork 內、不與 compact 重疊的實際外框
                outer_rect_rect: Mutex::new(Some(Rect::new(0, 0, 1280, 720))),
                integrity_ok: AtomicBool::new(true),
                integrity_snapshot: Mutex::new(None),
            }
        }
        pub fn calls_log(&self) -> Vec<(u32, u32, u32)> {
            self.calls.lock().unwrap().clone()
        }
        pub fn guard_calls_log(&self) -> Vec<u32> {
            self.guard_calls.lock().unwrap().clone()
        }
        pub fn set_integrity_ok(&self, ok: bool) {
            self.integrity_ok.store(ok, Ordering::SeqCst);
        }
        pub fn set_integrity_snapshot(&self, snap: WindowIntegritySnapshot) {
            *self.integrity_snapshot.lock().unwrap() = Some(snap);
        }
    }

    impl WorkloadWindow for FakeWindow {
        fn find_and_resize(&self, pid: u32, width: u32, height: u32) -> Result<bool, String> {
            self.calls.lock().unwrap().push((pid, width, height));
            match self.result.lock().unwrap().clone() {
                Some(r) => r,
                None => Ok(true),
            }
        }

        fn guard_close(&self, pid: u32) -> Result<bool, String> {
            self.guard_calls.lock().unwrap().push(pid);
            match self.guard_result.lock().unwrap().clone() {
                Some(r) => r,
                None => Ok(true),
            }
        }

        fn position_topmost(&self, pid: u32, x: i32, y: i32) -> Result<bool, String> {
            self.position_calls.lock().unwrap().push((pid, x, y));
            Ok(true)
        }

        fn outer_rect(&self, _pid: u32) -> Result<Option<Rect>, String> {
            Ok(*self.outer_rect_rect.lock().unwrap())
        }

        fn integrity(&self, _pid: u32, _expected: Rect) -> WindowIntegritySnapshot {
            if let Some(snap) = *self.integrity_snapshot.lock().unwrap() {
                return snap;
            }
            if self.integrity_ok.load(Ordering::SeqCst) {
                WindowIntegritySnapshot {
                    foreground: true,
                    position_ok: true,
                    topmost: true,
                    visible: true,
                    ..Default::default()
                }
            } else {
                WindowIntegritySnapshot::default()
            }
        }
    }

    /// 測試用主視窗控制器：固定 1920×1080@96dpi monitor、記錄 snapshot/apply/restore 次數。
    pub struct FakeWindowController {
        pub snapshot_count: AtomicU32,
        pub apply_count: AtomicU32,
        pub restore_count: AtomicU32,
        pub center_requested: AtomicBool,
    }

    impl FakeWindowController {
        pub fn new() -> Self {
            Self {
                snapshot_count: AtomicU32::new(0),
                apply_count: AtomicU32::new(0),
                restore_count: AtomicU32::new(0),
                center_requested: AtomicBool::new(false),
            }
        }
    }

    impl MainWindowController for FakeWindowController {
        fn monitor_info(&self) -> Result<MonitorInfo, String> {
            Ok(MonitorInfo {
                rc_work: Rect::new(0, 0, 1920, 1080),
                dpi: 96,
            })
        }
        fn snapshot(&self) -> Result<MainWindowSnapshot, String> {
            self.snapshot_count.fetch_add(1, Ordering::SeqCst);
            let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
            wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            Ok(MainWindowSnapshot {
                placement: wp,
                rc_work: Rect::new(0, 0, 1920, 1080),
            })
        }
        fn apply_compact(&self, _rect: Rect) -> Result<(), String> {
            self.apply_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn restore(&self, _snap: &MainWindowSnapshot) -> Result<(), String> {
            self.restore_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn request_center_restore(&self) {
            self.center_requested.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{
        CancelAfterSleeper, FakeCancel, FakeEnvironmentProbe, FakeProcessRunner,
        FakeWindowController,
    };
    use super::*;
    use crate::benchmark::assets::{self, BenchmarkAssets};
    use crate::gpu::fake::FakeBackend;
    use crate::gpu::{AffinityPolicy, GpuDevice, NoopSleeper, RegistryValueSnapshot};
    use crate::topology::{build_topology, Topology};
    use std::sync::atomic::Ordering;
    use uuid::Uuid;

    const GPU_A: &str = r"PCI\VEN_FAKE&DEV_1";

    fn fixed_identity() -> CpuIdentity {
        CpuIdentity {
            architecture: 9,
            family: 6,
            model: 183,
            stepping: 1,
        }
    }

    fn topo() -> Topology {
        build_topology((0..8u32).map(|c| (vec![c], 0, false)).collect())
    }

    fn device(instance: &str) -> GpuDevice {
        GpuDevice {
            instance_id: instance.to_string(),
            friendly_name: format!("GPU {instance}"),
        }
    }

    /// 建立可通過 assets::verify 的暫存資源：從 vendored 資源目錄連結真實檔案。
    /// verify 以內嵌 digest 比對，偽造內容無法通過；測試不會寫入這些檔，
    /// hard link 安全（跨磁碟時退回 copy）。
    /// d3d9-workload.exe 是 build 產物、不在 git 內：CI checkout 缺檔時以假內容
    /// 補位（該環境 D3D9 digest 為 None，verify 僅檢查存在）。
    fn make_assets(dir: &std::path::Path) -> BenchmarkAssets {
        std::fs::create_dir_all(dir).unwrap();
        let vendored =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/benchmark");
        for file in [
            assets::PRESENTMON_FILE,
            assets::VULKAN_WORKLOAD_FILE,
            assets::D3D9_WORKLOAD_FILE,
        ] {
            let dst = dir.join(file);
            if dst.exists() {
                continue;
            }
            let src = vendored.join(file);
            if src.exists() {
                std::fs::hard_link(&src, &dst)
                    .or_else(|_| std::fs::copy(&src, &dst).map(|_| ()))
                    .unwrap();
            } else {
                std::fs::write(&dst, b"placeholder-d3d9-not-vendored").unwrap();
            }
        }
        assets::load(dir)
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pacedock_runner_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn base_config() -> BenchmarkConfig {
        BenchmarkConfig {
            gpu_instance_id: Some(GPU_A.to_string()),
            workload: WorkloadKind::Vulkan,
            sample_secs: 3,
            warm_up_secs: 1,
            repetitions: 5,
            // 候選 LP 不得含 physical Core 0（測試拓撲 core 0 = LP 0）
            candidate_lps: vec![1, 2, 3],
            // 測試預設走 legacy Fixed 路徑（不跑校準），Adaptive 另有專屬測試。
            fps_cap_policy: FpsCapPolicy::Fixed,
            ..Default::default()
        }
    }

    /// 明確自訂尺寸 → workload_command 從 config.width/height 直接組出 args，
    /// 不被 product default 覆寫（D3D9 路徑直接讀欄位，最貼近序列化後的值）。
    #[test]
    fn workload_command_preserves_explicit_dimensions() {
        let dir = temp_root("wl_cmd_dims");
        let assets = make_assets(&dir);
        let config = BenchmarkConfig {
            workload: WorkloadKind::D3D9,
            fullscreen: false,
            width: 800,
            height: 600,
            ..Default::default()
        };
        let (_exe, args) = workload_command(&assets, &config, config.fps_cap);
        assert!(args.contains(&"--width=800".to_string()));
        assert!(args.contains(&"--height=600".to_string()));
        assert!(args.contains(&"--fullscreen=0".to_string()));
        assert!(!args.contains(&"--width=1280".to_string()));
        assert!(!args.contains(&"--height=720".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一組有效的 fake CSV（LP 不同 frametime；frametime 交替使 MAD > 0）
    fn csv_for_lp(lp: u32) -> String {
        // LP 越低 fps 越高（frametime 越低）→ 讓 best_lp 可預期
        let base = 20.0 - (lp as f64) * 2.0; // LP0=20ms(50fps), LP1=18ms, LP2=16ms
        csv_with_base(base)
    }

    /// 依指定 base frametime 產生交替 frametime 的 fake CSV（MAD > 0；三位小數精確）。
    fn csv_with_base(base: f64) -> String {
        let mut s = String::from("Application,ProcessID,msBetweenPresents\n");
        for i in 0..50 {
            let ft = if i % 2 == 0 { base } else { base + 0.5 };
            s.push_str(&format!("\"w (1)\",1,{ft:.3}\n"));
        }
        s
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn build_ctx(
        root: &std::path::Path,
        backend: Arc<dyn GpuBackend>,
        processes: Arc<dyn ProcessRunner>,
        cancel: Arc<dyn CancelSignal>,
        sleeper: Arc<dyn Sleep>,
        config: BenchmarkConfig,
        journal: &std::path::Path,
        on_progress: Option<Box<dyn FnMut(&BenchmarkProgress) + Send>>,
    ) -> RunContext {
        let session_id = Uuid::new_v4().to_string();
        let assets = make_assets(&root.join("assets"));
        RunContext {
            backend,
            sleeper,
            processes,
            cancel,
            env: Arc::new(FakeEnvironmentProbe::new()),
            topo: topo(),
            capture_quality: Default::default(),
            cpu_identity: fixed_identity(),
            assets,
            storage_root: root.join("benchmarks"),
            journal_path: journal.to_path_buf(),
            session_id,
            config,
            on_progress: on_progress.unwrap_or_else(|| Box::new(|_| {})),
            baseline: None,
            owned_processes: Vec::new(),
            window: Arc::new(fake::FakeWindow::new()),
            window_control: Arc::new(fake::FakeWindowController::new()),
            layout: None,
            on_integrity: Box::new(|_| {}),
            window_retries: 0,
            last_integrity: None,
        }
    }

    #[test]
    fn progress_phase_decodes_raw_round_namespaces() {
        assert_eq!(
            progress_phase("collecting", Some(0)),
            (Some(BenchmarkPhase::Screening), Some(1))
        );
        assert_eq!(
            progress_phase("collecting", Some(SCREENING_ROUNDS)),
            (Some(BenchmarkPhase::Refinement), Some(1))
        );
        assert_eq!(
            progress_phase("collecting", Some(CONFIRMATION_ROUND_BASE + 2)),
            (Some(BenchmarkPhase::Confirmation), Some(3))
        );
        assert_eq!(
            progress_phase("collecting", Some(REVERSE_ROUND_BASE)),
            (Some(BenchmarkPhase::ReverseConfirmation), Some(1))
        );
        assert_eq!(
            progress_phase("collecting", Some(EQUIVALENT_VALIDATION_ROUND_BASE + 1)),
            (Some(BenchmarkPhase::EquivalentValidation), Some(2))
        );
        assert_eq!(progress_phase("calibrating", Some(0)), (None, None));
    }

    #[test]
    fn panic_failure_kills_owned_processes_and_persists_failed_session() {
        let root = temp_root("panic_failure");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(FakeCancel::new()) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        ctx.owned_processes.push(4242);

        let result = panic_failure(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(result.error.as_deref(), Some(codes::BENCHMARK_RUNNER_PANIC));
        assert!(processes.killed_log().contains(&4242));
        let stored = storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap();
        assert_eq!(stored.summary.status, SessionStatus::Failed);
        assert_eq!(
            stored.summary.error.as_deref(),
            Some(codes::BENCHMARK_RUNNER_PANIC)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 建出一個 `window_control` 為自備 `FakeWindowController` 的 ctx（供 report_integrity 測試）。
    fn ctx_with_window_control(
        root: &std::path::Path,
        journal: &std::path::Path,
    ) -> (RunContext, Arc<FakeWindowController>) {
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        let cancel = FakeCancel::new();
        let mut ctx = build_ctx(
            root,
            backend as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            journal,
            None,
        );
        let wc = Arc::new(FakeWindowController::new());
        ctx.window_control = wc.clone();
        (ctx, wc)
    }

    #[test]
    fn report_integrity_foreground_loss_requests_center_restore() {
        let root = temp_root("center_req");
        let journal = root.join("journal.json");
        let (mut ctx, wc) = ctx_with_window_control(&root, &journal);

        let loss = WindowIntegritySnapshot {
            foreground: false,
            ..Default::default()
        };
        report_integrity(&mut ctx, &loss, None);
        assert!(wc.center_requested.load(Ordering::SeqCst));

        // 重複相同 snapshot 不影響正確性：旗標維持 true、不 panic
        report_integrity(&mut ctx, &loss, None);
        assert!(wc.center_requested.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn report_integrity_non_foreground_failure_does_not_request_center() {
        let root = temp_root("center_no_req");
        let journal = root.join("journal.json");
        let (mut ctx, wc) = ctx_with_window_control(&root, &journal);

        // minimized/position 失敗但 foreground 仍 true → 不要求置中
        let other = WindowIntegritySnapshot {
            foreground: true,
            minimized: true,
            position_ok: false,
            ..Default::default()
        };
        report_integrity(&mut ctx, &other, None);
        assert!(!wc.center_requested.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn success_completes_with_best_lp_and_restores_exact_policy() {
        let root = temp_root("success");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        // 原始策略（DevicePolicy=4, override mask=0b1000 → LP 3）
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // 每 LP 不同 frametime（LP 越大越快）→ LP3 決定性勝出，產生 verified best。
        for lp in [1u32, 2, 3] {
            processes
                .presentmon_csv_by_lp
                .lock()
                .unwrap()
                .insert(lp, csv_for_lp(lp));
        }
        let cancel = FakeCancel::new();

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "err={:?}",
            result.error
        );
        assert!(result.best_lp.is_some());
        // 策略還原到原始（逐位元組）
        assert_eq!(backend.current_policy(GPU_A), baseline);
        assert!(!journal.exists(), "成功後日誌應清除");
        // session 已寫入
        let detail = storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap();
        assert_eq!(detail.summary.status, SessionStatus::Completed);
        assert_eq!(detail.results.len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_workload_affinity_true_still_skips_workload_affinity() {
        // sync_workload_affinity 已棄用：即使傳入 true，runner 也不得
        // 設定 workload affinity。ProcessRunner trait 已無 affinity API surface，
        // 完成即驗證不受影響。
        let root = temp_root("sync_skip");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.sync_workload_affinity = true; // 舊語意，但 runner 必須忽略

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "sync_workload_affinity=true 仍不得影響 runner；應正常完成"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn success_merges_screening_refinement_and_confirmation_rounds() {
        // 新排程：3 篩選 round 全 LP + 2 refinement round（Top3）+ 5 確認 round（Top2）。
        // 同內容 CSV → 平手 → finalists = [1, 2]；確認效應為 0 → Equivalent，5 round 提早停。
        let root = temp_root("merge");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let config = base_config(); // candidate_lps [1,2,3]

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Completed);
        let sample_by_lp: HashMap<u32, u32> = result
            .detail
            .results
            .iter()
            .map(|r| (r.lp, r.sample_count))
            .collect();
        // finalists（LP1, LP2）合併 3 個 selection round + 5 確認 = 8 round；
        // 非 finalist（LP3）只測 3 個 selection round。
        assert_eq!(sample_by_lp[&1], 400);
        assert_eq!(sample_by_lp[&2], 400);
        assert_eq!(sample_by_lp[&3], 150);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 可靠性（reliability）判定 ──────────────────────────────────────────

    /// 寫出一個 (round, lp) 的 CSV，樣本 frametime 在 `frame_ms` 與 `frame_ms + 0.5`
    /// 交替（越低 FPS 越高）。交替兩值使 MAD > 0，供 log-ratio 確認分數使用。
    fn write_round_csv(dir: &Path, round: u32, lp: u32, frame_ms: f64) -> CapturedCsv {
        let path = dir.join(format!("round-{round}-lp-{lp}.csv"));
        let mut s = String::from("Application,ProcessID,msBetweenPresents\n");
        for i in 0..50 {
            let ft = if i % 2 == 0 { frame_ms } else { frame_ms + 0.5 };
            s.push_str(&format!("\"w (1)\",1,{ft:.3}\n"));
        }
        std::fs::write(&path, &s).unwrap();
        captured_csv(&path)
    }

    /// 完整（completed）且四項指標齊備的 LpResult fixture
    fn lp_res(lp: u32, avg: f64, p1: f64, p01: f64, stdev: f64) -> LpResult {
        LpResult {
            lp,
            avg_fps: Some(avg),
            p1_low: Some(p1),
            p01_low: Some(p01),
            stdev_fps: Some(stdev),
            completed: true,
            ..Default::default()
        }
    }

    /// 建置 K 個確認 round（round CONFIRMATION_ROUND_BASE..+K）的
    /// (candidate, runner) CSV map，逐 round 固定 frametime。
    fn confirmation_csvs(
        dir: &Path,
        candidate: u32,
        runner: u32,
        k: u32,
        c_frame: f64,
        r_frame: f64,
    ) -> RoundCsvs {
        let mut m: RoundCsvs = HashMap::new();
        for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + k) {
            m.entry(candidate)
                .or_default()
                .insert(round, write_round_csv(dir, round, candidate, c_frame));
            m.entry(runner)
                .or_default()
                .insert(round, write_round_csv(dir, round, runner, r_frame));
        }
        m
    }

    /// 建置確認 CSV map，逐 round 給定不同 (candidate, runner) frametime（供
    /// 一致性規則測試：單一 round 效應刻意低於門檻）。
    fn confirmation_csvs_varied(
        dir: &Path,
        candidate: u32,
        runner: u32,
        frames: &[(f64, f64)],
    ) -> RoundCsvs {
        let mut m: RoundCsvs = HashMap::new();
        for (i, &(cf, rf)) in frames.iter().enumerate() {
            let round = CONFIRMATION_ROUND_BASE + i as u32;
            m.entry(candidate)
                .or_default()
                .insert(round, write_round_csv(dir, round, candidate, cf));
            m.entry(runner)
                .or_default()
                .insert(round, write_round_csv(dir, round, runner, rf));
        }
        m
    }

    /// 由既有 CSV 檔建 CapturedCsv（測試用；等同 capture 完成當下的綁定）
    fn captured_csv(path: &Path) -> CapturedCsv {
        CapturedCsv::capture(path).expect("測試 CSV 應可讀取解析")
    }

    /// capture 前的 stale CSV 無法刪除（被鎖定）→ fail closed，不進行 capture
    #[test]
    fn run_capture_fails_closed_when_stale_csv_cannot_be_removed() {
        let root = temp_root("stale_lock");
        let csv = root.join("out.csv");
        std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents\n\"w\",1,10.0\n",
        )
        .unwrap();

        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(FakeCancel::new()) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &root.join("journal.json"),
            None,
        );

        // Windows 上以無 FILE_SHARE_DELETE 的 share mode 開檔即鎖定刪除
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _lock = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1) // FILE_SHARE_READ only
                .open(&csv)
                .unwrap();
            assert!(
                std::fs::remove_file(&csv).is_err(),
                "測試前提：share_mode(1) 的 handle 應鎖住刪除"
            );
            let err =
                run_capture(&mut ctx, 0, 1, 999, &csv, 1, 0, 8192, 1, Rect::default()).unwrap_err();
            assert_eq!(
                err,
                codes::BENCHMARK_CAPTURE_MISSING,
                "鎖定的 stale CSV 必須 fail closed"
            );
        }
        #[cfg(not(windows))]
        {
            let _ = &csv;
            let _ = &mut ctx;
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 驗證後遭置換的 CSV：digest 綁定必須偵測並拒絕
    #[test]
    fn captured_csv_detects_post_capture_replacement() {
        let root = temp_root("csv_bind");
        let path = root.join("out.csv");
        std::fs::write(&path, csv_for_lp(0)).unwrap();
        let captured = captured_csv(&path);
        assert!(captured.read().is_ok(), "原始內容應可讀取");

        std::fs::write(&path, csv_for_lp(1)).unwrap();
        assert!(
            captured.read().is_err(),
            "capture 後被置換的內容必須被 digest 檢查拒絕"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── evaluate_forward（effects → verdict）──

    #[test]
    fn evaluate_forward_passes_when_all_effects_above() {
        let dir = temp_root("eval_pass");
        let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 11.0);
        assert!(matches!(
            evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::CandidatePassed
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evaluate_forward_continues_when_one_effect_below_threshold() {
        let dir = temp_root("eval_early_consistent");
        let round_csvs =
            confirmation_csvs_varied(&dir, 0, 1, &[(10.0, 11.0), (10.0, 11.0), (10.0, 10.05)]);
        assert!(matches!(
            evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::Continue
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evaluate_forward_equivalent_at_5_6_7_rounds() {
        let dir = temp_root("eval_equiv");
        // 7 輪皆可忽略差異 → K=5/6/7 皆 Equivalent；K<5 只做 decisive → Continue。
        let round_csvs = confirmation_csvs(&dir, 0, 1, 7, 10.0, 10.05);
        assert!(matches!(
            evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::Continue
        ));
        assert!(matches!(
            evaluate_forward(&round_csvs, 0, 1, 4, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::Continue
        ));
        for k in [5u32, 6, 7] {
            assert_eq!(
                evaluate_forward(&round_csvs, 0, 1, k, CONFIRMATION_ROUND_BASE),
                ForwardVerdict::Equivalent,
                "K={k} 應判定 Equivalent"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evaluate_forward_decisive_win_precedes_equivalent_at_six_rounds() {
        let dir = temp_root("eval_decisive6");
        // K=6 但候選決定性勝出（10 vs 11）→ 優先回 CandidatePassed，不誤判 Equivalent。
        let round_csvs = confirmation_csvs(&dir, 0, 1, 6, 10.0, 11.0);
        assert_eq!(
            evaluate_forward(&round_csvs, 0, 1, 6, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::CandidatePassed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evaluate_forward_continues_on_straddle() {
        let dir = temp_root("eval_straddle");
        let mut round_csvs: RoundCsvs = HashMap::new();
        let frames = [(10.0, 11.0), (11.0, 10.0), (10.0, 11.0)];
        for (i, &(cf, rf)) in frames.iter().enumerate() {
            let round = CONFIRMATION_ROUND_BASE + i as u32;
            round_csvs
                .entry(0)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 0, cf));
            round_csvs
                .entry(1)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 1, rf));
        }
        assert!(matches!(
            evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::Continue
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evaluate_forward_reversal_when_runner_decisively_beats_candidate() {
        let dir = temp_root("eval_reversal");
        // runner(1) 每個確認 round 都明顯較快（10 vs 11）→ 反相 criteria 勝出。
        let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 11.0, 10.0);
        assert!(matches!(
            evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::RunnerUpReversal
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evaluate_forward_continues_when_evidence_missing() {
        let dir = temp_root("eval_missing");
        let mut round_csvs: RoundCsvs = HashMap::new();
        for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + 2) {
            round_csvs
                .entry(0)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 0, 10.0));
        }
        for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + 3) {
            round_csvs
                .entry(1)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 1, 12.0));
        }
        assert!(matches!(
            evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::Continue
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 確認一致性門檻（confirmation_passed）──

    #[test]
    fn confirmation_passed_consistency_thresholds_3_to_7() {
        let rails = true;
        // K=3：需 3/3
        assert!(!confirmation_passed(&[1.0, 1.0, 0.0], 1.0, rails));
        assert!(confirmation_passed(&[1.0, 1.0, 1.0], 1.0, rails));
        // K=4：需 4/4
        assert!(!confirmation_passed(&[1.0, 1.0, 1.0, 0.0], 1.0, rails));
        assert!(confirmation_passed(&[1.0; 4], 1.0, rails));
        // K=5：需 ≥4/5
        assert!(confirmation_passed(&[1.0, 1.0, 1.0, 1.0, 0.0], 1.0, rails));
        assert!(!confirmation_passed(&[1.0, 1.0, 1.0, 0.0, 0.0], 1.0, rails));
        // K=6：需 ≥5/6
        assert!(confirmation_passed(
            &[1.0, 1.0, 1.0, 1.0, 1.0, 0.0],
            1.0,
            rails
        ));
        assert!(!confirmation_passed(
            &[1.0, 1.0, 1.0, 1.0, 0.0, 0.0],
            1.0,
            rails
        ));
        // K=7：需 ≥6/7
        assert!(confirmation_passed(
            &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0],
            1.0,
            rails
        ));
        assert!(!confirmation_passed(
            &[1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0],
            1.0,
            rails
        ));
        // bootstrap 區間下界必須 > 門檻（等於門檻 → false）
        assert!(!confirmation_passed(
            &[1.0; 3],
            COMPOSITE_ADVANTAGE_MIN_PCT,
            rails
        ));
        // 護欄倒退 → false
        assert!(!confirmation_passed(&[1.0; 3], 1.0, false));
    }

    // ── 等效判定（raw median evidence + 單輪禁制）──

    /// 含 MAD/spike 的 LpResult fixture（等效判定用）。
    fn lp_raw(lp: u32, avg: f64, p1: f64, p01: f64, mad: f64, spike: f64) -> LpResult {
        LpResult {
            lp,
            avg_fps: Some(avg),
            p1_low: Some(p1),
            p01_low: Some(p01),
            frametime_mad_pct: Some(mad),
            spike_rate_pct: Some(spike),
            completed: true,
            ..Default::default()
        }
    }

    /// 把單一 (candidate, runner) 複製成 K 個完全相同的配對（模擬逐 round 重複量測）。
    fn identical_pairs(c: LpResult, r: LpResult, k: usize) -> Vec<(LpResult, LpResult)> {
        (0..k).map(|_| (c.clone(), r.clone())).collect()
    }

    #[test]
    fn equivalent_finalists_screenshot_case() {
        // 截圖型案例：avg 約 0、p1 −1.09%、p01 約 0、MAD/spike 極小 → Equivalent
        let c = lp_raw(0, 100.0, 90.0, 80.0, 5.0, 0.5);
        let r = lp_raw(1, 100.05, 91.0, 80.05, 5.2, 0.52);
        let pairs = identical_pairs(c, r, 5);
        let ev = equivalent_evidence(&pairs);
        // p1 改善 = (90 − 91)/91 × 100 ≈ −1.099%
        assert!(
            (ev.p1_improvement_pct.unwrap() + 1.099).abs() < 0.01,
            "p1={:?}",
            ev.p1_improvement_pct
        );
        assert!(ev.avg_improvement_pct.unwrap().abs() <= 0.5);
        assert!(ev.mad_delta_pp.unwrap().abs() <= 0.5);
        assert!(ev.spike_delta_pp.unwrap().abs() <= 0.10);
        assert!(
            equivalent_finalists(&pairs),
            "截圖型案例應判定為 Equivalent"
        );
    }

    #[test]
    fn equivalent_medians_boundary_in_and_out() {
        // 界線內：五項皆在門檻內 → Equivalent
        let c = lp_raw(0, 100.0, 90.0, 80.0, 5.0, 0.5);
        let r_in = lp_raw(1, 100.4, 91.26, 81.5, 5.4, 0.58);
        let pairs_in = identical_pairs(c.clone(), r_in, 5);
        assert!(equivalent_finalists(&pairs_in), "界線內應 Equivalent");
        // 界線外：avg 差 >0.5%（100 vs 100.6 → −0.596%）→ 非 Equivalent
        let r_out = lp_raw(1, 100.6, 90.0, 80.0, 5.0, 0.5);
        let pairs_out = identical_pairs(c, r_out, 5);
        assert!(!equivalent_finalists(&pairs_out), "avg 超界應非 Equivalent");
    }

    #[test]
    fn equivalent_single_round_severe_regression_forbidden() {
        // 4 輪可忽略 + 1 輪嚴重退步（avg 差 >3%、p1 差 >5%）→ 禁止 Equivalent
        let c_good = lp_raw(0, 100.0, 90.0, 80.0, 5.0, 0.5);
        let r_good = lp_raw(1, 100.05, 90.9, 80.1, 5.1, 0.51);
        let c_bad = lp_raw(0, 100.0, 90.0, 80.0, 5.0, 0.5);
        let r_bad = lp_raw(1, 104.0, 95.0, 84.0, 5.0, 0.5);
        let mut pairs = identical_pairs(c_good, r_good, 4);
        pairs.push((c_bad, r_bad));
        // 中位數仍落在門檻內，但單輪嚴重退步 → 禁制生效
        assert!(
            equivalent_medians_ok(&equivalent_evidence(&pairs)),
            "前置：中位數應仍在門檻內"
        );
        assert!(
            !equivalent_finalists(&pairs),
            "單輪嚴重退步應禁止 Equivalent"
        );
    }

    // ── 7 輪 Inconclusive + 反向預算 ──

    #[test]
    fn evaluate_forward_seven_round_straddle_is_inconclusive() {
        let dir = temp_root("eval_inconclusive");
        let mut round_csvs: RoundCsvs = HashMap::new();
        for i in 0..CONFIRMATION_MAX_ROUNDS {
            let round = CONFIRMATION_ROUND_BASE + i;
            let (cf, rf) = if i % 2 == 0 {
                (10.0, 11.0)
            } else {
                (11.0, 10.0)
            };
            round_csvs
                .entry(0)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 0, cf));
            round_csvs
                .entry(1)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 1, rf));
        }
        // 效應逐輪交替正負 → 各輪皆 Continue（含第 5、7 輪非 Equivalent）→ Inconclusive
        for r in [3u32, 4, 5, 6, 7] {
            assert_eq!(
                evaluate_forward(&round_csvs, 0, 1, r, CONFIRMATION_ROUND_BASE),
                ForwardVerdict::Continue,
                "round {r} 應 Continue"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reverse_max_rounds_respects_budget_and_min_three() {
        assert_eq!(reverse_max_rounds(3), 5);
        assert_eq!(reverse_max_rounds(5), 5);
        assert_eq!(reverse_max_rounds(6), 4);
        assert_eq!(reverse_max_rounds(7), 3, "forward 7 → reverse 最多 3");
        // 總 pair ≤ 10，且反向至少 3（前向 ≤7 必成立）
        for f in CONFIRMATION_MIN_ROUNDS..=CONFIRMATION_MAX_ROUNDS {
            assert!(f + reverse_max_rounds(f) <= TOTAL_PAIR_BUDGET);
            assert!(reverse_max_rounds(f) >= CONFIRMATION_MIN_ROUNDS);
        }
    }

    // ── compute_reliability（verdict → status + evidence）──

    #[test]
    fn compute_reliability_maps_candidate_passed() {
        let dir = temp_root("rel_pass");
        let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 11.0);
        let results = vec![
            lp_res(0, 100.0, 100.0, 100.0, 0.0),
            lp_res(1, 90.909, 90.909, 90.909, 0.0),
        ];
        let rel = compute_reliability(
            &round_csvs,
            &results,
            &[0, 1],
            3,
            Some(ForwardVerdict::CandidatePassed),
            false,
            false,
            0,
        );
        assert_eq!(rel.status, ReliabilityStatus::Passed);
        assert_eq!(rel.candidate_lp, Some(0));
        assert_eq!(rel.runner_up_lp, Some(1));
        assert_eq!(rel.evaluated_rounds, 3);
        assert_eq!(rel.screening_rounds, SCREENING_ROUNDS);
        assert_eq!(rel.confirmation_rounds, 3);
        assert_eq!(rel.stopping_reason, "passed");
        assert_eq!(rel.forward_verdict, "passed");
        assert_eq!(rel.candidate_wins, 3);
        assert_eq!(rel.per_round_winners, vec![Some(0); 3]);
        assert!(rel.composite_advantage_pct.unwrap() > COMPOSITE_ADVANTAGE_MIN_PCT);
        assert!(rel.ci_lower_pct.unwrap() > COMPOSITE_ADVANTAGE_MIN_PCT);
        assert!(!rel.reverse_ran);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compute_reliability_maps_reverse_passed() {
        let dir = temp_root("rel_reverse");
        let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 11.0);
        let results = vec![
            lp_res(0, 100.0, 100.0, 100.0, 0.0),
            lp_res(1, 90.909, 90.909, 90.909, 0.0),
        ];
        let rel = compute_reliability(
            &round_csvs,
            &results,
            &[0, 1],
            3,
            Some(ForwardVerdict::RunnerUpReversal),
            true,
            true,
            3,
        );
        assert_eq!(rel.status, ReliabilityStatus::Passed);
        assert_eq!(rel.stopping_reason, "reverse_passed");
        assert_eq!(rel.forward_verdict, "reversal");
        assert!(rel.reverse_ran);
        assert_eq!(rel.reverse_verdict, "passed");
        assert_eq!(rel.reverse_candidate_lp, Some(1));
        assert_eq!(rel.reverse_rounds, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compute_reliability_maps_equivalent() {
        let dir = temp_root("rel_equiv");
        let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 10.05);
        let results = vec![
            lp_res(0, 100.0, 100.0, 100.0, 0.0),
            lp_res(1, 99.502, 99.502, 99.502, 0.0),
        ];
        let rel = compute_reliability(
            &round_csvs,
            &results,
            &[0, 1],
            3,
            Some(ForwardVerdict::Equivalent),
            false,
            false,
            0,
        );
        assert_eq!(rel.status, ReliabilityStatus::Equivalent);
        assert_eq!(rel.stopping_reason, "equivalent");
        assert_eq!(rel.forward_verdict, "equivalent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compute_reliability_sets_algorithm_version_and_equivalent_evidence() {
        let dir = temp_root("rel_evidence");
        let round_csvs = confirmation_csvs(&dir, 0, 1, 5, 10.0, 10.05);
        let results = vec![
            lp_res(0, 100.0, 100.0, 100.0, 0.0),
            lp_res(1, 99.502, 99.502, 99.502, 0.0),
        ];
        let rel = compute_reliability(
            &round_csvs,
            &results,
            &[0, 1],
            5,
            Some(ForwardVerdict::Equivalent),
            false,
            false,
            0,
        );
        assert_eq!(rel.algorithm_version, 2);
        assert!(rel.equivalent_avg_improvement_pct.is_some());
        assert!(rel.equivalent_p1_improvement_pct.is_some());
        assert!(rel.equivalent_p01_improvement_pct.is_some());
        assert!(rel.equivalent_mad_delta_pp.is_some());
        assert!(rel.equivalent_spike_delta_pp.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compute_reliability_inconclusive_on_reversal_without_reverse_pass() {
        let dir = temp_root("rel_rev_fail");
        let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 11.0);
        let results = vec![
            lp_res(0, 100.0, 100.0, 100.0, 0.0),
            lp_res(1, 90.909, 90.909, 90.909, 0.0),
        ];
        // RunnerUpReversal 但反向驗證未 Passed → Inconclusive，不得套用。
        let rel = compute_reliability(
            &round_csvs,
            &results,
            &[0, 1],
            3,
            Some(ForwardVerdict::RunnerUpReversal),
            true,
            false,
            3,
        );
        assert_eq!(rel.status, ReliabilityStatus::Inconclusive);
        assert_eq!(rel.reverse_verdict, "inconclusive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compute_reliability_ignores_screening_rounds() {
        let dir = temp_root("rel_no_leak");
        let mut round_csvs: RoundCsvs = HashMap::new();
        // 篩選 round（0..SCREENING_ROUNDS）候選較慢；確認 round（base..）候選較快。
        for round in 0..SCREENING_ROUNDS {
            round_csvs
                .entry(0)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 0, 20.0));
            round_csvs
                .entry(1)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 1, 10.0));
        }
        for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + 3) {
            round_csvs
                .entry(0)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 0, 10.0));
            round_csvs
                .entry(1)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 1, 11.0));
        }
        let results = vec![
            lp_res(0, 100.0, 100.0, 100.0, 0.0),
            lp_res(1, 90.909, 90.909, 90.909, 0.0),
        ];
        let rel = compute_reliability(
            &round_csvs,
            &results,
            &[0, 1],
            3,
            Some(ForwardVerdict::CandidatePassed),
            false,
            false,
            0,
        );
        assert_eq!(rel.status, ReliabilityStatus::Passed);
        assert_eq!(rel.candidate_lp, Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compute_reliability_inconclusive_without_two_finalists() {
        let dir = temp_root("rel_one_lp");
        let round_csvs: RoundCsvs = HashMap::new();
        let results = vec![lp_res(0, 100.0, 100.0, 100.0, 0.0)];
        let rel = compute_reliability(&round_csvs, &results, &[], 0, None, false, false, 0);
        assert_eq!(rel.status, ReliabilityStatus::Inconclusive);
        assert_eq!(rel.candidate_lp, None);
        assert_eq!(rel.runner_up_lp, None);
        assert_eq!(rel.stopping_reason, "inconclusive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bootstrap 穩定性區間完全確定：相同輸入兩次呼叫逐位元組相等；常數效應 → 區間
    /// 退化為點；有散布 → 下界 ≤ 點估計 ≤ 上界。
    #[test]
    fn paired_bootstrap_interval_is_deterministic() {
        let e = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let a = paired_bootstrap_interval(&e);
        let b = paired_bootstrap_interval(&e);
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
        assert_eq!(a.2, b.2);
        assert_eq!(a.0, 3.0);
        assert!(a.1 <= a.0 && a.0 <= a.2);
        assert!(a.1 < a.2);
        let (p, lo, hi) = paired_bootstrap_interval(&[0.25, 0.25, 0.25]);
        assert_eq!(p, 0.25);
        assert_eq!(lo, 0.25);
        assert_eq!(hi, 0.25);
    }

    #[test]
    fn cancel_before_start_marks_cancelled_and_restores() {
        let root = temp_root("cancel0");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        let cancel = FakeCancel::new();
        cancel.set(true); // 一開始就取消

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Cancelled);
        assert!(processes.spawn_log().is_empty(), "取消時不該啟動 workload");
        assert_eq!(backend.current_policy(GPU_A), baseline, "策略不得被改動");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_during_lp_kills_owned_processes_and_restores() {
        let root = temp_root("cancelmid");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = Arc::new(FakeCancel::new());
        // 環境閘（5×1000ms）+ restart 穩定（5000ms）之後、warmup 期間的 13000ms 處取消
        let sleeper = Arc::new(CancelAfterSleeper::new(cancel.clone(), 13000));

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            cancel as Arc<dyn CancelSignal>,
            sleeper.clone() as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Cancelled);
        // 取消在 warmup 期間（16000ms 前）就被偵測，未睡滿 stabilize+warmup
        assert!(sleeper.elapsed_ms() < 16000);
        // workload 已啟動，取消後必須被終止
        assert!(processes
            .spawn_log()
            .iter()
            .any(|(n, _, _)| !n.contains("PresentMon")));
        assert!(
            !processes.killed_log().is_empty(),
            "取消時必須終止 owned 子程序"
        );
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 取消清理階段以有序、單調（0..100）的 cancel_progress 事件回報：
    /// stopping → restoring → finalizing，且不碰 benchmark `percentage` 語意。
    #[test]
    fn cancel_cleanup_emits_ordered_progress() {
        let root = temp_root("cancelprogress");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = Arc::new(FakeCancel::new());
        // 環境閘（5×1000ms）+ restart 穩定（5000ms）之後、warmup 期間的 13000ms 處取消
        let sleeper = Arc::new(CancelAfterSleeper::new(cancel.clone(), 13000));

        let events: std::sync::Mutex<Vec<BenchmarkProgress>> = std::sync::Mutex::new(Vec::new());
        let ev = std::sync::Arc::new(events);
        let ev_clone = ev.clone();
        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            cancel as Arc<dyn CancelSignal>,
            sleeper.clone() as Arc<dyn Sleep>,
            base_config(),
            &journal,
            Some(Box::new(move |p| {
                ev_clone.lock().unwrap().push(p.clone());
            })),
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Cancelled);
        // 提取取消事件（有 cancel_stage 者）
        let cancels: Vec<(String, u32)> = ev
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| {
                p.cancel_stage
                    .as_ref()
                    .map(|s| (s.clone(), p.cancel_progress.unwrap_or(0)))
            })
            .collect();
        let stages: Vec<&str> = cancels.iter().map(|(s, _)| s.as_str()).collect();
        let pos = |s: &str| {
            stages
                .iter()
                .position(|x| *x == s)
                .unwrap_or_else(|| panic!("缺少取消階段 {s}: {stages:?}"))
        };
        assert!(pos("stopping") < pos("restoring"));
        assert!(pos("restoring") < pos("finalizing"));
        // 百分比 0..100 且單調不倒退
        let mut last = 0u32;
        for (_s, pct) in &cancels {
            assert!(*pct <= 100, "取消百分比不可超過 100: {pct}");
            assert!(*pct >= last, "取消百分比不可倒退: {pct} < {last}");
            last = *pct;
        }
        assert_eq!(last, 100, "取消最終進度應達 100");
        // 取消事件不得改寫 benchmark percentage（維持 0，不冒充 benchmark 進度）
        for p in ev
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.cancel_stage.is_some())
        {
            assert_eq!(p.percentage, 0, "取消事件 percentage 應保持 0");
        }
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn presentmon_command_uses_process_id() {
        // PresentMon 必須以 -process_id 篩選已 spawn 的 workload PID，確保 Vulkan workload
        // 正確收集 present 事件（-process_name 在此情境不建立 CSV）
        let cfg = base_config();
        let args = presentmon_command(
            cfg.sample_secs,
            PRESENTMON_CIRCULAR_BUFFER_SIZE,
            1234,
            Path::new("x.csv"),
            "test-session",
        );
        let id_idx = args.iter().position(|a| a == "--process_id").unwrap();
        assert_eq!(args[id_idx + 1], "1234");
        assert!(
            !args.iter().any(|a| a == "--process_name"),
            "不該使用 -process_name"
        );
        // 仍有 output/timed
        assert!(args.iter().any(|a| a == "--output_file"));
        assert!(args.iter().any(|a| a == "--timed"));
    }

    #[test]
    fn presentmon_command_uses_process_id_for_d3d9() {
        let mut cfg = base_config();
        cfg.workload = WorkloadKind::D3D9;
        let args = presentmon_command(
            cfg.sample_secs,
            PRESENTMON_CIRCULAR_BUFFER_SIZE,
            5678,
            Path::new("x.csv"),
            "test-session",
        );
        let id_idx = args.iter().position(|a| a == "--process_id").unwrap();
        assert_eq!(args[id_idx + 1], "5678");
        assert!(
            !args.iter().any(|a| a == "--process_name"),
            "不該使用 -process_name"
        );
    }

    #[test]
    fn validate_config_rejects_zero_sample_secs_but_ignores_repetitions() {
        let t = topo();
        let mut c = base_config();
        c.sample_secs = 0;
        assert_eq!(
            validate_config(&c, &t).unwrap_err(),
            codes::BENCHMARK_INVALID_CONFIG
        );
        c.sample_secs = 3;
        // 新排程固定 3 篩選 + 2 refinement + 3..=7 確認，`repetitions` 欄位被忽略（保留供舊 session 相容）。
        for legacy in [2u32, 3, 4, 5, 6, 7, 8] {
            c.repetitions = legacy;
            assert!(
                validate_config(&c, &t).is_ok(),
                "repetitions={legacy} 應被忽略"
            );
        }
    }

    #[test]
    fn presentmon_command_includes_stale_session_cleanup() {
        // 上游 AutoGpuAffinity 語意：先停掉殘留 ETL session，避免 stale session 卡住 capture
        let cfg = base_config();
        let args = presentmon_command(
            cfg.sample_secs,
            PRESENTMON_CIRCULAR_BUFFER_SIZE,
            1234,
            Path::new("x.csv"),
            "test-session",
        );
        assert!(
            args.iter().any(|a| a == "--stop_existing_session"),
            "必須含 -stop_existing_session"
        );
        assert!(
            args.iter().any(|a| a == "--no_console_stats"),
            "必須含 --no_console_stats"
        );
        // -terminate_after_timed 讓 PresentMon 收集完自行退出，runner 才能有界等待
        assert!(args.iter().any(|a| a == "--terminate_after_timed"));
        assert!(args
            .windows(2)
            .any(|w| { w[0] == "--session_name" && w[1] == "test-session" }));
        assert!(args.iter().any(|a| a == "--v1_metrics"));
        assert!(args.windows(2).any(|w| {
            w[0] == "--set_circular_buffer_size"
                && w[1] == PRESENTMON_CIRCULAR_BUFFER_SIZE.to_string()
        }));
    }

    /// 低負載追蹤：停用 GPU/input（統計不需要），保留 display（frame 來源）。
    /// 停用 display 會使 CSV 完全無法建立（本次 regression 根因）。
    #[test]
    fn presentmon_command_uses_low_overhead_tracking() {
        let cfg = base_config();
        let args = presentmon_command(
            cfg.sample_secs,
            PRESENTMON_CIRCULAR_BUFFER_SIZE,
            1234,
            Path::new("x.csv"),
            "test-session",
        );
        assert!(
            args.iter().any(|a| a == "--no_track_gpu"),
            "必須停用 GPU 追蹤以降低 ETW 負載: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "--no_track_input"),
            "必須停用 input 追蹤以降低 ETW 負載: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--no_track_display"),
            "不得停用 display 追蹤（唯一 frame 來源）: {args:?}"
        );
    }

    #[test]
    fn presentmon_captures_by_process_id_from_spawned_workload() {
        let root = temp_root("pmpid");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "err={:?}",
            result.error
        );

        let log = processes.spawn_log();
        // 依 spawn 順序：每個 PresentMon 用 -process_id 對應已 spawn 的 workload PID
        let mut workload_pids: Vec<u32> = Vec::new();
        let mut presentmon_seen = 0u32;
        for (name, pid, args) in &log {
            if name.contains("PresentMon") {
                presentmon_seen += 1;
                let id_idx = args.iter().position(|a| a == "--process_id").unwrap();
                let pm_filter_pid: u32 = args[id_idx + 1].parse().unwrap();
                assert!(
                    workload_pids.contains(&pm_filter_pid),
                    "PresentMon -process_id {pm_filter_pid} 必須對應已 spawn 的 workload PID"
                );
                assert!(
                    !args.iter().any(|a| a == "--process_name"),
                    "不該使用 -process_name"
                );
            } else {
                workload_pids.push(*pid);
            }
        }
        assert!(presentmon_seen >= 1, "至少一次 PresentMon capture");
        assert!(!workload_pids.is_empty(), "每個 round 都要啟動 workload");
        // 連續 capture 每個都新鮮有效：capture 後清理
        for wl_pid in &workload_pids {
            assert!(
                processes.killed_log().contains(wl_pid),
                "workload {wl_pid} 必須被清理"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workload_launch_failure_fails_and_restores() {
        let root = temp_root("wlfail");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes.fail_workload.store(true, Ordering::SeqCst);
        let cancel = FakeCancel::new();

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_WORKLOAD_FAILED)
        );
        assert_eq!(
            backend.current_policy(GPU_A),
            baseline,
            "失敗後必須還原原始策略"
        );
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_csv_fails_with_partial_results_and_no_recommendation() {
        let root = temp_root("nocsvar");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // LP 1 有資料；LP 2/3 的 PresentMon spawn 失敗 → 該 LP 無 CSV
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        processes.fail_lp(2);
        processes.fail_lp(3);
        let cancel = FakeCancel::new();

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(), // candidate_lps 1,2,3
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(result.best_lp, None, "失敗不該有推薦");
        // 部分結果保留（LP 1 有完成）
        assert!(!result.detail.results.is_empty());
        // 部分 CSV 檔案保留
        let session_dir = ctx.storage_root.join(&ctx.session_id);
        let files = std::fs::read_dir(&session_dir).unwrap().count();
        assert!(files >= 1, "partial CSV 應保留");
        assert_eq!(
            backend.current_policy(GPU_A),
            baseline,
            "失敗後必須還原原始策略"
        );
        assert!(!journal.exists(), "還原成功應清日誌");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_csv_content_fails_session() {
        let root = temp_root("badcsv");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        // 垃圾 CSV（無 msBetweenPresents 欄）→ capture 驗證即失敗
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str("garbage,not,csv\n1,2,3\n");
        let cancel = FakeCancel::new();

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_EMPTY)
        );
        assert_eq!(result.best_lp, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_failure_keeps_journal_and_marks_recovery_required() {
        let root = temp_root("restorefail");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        // 終結還原的 restart 一直失敗
        backend.disable_fails.store(true, Ordering::SeqCst);

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        // 第一 LP 的 apply restart 就失敗 → 已寫入策略，還原 restart 也失敗
        assert_eq!(result.status, SessionStatus::Failed);
        assert!(result.recovery_required, "還原失敗必須要求 recovery");
        assert!(journal.exists(), "還原失敗必須保留日誌");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn progress_events_emitted_with_stages() {
        let root = temp_root("progress");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let events: std::sync::Mutex<Vec<BenchmarkProgress>> = std::sync::Mutex::new(Vec::new());
        let ev = std::sync::Arc::new(events);
        let ev_clone = ev.clone();

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            Some(Box::new(move |p| {
                ev_clone.lock().unwrap().push(p.clone());
            })),
        );
        run_benchmark(&mut ctx);
        let stages: Vec<String> = ev.lock().unwrap().iter().map(|p| p.stage.clone()).collect();
        assert!(stages.contains(&"applying".to_string()));
        assert!(stages.contains(&"collecting".to_string()));
        assert!(stages.contains(&"finalizing".to_string()));
        assert!(stages.iter().all(|s| !s.is_empty()));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── PresentMon capture 可靠性回歸測試 ────────────────────────────────

    #[test]
    fn presentmon_timeout_fails_and_restores_with_persisted_error() {
        let root = temp_root("pmtimeout");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // PresentMon 卡住：wait_exit 一直回傳「未退出」
        processes.presentmon_timeout.store(true, Ordering::SeqCst);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_PRESENTMON_TIMEOUT)
        );
        // 卡住的 PresentMon 與 workload 都必須被終止
        assert!(!processes.killed_log().is_empty());
        // 策略還原、日誌清除
        assert_eq!(backend.current_policy(GPU_A), baseline);
        assert!(!journal.exists());
        // 失敗原因已持久化（reload 後 UI 可顯示）
        let detail = storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap();
        assert_eq!(
            detail.summary.error.as_deref(),
            Some(codes::BENCHMARK_PRESENTMON_TIMEOUT)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn presentmon_no_output_file_fails() {
        let root = temp_root("nofile");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // PresentMon 正常退出但沒寫出 CSV（stale session / 依賴缺失）
        processes
            .presentmon_write_csv
            .store(false, Ordering::SeqCst);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_MISSING)
        );
        assert_eq!(result.best_lp, None, "缺檔不該有推薦");
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原策略");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn presentmon_header_only_output_fails_as_empty() {
        let root = temp_root("emptycsv");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // 只有 header、沒有任何 frametime 資料
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str("Application,ProcessID,msBetweenPresents\n");
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_EMPTY)
        );
        assert_eq!(backend.current_policy(GPU_A), baseline);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 核心回歸：stale CSV（上一個 session 殘留、格式看似有效）絕不能被當成
    /// 本次 capture 的輸出。capture 前必須先清除 stale 檔，本次沒產出新檔即失敗。
    #[test]
    fn stale_csv_cannot_be_mistaken_for_fresh_output() {
        let root = temp_root("stale");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // 本次 capture 不產出任何新檔
        processes
            .presentmon_write_csv
            .store(false, Ordering::SeqCst);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        // 在目標路徑預放「看似有效」的 stale CSV（50 個 frametime）
        let session_dir = ctx.storage_root.join(&ctx.session_id);
        let csv = session_dir.join("round-0-lp-1.csv");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(&csv, csv_for_lp(0)).unwrap();

        let result = run_benchmark(&mut ctx);
        // stale 不能讓 session「成功」：沒有新鮮輸出 → 失敗
        assert_eq!(
            result.status,
            SessionStatus::Failed,
            "stale CSV 不得被當成成功輸出"
        );
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_MISSING)
        );
        // stale 檔已在 capture 前被確定性清除，不會殘留在最終 session
        assert!(!csv.exists(), "stale CSV 必須在 capture 前被刪除");
        assert_eq!(result.best_lp, None);
        assert_eq!(backend.current_policy(GPU_A), baseline);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 多個連續 LP capture：每個 LP 都必須有「新鮮有效」的 CSV 才算成功；
    /// 前一個 LP 的輸出不能讓後續 LP 誤判。LP1 有效、LP3 無輸出 → 失敗且保留 LP1/LP2 部分結果。
    #[test]
    fn sequential_lp_captures_require_fresh_valid_csv_each() {
        let root = temp_root("seqfresh");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // 對所有 LP 都寫有效 CSV；但 LP3 的 PresentMon 不產出檔案
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        processes.fail_lp(3); // LP3 PresentMon spawn 失敗 → 該 LP 無新鮮 CSV
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1, 2, 3];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_PRESENTMON_FAILED)
        );
        // LP1、LP2 已驗證的部分結果保留；LP3 未完成
        assert!(
            result.detail.results.iter().all(|r| r.completed),
            "保留的部分結果必須都已完成"
        );
        assert!(result.detail.results.len() < 3, "LP3 失敗不該有結果");
        assert_eq!(result.best_lp, None);
        assert_eq!(backend.current_policy(GPU_A), baseline);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 讀取某 capture 的診斷檔
    fn read_diag(session_dir: &Path, round: u32, lp: u32) -> CaptureDiagnostics {
        let p = session_dir
            .join("diag")
            .join(format!("capture-round-{round}-lp-{lp}.json"));
        let text =
            std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("診斷檔讀取失敗 {p:?}: {e}"));
        serde_json::from_str(&text).unwrap()
    }

    /// 成功 capture 必須寫出診斷：workload 全程存活、PM 正常退出、CSV 存在。
    #[test]
    fn success_capture_writes_diagnostics() {
        let root = temp_root("diagok");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "err={:?}",
            result.error
        );

        let session_dir = ctx.storage_root.join(&ctx.session_id);
        let d = read_diag(&session_dir, 0, 1);
        assert_eq!(d.round, 0);
        assert_eq!(d.lp, 1);
        assert!(!d.started_at.is_empty(), "要有起始時間戳");
        assert!(d.finished_at.is_some(), "要有結束時間戳");
        assert_ne!(d.workload_pid, 0);
        assert!(d.workload_alive_before_pm, "啟動 PM 前 workload 必須活著");
        assert!(
            d.workload_alive_after_capture,
            "capture 後 workload 應仍活著"
        );
        assert_eq!(d.workload_exit_code, None, "存活中不該有 exit code");
        assert_ne!(d.presentmon_pid, 0);
        assert_eq!(d.presentmon_exit_code, Some(0), "PM 正常退出 exit code 0");
        assert!(d.wait_completed);
        assert!(!d.wait_timed_out);
        assert_eq!(d.wait_error, None);
        assert!(d.csv_exists, "成功 capture 必須有 CSV");
        assert!(d.csv_size_bytes > 0);
        assert_eq!(d.error, None);
        // bounded output tail 有被擷取（fake 有記錄）
        assert!(d.presentmon_stderr.contains("fake-presentmon-stderr"));
        // 診斷要記住 PresentMon 的篩選種類與值（未來 session 才看得出匹配目標）
        assert_eq!(d.capture_filter_kind, "process_id");
        assert!(
            !d.capture_filter_value.is_empty(),
            "capture_filter_value 必須記錄 PID"
        );
        let _: u32 = d
            .capture_filter_value
            .parse()
            .expect("capture_filter_value 必須為十進位 PID");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 缺檔失敗（presentmon 正常退出但沒產出 CSV）也要寫診斷，且 session.json
    /// 仍可正常讀取（診斷檔不影響既有匯入/相容性）。
    #[test]
    fn missing_output_writes_diagnostics_and_keeps_session_readable() {
        let root = temp_root("diagmiss");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_write_csv
            .store(false, Ordering::SeqCst);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_MISSING)
        );

        // 診斷檔在失敗後仍存在
        let session_dir = ctx.storage_root.join(&ctx.session_id);
        let d = read_diag(&session_dir, 0, 1);
        assert!(d.workload_alive_before_pm, "啟動 PM 前 workload 活著");
        assert!(d.workload_alive_after_capture);
        assert_eq!(d.presentmon_exit_code, Some(0), "PM 正常退出但無輸出");
        assert!(d.wait_completed);
        assert!(!d.wait_timed_out);
        assert!(!d.csv_exists, "缺檔：csv 不該存在");
        assert_eq!(d.csv_size_bytes, 0);
        assert_eq!(d.error.as_deref(), Some(codes::BENCHMARK_CAPTURE_MISSING));
        // 失敗路徑也保留篩選資訊（PM 用 -process_id 對應的 workload PID）
        assert_eq!(d.capture_filter_kind, "process_id");
        assert!(
            !d.capture_filter_value.is_empty(),
            "capture_filter_value 必須記錄 PID"
        );
        let _: u32 = d
            .capture_filter_value
            .parse()
            .expect("capture_filter_value 必須為十進位 PID");

        // session.json 仍可正常讀取（診斷檔不破壞舊相容性）
        let detail = storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap();
        assert_eq!(detail.summary.status, SessionStatus::Failed);
        assert_eq!(
            detail.summary.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_MISSING)
        );
        // list 也照常（診斷檔只是多餘檔案）
        let summaries = storage::list_at(&ctx.storage_root).unwrap();
        assert!(summaries.iter().any(|s| s.id == ctx.session_id));
        // 診斷檔計入總位元組（dir_size 掃全部檔案）
        assert!(detail.summary.total_bytes > 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// PresentMon 逾時卡住也寫診斷：wait_timed_out=true、PM exit code 未知。
    #[test]
    fn presentmon_timeout_writes_diagnostics() {
        let root = temp_root("diagtimeout");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes.presentmon_timeout.store(true, Ordering::SeqCst);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_PRESENTMON_TIMEOUT)
        );

        let session_dir = ctx.storage_root.join(&ctx.session_id);
        let d = read_diag(&session_dir, 0, 1);
        assert!(d.wait_completed);
        assert!(d.wait_timed_out, "逾時必須記錄");
        assert_eq!(d.presentmon_exit_code, None, "未退出不該有 exit code");
        assert_eq!(
            d.error.as_deref(),
            Some(codes::BENCHMARK_PRESENTMON_TIMEOUT)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 相容性：session 資料夾裡有 diag 檔案時，get/list/delete 照常運作，
    /// 且不含 diag 的舊 session.json 仍可載入（diag 是獨立檔案，不進 schema）。
    #[test]
    fn diagnostics_files_do_not_break_old_session_compat() {
        let root = temp_root("diagcompat");
        let storage_root = root.join("benchmarks");
        let id = Uuid::new_v4().to_string();
        let detail = SessionDetail {
            summary: crate::benchmark::SessionSummary {
                id: id.clone(),
                status: SessionStatus::Failed,
                started_at: "2026-08-11T00:00:00Z".into(),
                finished_at: Some("2026-08-11T00:01:00Z".into()),
                gpu_name: "Fake GPU".into(),
                gpu_instance_id: GPU_A.to_string(),
                cpu_fingerprint: "fixture".into(),
                best_lp: None,
                reliability: ReliabilitySummary::default(),
                severe_lps: vec![],
                sample_count: 0,
                total_bytes: 0,
                config: base_config(),
                error: Some(codes::BENCHMARK_CAPTURE_MISSING.to_string()),
                ..Default::default()
            },
            results: vec![],
            samples: vec![],
            ..Default::default()
        };
        storage::save_session_at(&storage_root, &detail).unwrap();
        // 放診斷檔（模擬新版本寫入）
        let diag_dir = storage_root.join(&id).join("diag");
        std::fs::create_dir_all(&diag_dir).unwrap();
        std::fs::write(
            diag_dir.join("capture-round-0-lp-0.json"),
            r#"{"round":0,"lp":0,"csvExists":false}"#,
        )
        .unwrap();

        let loaded = storage::get_at(&storage_root, &id).unwrap();
        assert_eq!(loaded.summary.status, SessionStatus::Failed);
        assert_eq!(
            loaded.summary.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_MISSING)
        );
        let list = storage::list_at(&storage_root).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].total_bytes > 0, "診斷檔計入 total_bytes");
        storage::delete_at(&storage_root, &id).unwrap();
        assert!(!storage_root.join(&id).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── capture retry 回歸測試 ─────────────────────────────────────────────

    /// 第一次 capture MISSING → retry 後用新 workload PID 成功 → session Completed
    #[test]
    fn retry_missing_recovers_on_second_attempt() {
        let root = temp_root("retry_ok");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        // LP 1 第一次 missing（不寫 CSV），第二次成功
        processes.first_attempt_missing.lock().unwrap().insert(1);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "retry 後應成功: err={:?}",
            result.error
        );
        // 單一 LP（N=1）無確認階段 → Inconclusive，無 best_lp；此測試重點是 retry 回收
        assert_eq!(result.best_lp, None);
        // 單一 LP 只跑一輪短篩：初次套用 + capture recovery 各重啟一次，
        // 加上終結還原 1 次 = 3 次。
        assert_eq!(
            backend.restart_count(),
            3,
            "missing capture retry 必須先重新啟動 GPU，再建立新 workload"
        );
        // 一輪中有兩個 workload PID（attempt 1 + retry）。
        let log = processes.spawn_log();
        let wl_pids: Vec<u32> = log
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .map(|(_, p, _)| *p)
            .collect();
        assert_eq!(wl_pids.len(), 2, "第一次 + retry 應各建立一個 workload PID");
        for p in &wl_pids {
            assert!(
                processes.killed_log().contains(p),
                "workload {p} 必須被清理"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 所有 capture attempt 都 MISSING → 最終失敗，無部分結果且 sample_count 為 0
    #[test]
    fn retry_missing_both_fails_cleanly() {
        let root = temp_root("retry_fail");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        // LP 1: 第一次與所有 retry 都 missing
        processes.first_attempt_missing.lock().unwrap().insert(1);
        processes
            .second_attempt_also_missing
            .lock()
            .unwrap()
            .insert(1);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_MISSING)
        );
        assert_eq!(result.best_lp, None, "失敗不該有推薦");
        assert!(
            result.detail.results.is_empty(),
            "無 LP 成功，不該有部分結果"
        );
        assert_eq!(result.detail.summary.sample_count, 0);
        // 每次 attempt 各 spawn 一個 workload
        let log = processes.spawn_log();
        let wl_pids: Vec<u32> = log
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .map(|(_, p, _)| *p)
            .collect();
        assert_eq!(
            wl_pids.len(),
            MAX_CAPTURE_ATTEMPTS as usize,
            "每次 capture attempt 各一個 workload"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 尚無任何成功 capture 時，第一個候選 LP 經所有 retry 仍 MISSING →
    /// fail-fast：立即終止 session，不再跑剩餘 LP/round，且進入 cleanup/restore。
    #[test]
    fn first_lp_missing_fails_fast_without_running_remaining_lps() {
        let root = temp_root("ff_missing");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // LP 1 所有 attempt 都 MISSING
        processes.first_attempt_missing.lock().unwrap().insert(1);
        processes
            .second_attempt_also_missing
            .lock()
            .unwrap()
            .insert(1);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1, 2, 3];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_MISSING)
        );
        assert_eq!(result.best_lp, None);
        assert!(
            result.detail.results.is_empty(),
            "無 LP 成功，不該有部分結果"
        );
        // 只測第一個 LP（其 MAX_CAPTURE_ATTEMPTS 次），後續 LP 不得 spawn workload
        let wl_spawns = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .count();
        assert_eq!(
            wl_spawns, MAX_CAPTURE_ATTEMPTS as usize,
            "fail-fast：僅第一個 LP 的 attempts，不跑剩餘 LP"
        );
        // 進入既有 cleanup/restore
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原策略");
        assert!(!journal.exists(), "還原成功應清日誌");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 已有成功 capture（LP 1 完成）時，後續 LP（LP 2）經所有 retry 仍 MISSING →
    /// 隔離該 LP 並繼續，保留已收集的部分結果，最終 Failed 且無推薦。
    #[test]
    fn later_lp_missing_isolates_and_continues() {
        let root = temp_root("isolate_missing");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // LP 1 有資料；LP 2 所有 attempt 都 MISSING
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        processes.first_attempt_missing.lock().unwrap().insert(2);
        processes
            .second_attempt_also_missing
            .lock()
            .unwrap()
            .insert(2);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1, 2];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_MISSING)
        );
        assert_eq!(result.best_lp, None, "部分失敗不該有推薦");
        assert!(
            !result.detail.results.is_empty(),
            "已有成功 capture 應保留部分結果"
        );
        // LP 1 每 round 一次成功（3 篩選 round）+ LP 2 每 round 三次 attempt（3 篩選 round）
        let wl_spawns = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .count();
        assert_eq!(
            wl_spawns,
            SCREENING_ROUNDS as usize + SCREENING_ROUNDS as usize * MAX_CAPTURE_ATTEMPTS as usize,
            "LP1 每 round 一次 + LP2 每 round 三次 attempts"
        );
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原策略");
        assert!(!journal.exists(), "還原成功應清日誌");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 第一次 capture EMPTY（僅 header）→ retry 成功
    #[test]
    fn retry_empty_recovers_on_second_attempt() {
        let root = temp_root("retry_empty");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        // full CSV for retry
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        // LP 1: 第一次 empty（header-only）
        processes.first_attempt_empty.lock().unwrap().insert(1);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "empty retry 後應成功: err={:?}",
            result.error
        );
        // 單一 LP（N=1）無確認階段 → Inconclusive，無 best_lp；此測試重點是 empty retry 回收
        assert_eq!(result.best_lp, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// PresentMon spawn 失敗不觸發 retry（非 MISSING/EMPTY）
    #[test]
    fn presentmon_spawn_failure_does_not_retry() {
        let root = temp_root("noretry_pmfail");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        // PresentMon spawn 失敗 → BENCHMARK_PRESENTMON_FAILED，不該 retry
        processes.fail_lp(1);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_PRESENTMON_FAILED)
        );
        // 只有一次 PresentMon spawn（沒 retry）
        let pm_spawns = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| n.contains("PresentMon"))
            .count();
        assert_eq!(pm_spawns, 1, "PresentMon spawn 失敗不該 retry");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ETW events lost 訊號的純函式偵測：大小寫、變體、誤判防護。
    #[test]
    fn stderr_etw_loss_detection() {
        // positive：數量前綴 / "Lost N" / 無數量 "were lost" / 大小寫變體
        assert!(stderr_has_etw_loss("Lost 9000000 ETW events"));
        assert!(stderr_has_etw_loss("ETW events lost"));
        assert!(stderr_has_etw_loss("123 ETW events lost"));
        assert!(stderr_has_etw_loss("Lost 123 ETW events"));
        assert!(stderr_has_etw_loss("ETW events were lost"));
        assert!(stderr_has_etw_loss("warning: events lost during capture"));
        assert!(stderr_has_etw_loss("Lost 123 events"));
        assert!(stderr_has_etw_loss("123 ETW EVENTS LOST"));
        assert!(stderr_has_etw_loss("EtW EvEnTs WeRe LoSt"));
        // negative：明確零遺失 / 否定表述不得誤判
        assert!(!stderr_has_etw_loss("0 ETW events lost"));
        assert!(!stderr_has_etw_loss("no ETW events lost"));
        assert!(!stderr_has_etw_loss("no events were lost"));
        assert!(!stderr_has_etw_loss("lost 0 events"));
        assert!(!stderr_has_etw_loss("without events lost"));
        assert!(!stderr_has_etw_loss("fake-presentmon-stderr"));
        assert!(!stderr_has_etw_loss(""));
        assert!(!stderr_has_etw_loss("etw session started"));
        assert!(!stderr_has_etw_loss("no lost here"));
        // 混合否定／肯定子句：否定只在其子句內生效，不得抑制另一子句的真正 loss
        assert!(stderr_has_etw_loss(
            "no events were lost; 9000 ETW events lost"
        ));
        assert!(stderr_has_etw_loss("0 events lost, 5000 ETW events lost"));
        assert!(stderr_has_etw_loss("error code 0; lost 5 ETW events"));
        assert!(stderr_has_etw_loss("lost 0 events; lost 9000 ETW events"));
        assert!(!stderr_has_etw_loss("no events lost; no ETW events lost"));
    }

    /// 解析 overflowed present events 數量（實測 PresentMon 措辭）。
    #[test]
    fn parse_overflowed_present_events_extracts_count() {
        let real = "warning: 47123 overflowed present events detected. This could be due to a high-fps application.\nConsider increasing the present event circular buffer size to a value larger than the default of 2048, e.g., --set_circular_buffer_size 4096.";
        assert_eq!(parse_overflowed_present_events(real), Some(47123));
        assert_eq!(
            parse_overflowed_present_events(
                "warning: 131072 overflowed present events detected. This could be due to a high-fps application."
            ),
            Some(131072)
        );
        assert_eq!(
            parse_overflowed_present_events("0 overflowed present events detected"),
            None
        );
        assert_eq!(
            parse_overflowed_present_events("no overflowed present events"),
            None
        );
        assert_eq!(
            parse_overflowed_present_events("fake-presentmon-stderr"),
            None
        );
        assert_eq!(parse_overflowed_present_events(""), None);
    }

    /// 解析 ETW events/buffers lost 數量（實測 PresentMon 措辭）。
    #[test]
    fn parse_etw_events_lost_extracts_count() {
        assert_eq!(
            parse_etw_events_lost("warning: 9000000 ETW events were lost."),
            Some(9000000)
        );
        assert_eq!(
            parse_etw_events_lost("warning: 3 ETW buffers were lost."),
            Some(3)
        );
        assert_eq!(parse_etw_events_lost("0 ETW events lost"), None);
        assert_eq!(parse_etw_events_lost("no events were lost"), None);
    }

    /// calibration_buffer：`max(8192, next_power_of_two(cap*8))`。
    #[test]
    fn calibration_buffer_sizes_correctly() {
        assert_eq!(calibration_buffer(240), 8192);
        assert_eq!(calibration_buffer(500), 8192);
        assert_eq!(calibration_buffer(1000), 8192);
        assert_eq!(calibration_buffer(2000), 16384);
        assert_eq!(calibration_buffer(4000), 32768);
    }

    /// 校準決策：增益 <10% 選「剛測試的新 tier」，非前一 tier。
    #[test]
    fn calibration_selects_new_clean_tier_when_gain_below_threshold() {
        // 前一 clean tier FPS=240、新 tier 500 FPS=250（增益 ~4.2% <10%）→ 選 500。
        assert_eq!(
            calibration_clean_decision(Some(240.0), 500, 250.0, false),
            Some(500)
        );
        // 增益 >10% → 繼續（None）。
        assert_eq!(
            calibration_clean_decision(Some(240.0), 500, 300.0, false),
            None
        );
        // 最後一個 tier clean → 選它（不管增益）。
        assert_eq!(
            calibration_clean_decision(Some(2000.0), 4000, 2100.0, true),
            Some(4000)
        );
        // 首 tier（無前一 clean FPS）→ 繼續。
        assert_eq!(calibration_clean_decision(None, 240, 240.0, false), None);
    }

    /// 有效 CSV + overflow warning → 拒絕（與 CSV 內容無關）。
    #[test]
    fn valid_csv_with_overflow_warning_is_rejected() {
        let dir = temp_root("integrity_overflow");
        let csv = dir.join("round-0-lp-1.csv");
        std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,0.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n\"w\",1,10.0,3.0\n",
        )
        .unwrap();
        let integ = assess_capture_integrity(&csv, 3, 47123, false);
        assert_eq!(
            integ.code.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_OVERFLOW)
        );
        assert_eq!(integ.reason.as_deref(), Some("overflowed_present_events"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 有效 CSV + ETW lost → 拒絕。
    #[test]
    fn valid_csv_with_etw_loss_is_rejected() {
        let dir = temp_root("integrity_etw");
        let csv = dir.join("round-0-lp-1.csv");
        std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,0.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n\"w\",1,10.0,3.0\n",
        )
        .unwrap();
        let integ = assess_capture_integrity(&csv, 3, 0, true);
        assert_eq!(
            integ.code.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_ETW_LOST)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 觀測時長 < 95% sample_secs → CSV_INVALID（duration）。
    #[test]
    fn capture_duration_below_95pct_is_rejected() {
        let dir = temp_root("integrity_duration");
        let csv = dir.join("round-0-lp-1.csv");
        std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,0.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n",
        )
        .unwrap();
        let integ = assess_capture_integrity(&csv, 3, 0, false);
        assert_eq!(integ.code.as_deref(), Some(codes::BENCHMARK_CSV_INVALID));
        assert_eq!(integ.reason.as_deref(), Some("duration"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非單調 capture 時間 → CSV_INVALID（monotonic）。
    #[test]
    fn non_monotonic_capture_time_is_rejected() {
        let dir = temp_root("integrity_monotonic");
        let csv = dir.join("round-0-lp-1.csv");
        std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,3.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n",
        )
        .unwrap();
        let integ = assess_capture_integrity(&csv, 3, 0, false);
        assert_eq!(integ.code.as_deref(), Some(codes::BENCHMARK_CSV_INVALID));
        assert_eq!(integ.reason.as_deref(), Some("monotonic"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 完整有效 CSV（含 TimeInSeconds，時長 ≥95%）→ 通過，duration 記錄。
    #[test]
    fn valid_csv_passes_integrity_with_duration() {
        let dir = temp_root("integrity_ok");
        let csv = dir.join("round-0-lp-1.csv");
        std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,0.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n\"w\",1,10.0,3.0\n",
        )
        .unwrap();
        let integ = assess_capture_integrity(&csv, 3, 0, false);
        assert_eq!(integ.code, None);
        assert_eq!(integ.reason, None);
        assert_eq!(integ.duration_secs, Some(3.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ETW events lost + 無 CSV → 專用錯誤、不可重試、fail-fast（後續 LP 不跑），
    /// 且進入既有 cleanup/restore（策略還原、日誌清除），診斷反映真實策略。
    #[test]
    fn etw_loss_fails_fast_and_does_not_retry() {
        let root = temp_root("etw_failfast");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes.presentmon_etw_loss.store(true, Ordering::SeqCst);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1, 2, 3];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_ETW_LOST)
        );
        // 不可重試：只有一次 PresentMon spawn（ETW loss 不進 retry loop）
        let pm_spawns = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| n.contains("PresentMon"))
            .count();
        assert_eq!(pm_spawns, 1, "ETW loss 不得重試");
        // fail-fast：只測第一個 LP（round 0 的 LP1），後續 LP 不得繼續
        let wl_spawns = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .count();
        assert_eq!(wl_spawns, 1, "後續 LP 不得繼續");
        assert!(
            result.detail.results.is_empty(),
            "無 LP 成功，不該有部分結果"
        );
        // 進入既有 cleanup/restore
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原策略");
        assert!(!journal.exists(), "還原成功應清日誌");
        // 診斷反映真實策略
        let session_dir = ctx.storage_root.join(&ctx.session_id);
        let d = read_diag(&session_dir, 0, 1);
        assert!(d.etw_events_lost, "診斷必須記錄 ETW loss");
        assert_eq!(d.error.as_deref(), Some(codes::BENCHMARK_CAPTURE_ETW_LOST));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// overflow retry 後摘要仍保留第一次 overflow count，且 total/valid/invalid 累計正確。
    #[test]
    fn overflow_retry_preserves_first_overflow_count_in_summary() {
        let root = temp_root("overflow_retry_summary");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        // LP 1 每個 round 的第一次 attempt 都 overflow（stderr 帶 47123），retry 乾淨。
        processes.first_attempt_overflow.lock().unwrap().insert(1);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "err={:?}",
            result.error
        );

        let q = &result.detail.summary.capture_quality;
        // 每 round：第一次 overflow（attempt 1）+ retry 成功（attempt 2）= 2 attempts。
        assert_eq!(q.total_captures, SCREENING_ROUNDS * 2);
        assert_eq!(q.valid_captures, SCREENING_ROUNDS);
        assert_eq!(q.invalid_captures, SCREENING_ROUNDS);
        assert_eq!(q.overflowed_present_events, 47123 * SCREENING_ROUNDS as u64);
        assert_eq!(q.etw_events_lost, 0);
        assert!(q.integrity_passed, "全部正式 capture 完整且 session 完成");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ETW loss 失敗摘要：失敗也保存已知累計（total/invalid/etw_events_lost），
    /// 且 integrity_passed 為 false。
    #[test]
    fn etw_loss_summary_records_failure_accumulation() {
        let root = temp_root("etw_loss_summary");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes.presentmon_etw_loss.store(true, Ordering::SeqCst);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_CAPTURE_ETW_LOST)
        );

        let q = &result.detail.summary.capture_quality;
        assert_eq!(q.total_captures, 1, "ETW loss 不可重試，只 1 次 attempt");
        assert_eq!(q.valid_captures, 0);
        assert_eq!(q.invalid_captures, 1);
        assert_eq!(q.etw_events_lost, 9000000);
        assert_eq!(q.overflowed_present_events, 0);
        assert!(!q.integrity_passed, "失敗 session 不該 integrity_passed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// retry 期間 cancel → Cancelled，不清除已收集的部分
    #[test]
    fn cancel_during_retry_aborts_and_restores() {
        let root = temp_root("retry_cancel");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        // LP 1 第一次 missing → 觸發 retry；retry 期間 cancel
        processes.first_attempt_missing.lock().unwrap().insert(1);
        let cancel = Arc::new(FakeCancel::new());
        // 累計 13000ms 取消：落在 retry restart 穩定（11000..16000ms）期間，
        // 此時不應建立 retry workload。
        let sleeper = Arc::new(CancelAfterSleeper::new(cancel.clone(), 13000));
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            cancel as Arc<dyn CancelSignal>,
            sleeper as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Cancelled);
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// capture wait（PresentMon 卡住不退出）期間取消 → 提前中斷、終止 owned、
    /// 還原策略、狀態 Cancelled。
    #[test]
    fn cancel_during_capture_wait_interrupts_and_kills() {
        let root = temp_root("cancel_capture");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // PresentMon 卡住：wait_exit 一直 Ok(false)
        processes.presentmon_timeout.store(true, Ordering::SeqCst);
        let cancel = Arc::new(FakeCancel::new());
        // stabilize(5000) + warmup(6000) = 11000ms 後進入 capture wait；
        // 11500ms 處取消（capture 開始後 ~500ms），不該等到 18s 逾時。
        let sleeper = Arc::new(CancelAfterSleeper::new(cancel.clone(), 11500));
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            cancel as Arc<dyn CancelSignal>,
            sleeper as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Cancelled);
        assert!(
            !processes.killed_log().is_empty(),
            "取消時必須終止 owned 子程序"
        );
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Vulkan windowed → 對 workload PID 要求 client area 設成 config width×height
    #[test]
    fn windowed_vulkan_resizes_client_area_to_config() {
        let root = temp_root("win_vk");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let window = Arc::new(fake::FakeWindow::new());
        let mut config = base_config();
        config.candidate_lps = vec![1];
        config.fullscreen = false;
        config.width = 640;
        config.height = 480;

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        ctx.window = window.clone();
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "err={:?}",
            result.error
        );

        // 每次 spawn workload（含 retry）都要求 resize 成 (640, 480)
        let wl_pids: Vec<u32> = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .map(|(_, p, _)| *p)
            .collect();
        let calls = window.calls_log();
        assert!(!calls.is_empty(), "windowed Vulkan 必須呼叫 resize");
        for (pid, w, h) in &calls {
            assert!(
                wl_pids.contains(pid),
                "resize 目標必須是 spawned workload PID"
            );
            assert_eq!(*w, 640);
            assert_eq!(*h, 480);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// fullscreen=true → validate_config 拒絕（強制視窗模式）
    #[test]
    fn fullscreen_is_rejected_by_validate_config() {
        let mut config = base_config();
        config.fullscreen = true;
        let err = validate_config(&config, &topo()).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_INVALID_CONFIG);
    }

    /// fullscreen=true → run_benchmark 立即 Failed（不進入 capture）
    #[test]
    fn fullscreen_benchmark_fails_fast() {
        let root = temp_root("fs_reject");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];
        config.fullscreen = true;

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_INVALID_CONFIG)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D3D9（即使非 fullscreen）→ 不強制 resize
    #[test]
    fn d3d9_does_not_resize() {
        let root = temp_root("d3d9");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let window = Arc::new(fake::FakeWindow::new());
        let mut config = base_config();
        config.candidate_lps = vec![1];
        config.workload = WorkloadKind::D3D9;
        config.fullscreen = false;

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        ctx.window = window.clone();
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Completed);
        assert!(window.calls_log().is_empty(), "D3D9 不該 resize");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 內建 Vulkan windowed → 安裝關閉防護 + resize client area
    #[test]
    fn windowed_vulkan_guards_close_and_resizes() {
        let root = temp_root("guard_win");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let window = Arc::new(fake::FakeWindow::new());
        let mut config = base_config();
        config.candidate_lps = vec![1];
        config.fullscreen = false;

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        ctx.window = window.clone();
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Completed);

        let wl_pids: Vec<u32> = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .map(|(_, p, _)| *p)
            .collect();
        let guards = window.guard_calls_log();
        assert!(!guards.is_empty(), "windowed Vulkan 必須安裝關閉防護");
        for pid in &guards {
            assert!(
                wl_pids.contains(pid),
                "guard 目標必須是 spawned workload PID"
            );
        }
        assert!(
            !window.calls_log().is_empty(),
            "windowed Vulkan 仍須 resize client area"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D3D9 → 不安裝關閉防護、不 resize
    #[test]
    fn d3d9_does_not_guard() {
        let root = temp_root("guard_d3d9");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let window = Arc::new(fake::FakeWindow::new());
        let mut config = base_config();
        config.candidate_lps = vec![1];
        config.workload = WorkloadKind::D3D9;
        config.fullscreen = false;

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        ctx.window = window.clone();
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Completed);
        assert!(window.guard_calls_log().is_empty(), "D3D9 不該安裝關閉防護");
        assert!(window.calls_log().is_empty(), "D3D9 不該 resize");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// guard 安裝失敗（helper 回 Err）→ 只 log warn，benchmark 仍正常完成
    #[test]
    fn guard_failure_does_not_fail_benchmark() {
        let root = temp_root("guard_fail");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let window = Arc::new(fake::FakeWindow::new());
        window
            .guard_result
            .lock()
            .unwrap()
            .replace(Err("guard 失敗".to_string()));
        let mut config = base_config();
        config.candidate_lps = vec![1];
        config.fullscreen = false;

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        ctx.window = window.clone();
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "guard 失敗不得中斷 benchmark: err={:?}",
            result.error
        );
        assert!(
            !window.guard_calls_log().is_empty(),
            "guard 失敗前仍應有呼叫紀錄"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 驗證 retry 後所有新舊 workload/PresentMon PID 都被清理
    #[test]
    fn retry_cleans_up_all_pids() {
        let root = temp_root("retry_clean");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        // LP 1 第一次 missing → retry 成功
        processes.first_attempt_missing.lock().unwrap().insert(1);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Completed);

        // 所有 spawned PIDs（workload + PresentMon）都必須被 kill
        let spawned: Vec<u32> = processes.spawn_log().iter().map(|(_, p, _)| *p).collect();
        let killed = processes.killed_log();
        for pid in &spawned {
            assert!(
                killed.contains(pid),
                "spawned PID {pid} 必須出現在 killed log"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// retry 產生兩個 attempt 的診斷檔，且 attempt 欄位正確
    #[test]
    fn retry_writes_per_attempt_diagnostics() {
        let root = temp_root("diag_retry");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        // LP 1 第一次 missing → retry 成功
        processes.first_attempt_missing.lock().unwrap().insert(1);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Completed);

        let session_dir = ctx.storage_root.join(&ctx.session_id);
        let diag_dir = session_dir.join("diag");

        // attempt 1 診斷檔
        let d1 = read_diag(&session_dir, 0, 1);
        assert_eq!(d1.attempt, 1);
        assert_eq!(d1.error.as_deref(), Some(codes::BENCHMARK_CAPTURE_MISSING));

        // attempt 2 診斷檔（獨立檔案）
        let d2_path = diag_dir.join("capture-round-0-lp-1-attempt-2.json");
        assert!(d2_path.exists(), "retry 診斷檔必須存在: {d2_path:?}");
        let d2_text = std::fs::read_to_string(&d2_path)
            .unwrap_or_else(|e| panic!("讀取 retry 診斷檔失敗: {e}"));
        let d2: CaptureDiagnostics =
            serde_json::from_str(&d2_text).expect("retry 診斷 JSON 解析失敗");
        assert_eq!(d2.attempt, 2);
        assert_eq!(d2.error, None, "retry 成功 error 應為 None");
        assert_ne!(
            d2.workload_pid, d1.workload_pid,
            "retry 必須用新 workload PID"
        );
        assert!(
            d2.workload_pid != 0 && d1.workload_pid != 0,
            "兩次 attempt 都應有有效 workload PID"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 部分失敗路徑的 summary.sample_count 反映已完成的 LP 結果
    #[test]
    fn partial_failure_sample_count_reflects_completed_lps() {
        let root = temp_root("partial_samples");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        backend.set_policy(AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        });
        let processes = Arc::new(FakeProcessRunner::new());
        // 所有 LP 共用一個有效 CSV（避免 csv_for_lp header 重複）
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        // 中間的 LP 2 第一次與所有 retry 都 missing；LP 3 仍應繼續完成
        processes.first_attempt_missing.lock().unwrap().insert(2);
        processes
            .second_attempt_also_missing
            .lock()
            .unwrap()
            .insert(2);
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1, 2, 3];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Failed);
        // LP1、LP3 各有 sample_count，證明中間 LP 失敗不會中止 session。
        assert_eq!(result.detail.results.len(), 2, "應保留 LP1, LP3 結果");
        assert_eq!(
            result
                .detail
                .results
                .iter()
                .map(|r| r.lp)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        let total_samples: u32 = result.detail.results.iter().map(|r| r.sample_count).sum();
        assert_eq!(total_samples, 100, "短篩成功的 LP1、LP3 各保留 50 samples");
        assert_eq!(
            result.detail.summary.sample_count, total_samples,
            "summary.sample_count 必須等於已完成 LP 的 sample_count 總和"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 自適應排程（全 LP 短篩 + Top5 racing + Top3 refinement + 確認）測試 ──

    /// 由 PresentMon spawn log 統計每 LP 實際被 capture 的 round 集合。
    fn rounds_per_lp(processes: &FakeProcessRunner) -> HashMap<u32, Vec<u32>> {
        let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
        for (name, _pid, args) in processes.spawn_log().iter() {
            if !name.contains("PresentMon") {
                continue;
            }
            let out = args
                .iter()
                .position(|a| a == "--output_file")
                .and_then(|i| args.get(i + 1))
                .cloned();
            let parsed = out.as_deref().and_then(|p| {
                let stem = std::path::Path::new(p).file_stem()?.to_str()?;
                let (head, lp_str) = stem.rsplit_once("-lp-")?;
                let lp: u32 = lp_str.parse().ok()?;
                let round: u32 = head.rsplit_once("round-")?.1.parse().ok()?;
                Some((round, lp))
            });
            if let Some((round, lp)) = parsed {
                map.entry(lp).or_default().push(round);
            }
        }
        for v in map.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        map
    }

    /// N=3：同內容 → 平手 → Equivalent 於 5 確認 round 提早停；精確
    /// 3*3 + 2*3 + 2*5 = 25 次 capture，只有前 2 名 finalists 進確認 round（base..）。
    #[test]
    fn adaptive_run_exact_min_captures_and_only_top_two_confirmed() {
        let root = temp_root("adaptive_counts");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let config = base_config(); // candidate_lps [1,2,3]

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "err={:?}",
            result.error
        );

        let wl_spawns = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .count();
        assert_eq!(wl_spawns, 19, "N=3 應為 3 + 3 + 3 + 2*5 = 19 次 capture");

        // 同內容 CSV → 平手 → finalists = [1, 2]；確認 5 round 即停（Equivalent）。
        let b = CONFIRMATION_ROUND_BASE;
        let rl = rounds_per_lp(&processes);
        assert_eq!(rl[&1], vec![0, 1, 2, b, b + 1, b + 2, b + 3, b + 4]);
        assert_eq!(rl[&2], vec![0, 1, 2, b, b + 1, b + 2, b + 3, b + 4]);
        assert_eq!(rl[&3], vec![0, 1, 2]);
        assert_eq!(
            result.detail.summary.reliability.confirmation_rounds,
            EQUIVALENT_MIN_ROUNDS
        );
        assert_eq!(
            result.detail.summary.reliability.status,
            ReliabilityStatus::Equivalent
        );
        assert_eq!(result.best_lp, None, "Equivalent 不得有推薦");
        assert_eq!(result.detail.summary.verified_best_lp, None);
        assert_eq!(result.detail.summary.confirmation_winner_lp, None);
        assert_eq!(result.detail.summary.equivalent_finalist_lps, vec![1, 2]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 候選略優（~0.78% avg）但不足 decisive、又超等效門檻 → 跑到 7 輪仍 Inconclusive。
    #[test]
    fn adaptive_run_seven_round_inconclusive_when_narrow_margin() {
        let root = temp_root("inconclusive7");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        // LP1=10.0（最快）、LP2=10.08（次）、LP3=12.0（淘汰）。
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(1, csv_with_base(10.0));
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(2, csv_with_base(10.08));
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(3, csv_with_base(12.0));
        let cancel = FakeCancel::new();
        let config = base_config(); // candidate_lps [1,2,3]

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "err={:?}",
            result.error
        );
        assert_eq!(result.best_lp, None, "非 decisive 不得有推薦");
        assert_eq!(
            result.detail.summary.reliability.status,
            ReliabilityStatus::Inconclusive
        );
        assert_eq!(
            result.detail.summary.reliability.confirmation_rounds,
            CONFIRMATION_MAX_ROUNDS
        );
        // finalists [1,2] 各跑滿 7 確認 round；LP3 只測三層 selection。
        let b = CONFIRMATION_ROUND_BASE;
        let rl = rounds_per_lp(&processes);
        assert_eq!(
            rl[&1],
            vec![0, 1, 2, b, b + 1, b + 2, b + 3, b + 4, b + 5, b + 6]
        );
        assert_eq!(
            rl[&2],
            vec![0, 1, 2, b, b + 1, b + 2, b + 3, b + 4, b + 5, b + 6]
        );
        assert_eq!(rl[&3], vec![0, 1, 2]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 篩選平手：同分數時依中位數 → worst-round → 較小 LP 決定，取前 2。
    #[test]
    fn select_finalists_deterministic_tie_picks_lower_lps() {
        let dir = temp_root("sel_tie");
        let mut round_csvs: RoundCsvs = HashMap::new();
        for lp in 0..3u32 {
            for round in 0..3u32 {
                round_csvs
                    .entry(lp)
                    .or_default()
                    .insert(round, write_round_csv(&dir, round, lp, 10.0));
            }
        }
        let finalists = select_top_candidates(&round_csvs, SCREENING_ROUNDS, MAX_FINALISTS);
        assert_eq!(finalists, vec![0, 1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 篩選少於兩個完整候選 → 回傳空（呼叫端跳過確認）。
    #[test]
    fn select_finalists_empty_when_fewer_than_two_complete() {
        let dir = temp_root("sel_few");
        let mut round_csvs: RoundCsvs = HashMap::new();
        // LP0 有三個完整 selection round。
        for round in 0..(SCREENING_ROUNDS + REFINEMENT_ROUNDS) {
            round_csvs
                .entry(0)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 0, 10.0));
        }
        // LP1 只測 1 個篩選 round → 非完整候選
        round_csvs
            .entry(1)
            .or_default()
            .insert(0, write_round_csv(&dir, 0, 1, 11.0));
        assert!(select_top_candidates(
            &round_csvs,
            SCREENING_ROUNDS + REFINEMENT_ROUNDS,
            MAX_FINALISTS
        )
        .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 finalist（僅篩選 round）不得出現在確認勝者中；best 由確認推論的 finalists 決定。
    #[test]
    fn non_finalist_does_not_become_confirmation_winner() {
        let dir = temp_root("rel_adaptive");
        let mut round_csvs: RoundCsvs = HashMap::new();
        // finalists 0/1 各 5 確認 round（CONFIRMATION_ROUND_BASE..+5）
        for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + 5) {
            round_csvs
                .entry(0)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 0, 10.0));
            round_csvs
                .entry(1)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 1, 11.0));
        }
        // 非 finalist LP2 只有篩選 round（8ms 最快），不得影響確認推論
        for round in 0..SCREENING_ROUNDS {
            round_csvs
                .entry(2)
                .or_default()
                .insert(round, write_round_csv(&dir, round, 2, 8.0));
        }
        let results = vec![
            lp_res(0, 100.0, 100.0, 100.0, 0.0),
            lp_res(1, 90.909, 90.909, 90.909, 0.0),
            lp_res(2, 125.0, 125.0, 125.0, 0.0),
        ];
        let rel = compute_reliability(
            &round_csvs,
            &results,
            &[0, 1],
            5,
            Some(ForwardVerdict::CandidatePassed),
            false,
            false,
            0,
        );
        assert_eq!(rel.status, ReliabilityStatus::Passed);
        assert_eq!(rel.candidate_lp, Some(0));
        assert_eq!(rel.runner_up_lp, Some(1));
        assert!(
            !rel.per_round_winners.contains(&Some(2)),
            "非 finalist 不得成為任何確認 round 勝者"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// N=1：只測 3 篩選 round 即停（無確認），session Completed 但可靠性
    /// Inconclusive（少於兩個 finalists，無法 Passed/Equivalent）。
    #[test]
    fn single_lp_skips_confirmation_and_stays_inconclusive() {
        let root = temp_root("n1");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "err={:?}",
            result.error
        );
        assert_eq!(result.best_lp, None, "單一 LP 不得有推薦");
        assert_eq!(
            result.detail.summary.reliability.status,
            ReliabilityStatus::Inconclusive
        );
        let wl_spawns = processes
            .spawn_log()
            .iter()
            .filter(|(n, _, _)| !n.contains("PresentMon"))
            .count();
        assert_eq!(wl_spawns, 1, "N=1 只需一輪短篩，無比較或確認");
        assert_eq!(rounds_per_lp(&processes)[&1], vec![0]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 進度在兩階段過渡時單調遞增、最終達 100 且不超過 100。
    #[test]
    fn progress_monotonic_and_capped_at_100() {
        let root = temp_root("progress_adaptive");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let events: Arc<std::sync::Mutex<Vec<BenchmarkProgress>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();

        let mut ctx = build_ctx(
            &root,
            backend as Arc<dyn GpuBackend>,
            processes as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            Some(Box::new(move |p| ev.lock().unwrap().push(p.clone()))),
        );
        run_benchmark(&mut ctx);
        let pcts: Vec<u32> = events
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.percentage)
            .collect();
        assert!(!pcts.is_empty());
        assert!(pcts.iter().all(|&p| p <= 100), "進度不得超過 100");
        assert_eq!(*pcts.last().unwrap(), 100, "最終進度應達 100");
        for w in pcts.windows(2) {
            assert!(w[0] <= w[1], "進度不得倒退: {pcts:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 確認階段（round 3 起）收到取消 → Cancelled 並還原。
    #[test]
    fn cancel_during_confirmation_aborts_and_restores() {
        let root = temp_root("cancel_confirm");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = Arc::new(FakeCancel::new());
        let cancel2 = cancel.clone();
        let config = base_config(); // candidate_lps [1,2,3]

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            cancel as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            Some(Box::new(move |p| {
                // 進入確認階段（round 3）即觸發取消
                if p.round == Some(SCREENING_ROUNDS) {
                    cancel2.set(true);
                }
            })),
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Cancelled);
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 確認階段某 finalist 的 PresentMon spawn 失敗 → Failed 且還原。
    #[test]
    fn error_during_confirmation_fails_and_restores() {
        let root = temp_root("err_confirm");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        // 第一個 confirmation round 的 PresentMon spawn 失敗。
        processes
            .fail_presentmon_rounds
            .lock()
            .unwrap()
            .insert(CONFIRMATION_ROUND_BASE);
        let cancel = FakeCancel::new();
        let config = base_config(); // candidate_lps [1,2,3]

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        let result = run_benchmark(&mut ctx);
        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_PRESENTMON_FAILED)
        );
        assert_eq!(result.best_lp, None, "失敗不該有推薦");
        assert_eq!(
            result.detail.results.len(),
            3,
            "三層 selection 的部分結果應保留"
        );
        assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
        assert!(!journal.exists());
        // 確認階段確實被觸及。
        let reached_confirmation = processes.spawn_log().iter().any(|(n, _, args)| {
            n.contains("PresentMon")
                && args
                    .iter()
                    .any(|a| a.contains(&format!("round-{}-lp-", CONFIRMATION_ROUND_BASE)))
        });
        assert!(
            reached_confirmation,
            "應在第一個 confirmation round 觸發失敗"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 等效安全驗證（run_equivalent_validation）──

    fn equivalent_validation_baseline(reference_lp: u32) -> AffinityPolicy {
        AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(single_lp_mask_bytes(
                reference_lp,
            )),
        }
    }

    #[test]
    fn run_equivalent_validation_passes_when_selected_not_worse() {
        let root = temp_root("equiv_val_pass");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = equivalent_validation_baseline(2);
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // selected(1) 與 reference(2) 內容相同 → selected 不更差 → Passed。
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(1, csv_with_base(10.0));
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(2, csv_with_base(10.0));
        let cancel = FakeCancel::new();

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let outcome = run_equivalent_validation(&mut ctx, 1, 2, 240, 8192);
        assert_eq!(
            outcome.status,
            EquivalentSafetyStatus::Passed,
            "reason={:?}",
            outcome.reason
        );
        assert_eq!(outcome.rounds, EQUIVALENT_VALIDATION_ROUNDS);
        assert!(outcome.avg_improvement_pct.is_some());
        assert_eq!(
            backend.current_policy(GPU_A),
            baseline,
            "必須還原 reference policy"
        );
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_equivalent_validation_fails_when_selected_materially_worse() {
        let root = temp_root("equiv_val_fail");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = equivalent_validation_baseline(2);
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        // selected(1) 12ms 明顯慢於 reference(2) 10ms → Failed。
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(1, csv_with_base(12.0));
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(2, csv_with_base(10.0));
        let cancel = FakeCancel::new();

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        let outcome = run_equivalent_validation(&mut ctx, 1, 2, 240, 8192);
        assert_eq!(outcome.status, EquivalentSafetyStatus::Failed);
        assert_eq!(backend.current_policy(GPU_A), baseline, "失敗仍須還原");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_equivalent_validation_cancelled_and_restores() {
        let root = temp_root("equiv_val_cancel");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = equivalent_validation_baseline(2);
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(1, csv_with_base(10.0));
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(2, csv_with_base(10.0));
        let cancel = Arc::new(FakeCancel::new());
        let cancel2 = cancel.clone();

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            cancel as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            Some(Box::new(move |p| {
                // 進入第一個驗證 round 的 applying 即觸發取消。
                if p.round == Some(EQUIVALENT_VALIDATION_ROUND_BASE) && p.stage == "applying" {
                    cancel2.set(true);
                }
            })),
        );
        let outcome = run_equivalent_validation(&mut ctx, 1, 2, 240, 8192);
        assert_eq!(outcome.status, EquivalentSafetyStatus::Cancelled);
        assert_eq!(
            backend.current_policy(GPU_A),
            baseline,
            "取消必須還原 reference policy"
        );
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 單向退步禁制：單輪 selected 明顯「改善」不得誤拒（反向前向 Equivalent 的雙向 abs）。
    #[test]
    fn equivalent_validation_regressed_improvement_not_rejected() {
        let selected = lp_raw(1, 110.0, 95.0, 85.0, 10.0, 1.0); // avg +10%、p1 較佳
        let reference = lp_raw(2, 100.0, 90.0, 80.0, 10.0, 1.0);
        assert!(!equivalent_validation_regressed(&[(selected, reference)]));
        // spike 單輪明顯「更好」（下降 >0.5pp）也不得拒
        let better_spike = lp_raw(1, 100.0, 90.0, 80.0, 10.0, 0.2);
        assert!(!equivalent_validation_regressed(&[(
            better_spike,
            lp_raw(2, 100.0, 90.0, 80.0, 10.0, 1.0)
        )]));
    }

    /// 單向退步禁制：avg / p1 / spike 任一單輪明顯退步 → 拒絕。
    #[test]
    fn equivalent_validation_regressed_rejects_material_regression() {
        let reference = lp_raw(2, 100.0, 90.0, 80.0, 10.0, 1.0);
        // avg 96 vs 100 → -4% < -3% → 退步
        assert!(equivalent_validation_regressed(&[(
            lp_raw(1, 96.0, 90.0, 80.0, 10.0, 1.0),
            reference.clone()
        )]));
        // p1 84 vs 90 → (84-90)/90 = -6.67% < -5% → 退步
        assert!(equivalent_validation_regressed(&[(
            lp_raw(1, 100.0, 84.0, 80.0, 10.0, 1.0),
            reference.clone()
        )]));
        // spike 1.7 vs 1.0 → +0.7pp > 0.5pp → 退步
        assert!(equivalent_validation_regressed(&[(
            lp_raw(1, 100.0, 90.0, 80.0, 10.0, 1.7),
            reference
        )]));
    }

    /// 視窗完整性持續異常（warmup 期間）→ session Failed（BENCHMARK_WINDOW_INTEGRITY）且策略還原。
    #[test]
    fn window_integrity_failure_fails_session_and_restores() {
        let root = temp_root("win_integrity");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let window = Arc::new(fake::FakeWindow::new());
        window.set_integrity_ok(false); // 前景/位置持續異常
        let mut config = base_config();
        config.candidate_lps = vec![1];

        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            config,
            &journal,
            None,
        );
        ctx.window = window.clone();
        let result = run_benchmark(&mut ctx);

        assert_eq!(result.status, SessionStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some(codes::BENCHMARK_WINDOW_INTEGRITY)
        );
        // cleanup_run 還原原始策略、清除日誌
        assert_eq!(backend.current_policy(GPU_A), baseline);
        assert!(!journal.exists(), "還原成功後日誌應清除");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// run_capture 期間視窗完整性破壞 → 回 BENCHMARK_WINDOW_INTEGRITY 且計 window_invalid。
    #[test]
    fn run_capture_window_integrity_fails_and_counts() {
        let root = temp_root("run_cap_win");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let processes = Arc::new(FakeProcessRunner::new());
        processes
            .presentmon_csv
            .lock()
            .unwrap()
            .push_str(&csv_for_lp(0));
        let cancel = FakeCancel::new();
        let window = Arc::new(fake::FakeWindow::new());
        window.set_integrity_ok(false);
        let mut ctx = build_ctx(
            &root,
            backend.clone() as Arc<dyn GpuBackend>,
            processes.clone() as Arc<dyn ProcessRunner>,
            Arc::new(cancel) as Arc<dyn CancelSignal>,
            Arc::new(NoopSleeper) as Arc<dyn Sleep>,
            base_config(),
            &journal,
            None,
        );
        ctx.window = window.clone();
        let csv = root.join("out.csv");
        let expected = Rect::new(0, 0, 1280, 720);
        let err = run_capture(&mut ctx, 0, 1, 999, &csv, 1, 0, 8192, 1, expected).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_WINDOW_INTEGRITY);
        assert_eq!(ctx.capture_quality.window_invalid_captures, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// run_capture 等待期間 workload 失去前景 → 要求置中還原（與 report_integrity 路徑一致）。
    #[test]
    fn run_capture_foreground_loss_requests_center_restore() {
        let root = temp_root("run_cap_center");
        let journal = root.join("journal.json");
        let (mut ctx, wc) = ctx_with_window_control(&root, &journal);
        let window = Arc::new(fake::FakeWindow::new());
        window.set_integrity_ok(false); // 前景失敗（foreground=false）
        ctx.window = window.clone();
        let csv = root.join("out.csv");
        let expected = Rect::new(0, 0, 1280, 720);
        let err = run_capture(&mut ctx, 0, 1, 999, &csv, 1, 0, 8192, 1, expected).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_WINDOW_INTEGRITY);
        assert!(wc.center_requested.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// run_capture 等待期間僅 position/topmost 等失敗（foreground 仍 true）→ 不要求置中。
    #[test]
    fn run_capture_non_foreground_failure_does_not_request_center() {
        let root = temp_root("run_cap_nocenter");
        let journal = root.join("journal.json");
        let (mut ctx, wc) = ctx_with_window_control(&root, &journal);
        let window = Arc::new(fake::FakeWindow::new());
        window.set_integrity_snapshot(WindowIntegritySnapshot {
            foreground: true,
            position_ok: false,
            ..Default::default()
        });
        ctx.window = window.clone();
        let csv = root.join("out.csv");
        let expected = Rect::new(0, 0, 1280, 720);
        let err = run_capture(&mut ctx, 0, 1, 999, &csv, 1, 0, 8192, 1, expected).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_WINDOW_INTEGRITY);
        assert!(!wc.center_requested.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn quick_physical_captures_and_authenticated_apply_use_whole_cores() {
        use crate::benchmark::{
            manager::{core_apply::tested_target, BenchmarkManager},
            physical,
        };
        let root = temp_root("quick_physical_full");
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.into(),
            device_policy: RegistryValueSnapshot::dword(4),
            assignment_set_override: RegistryValueSnapshot::binary(vec![0x04, 0]),
        };
        backend.set_policy(baseline.clone());
        let processes = Arc::new(FakeProcessRunner::new());
        for lp in [0, 2, 4] {
            processes
                .presentmon_csv_by_lp
                .lock()
                .unwrap()
                .insert(lp, csv_for_lp(lp));
        }
        let mut config = base_config();
        config.method_version = physical::METHOD_VERSION;
        config.candidate_lps.clear();
        config.sample_secs = 10;
        config.warm_up_secs = 3;
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = observed.clone();
        let gpu = backend.clone();
        let mut ctx = build_ctx(
            &root,
            backend.clone(),
            processes.clone(),
            Arc::new(FakeCancel::new()),
            Arc::new(NoopSleeper),
            config,
            &journal,
            Some(Box::new(move |p| {
                if p.stage == "collecting" {
                    seen.lock()
                        .unwrap()
                        .push((p.target.clone(), gpu.current_policy(GPU_A)));
                }
            })),
        );
        ctx.topo = build_topology(vec![
            (vec![0, 1], 0, true),
            (vec![2, 3], 0, true),
            (vec![4, 63], 0, true),
        ]);
        let result = run_benchmark(&mut ctx);
        assert_eq!(
            result.status,
            SessionStatus::Completed,
            "{:?}",
            result.error
        );
        let q = result.detail.summary.quick.as_ref().unwrap();
        assert_eq!(q.screening.len(), 3);
        assert_eq!(q.retest.len(), 2);
        assert_eq!(result.detail.results.len(), 2);
        assert_eq!(result.detail.confirmation_results.len(), 0);
        assert_eq!(result.detail.summary.best_lp, None);
        assert_eq!(result.detail.summary.capture_quality.total_captures, 7); // 2 基線 + 5 候選
        assert_eq!(backend.restart_count(), 6); // 5 候選 captures + restoration（基線不重啟）
        assert_eq!(q.screening_order, physical::shuffled(&[0, 1, 2], q.seed));
        let expected: Vec<_> = q
            .screening_order
            .iter()
            .rev()
            .filter(|id| q.screening.iter().take(2).any(|r| r.target.core_id == **id))
            .copied()
            .collect();
        assert_eq!(q.retest_order, expected);
        assert_eq!(backend.current_policy(GPU_A), baseline);
        assert!(!journal.exists());
        for (target, policy) in observed.lock().unwrap().iter() {
            let target = target.as_ref().expect("core target on progress");
            assert_eq!(
                policy.assignment_set_override.bytes,
                Some(physical::mask(target, &ctx.topo).unwrap())
            );
        }
        let detail = storage::get_at_verified(&ctx.storage_root, &ctx.session_id).unwrap();
        let selected = q.retest[1].target.core_id; // runner-up is also selectable
        assert!(tested_target(&detail, &ctx.topo, &ctx.cpu_identity, selected).is_ok());
        let manager = BenchmarkManager::new(backend.clone());
        let restore = root.join("restore.json");
        manager
            .apply_core_at(
                &ctx.topo,
                &ctx.cpu_identity,
                GPU_A,
                selected,
                Some(&ctx.session_id),
                &ctx.storage_root,
                &journal,
                &restore,
            )
            .unwrap();
        assert_eq!(
            backend.current_policy(GPU_A).assignment_set_override.bytes,
            Some(physical::mask(&q.retest[1].target, &ctx.topo).unwrap())
        );
        crate::benchmark::manager::restore_previous_affinity(
            backend.as_ref(),
            &NoopSleeper,
            &restore,
        )
        .unwrap();
        assert_eq!(backend.current_policy(GPU_A), baseline);
        let mut changed = ctx.topo.clone();
        changed.physical_cores[0].lp_indices = vec![0];
        assert!(tested_target(&detail, &changed, &ctx.cpu_identity, selected).is_err());
        let mut legacy = detail.clone();
        legacy.summary.quick = None;
        assert!(tested_target(&legacy, &ctx.topo, &ctx.cpu_identity, selected).is_err());
        for status in [SessionStatus::Failed, SessionStatus::Cancelled] {
            let mut invalid = detail.clone();
            invalid.summary.status = status;
            assert!(tested_target(&invalid, &ctx.topo, &ctx.cpu_identity, selected).is_err());
        }
        let file = ctx.storage_root.join(&ctx.session_id).join("session.json");
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, text.replace("Completed", "Failed")).unwrap();
        assert!(manager
            .apply_core_at(
                &ctx.topo,
                &ctx.cpu_identity,
                GPU_A,
                selected,
                Some(&ctx.session_id),
                &ctx.storage_root,
                &journal,
                &restore
            )
            .is_err());
        assert_eq!(backend.current_policy(GPU_A), baseline);
    }

    #[test]
    fn quick_single_cancel_and_capture_failure_restore() {
        use crate::benchmark::physical::{RankingStatus, METHOD_VERSION};
        for mode in [
            "single",
            "cancel",
            "capture",
            "restore",
            "write",
            "readback",
            "restart",
            "gpu_missing",
        ] {
            let root = temp_root(&format!("quick_{mode}"));
            let journal = root.join("journal.json");
            let backend = Arc::new(FakeBackend::new(if mode == "gpu_missing" {
                vec![]
            } else {
                vec![device(GPU_A)]
            }));
            let baseline = AffinityPolicy {
                instance_id: GPU_A.into(),
                ..Default::default()
            };
            let processes = Arc::new(FakeProcessRunner::new());
            *processes.presentmon_csv.lock().unwrap() = csv_for_lp(0);
            if mode == "capture" {
                processes.fail_presentmon_rounds.lock().unwrap().insert(1);
            }
            if mode == "restore" {
                backend.disable_fails.store(true, Ordering::SeqCst);
            }
            if mode == "write" {
                backend.fail_next_write();
            }
            if mode == "readback" {
                backend.fail_nth_read_mismatch(2);
            }
            if mode == "restart" {
                backend.fail_next_restart.store(true, Ordering::SeqCst);
            }
            let cancel = Arc::new(FakeCancel::new());
            let cancel_at_retest = cancel.clone();
            let mut config = base_config();
            config.method_version = METHOD_VERSION;
            config.candidate_lps.clear();
            let mut ctx = build_ctx(
                &root,
                backend.clone(),
                processes,
                cancel,
                Arc::new(NoopSleeper),
                config,
                &journal,
                Some(Box::new(move |p| {
                    if mode == "cancel" && p.round == Some(1) {
                        cancel_at_retest.set(true);
                    }
                })),
            );
            ctx.topo = build_topology(vec![(vec![0, 63], 0, true)]);
            let result = run_benchmark(&mut ctx);
            if mode == "single" {
                assert_eq!(
                    result.status,
                    SessionStatus::Completed,
                    "{:?}",
                    result.error
                );
                assert_eq!(
                    result.detail.summary.quick.as_ref().unwrap().status,
                    RankingStatus::SingleCandidate
                );
                assert_eq!(result.detail.summary.capture_quality.total_captures, 4);
            // 2 基線 + 2 候選
            } else if mode == "cancel" {
                assert_eq!(result.status, SessionStatus::Cancelled);
            } else {
                assert_eq!(result.status, SessionStatus::Failed, "{mode}");
            }
            if mode == "restore" {
                assert!(journal.exists());
                assert!(result.recovery_required);
            } else {
                assert_eq!(backend.current_policy(GPU_A), baseline);
                assert!(!journal.exists());
            }
        }
    }
}
