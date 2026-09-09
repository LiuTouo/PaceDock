//! 基準測試領域模型（Task 1 基礎建設）。
//! 序列化慣例：struct 欄位 camelCase、enum variant PascalCase 字串。
//! 本檔只放可序列化型別與共用工具；儲存/還原/協調分在 storage、recovery、manager。

use serde::{Deserialize, Serialize};

pub mod assets;
pub mod env;
pub mod ipc;
pub mod manager;
pub mod metrics;
pub mod physical;
pub mod process_win;
pub mod recovery;
pub mod runner;
pub mod storage;
pub mod window_layout;
pub mod window_win;

// ── GPU 基準測試領域型別 ────────────────────────────────────────────────

/// Session 狀態機
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SessionStatus {
    /// 已建立、尚未開始
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 執行階段（progress 用）
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BenchmarkStage {
    #[default]
    Init,
    Warmup,
    Collecting,
    Finalizing,
}

/// 進度事件的排程 phase；與低階 stage（launching/collecting 等）正交。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchmarkPhase {
    Screening,
    Refinement,
    Confirmation,
    ReverseConfirmation,
    EquivalentValidation,
}

/// workload 種類
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WorkloadKind {
    /// 內建 liblava Vulkan workload（lava-triangle.exe）
    #[default]
    Vulkan,
    /// 內建 Rust 編譯的 Direct3D9 workload（d3d9-workload.exe）
    D3D9,
}

/// FPS cap 策略：Adaptive = 校準選定最高安全 cap（新預設）；Fixed = 沿用 `fps_cap`
/// （legacy / 固定輸入）。舊 session JSON 缺此欄位時由 `#[serde(default)]` 解讀為
/// Adaptive（新產品預設），`fps_cap` 欄位仍保留供 Fixed 模式沿用。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FpsCapPolicy {
    /// 校準選定（預設）：忽略 fps_cap，依校準 tier 選定正式 cap。
    #[default]
    Adaptive,
    /// 沿用 `config.fps_cap`（0 = 不限制）。
    Fixed,
}

/// 基準測試參數
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkConfig {
    #[serde(default)]
    pub method_version: u32,
    #[serde(default)]
    pub candidate_core_ids: Vec<u32>,
    #[serde(default = "default_retest_warmup")]
    pub retest_warm_up_secs: u32,
    #[serde(default = "default_retest_sample")]
    pub retest_sample_secs: u32,
    /// 要逐一測試的候選 LP；空 = 全部支援 LP
    #[serde(default)]
    pub candidate_lps: Vec<u32>,
    #[serde(default)]
    pub gpu_instance_id: Option<String>,
    #[serde(default)]
    pub workload: WorkloadKind,
    #[serde(default = "default_warm_up_secs")]
    pub warm_up_secs: u32,
    #[serde(default = "default_sample_secs")]
    pub sample_secs: u32,
    /// 舊單 LP 格式相容欄位；新版兩階段流程忽略此欄位。
    /// 保留供舊 session JSON 向後相容；預設 5。
    #[serde(default = "default_repetitions")]
    pub repetitions: u32,
    /// 已棄用：production runner 不再繫結 workload process affinity。
    /// 保留欄位供舊 session JSON 向後相容；預設 false。
    #[serde(default)]
    pub sync_workload_affinity: bool,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    /// FPS cap；0 = 不限制。僅在 `fps_cap_policy == Fixed` 時作為正式 cap；
    /// Adaptive 模式忽略此欄位（由校準選定）。
    #[serde(default)]
    pub fps_cap: u32,
    /// FPS cap 策略（新欄位，serde default = Adaptive）。
    #[serde(default)]
    pub fps_cap_policy: FpsCapPolicy,
    #[serde(default)]
    pub triple_buffer: bool,
    /// Vulkan workload 的額外參數（workload=Vulkan 時必須非空）
    #[serde(default)]
    pub vulkan_args: Vec<String>,
    /// 相容舊欄位：遊戲路徑（現由 workload 種類取代）
    #[serde(default)]
    pub game_path: Option<String>,
    #[serde(default)]
    pub window_title: Option<String>,
}

fn default_warm_up_secs() -> u32 {
    3
}
fn default_sample_secs() -> u32 {
    10
}
fn default_retest_warmup() -> u32 {
    5
}
fn default_retest_sample() -> u32 {
    20
}
fn default_repetitions() -> u32 {
    5
}
fn default_width() -> u32 {
    1280
}
fn default_height() -> u32 {
    720
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            method_version: 0,
            candidate_core_ids: Vec::new(),
            retest_warm_up_secs: 5,
            retest_sample_secs: 20,
            candidate_lps: Vec::new(),
            gpu_instance_id: None,
            workload: WorkloadKind::Vulkan,
            warm_up_secs: default_warm_up_secs(),
            sample_secs: default_sample_secs(),
            repetitions: default_repetitions(),
            sync_workload_affinity: false,
            fullscreen: false,
            width: default_width(),
            height: default_height(),
            fps_cap: 0,
            fps_cap_policy: FpsCapPolicy::default(),
            triple_buffer: false,
            vulkan_args: default_vulkan_args(),
            game_path: None,
            window_title: None,
        }
    }
}

/// 內建 Vulkan workload 的預設參數（AutoGpuAffinity 內附 liblava 相容格式：
/// `--fullscreen=<0|1> --width=<n> --height=<n> --fps_cap=<n> --triple_buffering=<0|1>`）
fn default_vulkan_args() -> Vec<String> {
    vec![
        "--fullscreen=0".to_string(),
        "--width=1280".to_string(),
        "--height=720".to_string(),
        "--fps_cap=0".to_string(),
        "--triple_buffering=0".to_string(),
    ]
}

/// 單一候選 LP 的最終測試結果
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct LpResult {
    #[serde(default)]
    pub lp: u32,
    #[serde(default)]
    pub avg_fps: Option<f64>,
    /// 1% low：最慢 1% 個 instantaneous FPS 的平均（frame-count）
    #[serde(default)]
    pub p1_low: Option<f64>,
    /// 0.1% low：最慢 0.1% 個 instantaneous FPS 的平均（frame-count）
    #[serde(default)]
    pub p01_low: Option<f64>,
    /// 0.01% low：最慢 0.01% 個 instantaneous FPS 的平均（frame-count）
    #[serde(default)]
    pub p001_low: Option<f64>,
    /// 0.005% low：最慢 0.005% 個 instantaneous FPS 的平均（frame-count）
    #[serde(default)]
    pub p0005_low: Option<f64>,
    /// 1% percentile：最慢 1% 分位數的 instantaneous FPS（非平均）
    #[serde(default)]
    pub p1_percentile: Option<f64>,
    /// 0.1% percentile：最慢 0.1% 分位數的 instantaneous FPS（非平均）
    #[serde(default)]
    pub p01_percentile: Option<f64>,
    /// 0.01% percentile：最慢 0.01% 分位數的 instantaneous FPS（非平均）
    #[serde(default)]
    pub p001_percentile: Option<f64>,
    /// 0.005% percentile：最慢 0.005% 分位數的 instantaneous FPS（非平均）
    #[serde(default)]
    pub p0005_percentile: Option<f64>,
    #[serde(default)]
    pub max_fps: Option<f64>,
    #[serde(default)]
    pub min_fps: Option<f64>,
    /// Bessel（n-1）校正的即時 FPS 標準差
    #[serde(default)]
    pub stdev_fps: Option<f64>,
    /// frametime MAD（中位數絕對差）正規化為 frametime 中位數的百分比（越低越穩）
    #[serde(default)]
    pub frametime_mad_pct: Option<f64>,
    /// 慢幀 spike rate：frametime 超過 2×中位數的幀佔比（百分比，越低越好）
    #[serde(default)]
    pub spike_rate_pct: Option<f64>,
    #[serde(default)]
    pub sample_count: u32,
    #[serde(default)]
    pub avg_frame_time_ms: Option<f64>,
    #[serde(default)]
    pub completed: bool,
    /// 錯誤代碼（查 i18n errors.*）
    #[serde(default)]
    pub error: Option<String>,
}

/// 執行期 progress 事件（emit `gpu-benchmark-progress`）
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkProgress {
    pub target: Option<physical::CoreTarget>,
    pub session_id: String,
    pub stage: String,
    #[serde(default)]
    pub round: Option<u32>,
    /// 已由後端解碼的排程 phase，避免前端猜測 raw round 命名空間。
    #[serde(default)]
    pub phase: Option<BenchmarkPhase>,
    /// phase 內 1-based 輪次。
    #[serde(default)]
    pub phase_round: Option<u32>,
    #[serde(default)]
    pub lp: Option<u32>,
    #[serde(default)]
    pub percentage: u32,
    #[serde(default)]
    pub eta_secs: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    /// 視窗完整性快照（capture/warmup 期間輪詢時，狀態改變才附帶）。None = 未回報。
    #[serde(default)]
    pub window_integrity: Option<WindowIntegrity>,
    /// 取消專用階段（`requested`/`stopping`/`restoring`/`finalizing`）；非取消事件為 None。
    /// 與 `stage` 分離：`stage` 承載一般 benchmark 階段，此欄位只承載取消清理階段。
    #[serde(default)]
    pub cancel_stage: Option<String>,
    /// 取消專用百分比（0..100，單調）；非取消事件為 None。
    /// 不碰 `percentage`（benchmark 進度語意維持不變）。
    #[serde(default)]
    pub cancel_progress: Option<u32>,
}

/// 執行期間的原始取樣（單一 LP 單一時刻；Task 2 runner 產生）
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct CoreSample {
    pub lp: u32,
    pub fps: f64,
    pub frame_time_ms: f64,
}

/// 可靠性（confidence）判定結果。僅成功的 session 由 runner 計算；
/// 舊 session.json 缺此欄位時經 `#[serde(default)]` 解讀為 `Unassessed`。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReliabilityStatus {
    /// 舊 session 或未計算（非 Completed 的 session）——尚未評估，不可套用。
    #[default]
    Unassessed,
    /// 確認證據（一致性規則 + bootstrap 穩定性區間 + 護欄）支持候選超越最小實質
    /// 效應門檻。屬小型樣本決策啟發式，非形式化顯著性。
    Passed,
    /// 確認證據落在可忽略差異帶內——僅描述「觀測到的實務等效」，非統計證明的等效。
    Equivalent,
    /// 確認證據不足以判定（一致性不足、穩定性區間橫跨門檻、確認不足、護欄倒退、
    /// 或證據不完整/無效）。
    Inconclusive,
}

/// 舊單 LP 可靠性/信心摘要，僅保留歷史格式相容；新版使用 physical::QuickResult。
/// 供前端顯示狀態、逐 round 勝者、候選/亞軍 LP、勝場數、複合分數優勢與
/// 護欄（Avg/1% low/spike）比較結果。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReliabilitySummary {
    #[serde(default)]
    pub status: ReliabilityStatus,
    /// 各預期 round（0..evaluated_rounds）的勝者 LP。動態長度對應 round 數，
    /// 無勝者（round 缺漏或無合格 LP）為 `None`，保留位置以區分「缺 round N」
    /// 與「僅有其餘 round 勝者」。
    #[serde(default)]
    pub per_round_winners: Vec<Option<u32>>,
    /// 穩健候選 LP（跨 round 複合分數中位數最高）
    #[serde(default)]
    pub candidate_lp: Option<u32>,
    /// 穩健亞軍 LP（同規則次高；單一 LP 時為 None）
    #[serde(default)]
    pub runner_up_lp: Option<u32>,
    /// 候選在所有預期 round 中的勝場數
    #[serde(default)]
    pub candidate_wins: u32,
    /// 舊版改善欄位（聚合結果，供既有前端沿用）：
    /// (candidate - runner_up) / runner_up * 100；不可得（缺亞軍/非有限/≤0）為 None
    #[serde(default)]
    pub avg_fps_pct: Option<f64>,
    #[serde(default)]
    pub p1_low_pct: Option<f64>,
    #[serde(default)]
    pub p01_low_pct: Option<f64>,
    /// 評估的確認 round 數（3..=7；配對測量）。舊 session 缺欄為 0。
    #[serde(default)]
    pub evaluated_rounds: u32,
    /// 已停用（新排程以一致性規則 + bootstrap 穩定性區間判定，不再用勝場門檻）；
    /// 固定 0。保留欄位供舊 session 向後相容。
    #[serde(default)]
    pub required_wins: u32,
    /// 確認證據：候選相對亞軍的配對複合分數優勢點估計（%，逐確認 round 平均）。
    /// 不可得（確認不足/無效）為 None。
    #[serde(default)]
    pub composite_advantage_pct: Option<f64>,
    /// 護欄：候選 Avg FPS 相較亞軍（確認 round 中位數，%）
    #[serde(default)]
    pub avg_fps_advantage_pct: Option<f64>,
    /// 護欄：候選 1% low 相較亞軍（確認 round 中位數，%）
    #[serde(default)]
    pub p1_low_advantage_pct: Option<f64>,
    /// 護欄：候選 spike rate 相較亞軍（絕對百分點，正 = 候選較差）
    #[serde(default)]
    pub spike_rate_delta_pp: Option<f64>,
    /// 篩選 round 數（新排程固定 1，僅用於選候選，不參與推論）；舊 session 缺欄為 0。
    #[serde(default)]
    pub screening_rounds: u32,
    /// 確認 round 數（3..=7，配對/區塊排序以控制時間漂移）；舊 session 缺欄為 0。
    #[serde(default)]
    pub confirmation_rounds: u32,
    /// 確認證據：bootstrap 穩定性區間下界（%，複合分數優勢）。
    /// 序列化欄位名保留 `ciLowerPct` 供向後相容；此非統計信賴區間。
    #[serde(default)]
    pub ci_lower_pct: Option<f64>,
    /// 確認證據：bootstrap 穩定性區間上界（%，複合分數優勢）。
    /// 序列化欄位名保留 `ciUpperPct` 供向後相容；此非統計信賴區間。
    #[serde(default)]
    pub ci_upper_pct: Option<f64>,
    /// 停止原因：`"passed"` / `"equivalent"` / `"inconclusive"`；舊 session 為空字串。
    #[serde(default)]
    pub stopping_reason: String,
    /// 前向確認 phase 的最終判定（"passed"/"reversal"/"equivalent"/"inconclusive"）。
    /// 舊 session 為空字串。
    #[serde(default)]
    pub forward_verdict: String,
    /// 是否執行過反向驗證 phase（RunnerUpReversal 觸發）。
    #[serde(default)]
    pub reverse_ran: bool,
    /// 反向驗證 phase 的判定（"passed"/"inconclusive"/""）。
    #[serde(default)]
    pub reverse_verdict: String,
    /// 反向驗證的 predeclared 候選（前向 phase 的亞軍）；未執行為 None。
    #[serde(default)]
    pub reverse_candidate_lp: Option<u32>,
    /// 反向驗證實際完成的配對 round 數。
    #[serde(default)]
    pub reverse_rounds: u32,
    /// 演算法版本：新確認演算法（有界 log-ratio 分數 + 等效判定）為 2；
    /// 舊 session 缺欄 → 0。
    #[serde(default)]
    pub algorithm_version: u32,
    /// 等效判定的 raw median evidence：候選 Avg FPS 相較亞軍（確認 round 中位數，%）。
    /// 非 Equivalent 判定時為 None。
    #[serde(default)]
    pub equivalent_avg_improvement_pct: Option<f64>,
    /// 等效判定的 raw median evidence：候選 1% low 相較亞軍（確認 round 中位數，%）。
    #[serde(default)]
    pub equivalent_p1_improvement_pct: Option<f64>,
    /// 等效判定的 raw median evidence：候選 0.1% low 相較亞軍（確認 round 中位數，%）。
    #[serde(default)]
    pub equivalent_p01_improvement_pct: Option<f64>,
    /// 等效判定的 raw median evidence：候選 frametime MAD 相較亞軍（絕對百分點差）。
    #[serde(default)]
    pub equivalent_mad_delta_pp: Option<f64>,
    /// 等效判定的 raw median evidence：候選 spike rate 相較亞軍（絕對百分點差）。
    #[serde(default)]
    pub equivalent_spike_delta_pp: Option<f64>,
}

/// Session 層的 capture 完整性摘要（新欄位；舊 session 缺欄 → serde default）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct CaptureQuality {
    /// 所有 capture attempt 總數（含校準、overflow retry、drift rerun）。
    #[serde(default)]
    pub total_captures: u32,
    /// 通過完整性閘（valid）的 capture attempt 數。
    #[serde(default)]
    pub valid_captures: u32,
    /// 未通過完整性閘（invalid）的 capture attempt 數。
    #[serde(default)]
    pub invalid_captures: u32,
    /// 因 workload 視窗完整性（前景/位置/遮擋）失敗而判定 invalid 的 capture 數。
    /// additive：舊 session 缺欄 → serde default 0。
    #[serde(default)]
    pub window_invalid_captures: u32,
    /// 因視窗完整性失敗而觸發的 capture 重跑次數（不含首次）。
    /// additive：舊 session 缺欄 → serde default 0。
    #[serde(default)]
    pub window_retry_captures: u32,
    /// 累計 overflowed present events（所有 capture 加總）。
    #[serde(default)]
    pub overflowed_present_events: u64,
    /// 累計 ETW events lost（所有 capture 加總）。
    #[serde(default)]
    pub etw_events_lost: u64,
    /// 所有正式用於結果的 capture 皆完整且 session 完成才 true。
    #[serde(default)]
    pub integrity_passed: bool,
    /// 校準/正式鎖定的有效 FPS cap（Fixed 模式 = config.fps_cap）。
    #[serde(default)]
    pub effective_fps_cap: u32,
    /// 校準/正式鎖定的 circular buffer size。
    #[serde(default)]
    pub circular_buffer_size: u32,
}

/// Session 層的環境穩定度摘要（新欄位；舊 session 缺欄 → serde default）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentStability {
    /// 環境閘（AC/電池節能/CPU idle）是否通過。
    #[serde(default)]
    pub passed: bool,
    /// 累計漂移重跑次數（screening block + confirmation pair）。
    #[serde(default)]
    pub drift_reruns: u32,
    /// 不穩定時的穩定錯誤碼（= BENCHMARK_ENV_UNSTABLE）；穩定為 None。
    #[serde(default)]
    pub error: Option<String>,
}

/// 歷史列表用的摘要
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick: Option<physical::QuickResult>,
    pub id: String,
    #[serde(default)]
    pub status: SessionStatus,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub gpu_name: String,
    /// GPU fingerprint = 穩定 PnP instance id
    #[serde(default)]
    pub gpu_instance_id: String,
    /// CPU fingerprint：`cpu_fingerprint_with`（CPU 身分 + 拓撲），判斷相容性
    #[serde(default)]
    pub cpu_fingerprint: String,
    #[serde(default)]
    pub best_lp: Option<u32>,
    /// 可靠性/信心摘要。`#[serde(default)]` 讓沒有此欄位的舊 session.json 仍可載入，
    /// 解讀為 `ReliabilityStatus::Unassessed`（不可套用）。
    #[serde(default)]
    pub reliability: ReliabilitySummary,
    /// 嚴重 LP（avg/1%/0.1% 低於中位數 85%，或 STDEV 高於 150%）
    #[serde(default)]
    pub severe_lps: Vec<u32>,
    #[serde(default)]
    pub sample_count: u32,
    /// 整個 session 資料夾的位元組數（list/get 時即時計算）
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub config: BenchmarkConfig,
    /// 終結失敗原因（穩定錯誤代碼，查 i18n errors.*）；成功/取消為 None。
    /// `#[serde(default)]` 讓沒有此欄位的舊 session.json 仍可載入。
    #[serde(default)]
    pub error: Option<String>,
    /// 篩選（1 輪）+ refinement（2 輪）後的 Top 1 候選 LP（新欄位；舊 session 缺欄 → None）。
    #[serde(default)]
    pub screening_candidate_lp: Option<u32>,
    /// 篩選（1 輪）+ refinement（2 輪）後的 Top 2 亞軍 LP（新欄位；舊 session 缺欄 → None）。
    #[serde(default)]
    pub screening_runner_up_lp: Option<u32>,
    /// 前向確認 phase 的勝者 LP（Passed=候選；Reversal=亞軍；否則 None）。
    #[serde(default)]
    pub confirmation_winner_lp: Option<u32>,
    /// 反向驗證確認的最終最佳 LP（只有反向 Passed 才設置）。新 session 的
    /// `best_lp` 僅在此值存在時才設置；舊 Passed session 仍可用 `best_lp` fallback。
    #[serde(default)]
    pub verified_best_lp: Option<u32>,
    /// capture 完整性摘要（新欄位；舊 session 缺欄 → default）。
    #[serde(default)]
    pub capture_quality: CaptureQuality,
    /// 環境穩定度摘要（新欄位；舊 session 缺欄 → default）。
    #[serde(default)]
    pub environment_stability: EnvironmentStability,
    /// Equivalent 判定時的等效 finalists（[candidate, runner]）；非 Equivalent → 空。
    /// 新欄位；舊 session 缺欄 → 空。
    #[serde(default)]
    pub equivalent_finalist_lps: Vec<u32>,
}

/// 歷史 session 的「可否套用」狀態（前端顯示用；相容性判定只存在後端）
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[cfg(test)]
pub struct ApplyStatus {
    pub can_apply: bool,
    /// None = 可套用；Some(穩定錯誤代碼) 查 i18n errors.*
    #[serde(default)]
    pub reason: Option<String>,
    /// 是否為 equivalent-mode session（`reliability.status == Equivalent`）。
    #[serde(default)]
    pub equivalent_mode: bool,
    /// 允許套用的 LP（equivalent finalists）；非 equivalent → 空。
    #[serde(default)]
    pub allowed_lps: Vec<u32>,
    /// 套用前是否需先完成 safety validation（equivalent 且尚未 Passed）。
    #[serde(default)]
    pub requires_safety_validation: bool,
}

/// 等效安全驗證狀態（serde PascalCase；default None）。
/// 供後續 task 執行等效 finalists 的安全驗證；目前僅定義資料型別。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EquivalentSafetyStatus {
    #[default]
    None,
    Pending,
    Passed,
    Failed,
    Cancelled,
}

/// 等效安全驗證 contract（供後續 task 填值；現僅定義資料型別，
/// serde default 全空，舊 JSON 必能載入）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct EquivalentSafetyValidation {
    #[serde(default)]
    pub status: EquivalentSafetyStatus,
    /// 被選為最終等效 LP（validation 對象）。
    #[serde(default)]
    pub selected_lp: Option<u32>,
    /// 參考 LP（與 selected_lp 比較的基準）。
    #[serde(default)]
    pub reference_lp: Option<u32>,
    #[serde(default)]
    pub rounds: u32,
    /// 五個 delta：avg/p1/p01 改善 %（相對）、mad/spike 絕對百分點差。
    #[serde(default)]
    pub avg_improvement_pct: Option<f64>,
    #[serde(default)]
    pub p1_improvement_pct: Option<f64>,
    #[serde(default)]
    pub p01_improvement_pct: Option<f64>,
    #[serde(default)]
    pub mad_delta_pp: Option<f64>,
    #[serde(default)]
    pub spike_delta_pp: Option<f64>,
    #[serde(default)]
    pub capture_quality: CaptureQuality,
    #[serde(default)]
    pub environment_stability: EnvironmentStability,
    #[serde(default)]
    pub validated_at: Option<String>,
    /// reference policy mask 快照（可精確驗證單 LP 策略的 snapshot bytes）。
    #[serde(default)]
    pub reference_policy_mask: Option<Vec<u8>>,
    /// 失敗原因（穩定錯誤代碼或 `"passed"`）；Failed/Cancelled 才有值。
    /// `ImmediatePass`（目前核心已在等效組、無 capture）標記 `"already_in_equivalent_pair"`
    /// 且 `rounds=0`，供前端區分「無額外 capture」與一般 3-round 驗證通過。
    #[serde(default)]
    pub reason: Option<String>,
}

/// 單一 session 完整內容（儲存於 session.json）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub summary: SessionSummary,
    #[serde(default)]
    pub results: Vec<LpResult>,
    #[serde(default)]
    pub samples: Vec<CoreSample>,
    /// 篩選階段結果；新版每顆核心以第一個 LP 記錄指標，完整目標見 summary.quick。
    #[serde(default)]
    pub screening_results: Vec<LpResult>,
    /// 新版前兩名的獨立複測結果；舊格式仍保留原 refinement 語意。
    #[serde(default)]
    pub refinement_results: Vec<LpResult>,
    /// 前向確認階段（Top 2，3..=7 round）的逐 LP 聚合結果；不混入篩選/refinement。
    #[serde(default)]
    pub confirmation_results: Vec<LpResult>,
    /// 等效安全驗證 contract（後續 task 填值；目前 None）。舊 session 缺欄 → None。
    #[serde(default)]
    pub equivalent_safety_validation: Option<EquivalentSafetyValidation>,
}

/// 目前背景 GPU 操作種類（runtime contract）。serde PascalCase；null = 無操作。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchmarkOperation {
    /// 一般基準測試（run_benchmark）。
    Benchmark,
    /// 等效安全驗證（run_equivalent_validation）。
    EquivalentValidation,
}

/// FrameAnchor 主視窗的執行期版面（runtime contract）。serde PascalCase。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WindowLayout {
    /// 一般 UI（完整導覽/設定）。
    #[default]
    Normal,
    /// 基準測試 compact progress 視窗（480×300 logical、右下角、僅進度/取消）。
    CompactProgress,
}

/// workload 視窗完整性快照（capture/warmup 期間輪詢回報；camelCase）。
/// `foreground`/`position` 為「良好」布林（true = ok）；`minimized`/`occlusion`
/// 為「不良」布林（true = 異常）。`retries` = 累計視窗完整性重跑次數；
/// `error` = 重試上限用盡後的穩定錯誤碼（查 i18n errors.*）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct WindowIntegrity {
    #[serde(default)]
    pub foreground: bool,
    #[serde(default)]
    pub minimized: bool,
    #[serde(default)]
    pub position: bool,
    #[serde(default)]
    pub occlusion: bool,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub error: Option<String>,
}

/// 執行期狀態（AppState 持有，get_benchmark_state 回傳給前端）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkState {
    #[serde(default)]
    pub current_phase: Option<BenchmarkPhase>,
    #[serde(default)]
    pub gpu_busy: bool,
    #[serde(default)]
    pub current_target: Option<physical::CoreTarget>,
    #[serde(default)]
    pub status: SessionStatus,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub current_lp: Option<u32>,
    #[serde(default)]
    pub stage: BenchmarkStage,
    #[serde(default)]
    pub progress_pct: u32,
    #[serde(default)]
    pub elapsed_secs: u64,
    #[serde(default)]
    pub cancel_requested: bool,
    /// 啟動還原失敗 → true：封鎖新的 test/apply
    #[serde(default)]
    pub recovery_required: bool,
    /// 目前背景 GPU 操作（Benchmark | EquivalentValidation | null）。
    #[serde(default)]
    pub operation: Option<BenchmarkOperation>,
    /// 主視窗執行期版面（Normal | CompactProgress）。
    #[serde(default)]
    pub window_layout: WindowLayout,
    /// workload 視窗完整性快照（預設全「良好」）。
    #[serde(default)]
    pub window_integrity: WindowIntegrity,
    /// 取消專用階段（requested/stopping/restoring/finalizing）；無取消為 None。
    #[serde(default)]
    pub cancel_stage: Option<String>,
    /// 取消專用百分比（0..100）；無取消為 None。
    #[serde(default)]
    pub cancel_progress: Option<u32>,
}

/// 儲存體資訊（get_benchmark_storage_info）
#[derive(Serialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfo {
    pub total_bytes: u64,
    pub session_count: usize,
}

// ── CPU fingerprint ─────────────────────────────────────────────────────

/// 穩定 CPU 身分（不含時脈等易變數值）：arch/family/model/stepping。
/// 由 `GetNativeSystemInfo` 取得，無外部工具；同一顆 CPU 永不改變。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuIdentity {
    /// PROCESSOR_ARCHITECTURE（如 9 = PROCESSOR_ARCHITECTURE_AMD64）
    pub architecture: u16,
    /// wProcessorLevel（family）
    pub family: u16,
    /// wProcessorRevision 高 byte（model）
    pub model: u8,
    /// wProcessorRevision 低 byte（stepping）
    pub stepping: u8,
}

/// 偵測目前 CPU 身分（純 Win32，無外部工具）
pub fn detect_cpu_identity() -> CpuIdentity {
    use windows::Win32::System::SystemInformation::{GetNativeSystemInfo, SYSTEM_INFO};
    let mut si = SYSTEM_INFO::default();
    unsafe {
        GetNativeSystemInfo(&mut si);
        // union 欄位與 API 呼叫都需 unsafe 區塊
        CpuIdentity {
            architecture: si.Anonymous.Anonymous.wProcessorArchitecture.0,
            family: si.wProcessorLevel,
            // wProcessorRevision：高 byte = model，低 byte = stepping
            model: (si.wProcessorRevision >> 8) as u8,
            stepping: si.wProcessorRevision as u8,
        }
    }
}

/// 純函式 CPU 指紋：canonical 描述（CPU 身分 + LP 數 + 每實體核心的
/// efficiency/LP 映射）的 sha256 hex。測試注入固定 `CpuIdentity` 即為
/// 完全確定性；生產用 `cpu_fingerprint_with(&topo, &detect_cpu_identity())`。
pub fn cpu_fingerprint_with(topo: &crate::topology::Topology, cpu: &CpuIdentity) -> String {
    use sha2::{Digest, Sha256};
    let mut canonical = format!(
        "arch={};family={};model={};stepping={};lp={};",
        cpu.architecture, cpu.family, cpu.model, cpu.stepping, topo.total_lp
    );
    for core in &topo.physical_cores {
        let lps = core
            .lp_indices
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        canonical.push_str(&format!("{}:{}\n", core.efficiency_class, lps));
    }
    let mut h = Sha256::new();
    h.update(canonical.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{build_topology, Topology};

    fn fixed_identity() -> CpuIdentity {
        CpuIdentity {
            architecture: 9, // PROCESSOR_ARCHITECTURE_AMD64
            family: 6,
            model: 183,
            stepping: 1,
        }
    }

    fn fp(t: &Topology) -> String {
        cpu_fingerprint_with(t, &fixed_identity())
    }

    fn topo_8c16t() -> Topology {
        build_topology(
            (0..8u32)
                .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
                .collect(),
        )
    }

    #[test]
    fn identical_topology_same_fingerprint() {
        let a = topo_8c16t();
        let b = topo_8c16t();
        assert_eq!(fp(&a), fp(&b));
        assert_eq!(fp(&a).len(), 64); // sha256 hex
    }

    #[test]
    fn different_topology_different_fingerprint() {
        let a = topo_8c16t();
        let b = build_topology((0..8u32).map(|c| (vec![c], 0, false)).collect());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn smt_on_off_different_fingerprint() {
        let a = topo_8c16t();
        let b = build_topology((0..8u32).map(|c| (vec![c * 2], 0, true)).collect());
        assert_ne!(fp(&a), fp(&b));
    }

    #[test]
    fn different_cpu_identity_different_fingerprint() {
        let t = topo_8c16t();
        let intel = CpuIdentity {
            architecture: 9,
            family: 6,
            model: 183,
            stepping: 1,
        };
        let amd = CpuIdentity {
            architecture: 9,
            family: 25,
            model: 33,
            stepping: 2,
        };
        assert_ne!(
            cpu_fingerprint_with(&t, &intel),
            cpu_fingerprint_with(&t, &amd)
        );
    }

    #[test]
    fn fingerprint_stable_across_serialization() {
        let t = topo_8c16t();
        let fp = fp(&t);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn benchmark_state_serializes_camel_case() {
        let s = BenchmarkState {
            current_phase: None,
            gpu_busy: false,
            current_target: None,
            status: SessionStatus::Running,
            session_id: Some("x".into()),
            current_lp: Some(3),
            stage: BenchmarkStage::Collecting,
            progress_pct: 40,
            elapsed_secs: 8,
            cancel_requested: false,
            recovery_required: false,
            operation: Some(BenchmarkOperation::Benchmark),
            window_layout: WindowLayout::CompactProgress,
            window_integrity: WindowIntegrity {
                foreground: true,
                position: true,
                retries: 1,
                ..Default::default()
            },
            cancel_stage: Some("stopping".to_string()),
            cancel_progress: Some(20),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"sessionId\""));
        assert!(json.contains("\"currentLp\""));
        assert!(json.contains("\"progressPct\""));
        assert!(json.contains("\"recoveryRequired\""));
        assert!(json.contains("\"Running\""));
        assert!(json.contains("\"Collecting\""));
        assert!(json.contains("\"operation\":\"Benchmark\""));
        assert!(json.contains("\"windowLayout\":\"CompactProgress\""));
        assert!(json.contains("\"windowIntegrity\""));
        assert!(json.contains("\"retries\":1"));
        // 取消欄位 camelCase 序列化
        assert!(json.contains("\"cancelStage\":\"stopping\""), "json={json}");
        assert!(json.contains("\"cancelProgress\":20"));
    }

    /// 取消欄位為 optional（serde default）：舊 state JSON 缺欄反序列化為 None，
    /// 不破壞向後相容；新欄位可 camelCase 回讀。
    #[test]
    fn benchmark_state_cancel_fields_backward_compat() {
        let old = r#"{"status":"Running","sessionId":"x","progressPct":40}"#;
        let back: BenchmarkState = serde_json::from_str(old).unwrap();
        assert_eq!(back.cancel_stage, None);
        assert_eq!(back.cancel_progress, None);
    }

    /// 新 percentile 欄位以 camelCase 序列化；舊 session 缺欄反序列化為 None（向後相容）。
    #[test]
    fn lp_result_percentile_serializes_and_deserializes_backward_compat() {
        let r = LpResult {
            lp: 0,
            p1_percentile: Some(100.0),
            p01_percentile: Some(90.0),
            p001_percentile: Some(80.0),
            p0005_percentile: Some(70.0),
            ..Default::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"p1Percentile\":100.0"), "json={json}");
        assert!(json.contains("\"p0005Percentile\":70.0"));
        // 舊 session 只帶 low 欄位、缺 percentile → 反序列化後 percentile 為 None
        let back: LpResult =
            serde_json::from_str(r#"{"lp":3,"avgFps":240.0,"p1Low":90.0}"#).unwrap();
        assert_eq!(back.p1_percentile, None);
        assert_eq!(back.p1_low, Some(90.0));
    }

    /// per_round_winners 以三欄固定位置（含 None）序列化，round 1 缺漏不會
    /// 被壓縮掉，能與 round 0/2 的勝者區分。
    #[test]
    fn reliability_per_round_winners_preserves_missing_positions() {
        let rel = ReliabilitySummary {
            status: ReliabilityStatus::Passed,
            per_round_winners: vec![Some(0), None, Some(2)],
            candidate_lp: Some(0),
            runner_up_lp: Some(2),
            candidate_wins: 2,
            avg_fps_pct: Some(1.0),
            p1_low_pct: Some(1.0),
            p01_low_pct: Some(1.0),
            ..Default::default()
        };
        let json = serde_json::to_string(&rel).unwrap();
        // None 序列化為 null（不是省略），固定三欄位置保留
        assert!(
            json.contains("\"perRoundWinners\":[0,null,2]"),
            "per_round_winners 應保留缺漏位置: {json}"
        );
        let back: ReliabilitySummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.per_round_winners, vec![Some(0), None, Some(2)]);
    }

    /// 舊 session 缺新欄位（evaluated_rounds/composite/護欄）→ 反序列化為預設值
    /// （0/None）不報錯；`Equivalent` 可正確序列化回讀。
    #[test]
    fn reliability_old_json_and_equivalent_roundtrip() {
        let old = r#"{"status":"Passed","perRoundWinners":[0,null,2],"candidateLp":0,"runnerUpLp":2,"candidateWins":2,"avgFpsPct":1.0,"p1LowPct":1.0,"p01LowPct":1.0}"#;
        let back: ReliabilitySummary = serde_json::from_str(old).unwrap();
        assert_eq!(back.status, ReliabilityStatus::Passed);
        assert_eq!(back.evaluated_rounds, 0);
        assert_eq!(back.required_wins, 0);
        assert_eq!(back.composite_advantage_pct, None);
        assert_eq!(back.avg_fps_advantage_pct, None);
        assert_eq!(back.p1_low_advantage_pct, None);
        assert_eq!(back.spike_rate_delta_pp, None);
        // 新排程欄位：舊 session 缺欄 → serde 預設值（0/None/空字串）
        assert_eq!(back.screening_rounds, 0);
        assert_eq!(back.confirmation_rounds, 0);
        assert_eq!(back.ci_lower_pct, None);
        assert_eq!(back.ci_upper_pct, None);
        assert_eq!(back.stopping_reason, "");

        let eq = ReliabilitySummary {
            status: ReliabilityStatus::Equivalent,
            ..Default::default()
        };
        let json = serde_json::to_string(&eq).unwrap();
        assert!(json.contains("\"status\":\"Equivalent\""), "json={json}");
        let back2: ReliabilitySummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back2.status, ReliabilityStatus::Equivalent);
    }

    /// 新排程欄位以 camelCase 序列化，並可回讀（screening/confirmation/CI/stopping）。
    #[test]
    fn reliability_new_schedule_fields_roundtrip() {
        let rel = ReliabilitySummary {
            status: ReliabilityStatus::Passed,
            screening_rounds: 2,
            confirmation_rounds: 3,
            ci_lower_pct: Some(1.5),
            ci_upper_pct: Some(4.2),
            stopping_reason: "passed".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&rel).unwrap();
        assert!(json.contains("\"screeningRounds\":2"), "json={json}");
        assert!(json.contains("\"confirmationRounds\":3"));
        assert!(json.contains("\"ciLowerPct\":1.5"));
        assert!(json.contains("\"ciUpperPct\":4.2"));
        assert!(json.contains("\"stoppingReason\":\"passed\""));
        let back: ReliabilitySummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.screening_rounds, 2);
        assert_eq!(back.confirmation_rounds, 3);
        assert_eq!(back.ci_lower_pct, Some(1.5));
        assert_eq!(back.ci_upper_pct, Some(4.2));
        assert_eq!(back.stopping_reason, "passed");
    }

    /// 舊 session 的 LpResult 缺 MAD/spike → 反序列化為 None
    #[test]
    fn lp_result_frametime_robustness_backward_compat() {
        let back: LpResult =
            serde_json::from_str(r#"{"lp":3,"avgFps":240.0,"p1Low":90.0}"#).unwrap();
        assert_eq!(back.frametime_mad_pct, None);
        assert_eq!(back.spike_rate_pct, None);
    }

    #[test]
    fn default_config_uses_product_defaults() {
        let c = BenchmarkConfig::default();
        assert!(!c.fullscreen, "產品預設應為視窗模式");
        assert_eq!(c.width, 1280);
        assert_eq!(c.height, 720);
        assert_eq!(c.width, default_width());
        assert_eq!(c.height, default_height());
        assert_eq!(c.sample_secs, 10);
        assert_eq!(c.warm_up_secs, 3);
        assert_eq!(c.retest_warm_up_secs, 5);
        assert_eq!(c.retest_sample_secs, 20);
        assert_eq!(c.repetitions, 5, "產品預設應為 5 round");
    }

    #[test]
    fn default_vulkan_args_match_default_config() {
        let c = BenchmarkConfig::default();
        assert_eq!(c.vulkan_args, default_vulkan_args());
        let args = default_vulkan_args();
        assert!(args.iter().any(|a| a == "--fullscreen=0"));
        assert!(args.iter().any(|a| a == "--width=1280"));
        assert!(args.iter().any(|a| a == "--height=720"));
    }

    #[test]
    fn missing_fields_get_product_defaults() {
        let c: BenchmarkConfig = serde_json::from_str("{}").unwrap();
        assert!(!c.fullscreen);
        assert_eq!(c.width, 1280);
        assert_eq!(c.height, 720);
    }

    #[test]
    fn explicit_fields_not_overwritten() {
        let c: BenchmarkConfig =
            serde_json::from_str(r#"{"fullscreen":true,"width":800,"height":600}"#).unwrap();
        assert!(c.fullscreen, "顯式 fullscreen=true 不得被覆寫");
        assert_eq!(c.width, 800);
        assert_eq!(c.height, 600);
    }

    /// 新演算法欄位（algorithm_version + 等效 raw median evidence）以 camelCase 序列化，
    /// 舊 session 缺欄 → 0 / None（向後相容）。
    #[test]
    fn reliability_algorithm_version_and_equivalent_evidence_roundtrip() {
        let rel = ReliabilitySummary {
            status: ReliabilityStatus::Equivalent,
            algorithm_version: 2,
            equivalent_avg_improvement_pct: Some(0.1),
            equivalent_p1_improvement_pct: Some(-1.09),
            equivalent_p01_improvement_pct: Some(0.05),
            equivalent_mad_delta_pp: Some(0.3),
            equivalent_spike_delta_pp: Some(0.01),
            ..Default::default()
        };
        let json = serde_json::to_string(&rel).unwrap();
        assert!(json.contains("\"algorithmVersion\":2"), "json={json}");
        assert!(json.contains("\"equivalentAvgImprovementPct\":0.1"));
        assert!(json.contains("\"equivalentP1ImprovementPct\":-1.09"));
        assert!(json.contains("\"equivalentP01ImprovementPct\":0.05"));
        assert!(json.contains("\"equivalentMadDeltaPp\":0.3"));
        assert!(json.contains("\"equivalentSpikeDeltaPp\":0.01"));
        let back: ReliabilitySummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.algorithm_version, 2);
        assert_eq!(back.equivalent_avg_improvement_pct, Some(0.1));
        assert_eq!(back.equivalent_p1_improvement_pct, Some(-1.09));

        // 舊 session 缺新欄位 → 0 / None
        let old =
            r#"{"status":"Passed","perRoundWinners":[0,null,2],"candidateLp":0,"runnerUpLp":2}"#;
        let old_rel: ReliabilitySummary = serde_json::from_str(old).unwrap();
        assert_eq!(old_rel.algorithm_version, 0);
        assert_eq!(old_rel.equivalent_avg_improvement_pct, None);
        assert_eq!(old_rel.equivalent_mad_delta_pp, None);
        assert_eq!(old_rel.equivalent_spike_delta_pp, None);
    }

    /// SessionSummary 新增 equivalent_finalist_lps：非 Equivalent 空、舊 session 缺欄空。
    #[test]
    fn session_summary_equivalent_finalist_lps_roundtrip() {
        let s = SessionSummary {
            id: "x".into(),
            status: SessionStatus::Completed,
            started_at: "t".into(),
            equivalent_finalist_lps: vec![3, 7],
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains("\"equivalentFinalistLps\":[3,7]"),
            "json={json}"
        );
        let back: SessionSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.equivalent_finalist_lps, vec![3, 7]);
        // 舊 session 缺欄 → 空
        let old: SessionSummary = serde_json::from_str(r#"{"id":"y","startedAt":"t"}"#).unwrap();
        assert!(old.equivalent_finalist_lps.is_empty());
    }

    /// 等效安全驗證 contract：default 全空、PascalCase status、舊 JSON 必能載入。
    #[test]
    fn equivalent_safety_validation_defaults_and_roundtrip() {
        let v = EquivalentSafetyValidation {
            status: EquivalentSafetyStatus::Passed,
            selected_lp: Some(3),
            reference_lp: Some(7),
            rounds: 3,
            p1_improvement_pct: Some(-1.09),
            reference_policy_mask: Some(vec![0x08, 0x00]),
            ..Default::default()
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"status\":\"Passed\""), "json={json}");
        assert!(json.contains("\"selectedLp\":3"));
        assert!(json.contains("\"referencePolicyMask\":[8,0]"));
        let back: EquivalentSafetyValidation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, EquivalentSafetyStatus::Passed);
        assert_eq!(back.selected_lp, Some(3));
        assert_eq!(back.reference_policy_mask, Some(vec![8, 0]));
        // default status = None
        assert_eq!(
            EquivalentSafetyValidation::default().status,
            EquivalentSafetyStatus::None
        );

        // SessionDetail 舊 JSON 缺欄 → None
        let detail: SessionDetail =
            serde_json::from_str(r#"{"summary":{"id":"x","startedAt":"t","status":"Completed"}}"#)
                .unwrap();
        assert!(detail.equivalent_safety_validation.is_none());
    }

    /// WindowIntegrity camelCase 序列化 + 舊 session 缺欄 → default。
    #[test]
    fn window_integrity_serializes_camel_case_and_defaults() {
        let wi = WindowIntegrity {
            foreground: true,
            minimized: false,
            position: true,
            occlusion: true,
            retries: 2,
            error: Some("BENCHMARK_WINDOW_INTEGRITY".to_string()),
        };
        let json = serde_json::to_string(&wi).unwrap();
        assert!(json.contains("\"foreground\":true"));
        assert!(json.contains("\"occlusion\":true"));
        assert!(json.contains("\"retries\":2"));
        assert!(json.contains("\"error\":\"BENCHMARK_WINDOW_INTEGRITY\""));
        // default 全空（retries=0, error=None）
        assert_eq!(WindowIntegrity::default().retries, 0);
        assert_eq!(WindowIntegrity::default().error, None);
    }

    /// CaptureQuality 新增視窗計數器 additive：舊 session 缺欄 → serde default 0。
    #[test]
    fn capture_quality_window_counters_backward_compat() {
        let old = r#"{"totalCaptures":10,"validCaptures":9,"invalidCaptures":1,"overflowedPresentEvents":0,"etwEventsLost":0,"integrityPassed":true,"effectiveFpsCap":500,"circularBufferSize":8192}"#;
        let back: CaptureQuality = serde_json::from_str(old).unwrap();
        assert_eq!(back.total_captures, 10);
        assert_eq!(back.window_invalid_captures, 0);
        assert_eq!(back.window_retry_captures, 0);
        // 新值可序列化並回讀
        let cq = CaptureQuality {
            window_invalid_captures: 3,
            window_retry_captures: 2,
            ..Default::default()
        };
        let json = serde_json::to_string(&cq).unwrap();
        assert!(json.contains("\"windowInvalidCaptures\":3"));
        assert!(json.contains("\"windowRetryCaptures\":2"));
    }
}
