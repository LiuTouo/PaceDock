#[path = "core_apply.rs"]
pub mod core_apply;
// 基準測試管理者（Task 1 骨架）：AppState 持有 `BenchmarkManager`，
// 提供狀態查詢、取消訊號、實體核心套用、先前策略還原與啟動回復。
//
// 所有會動系統的協調都寫成接受注入路徑的 free function，
// 單元測試用 fake backend + 暫存目錄跑完整流程，不碰真實 HKLM/裝置。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use tauri::{AppHandle, Emitter, Manager};

#[cfg(test)]
use super::{
    ApplyStatus, EnvironmentStability, EquivalentSafetyStatus, EquivalentSafetyValidation,
    ReliabilityStatus, SessionSummary,
};
use crate::config;
use crate::error::codes;
#[cfg(test)]
use crate::gpu::single_lp_mask_bytes;
use crate::gpu::{
    policy_matches, restore_snapshot, AffinityPolicy, GpuBackend, RealSleeper,
    RegistryValueSnapshot, Sleep, DEVICE_POLICY_SINGLE_PROCESSOR,
};
use crate::topology::Topology;

use super::assets::{self, BenchmarkAssets};
use super::env::RealEnvironmentProbe;
use super::process_win::RealProcessRunner;
use super::recovery::{self, RecoveryJournal, RecoveryStage};
use super::runner::{self, CancelSignal, ProcessRunner, RunContext};
use super::storage;
use super::window_layout::{self, plan_layout, RealMainWindowController};
use super::window_win::RealWorkloadWindow;
use super::{
    cpu_fingerprint_with, detect_cpu_identity, BenchmarkConfig, BenchmarkOperation,
    BenchmarkProgress, BenchmarkStage, BenchmarkState, CpuIdentity, SessionDetail, SessionStatus,
    WindowIntegrity, WindowLayout,
};

/// 一層還原記錄檔：`%APPDATA%\FrameAnchor\gpu-restore.json`。
/// 只保留最近一次成功套用的快照（one-level）。
pub fn restore_record_path() -> PathBuf {
    config::config_dir().join("gpu-restore.json")
}

/// apply mutation 的失敗結果：穩定錯誤碼 + 是否已乾淨還原。
/// `clean=false` 表示本次 mutation 無法證明「完整 rollback + 所有 recovery
/// artifact 清理成功」，呼叫端（manager）必須設 recoveryRequired 封鎖後續
/// mutation/benchmark，不依賴 journal 是否存在（journal 可能停在過低 stage）。
#[derive(Debug, Clone)]
pub struct ApplyError {
    pub code: String,
    pub clean: bool,
}

impl ApplyError {
    /// 尚未動到任何狀態（前置驗證）的失敗：clean。
    fn clean(code: &str) -> Self {
        ApplyError {
            code: code.to_string(),
            clean: true,
        }
    }
}

/// 前置失敗（尚未 mutation）以 String 錯誤碼解讀為 clean。
impl From<String> for ApplyError {
    fn from(code: String) -> Self {
        ApplyError { code, clean: true }
    }
}

/// 與穩定錯誤碼字串比較（只比 `code`），供測試 `assert_eq!(err, codes::X)` 使用。
impl PartialEq<&str> for ApplyError {
    fn eq(&self, other: &&str) -> bool {
        self.code == *other
    }
}

// ── 協調流程（free function，可注入路徑測試）────────────────────────────

/// 把已完成 session 的最佳 LP 套用到對應 GPU。
/// 步驟：相容性驗證 → BasicDisplay 防呆 → 委派到 [`apply_affinity_to_gpu`]。
/// `sleeper` 與 `cpu_identity` 注入，讓測試不真睡、不依賴真實 CPU 身分。
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn apply_best_affinity(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    cpu_identity: &CpuIdentity,
    topo: &Topology,
    storage_root: &Path,
    journal_path: &Path,
    restore_path: &Path,
    session_id: &str,
) -> Result<(), ApplyError> {
    // 1) session 存在且已完成、有最佳 LP；內容必須通過 HMAC 認證
    //（session.json 位於可寫 APPDATA，偽造 Passed/bestLp 會驅動特權 GPU mutation）
    let detail = storage::get_at_verified(storage_root, session_id)?;
    if detail.summary.status != SessionStatus::Completed {
        return Err(ApplyError::clean(codes::BENCHMARK_SESSION_NOT_COMPLETED));
    }
    let best_lp = detail
        .summary
        .best_lp
        .ok_or_else(|| ApplyError::clean(codes::BENCHMARK_SESSION_NOT_COMPLETED))?;
    // 可靠性必須 Passed（舊 session 無欄位 → Unassessed → 拒絕）
    if detail.summary.reliability.status != ReliabilityStatus::Passed {
        return Err(ApplyError::clean(codes::BENCHMARK_RELIABILITY_NOT_PASSED));
    }
    // AssignmentSetOverride 是 64-bit 單 LP mask（REG_BINARY）；但 LP 必須落在
    // 拓撲實際存在、且 group 0 上限（64）以內
    if best_lp >= topo.total_lp.min(64) {
        return Err(ApplyError::clean(codes::BENCHMARK_SESSION_INCOMPATIBLE));
    }

    // 2) 相容性：CPU 指紋與 GPU instance 必須與本次環境一致
    if detail.summary.cpu_fingerprint != cpu_fingerprint_with(topo, cpu_identity) {
        return Err(ApplyError::clean(codes::BENCHMARK_SESSION_INCOMPATIBLE));
    }
    let instance_id = &detail.summary.gpu_instance_id;
    let present = backend
        .enumerate_present_adapters()
        .map_err(|e| ApplyError::clean(e.code()))?
        .iter()
        .any(|d| d.instance_id.eq_ignore_ascii_case(instance_id));
    if !present {
        return Err(ApplyError::clean(codes::GPU_NOT_FOUND));
    }

    // 3) BasicDisplay 未停用（避免重啟後無顯示 fallback）
    if !backend
        .basic_display_enabled()
        .map_err(|e| ApplyError::clean(e.code()))?
    {
        return Err(ApplyError::clean(codes::GPU_BASIC_DISPLAY_DISABLED));
    }

    // 4) 委派到共享 mutation 路徑
    apply_affinity_to_gpu(
        backend,
        sleeper,
        instance_id,
        best_lp,
        journal_path,
        restore_path,
    )
}

/// 將 GPU 中斷親和性套用到指定 LP。
/// 步驟：快照 + 還原日誌 → 寫策略 → 重啟裝置 → 驗證 → 持久化還原記錄 → 清除日誌。
/// 任何在「策略可能已被修改」之後的失敗都走 [`rollback`]：還原快照並清日誌/記錄；
/// 還原失敗則寫入 stage=PolicyApplied 的日誌等啟動重試（見 [`rollback`]）。
/// 呼叫端負責所有前置驗證（LP 範圍、GPU 存在、BasicDisplay、recovery_required）。
#[cfg(test)]
pub fn apply_affinity_to_gpu(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    instance_id: &str,
    lp: u32,
    journal_path: &Path,
    restore_path: &Path,
) -> Result<(), ApplyError> {
    apply_mask_to_gpu(
        backend,
        sleeper,
        instance_id,
        single_lp_mask_bytes(lp),
        journal_path,
        restore_path,
    )
}

pub fn apply_mask_to_gpu(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    instance_id: &str,
    override_bytes: Vec<u8>,
    journal_path: &Path,
    restore_path: &Path,
) -> Result<(), ApplyError> {
    if override_bytes.is_empty()
        || override_bytes.len() > 8
        || override_bytes.iter().all(|b| *b == 0)
    {
        return Err(ApplyError::clean(codes::BENCHMARK_SESSION_INCOMPATIBLE));
    }
    // 1) 快照目前策略 + 寫還原日誌（第一次變更之前）
    let snapshot = backend
        .read_affinity_policy(instance_id)
        .map_err(|e| ApplyError::clean(e.code()))?;
    recovery::begin_at(journal_path, &snapshot)?;

    // 2) 寫入新策略：DevicePolicy=4（DWORD）+ AssignmentSetOverride=單 LP mask（REG_BINARY）
    let new_policy = AffinityPolicy {
        instance_id: instance_id.to_string(),
        device_policy: RegistryValueSnapshot::dword(DEVICE_POLICY_SINGLE_PROCESSOR),
        assignment_set_override: RegistryValueSnapshot::binary(override_bytes.clone()),
    };
    if let Err(_e) = backend.write_affinity_policy(&new_policy) {
        return Err(rollback(
            backend,
            sleeper,
            &snapshot,
            journal_path,
            restore_path,
            codes::GPU_APPLY_FAILED,
        ));
    }
    if let Err(e) = advance_stage(journal_path, RecoveryStage::PolicyApplied) {
        return Err(rollback(
            backend,
            sleeper,
            &snapshot,
            journal_path,
            restore_path,
            &e,
        ));
    }

    // 3) 重啟裝置（disable→停頓→enable→停頓）
    if let Err(_e) = backend.restart_device(instance_id, sleeper) {
        return Err(rollback(
            backend,
            sleeper,
            &snapshot,
            journal_path,
            restore_path,
            codes::GPU_RESTART_FAILED,
        ));
    }
    if let Err(e) = advance_stage(journal_path, RecoveryStage::DeviceRestarted) {
        return Err(rollback(
            backend,
            sleeper,
            &snapshot,
            journal_path,
            restore_path,
            &e,
        ));
    }

    // 4) 驗證新策略已生效（AssignmentSetOverride 逐位元組比對）
    let read_back = match backend.read_affinity_policy(instance_id) {
        Ok(p) => p,
        Err(e) => {
            return Err(rollback(
                backend,
                sleeper,
                &snapshot,
                journal_path,
                restore_path,
                e.code(),
            ));
        }
    };
    if read_back.device_policy.as_dword() != Some(DEVICE_POLICY_SINGLE_PROCESSOR)
        || read_back.assignment_set_override.bytes.as_deref() != Some(override_bytes.as_slice())
    {
        return Err(rollback(
            backend,
            sleeper,
            &snapshot,
            journal_path,
            restore_path,
            codes::GPU_APPLY_FAILED,
        ));
    }

    // 5) 持久化一層還原記錄，清除日誌
    if let Err(e) = write_restore_record(restore_path, &snapshot) {
        log::error!("寫入還原記錄失敗: {e}");
        return Err(rollback(
            backend,
            sleeper,
            &snapshot,
            journal_path,
            restore_path,
            codes::GPU_APPLY_FAILED,
        ));
    }
    if let Err(e) = recovery::clear_at(journal_path) {
        // 日誌清除失敗：新策略已驗證、還原記錄已寫入，但 stale 的 DeviceRestarted
        // 日誌會在下次啟動誤還原 → 為安全起見 rollback 整個 apply。
        log::error!("清除還原日誌失敗: {e}");
        return Err(rollback(
            backend,
            sleeper,
            &snapshot,
            journal_path,
            restore_path,
            codes::GPU_APPLY_FAILED,
        ));
    }
    Ok(())
}

/// 統一 rollback：把本次 mutation 的 snapshot 寫回並驗證，回報是否乾淨。
/// - 還原成功且 journal + restore record 都清除成功 → `clean=true`。
/// - 還原成功但任一清理失敗，或還原失敗 → `clean=false`：以
///   [`recovery::mark_restore_needed_at`] 寫入 stage=PolicyApplied 日誌作為
///   可偵測的 dirty marker；即使該寫入也失敗，`clean=false` 仍會讓 manager
///   直接封鎖後續 mutation/benchmark，不依賴 journal 是否存在。
fn rollback(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    snapshot: &AffinityPolicy,
    journal_path: &Path,
    restore_path: &Path,
    error_code: &str,
) -> ApplyError {
    match restore_snapshot(backend, sleeper, snapshot) {
        Ok(()) => {
            let cleared_journal = recovery::clear_at(journal_path);
            let cleared_record = clear_restore_record(restore_path);
            if cleared_journal.is_ok() && cleared_record.is_ok() {
                return ApplyError {
                    code: error_code.to_string(),
                    clean: true,
                };
            }
            // 任一清理失敗 → 殘留 stale journal / restore record，本次 mutation
            // 不得視為乾淨；寫回「要求完整 restore」日誌作為可偵測的 dirty marker。
            log::error!(
                "rollback 清理失敗（journal={} restore_record={}）；寫入還原日誌封鎖後續操作",
                cleared_journal.is_ok(),
                cleared_record.is_ok()
            );
            let _ = recovery::mark_restore_needed_at(journal_path, snapshot);
            ApplyError {
                code: error_code.to_string(),
                clean: false,
            }
        }
        Err(e) => {
            log::error!("mutation 還原失敗: {e}；寫入可復原日誌等啟動重試");
            if let Err(mark) = recovery::mark_restore_needed_at(journal_path, snapshot) {
                log::error!("mark_restore_needed 也失敗: {mark}；封鎖後續操作");
            }
            ApplyError {
                code: error_code.to_string(),
                clean: false,
            }
        }
    }
}

/// journal stage advance：讀回 journal 並寫入新 stage。
/// 失敗一律回穩定代碼 [`codes::GPU_APPLY_FAILED`]（內部細節只進 log）。
fn advance_stage(journal_path: &Path, stage: RecoveryStage) -> Result<(), String> {
    let journal = require_journal(journal_path)?;
    recovery::advance_to_at(journal_path, &journal, stage).map_err(|e| {
        log::error!("還原日誌 stage advance 失敗: {e}");
        codes::GPU_APPLY_FAILED.to_string()
    })
}

/// 還原到「上次成功套用」之前的策略（一層還原記錄）。
#[cfg(test)]
pub fn restore_previous_affinity(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    restore_path: &Path,
) -> Result<(), String> {
    let snapshot =
        load_restore_record(restore_path)?.ok_or_else(|| codes::GPU_RESTORE_FAILED.to_string())?;
    restore_snapshot(backend, sleeper, &snapshot)?;
    clear_restore_record(restore_path)?;
    Ok(())
}

/// 啟動時呼叫：存在 pending 日誌則依 stage 還原並清除；失敗回傳 Err。
pub fn attempt_startup_recovery(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    journal_path: &Path,
) -> Result<(), String> {
    let Some(journal) = recovery::load_from(journal_path)? else {
        return Ok(());
    };
    match journal.stage {
        // 尚未改寫任何策略：驗證目前仍等於快照即可清除
        RecoveryStage::SnapshotTaken => {
            let current = backend
                .read_affinity_policy(&journal.instance_id)
                .map_err(|e| e.code().to_string())?;
            if !policy_matches(&journal.snapshot, &current) {
                return Err(codes::GPU_RESTORE_FAILED.to_string());
            }
        }
        // 已寫入/已重啟：完整還原（寫回 + 重啟 + 驗證）
        RecoveryStage::PolicyApplied | RecoveryStage::DeviceRestarted => {
            restore_snapshot(backend, sleeper, &journal.snapshot)?;
        }
    }
    recovery::clear_at(journal_path)
}

fn require_journal(journal_path: &Path) -> Result<RecoveryJournal, String> {
    // 內部不變量防呆：apply 中途日誌不可能消失；若發生回穩定代碼
    recovery::load_from(journal_path)?.ok_or_else(|| codes::GPU_APPLY_FAILED.to_string())
}

// ── 一層還原記錄 ────────────────────────────────────────────────────────

fn write_restore_record(path: &Path, snapshot: &AffinityPolicy) -> Result<(), String> {
    let text = serde_json::to_string_pretty(snapshot).map_err(|e| format!("序列化: {e}"))?;
    crate::state_auth::auth_write(path, &text)
}

fn load_restore_record(path: &Path) -> Result<Option<AffinityPolicy>, String> {
    if !path.exists() {
        return Ok(None);
    }
    // 還原記錄驅動提升權限 HKLM 寫回與裝置重啟 — 必須通過 HMAC 認證
    let text = crate::state_auth::auth_read(path)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("還原記錄解析失敗: {e}"))
}

fn clear_restore_record(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if inject::consume_clear_restore() {
        return Err("injected clear restore record failure".to_string());
    }
    match std::fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("清除還原記錄失敗: {e}")),
    }
}

/// 測試用 fault injection（僅 `#[cfg(test)]`；production 編譯不含）。
/// 採 thread-local，避免測試平行執行時彼此干擾。
#[cfg(test)]
pub mod inject {
    use std::cell::Cell;

    thread_local! {
        static FAIL_CLEAR_RESTORE: Cell<bool> = const { Cell::new(false) };
    }

    /// 讓下一次 `clear_restore_record` 失敗
    pub fn fail_next_clear_restore_record() {
        FAIL_CLEAR_RESTORE.with(|c| c.set(true));
    }
    pub(super) fn consume_clear_restore() -> bool {
        FAIL_CLEAR_RESTORE.with(|c| c.replace(false))
    }
}

// ── GPU 操作 reservation（單一 race-free 排他）───────────────────────────

/// reservation 狀態：Idle = 無操作、Benchmark = 基準測試進行中、
/// Mutation = apply_best / manual apply / restore_previous 進行中。
/// 所有會動 GPU 的操作都必須先 `reserve` 取得排他權；衝突一律回
/// [`codes::BENCHMARK_ALREADY_RUNNING`]（不暴露內部 enum）。
const OP_IDLE: u8 = 0;
const OP_BENCHMARK: u8 = 1;
const OP_MUTATION: u8 = 2;
const OP_VALIDATION: u8 = 3;
const OP_UPDATE: u8 = 4;

/// RAII 釋放：drop 時把 reservation 歸零。背景 benchmark 的 guard 會被移入
/// runner 的 closure，直到 runner 終結（寫完最終 status 後）才 drop，確保
/// 執行期間其他 mutation/start 全被拒絕；panic 也會觸發 drop。
pub(crate) struct GpuOperationGuard {
    reservation: Arc<AtomicU8>,
}

impl Drop for GpuOperationGuard {
    fn drop(&mut self) {
        self.reservation.store(OP_IDLE, Ordering::Release);
    }
}

// ── AppState 持有的管理者 ───────────────────────────────────────────────

/// 基準測試管理者。state 為執行期狀態；recovery_required 標記啟動還原失敗
/// （封鎖新的 test/apply）；cancel 為 runner 用的訊號；reservation 為
/// 單一 race-free 的 GPU 操作排他鎖（Idle/Benchmark/Mutation）。
pub struct BenchmarkManager {
    pub state: RwLock<BenchmarkState>,
    pub backend: Arc<dyn GpuBackend>,
    /// 重啟等待策略（Task 2 亦共用）
    sleeper: Arc<dyn Sleep>,
    pub recovery_required: AtomicBool,
    reservation: Arc<AtomicU8>,
    cancel_tx: tokio::sync::watch::Sender<bool>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
}

impl BenchmarkManager {
    pub fn new(backend: Arc<dyn GpuBackend>) -> Self {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        Self {
            state: RwLock::new(BenchmarkState::default()),
            backend,
            sleeper: Arc::new(RealSleeper),
            recovery_required: AtomicBool::new(false),
            reservation: Arc::new(AtomicU8::new(OP_IDLE)),
            cancel_tx,
            cancel_rx,
        }
    }

    /// 以 CAS 原子取得 GPU 操作排他權（`kind` = OP_BENCHMARK / OP_MUTATION）。
    /// 成功回傳 RAII guard（drop 即釋放）；已有任何操作 → 回穩定代碼
    /// [`codes::BENCHMARK_ALREADY_RUNNING`]。single-flight 由這個原子 CAS 保證，
    /// 兩個同時 `start` 只會有一個成功。
    fn reserve(&self, kind: u8) -> Result<GpuOperationGuard, String> {
        match self
            .reservation
            .compare_exchange(OP_IDLE, kind, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(GpuOperationGuard {
                reservation: self.reservation.clone(),
            }),
            Err(_) => Err(codes::BENCHMARK_ALREADY_RUNNING.to_string()),
        }
    }

    pub(crate) fn reserve_mutation(&self) -> Result<GpuOperationGuard, String> {
        self.reserve(OP_MUTATION)
    }
    pub(crate) fn reserve_update(&self) -> Result<GpuOperationGuard, String> {
        self.reserve(OP_UPDATE)
    }
    pub fn begin_update(&self) -> Result<(), String> {
        self.reservation
            .compare_exchange(OP_IDLE, OP_UPDATE, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| codes::BENCHMARK_ALREADY_RUNNING.into())
    }
    pub fn end_update(&self) {
        let _ = self.reservation.compare_exchange(
            OP_UPDATE,
            OP_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
    pub fn can_exit(&self) -> bool {
        matches!(
            self.reservation.load(Ordering::Acquire),
            OP_IDLE | OP_UPDATE
        ) && !self.is_running()
    }

    /// 開始新 session 前把取消訊號歸零（watch channel 實際值 + state.cancel_requested）。
    /// 修復 request_cancel 只清 state 未清 channel，導致下一場 session 立即 Cancelled。
    fn reset_cancel(&self) {
        let _ = self.cancel_tx.send(false);
        if let Ok(mut s) = self.state.write() {
            s.cancel_requested = false;
            s.cancel_stage = None;
            s.cancel_progress = None;
        }
    }

    /// 啟動時呼叫：存在 pending 還原日誌則嘗試還原。
    /// 失敗 → recovery_required=true，封鎖新的 test/apply。
    pub fn attempt_startup_recovery(&self) {
        match attempt_startup_recovery(
            self.backend.as_ref(),
            self.sleeper.as_ref(),
            &recovery::recovery_path(),
        ) {
            Ok(()) => log::info!("啟動還原：無 pending 或已還原"),
            Err(e) => {
                log::error!("啟動還原失敗: {e}；封鎖基準測試與套用操作");
                self.set_recovery_required();
            }
        }
    }

    pub fn recovery_required(&self) -> bool {
        self.recovery_required.load(Ordering::Relaxed)
    }

    /// 標記 recoveryRequired（atomic flag + state 欄位），封鎖後續 mutation/benchmark。
    fn set_recovery_required(&self) {
        self.recovery_required.store(true, Ordering::Relaxed);
        if let Ok(mut s) = self.state.write() {
            s.recovery_required = true;
        }
    }

    /// apply 回 Err 後，若結果標記 `clean=false`（rollback 無法證明完整還原 +
    /// artifact 清理），設 recoveryRequired 封鎖後續操作等啟動重試。
    /// 不依賴「journal 是否存在」——journal 可能因 `mark_restore_needed` 也失敗而
    /// 停在過低 stage（SnapshotTaken），仍須封鎖。
    fn flag_recovery_if_needed(&self, result: &Result<(), ApplyError>) {
        if let Err(e) = result {
            if !e.clean {
                self.set_recovery_required();
            }
        }
    }

    /// 基準測試執行中？
    pub fn is_running(&self) -> bool {
        self.state
            .read()
            .map(|s| s.status == SessionStatus::Running)
            .unwrap_or(false)
    }

    /// 等效安全驗證背景 capture 進行中？（以 reservation 辨識，不改變 session status）
    pub fn validation_running(&self) -> bool {
        self.reservation.load(Ordering::Acquire) == OP_VALIDATION
    }

    /// 執行中 → 拒絕退出/重啟（讓 runner 完成或安全取消/還原），
    /// 非執行中（Idle/Completed/Failed/Cancelled）→ 允許。
    pub fn refuse_exit_if_running(&self) -> Result<(), String> {
        if self.is_running() || self.reservation.load(Ordering::Acquire) != 0 {
            Err(codes::BENCHMARK_ALREADY_RUNNING.to_string())
        } else {
            Ok(())
        }
    }

    /// 回傳目前狀態（附 recovery_required）
    pub fn state_snapshot(&self) -> BenchmarkState {
        let mut s = self.state.read().map(|s| s.clone()).unwrap_or_default();
        s.recovery_required = self.recovery_required();
        s.gpu_busy = self.reservation.load(Ordering::Acquire) != OP_IDLE;
        s
    }

    /// 套用最佳 LP。recovery 未完成或已有任何 GPU 操作時封鎖。
    #[cfg(test)]
    pub fn apply_best(&self, topo: &Topology, session_id: &str) -> Result<(), String> {
        if self.recovery_required() {
            return Err(codes::BENCHMARK_RECOVERY_REQUIRED.to_string());
        }
        let _guard = self.reserve(OP_MUTATION)?;
        let journal = recovery::recovery_path();
        let result = apply_best_affinity(
            self.backend.as_ref(),
            self.sleeper.as_ref(),
            &detect_cpu_identity(),
            topo,
            &storage::benchmarks_root(),
            &journal,
            &restore_record_path(),
            session_id,
        );
        self.flag_recovery_if_needed(&result);
        result.map_err(|e| e.code)
    }

    /// 只判定某個歷史 session「現在可否套用」（不做任何變更）。
    /// 相容性判定全在後端，前端不重算。
    #[cfg(test)]
    pub fn check_apply(&self, topo: &Topology, session_id: &str) -> ApplyStatus {
        if self.recovery_required() {
            return ApplyStatus {
                can_apply: false,
                reason: Some(codes::BENCHMARK_RECOVERY_REQUIRED.to_string()),
                equivalent_mode: false,
                allowed_lps: Vec::new(),
                requires_safety_validation: false,
            };
        }
        check_apply_at(
            self.backend.as_ref(),
            topo,
            &detect_cpu_identity(),
            &storage::benchmarks_root(),
            session_id,
        )
    }

    /// 可匯入的已完成 session（相容：CPU 指紋一致 + GPU 存在 + 有 bestLp）。
    /// 相容性判定只在後端，前端不重算。
    #[cfg(test)]
    pub fn list_importable(&self, topo: &Topology) -> Vec<SessionSummary> {
        list_importable(
            self.backend.as_ref(),
            topo,
            &detect_cpu_identity(),
            &storage::benchmarks_root(),
        )
    }

    /// 手動套用 GPU 中斷親和性到指定 LP。前置驗證：recovery、執行中、
    /// LP 範圍、GPU 存在、BasicDisplay；驗證後委派到共享 mutation 路徑。
    #[cfg(test)]
    pub fn apply_gpu_affinity(
        &self,
        topo: &Topology,
        instance_id: &str,
        lp: u32,
    ) -> Result<(), String> {
        self.apply_gpu_affinity_at(
            topo,
            instance_id,
            lp,
            &recovery::recovery_path(),
            &restore_record_path(),
        )
    }

    /// 手動套用 GPU 中斷親和性到指定 LP（可注入還原日誌與還原記錄路徑供測試隔離）。
    /// 前置驗證與 [`apply_gpu_affinity`] 相同。
    #[cfg(test)]
    pub fn apply_gpu_affinity_at(
        &self,
        topo: &Topology,
        instance_id: &str,
        lp: u32,
        journal_path: &Path,
        restore_path: &Path,
    ) -> Result<(), String> {
        if self.recovery_required() {
            return Err(codes::BENCHMARK_RECOVERY_REQUIRED.to_string());
        }
        // 原子取得 mutation 排他權：benchmark 執行中或另一 mutation 進行中 → 拒絕。
        // 取代原先的 is_running() 讀取（TOCTOU：讀鎖釋放後仍可能被並行 start 搶入）。
        let _guard = self.reserve(OP_MUTATION)?;
        if lp >= topo.total_lp.min(64) {
            return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.to_string());
        }
        let present = self
            .backend
            .enumerate_present_adapters()
            .map_err(|e| e.code().to_string())?
            .iter()
            .any(|d| d.instance_id.eq_ignore_ascii_case(instance_id));
        if !present {
            return Err(codes::GPU_NOT_FOUND.to_string());
        }
        if !self
            .backend
            .basic_display_enabled()
            .map_err(|e| e.code().to_string())?
        {
            return Err(codes::GPU_BASIC_DISPLAY_DISABLED.to_string());
        }
        let result = apply_affinity_to_gpu(
            self.backend.as_ref(),
            self.sleeper.as_ref(),
            instance_id,
            lp,
            journal_path,
            restore_path,
        );
        self.flag_recovery_if_needed(&result);
        result.map_err(|e| e.code)
    }

    /// 還原到先前策略。這是使用者顯式還原，不因 recovery_required 封鎖，
    /// 但仍需取得 mutation 排他權（benchmark / 另一 mutation 進行中 → 拒絕）。
    #[cfg(test)]
    pub fn restore_previous(&self) -> Result<(), String> {
        self.restore_reserved(self.reserve(OP_MUTATION)?)
    }

    pub(crate) fn restore_reserved(&self, _guard: GpuOperationGuard) -> Result<(), String> {
        self.restore_previous_at(&recovery::recovery_path(), &restore_record_path())
    }

    fn restore_previous_at(&self, journal: &Path, restore: &Path) -> Result<(), String> {
        // 未完成的測試回復優先，不以更舊的手動還原覆蓋它。
        if self.recovery_required() || journal.exists() {
            if !journal.exists() {
                return Err(codes::BENCHMARK_RECOVERY_REQUIRED.into());
            }
            attempt_startup_recovery(self.backend.as_ref(), self.sleeper.as_ref(), journal)?;
            self.recovery_required.store(false, Ordering::Release);
            return Ok(());
        }
        let snapshot =
            load_restore_record(restore)?.ok_or_else(|| codes::GPU_RESTORE_FAILED.to_string())?;
        recovery::mark_restore_needed_at(journal, &snapshot)?;
        let result = restore_snapshot(self.backend.as_ref(), self.sleeper.as_ref(), &snapshot)
            .and_then(|_| clear_restore_record(restore))
            .and_then(|_| recovery::clear_at(journal));
        if result.is_err() {
            self.set_recovery_required();
        }
        result
    }

    /// 等效安全驗證：single-flight。目前鎖定核心已在 pair 內 → 立即 Passed；
    /// 否則 spawn_blocking 跑 3 組 AB/BA，結果寫回原 session（保持 Completed）。
    #[cfg(test)]
    pub fn validate_equivalent_candidate(
        self: &Arc<Self>,
        app: &AppHandle,
        topo: &Topology,
        session_id: String,
        selected_lp: u32,
    ) -> Result<(), String> {
        if self.recovery_required() {
            return Err(codes::BENCHMARK_RECOVERY_REQUIRED.to_string());
        }
        let guard = self.reserve(OP_VALIDATION)?;
        let storage_root = storage::benchmarks_root();
        let detail = storage::get_at(&storage_root, &session_id)?;
        let current_policy = self
            .backend
            .read_affinity_policy(&detail.summary.gpu_instance_id)
            .map_err(|e| e.code().to_string())?;
        let cpu_identity = detect_cpu_identity();
        let plan = equivalent_validation_plan(
            self.backend.as_ref(),
            topo,
            &cpu_identity,
            &detail,
            &current_policy,
            selected_lp,
        )?;
        let ref_mask = current_policy.assignment_set_override.bytes.clone();

        match plan {
            EquivalentValidationPlan::ImmediatePass { reference_lp } => {
                let validation = EquivalentSafetyValidation {
                    status: EquivalentSafetyStatus::Passed,
                    selected_lp: Some(selected_lp),
                    reference_lp: Some(reference_lp),
                    rounds: 0,
                    capture_quality: detail.summary.capture_quality.clone(),
                    environment_stability: detail.summary.environment_stability.clone(),
                    validated_at: Some(chrono::Local::now().to_rfc3339()),
                    reference_policy_mask: ref_mask,
                    reason: Some("already_in_equivalent_pair".to_string()),
                    ..Default::default()
                };
                write_equivalent_validation(&storage_root, &session_id, validation)
            }
            EquivalentValidationPlan::RunCaptures { reference_lp } => {
                // 資源解析 + 驗證同步執行（失敗立即回 Err，不寫任何 validation 狀態、
                // 不進入背景）。先驗證 assets，成功才寫 Pending 並 spawn，避免失敗留下永久 Pending。
                let assets = begin_equivalent_validation(
                    &storage_root,
                    &session_id,
                    selected_lp,
                    reference_lp,
                    ref_mask.clone(),
                    resolve_and_verify_assets(app),
                )?;
                self.spawn_equivalent_validation(
                    app,
                    topo,
                    session_id,
                    selected_lp,
                    reference_lp,
                    ref_mask,
                    detail,
                    assets,
                    guard,
                );
                Ok(())
            }
        }
    }

    /// 套用等效親和性：驗證 validation Passed / selected 一致 / live reference 未變，
    /// 然後委派到共享 mutation 路徑 [`apply_affinity_to_gpu`]。
    #[cfg(test)]
    pub fn apply_equivalent_gpu_affinity(
        &self,
        topo: &Topology,
        session_id: &str,
        selected_lp: u32,
    ) -> Result<(), String> {
        if self.recovery_required() {
            return Err(codes::BENCHMARK_RECOVERY_REQUIRED.to_string());
        }
        let _guard = self.reserve(OP_MUTATION)?;
        let storage_root = storage::benchmarks_root();
        let detail = storage::get_at(&storage_root, session_id)?;
        let current_policy = self
            .backend
            .read_affinity_policy(&detail.summary.gpu_instance_id)
            .map_err(|e| e.code().to_string())?;
        let cpu_identity = detect_cpu_identity();
        let target_lp = apply_equivalent_decision(
            self.backend.as_ref(),
            topo,
            &cpu_identity,
            &detail,
            &current_policy,
            selected_lp,
        )?;
        let journal = recovery::recovery_path();
        let result = apply_affinity_to_gpu(
            self.backend.as_ref(),
            self.sleeper.as_ref(),
            &detail.summary.gpu_instance_id,
            target_lp,
            &journal,
            &restore_record_path(),
        );
        self.flag_recovery_if_needed(&result);
        result.map_err(|e| e.code)
    }

    /// 背景執行 3 AB/BA 等效安全驗證（寫回結果後 drop reservation guard）。
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    fn spawn_equivalent_validation(
        self: &Arc<Self>,
        app: &AppHandle,
        topo: &Topology,
        session_id: String,
        selected_lp: u32,
        reference_lp: u32,
        ref_mask: Option<Vec<u8>>,
        detail: SessionDetail,
        assets: BenchmarkAssets,
        guard: GpuOperationGuard,
    ) {
        let storage_root = storage::benchmarks_root();
        let config = detail.summary.config.clone();
        let fps_cap = detail.summary.capture_quality.effective_fps_cap;
        let buffer = detail.summary.capture_quality.circular_buffer_size;
        let validation_dir = std::env::temp_dir().join(format!("frameanchor_equiv_{session_id}"));
        let _ = std::fs::create_dir_all(&validation_dir);

        self.reset_cancel();
        // 主視窗 compact + runtime operation/layout（runner 於背景 snapshot/compact/RAII 還原）
        let window_control: Arc<dyn window_layout::MainWindowController> =
            Arc::new(RealMainWindowController::new(app.clone()));
        if let Ok(mut st) = self.state.write() {
            st.operation = Some(BenchmarkOperation::EquivalentValidation);
            st.window_layout = WindowLayout::CompactProgress;
            st.window_integrity = WindowIntegrity::default();
        }
        let process_runner: Arc<dyn ProcessRunner> = Arc::new(RealProcessRunner::new());
        let cancel: Arc<dyn CancelSignal> = Arc::new(ManagerCancel {
            rx: self.cancel_rx.clone(),
        });
        let validation_id = uuid::Uuid::new_v4().to_string();
        let app_emit = app.clone();
        let manager_done = self.clone();
        let manager_integrity = self.clone();

        let mut ctx = RunContext {
            backend: self.backend.clone(),
            sleeper: self.sleeper.clone(),
            processes: process_runner,
            cancel,
            env: Arc::new(RealEnvironmentProbe::new()),
            topo: topo.clone(),
            capture_quality: Default::default(),
            cpu_identity: detect_cpu_identity(),
            assets,
            storage_root: validation_dir.clone(),
            journal_path: recovery::recovery_path(),
            session_id: validation_id,
            config,
            on_progress: Box::new(move |p| {
                let _ = app_emit.emit("gpu-benchmark-progress", p);
            }),
            baseline: None,
            owned_processes: Vec::new(),
            window: Arc::new(RealWorkloadWindow::new()),
            window_control,
            layout: None,
            on_integrity: Box::new(move |wi| {
                if let Ok(mut st) = manager_integrity.state.write() {
                    st.window_integrity = wi.clone();
                }
            }),
            window_retries: 0,
            last_integrity: None,
        };

        tauri::async_runtime::spawn_blocking(move || {
            let _guard = guard;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runner::run_equivalent_validation(
                    &mut ctx,
                    selected_lp,
                    reference_lp,
                    fps_cap,
                    buffer,
                )
            }))
            .unwrap_or_else(|_| {
                log::error!(
                    "等效安全驗證 runner panic: {}",
                    codes::BENCHMARK_RUNNER_PANIC
                );
                runner::equivalent_panic_failure(&mut ctx)
            });
            if outcome.recovery_required {
                manager_done.set_recovery_required();
            }
            if let Ok(mut st) = manager_done.state.write() {
                st.operation = None;
                st.window_layout = WindowLayout::Normal;
            }
            let validation = EquivalentSafetyValidation {
                status: outcome.status,
                selected_lp: Some(selected_lp),
                reference_lp: Some(reference_lp),
                rounds: outcome.rounds,
                avg_improvement_pct: outcome.avg_improvement_pct,
                p1_improvement_pct: outcome.p1_improvement_pct,
                p01_improvement_pct: outcome.p01_improvement_pct,
                mad_delta_pp: outcome.mad_delta_pp,
                spike_delta_pp: outcome.spike_delta_pp,
                capture_quality: outcome.capture_quality,
                environment_stability: EnvironmentStability {
                    passed: outcome.status == EquivalentSafetyStatus::Passed,
                    drift_reruns: outcome.drift_reruns,
                    error: outcome.reason.clone(),
                },
                validated_at: Some(chrono::Local::now().to_rfc3339()),
                reference_policy_mask: ref_mask,
                reason: outcome.reason,
            };
            if let Err(e) = write_equivalent_validation(&storage_root, &session_id, validation) {
                log::error!("寫入等效安全驗證結果失敗: {e}");
            }
            let _ = std::fs::remove_dir_all(&validation_dir);
        });
    }

    /// 開始基準測試：單一並行的背景 session。前置驗證（config、資源 hash、
    /// GPU 存在、BasicDisplay、未在 RecoveryRequired）通過後立即回傳 Ok。
    pub fn start(
        self: &Arc<Self>,
        app: &AppHandle,
        topo: &Topology,
        config: BenchmarkConfig,
    ) -> Result<(), String> {
        if self.recovery_required() {
            return Err(codes::BENCHMARK_RECOVERY_REQUIRED.to_string());
        }
        // 原子取得 benchmark 排他權（單一場，兩個同時 start 只會一個成功）。
        // 前置驗證若失敗，guard 隨函式回傳而 drop，reservation 不會被卡住。
        let guard = self.reserve(OP_BENCHMARK)?;
        // 前置驗證（先於標記 Running，讓使用者即時拿到錯誤）
        runner::validate_config(&config, topo)?;
        let assets = resolve_assets(app)?;
        assets::verify(&assets).map_err(|e| {
            log::error!("基準測試資源驗證失敗: {e}");
            e.code().to_string()
        })?;
        let instance = config
            .gpu_instance_id
            .clone()
            .ok_or_else(|| codes::BENCHMARK_INVALID_CONFIG.to_string())?;
        let present = self
            .backend
            .enumerate_present_adapters()
            .map_err(|e| e.code().to_string())?
            .iter()
            .any(|d| d.instance_id.eq_ignore_ascii_case(&instance));
        if !present {
            return Err(codes::GPU_NOT_FOUND.to_string());
        }
        if !self
            .backend
            .basic_display_enabled()
            .map_err(|e| e.code().to_string())?
        {
            return Err(codes::GPU_BASIC_DISPLAY_DISABLED.to_string());
        }

        // 空間預檢：主視窗所在 monitor 的 rcWork 內，workload 與 compact 視窗不可重疊。
        // 不足立即拒絕（穩定錯誤碼），不縮 workload、不進入 Running。runner 於背景會再
        // 以相同 plan_layout 做一次（並 snapshot/compact/RAII 還原）。
        let window_control: Arc<dyn window_layout::MainWindowController> =
            Arc::new(RealMainWindowController::new(app.clone()));
        let mon = window_control.monitor_info()?;
        plan_layout(mon.rc_work, mon.dpi, (config.width, config.height))?;

        // 建立 CancelSignal receiver 前，把 watch channel 實際值重設 false，
        // 否則上一場 request_cancel 留下的 true 會讓新 session 立即 Cancelled。
        self.reset_cancel();

        let sid = uuid::Uuid::new_v4().to_string();
        {
            let mut st = self.state.write().map_err(|e| e.to_string())?;
            st.status = SessionStatus::Running;
            st.session_id = Some(sid.clone());
            st.stage = BenchmarkStage::Init;
            st.progress_pct = 0;
            st.elapsed_secs = 0;
            st.current_lp = None;
            st.current_target = None;
            st.current_phase = None;
            st.operation = Some(BenchmarkOperation::Benchmark);
            st.window_layout = WindowLayout::CompactProgress;
            st.window_integrity = WindowIntegrity::default();
        }

        let process_runner: Arc<dyn ProcessRunner> = Arc::new(RealProcessRunner::new());
        let cancel: Arc<dyn CancelSignal> = Arc::new(ManagerCancel {
            rx: self.cancel_rx.clone(),
        });

        let manager_progress = self.clone();
        let manager_done = self.clone();
        let manager_integrity = self.clone();
        let app_emit = app.clone();
        let app_final = app.clone();
        let started = std::time::Instant::now();
        let mut ctx = RunContext {
            backend: self.backend.clone(),
            sleeper: self.sleeper.clone(),
            processes: process_runner,
            cancel,
            env: Arc::new(RealEnvironmentProbe::new()),
            topo: topo.clone(),
            capture_quality: Default::default(),
            cpu_identity: detect_cpu_identity(),
            assets,
            storage_root: storage::benchmarks_root(),
            journal_path: recovery::recovery_path(),
            session_id: sid.clone(),
            config: config.clone(),
            on_progress: Box::new(move |p| {
                if let Ok(mut st) = manager_progress.state.write() {
                    st.session_id = Some(p.session_id.clone());
                    st.stage = runner_stage_to_enum(&p.stage);
                    st.current_lp = p.lp;
                    if p.target.is_some() {
                        st.current_target = p.target.clone();
                    }
                    if p.phase.is_some() {
                        st.current_phase = p.phase;
                    }
                    st.progress_pct = p.percentage;
                    st.elapsed_secs = started.elapsed().as_secs();
                    // 取消欄位只在事件有值時更新，避免一般 progress（None）抹掉
                    // request_cancel 已寫入的「requested」階段。
                    if let Some(cs) = &p.cancel_stage {
                        st.cancel_stage = Some(cs.clone());
                    }
                    if let Some(cp) = p.cancel_progress {
                        st.cancel_progress = Some(cp);
                    }
                }
                let _ = app_emit.emit("gpu-benchmark-progress", p);
            }),
            baseline: None,
            owned_processes: Vec::new(),
            window: Arc::new(RealWorkloadWindow::new()),
            window_control,
            layout: None,
            on_integrity: Box::new(move |wi| {
                if let Ok(mut st) = manager_integrity.state.write() {
                    st.window_integrity = wi.clone();
                }
            }),
            window_retries: 0,
            last_integrity: None,
        };

        tauri::async_runtime::spawn_blocking(move || {
            // reservation guard 存活到 closure 結束（runner 終結、寫完最終 status 後）
            // 才 drop；期間其他 mutation/start 一律被拒，panic 也會經 drop 釋放。
            let _guard = guard;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runner::run_benchmark(&mut ctx)
            }))
            .unwrap_or_else(|_| {
                log::error!("benchmark runner panic: {}", codes::BENCHMARK_RUNNER_PANIC);
                runner::panic_failure(&mut ctx)
            });
            log::info!(
                "基準測試 session {} 結束: status={:?}, best_lp={:?}, severe_lps={:?}, recommended={:?}",
                result.detail.summary.id,
                result.status,
                result.best_lp,
                result.severe_lps,
                result.recommended_cores
            );
            if let Some(e) = &result.error {
                log::error!("基準測試失敗原因: {e}");
            }
            if let Ok(mut st) = manager_done.state.write() {
                st.status = result.status;
                st.progress_pct = 100;
                st.stage = BenchmarkStage::Finalizing;
                st.current_lp = None;
                st.current_target = None;
                st.current_phase = None;
                st.elapsed_secs = started.elapsed().as_secs();
                st.operation = None;
                st.window_layout = WindowLayout::Normal;
            }
            if result.recovery_required {
                manager_done
                    .recovery_required
                    .store(true, Ordering::Relaxed);
                if let Ok(mut st) = manager_done.state.write() {
                    st.recovery_required = true;
                }
            }
            // 終態 state 寫入完成後，再 emit 一個事件，讓 App 的既有 listener 重新
            // get_benchmark_state 讀到已提交的終態（Completed/Failed/Cancelled），
            // 不依賴 setTimeout race。
            let final_progress = BenchmarkProgress {
                target: None,
                session_id: result.detail.summary.id.clone(),
                stage: "finalizing".to_string(),
                round: None,
                phase: None,
                phase_round: None,
                lp: None,
                percentage: 100,
                eta_secs: None,
                error: result.error.clone(),
                window_integrity: None,
                cancel_stage: (result.status == SessionStatus::Cancelled)
                    .then(|| "finalizing".to_string()),
                cancel_progress: (result.status == SessionStatus::Cancelled).then_some(100),
            };
            let _ = app_final.emit("gpu-benchmark-progress", final_progress);
        });
        Ok(())
    }

    /// 請求取消正在跑的基準測試（runner 在安全階段邊界檢查）。
    /// 立即在 state 標記取消階段=requested、百分比=0，讓前端不等 runner 下一個
    /// capture boundary event 就顯示「已收到取消請求 / 0%」。
    pub fn request_cancel(&self) {
        let _ = self.cancel_tx.send(true);
        if let Ok(mut s) = self.state.write() {
            s.cancel_requested = true;
            s.cancel_stage = Some("requested".to_string());
            s.cancel_progress = Some(0);
        }
    }

    pub fn cancel_requested(&self) -> bool {
        *self.cancel_rx.borrow()
    }

    pub fn cancel_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.cancel_rx.clone()
    }
}

/// 判定某個 session「現在可否套用」的核心邏輯（free function，注入路徑/身分供測試）。
/// 順序：session 存在 →（Equivalent 走等效契約）→ Completed + bestLp → 可靠性 Passed →
/// LP 範圍 → CPU 指紋 → GPU 存在 → BasicDisplay。
#[cfg(test)]
fn check_apply_at(
    backend: &dyn GpuBackend,
    topo: &Topology,
    cpu_identity: &CpuIdentity,
    storage_root: &Path,
    session_id: &str,
) -> ApplyStatus {
    let cannot = |reason: &str| ApplyStatus {
        can_apply: false,
        reason: Some(reason.to_string()),
        equivalent_mode: false,
        allowed_lps: Vec::new(),
        requires_safety_validation: false,
    };
    let Ok(detail) = storage::get_at(storage_root, session_id) else {
        return cannot(codes::BENCHMARK_SESSION_NOT_FOUND);
    };
    // Equivalent 契約：algorithmVersion=2 且 reliability=Equivalent → 走等效套用路徑。
    if detail.summary.reliability.status == ReliabilityStatus::Equivalent
        && detail.summary.reliability.algorithm_version == 2
    {
        return check_equivalent_apply(backend, topo, cpu_identity, &detail);
    }
    if detail.summary.status != SessionStatus::Completed || detail.summary.best_lp.is_none() {
        return cannot(codes::BENCHMARK_SESSION_NOT_COMPLETED);
    }
    if detail.summary.reliability.status != ReliabilityStatus::Passed {
        return cannot(codes::BENCHMARK_RELIABILITY_NOT_PASSED);
    }
    let best_lp = detail.summary.best_lp.unwrap();
    if best_lp >= topo.total_lp.min(64) {
        return cannot(codes::BENCHMARK_SESSION_INCOMPATIBLE);
    }
    if detail.summary.cpu_fingerprint != cpu_fingerprint_with(topo, cpu_identity) {
        return cannot(codes::BENCHMARK_SESSION_INCOMPATIBLE);
    }
    let present = backend
        .enumerate_present_adapters()
        .map(|a| {
            a.iter().any(|d| {
                d.instance_id
                    .eq_ignore_ascii_case(&detail.summary.gpu_instance_id)
            })
        })
        .unwrap_or(false);
    if !present {
        return cannot(codes::GPU_NOT_FOUND);
    }
    if !backend.basic_display_enabled().unwrap_or(false) {
        return cannot(codes::GPU_BASIC_DISPLAY_DISABLED);
    }
    ApplyStatus {
        can_apply: true,
        reason: None,
        equivalent_mode: false,
        allowed_lps: Vec::new(),
        requires_safety_validation: false,
    }
}

/// Equivalent-mode session 的「可否套用」契約（與 legacy `bestLp` 路徑分離）。
#[cfg(test)]
fn check_equivalent_apply(
    backend: &dyn GpuBackend,
    topo: &Topology,
    cpu_identity: &CpuIdentity,
    detail: &SessionDetail,
) -> ApplyStatus {
    let finalists = &detail.summary.equivalent_finalist_lps;
    let cannot = |reason: &str| ApplyStatus {
        can_apply: false,
        reason: Some(reason.to_string()),
        equivalent_mode: true,
        allowed_lps: finalists.clone(),
        requires_safety_validation: true,
    };
    if detail.summary.status != SessionStatus::Completed || finalists.len() != 2 {
        return cannot(codes::BENCHMARK_NOT_EQUIVALENT);
    }
    if finalists.iter().any(|&lp| lp >= topo.total_lp.min(64)) {
        return cannot(codes::BENCHMARK_SESSION_INCOMPATIBLE);
    }
    if detail.summary.cpu_fingerprint != cpu_fingerprint_with(topo, cpu_identity) {
        return cannot(codes::BENCHMARK_SESSION_INCOMPATIBLE);
    }
    let present = backend
        .enumerate_present_adapters()
        .map(|a| {
            a.iter().any(|d| {
                d.instance_id
                    .eq_ignore_ascii_case(&detail.summary.gpu_instance_id)
            })
        })
        .unwrap_or(false);
    if !present {
        return cannot(codes::GPU_NOT_FOUND);
    }
    if !backend.basic_display_enabled().unwrap_or(false) {
        return cannot(codes::GPU_BASIC_DISPLAY_DISABLED);
    }
    // 已通過 safety validation（Passed 且 selected 在 pair 內）→ 可套用。
    let validation = detail.equivalent_safety_validation.as_ref();
    let validated = validation.is_some_and(|v| {
        v.status == EquivalentSafetyStatus::Passed
            && v.selected_lp.is_some_and(|lp| finalists.contains(&lp))
    });
    if validated {
        // live reference policy 必須仍與驗證 snapshot 一致，否則需重驗（不可先顯示可套用）。
        let reference_ok = validation.is_some_and(|v| {
            backend
                .read_affinity_policy(&detail.summary.gpu_instance_id)
                .map(|current| equivalent_reference_matches(v, &current))
                .unwrap_or(false)
        });
        if !reference_ok {
            return cannot(codes::BENCHMARK_EQUIVALENT_REFERENCE_CHANGED);
        }
        ApplyStatus {
            can_apply: true,
            reason: None,
            equivalent_mode: true,
            allowed_lps: finalists.clone(),
            requires_safety_validation: false,
        }
    } else {
        cannot(codes::BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED)
    }
}

/// 可匯入的已完成 session（free function，注入路徑/身分供測試）。
#[cfg(test)]
fn list_importable(
    backend: &dyn GpuBackend,
    topo: &Topology,
    cpu_identity: &CpuIdentity,
    storage_root: &Path,
) -> Vec<SessionSummary> {
    let current_fp = cpu_fingerprint_with(topo, cpu_identity);
    let present: Vec<String> = backend
        .enumerate_present_adapters()
        .map(|a| a.iter().map(|d| d.instance_id.clone()).collect())
        .unwrap_or_default();
    storage::list_at(storage_root)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| {
            s.status == SessionStatus::Completed
                && s.best_lp.is_some()
                && s.reliability.status == ReliabilityStatus::Passed
                && s.cpu_fingerprint == current_fp
                && present
                    .iter()
                    .any(|g| g.eq_ignore_ascii_case(&s.gpu_instance_id))
        })
        .collect()
}

/// 由精簡 LE 單 LP mask bytes 反解 LP index（單一位元）；非單一位元 → None。
#[cfg(test)]
pub fn mask_bytes_to_lp(bytes: Option<&[u8]>) -> Option<u32> {
    let bytes = bytes?;
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let mut le = [0u8; 8];
    le[..bytes.len()].copy_from_slice(bytes);
    let mask = u64::from_le_bytes(le);
    if mask == 0 || !mask.is_power_of_two() {
        return None;
    }
    Some(mask.trailing_zeros())
}

/// 等效安全驗證的前置決策。
#[derive(Debug)]
#[cfg(test)]
pub enum EquivalentValidationPlan {
    /// 目前鎖定核心已在 pair 內 → 立即 Passed（同核心 no-op 或選另一 finalist）。
    ImmediatePass { reference_lp: u32 },
    /// 目前鎖定核心不在 pair 內 → 跑 3 組 AB/BA 比較 selected vs reference。
    RunCaptures { reference_lp: u32 },
}

/// 前置決策：session 為 equivalent 契約、selected 在 pair 內、相容性通過，且能由目前
/// policy 解出鎖定核心（reference）。回傳 ImmediatePass / RunCaptures，或拒絕原因。
#[cfg(test)]
pub fn equivalent_validation_plan(
    backend: &dyn GpuBackend,
    topo: &Topology,
    cpu_identity: &CpuIdentity,
    detail: &SessionDetail,
    current_policy: &AffinityPolicy,
    selected_lp: u32,
) -> Result<EquivalentValidationPlan, String> {
    let finalists = &detail.summary.equivalent_finalist_lps;
    if detail.summary.status != SessionStatus::Completed
        || detail.summary.reliability.status != ReliabilityStatus::Equivalent
        || detail.summary.reliability.algorithm_version != 2
        || finalists.len() != 2
    {
        return Err(codes::BENCHMARK_NOT_EQUIVALENT.to_string());
    }
    if !finalists.contains(&selected_lp) {
        return Err(codes::BENCHMARK_EQUIVALENT_LP_INVALID.to_string());
    }
    if finalists.iter().any(|&lp| lp >= topo.total_lp.min(64)) {
        return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.to_string());
    }
    if detail.summary.cpu_fingerprint != cpu_fingerprint_with(topo, cpu_identity) {
        return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.to_string());
    }
    let present = backend
        .enumerate_present_adapters()
        .map_err(|e| e.code().to_string())?
        .iter()
        .any(|d| {
            d.instance_id
                .eq_ignore_ascii_case(&detail.summary.gpu_instance_id)
        });
    if !present {
        return Err(codes::GPU_NOT_FOUND.to_string());
    }
    if !backend
        .basic_display_enabled()
        .map_err(|e| e.code().to_string())?
    {
        return Err(codes::GPU_BASIC_DISPLAY_DISABLED.to_string());
    }
    let reference_lp = mask_bytes_to_lp(current_policy.assignment_set_override.bytes.as_deref())
        .ok_or_else(|| codes::BENCHMARK_EQUIVALENT_NO_REFERENCE.to_string())?;
    if finalists.contains(&reference_lp) {
        Ok(EquivalentValidationPlan::ImmediatePass { reference_lp })
    } else {
        Ok(EquivalentValidationPlan::RunCaptures { reference_lp })
    }
}

/// 套用等效親和性的前置決策：validation Passed、selected 一致、live reference policy
/// 未變、相容性通過。通過 → 回傳要套用的 selected_lp；否則拒絕原因。
#[cfg(test)]
pub fn apply_equivalent_decision(
    backend: &dyn GpuBackend,
    topo: &Topology,
    cpu_identity: &CpuIdentity,
    detail: &SessionDetail,
    current_policy: &AffinityPolicy,
    selected_lp: u32,
) -> Result<u32, String> {
    let finalists = &detail.summary.equivalent_finalist_lps;
    if detail.summary.status != SessionStatus::Completed
        || detail.summary.reliability.status != ReliabilityStatus::Equivalent
        || detail.summary.reliability.algorithm_version != 2
        || finalists.len() != 2
    {
        return Err(codes::BENCHMARK_NOT_EQUIVALENT.to_string());
    }
    if !finalists.contains(&selected_lp) {
        return Err(codes::BENCHMARK_EQUIVALENT_LP_INVALID.to_string());
    }
    if finalists.iter().any(|&lp| lp >= topo.total_lp.min(64)) {
        return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.to_string());
    }
    if detail.summary.cpu_fingerprint != cpu_fingerprint_with(topo, cpu_identity) {
        return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.to_string());
    }
    let present = backend
        .enumerate_present_adapters()
        .map_err(|e| e.code().to_string())?
        .iter()
        .any(|d| {
            d.instance_id
                .eq_ignore_ascii_case(&detail.summary.gpu_instance_id)
        });
    if !present {
        return Err(codes::GPU_NOT_FOUND.to_string());
    }
    if !backend
        .basic_display_enabled()
        .map_err(|e| e.code().to_string())?
    {
        return Err(codes::GPU_BASIC_DISPLAY_DISABLED.to_string());
    }
    let validation = detail
        .equivalent_safety_validation
        .as_ref()
        .ok_or_else(|| codes::BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED.to_string())?;
    if validation.status != EquivalentSafetyStatus::Passed
        || validation.selected_lp != Some(selected_lp)
    {
        return Err(codes::BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED.to_string());
    }
    if !equivalent_reference_matches(validation, current_policy) {
        return Err(codes::BENCHMARK_EQUIVALENT_REFERENCE_CHANGED.to_string());
    }
    Ok(selected_lp)
}

/// 把等效安全驗證結果寫回原 session（不重算、不遷移歷史；原 session 保持 Completed）。
#[cfg(test)]
pub fn write_equivalent_validation(
    storage_root: &Path,
    session_id: &str,
    validation: EquivalentSafetyValidation,
) -> Result<(), String> {
    let mut detail = storage::get_at(storage_root, session_id)?;
    detail.equivalent_safety_validation = Some(validation);
    storage::save_session_at(storage_root, &detail)
}

/// 等效安全驗證的前置：assets 解析/驗證成功（`assets` 為 Ok）才寫 `Pending` 並回傳
/// 已驗證的 assets；失敗不寫任何狀態、直接回傳 Err。讓呼叫端在 spawn 前失敗時
/// session 保持原狀（可重試），不會留下永久 `Pending`。
#[cfg(test)]
fn begin_equivalent_validation(
    storage_root: &Path,
    session_id: &str,
    selected_lp: u32,
    reference_lp: u32,
    ref_mask: Option<Vec<u8>>,
    assets: Result<BenchmarkAssets, String>,
) -> Result<BenchmarkAssets, String> {
    let assets = assets?;
    write_equivalent_validation(
        storage_root,
        session_id,
        EquivalentSafetyValidation {
            status: EquivalentSafetyStatus::Pending,
            selected_lp: Some(selected_lp),
            reference_lp: Some(reference_lp),
            rounds: 0,
            reference_policy_mask: ref_mask,
            ..Default::default()
        },
    )?;
    Ok(assets)
}

/// validation 的 reference snapshot 是否仍與目前 live policy 一致（逐位元組）。
/// snapshot 為 None（舊資料或缺漏）→ false（保守拒絕）。
#[cfg(test)]
fn equivalent_reference_matches(
    validation: &EquivalentSafetyValidation,
    current: &AffinityPolicy,
) -> bool {
    match validation.reference_policy_mask.as_deref() {
        Some(mask) => current.assignment_set_override.bytes.as_deref() == Some(mask),
        None => false,
    }
}

/// 解析並驗證內建資源（assets）；任一失敗不寫任何 validation 狀態。
#[cfg(test)]
fn resolve_and_verify_assets(app: &AppHandle) -> Result<BenchmarkAssets, String> {
    let assets = resolve_assets(app)?;
    assets::verify(&assets).map_err(|e| e.code().to_string())?;
    Ok(assets)
}

/// 由 AppHandle 解析內建資源目錄（tauri.conf.json `bundle.resources`）。
/// Windows 上 `resource_dir()` 等於 exe 所在目錄，而 `resources/**` 會以完整
/// 相對路徑（含 `resources/` 前綴）安裝到該目錄 → 實際位置是 `resources/benchmark`。
/// 不接受 caller 指定 executable 路徑：spawn 一律限縮到內建資源
/// （digest 內嵌主程式驗證,見 `assets::verify`）。
fn resolve_assets(app: &AppHandle) -> Result<BenchmarkAssets, String> {
    let dir = app
        .path()
        .resolve("resources/benchmark", tauri::path::BaseDirectory::Resource)
        .map_err(|e| format!("資源目錄解析失敗: {e}"))?;
    Ok(assets::load(&dir))
}

/// runner 的 stage 字串 → BenchmarkStage（執行期狀態）
fn runner_stage_to_enum(stage: &str) -> BenchmarkStage {
    match stage {
        "collecting" => BenchmarkStage::Collecting,
        "finalizing" => BenchmarkStage::Finalizing,
        "applying" | "launching" | "restarting" => BenchmarkStage::Warmup,
        _ => BenchmarkStage::Init,
    }
}

/// manager 的 cancel watch channel 實作 CancelSignal
struct ManagerCancel {
    rx: tokio::sync::watch::Receiver<bool>,
}

impl CancelSignal for ManagerCancel {
    fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }
}

// ── 測試 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benchmark::{
        cpu_fingerprint_with, CpuIdentity, ReliabilityStatus, ReliabilitySummary, SessionDetail,
    };
    use crate::gpu::fake::FakeBackend;
    use crate::gpu::{AffinityPolicy, GpuDevice, NoopSleeper, RegistryValueSnapshot};
    use crate::topology::{build_topology, Topology};
    use uuid::Uuid;
    use windows::Win32::System::Registry::{REG_BINARY, REG_DWORD};

    const GPU_A: &str = r"PCI\VEN_FAKE&DEV_1";
    const GPU_B: &str = r"PCI\VEN_FAKE&DEV_2";

    /// 測試用的固定 CPU 身分（x64 Intel），讓指紋完全確定、不依賴真實機器
    fn fixed_identity() -> CpuIdentity {
        CpuIdentity {
            architecture: 9, // PROCESSOR_ARCHITECTURE_AMD64
            family: 6,
            model: 183,
            stepping: 1,
        }
    }

    fn topo() -> Topology {
        build_topology((0..8u32).map(|c| (vec![c], 0, false)).collect())
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("frameanchor_mgr_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join("journal.json"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn device(instance: &str) -> GpuDevice {
        GpuDevice {
            instance_id: instance.to_string(),
            friendly_name: format!("GPU {instance}"),
        }
    }

    /// 測試用「已通過可靠性」的摘要（status=Passed）
    fn passed_reliability() -> ReliabilitySummary {
        ReliabilitySummary {
            status: ReliabilityStatus::Passed,
            ..Default::default()
        }
    }

    fn completed_session(storage_root: &Path, topo: &Topology, gpu: &str, best_lp: u32) -> String {
        let id = Uuid::new_v4().to_string();
        let detail = SessionDetail {
            summary: crate::benchmark::SessionSummary {
                id: id.clone(),
                status: SessionStatus::Completed,
                started_at: "2026-08-11T00:00:00Z".into(),
                finished_at: Some("2026-08-11T00:01:00Z".into()),
                gpu_name: "GPU".into(),
                gpu_instance_id: gpu.to_string(),
                cpu_fingerprint: cpu_fingerprint_with(topo, &fixed_identity()),
                best_lp: Some(best_lp),
                reliability: passed_reliability(),
                severe_lps: vec![],
                sample_count: 5,
                total_bytes: 0,
                config: BenchmarkConfig::default(),
                error: None,
                ..Default::default()
            },
            results: vec![],
            samples: vec![],
            ..Default::default()
        };
        storage::save_session_at(storage_root, &detail).unwrap();
        id
    }

    /// u32 → 精簡 little-endian bytes（尾端零移除），fixture 用
    fn le_trimmed(v: u32) -> Vec<u8> {
        let b = v.to_le_bytes();
        let mut len = b.len();
        while len > 0 && b[len - 1] == 0 {
            len -= 1;
        }
        b[..len].to_vec()
    }

    /// 既有策略 fixture：DevicePolicy 是 DWORD；AssignmentSetOverride 是 REG_BINARY
    fn policy_on(instance: &str, device_policy: u32, override_mask: u32) -> AffinityPolicy {
        AffinityPolicy {
            instance_id: instance.to_string(),
            device_policy: RegistryValueSnapshot::dword(device_policy),
            assignment_set_override: RegistryValueSnapshot::binary(le_trimmed(override_mask)),
        }
    }

    fn assert_dword(policy: &AffinityPolicy, name: &str, expected: u32) {
        let v = if name == "DevicePolicy" {
            policy.device_policy.as_dword()
        } else {
            policy.assignment_set_override.as_dword()
        };
        assert_eq!(v, Some(expected), "{name} 值不符");
    }

    /// 斷言 AssignmentSetOverride 是 REG_BINARY，且位元組等於給定 mask 的精簡 LE
    fn assert_override_mask(policy: &AffinityPolicy, mask: u32) {
        let o = &policy.assignment_set_override;
        assert_eq!(
            o.value_type,
            Some(REG_BINARY.0),
            "AssignmentSetOverride 應為 REG_BINARY"
        );
        assert_eq!(
            o.bytes.as_deref(),
            Some(le_trimmed(mask).as_slice()),
            "AssignmentSetOverride bytes 不符"
        );
    }

    #[test]
    fn apply_success_writes_policy_restarts_and_records() {
        let dir = temp_dir("ok");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        // 模擬既有策略（已鎖定 LP 0）
        backend.set_policy(policy_on(GPU_A, 4, 1));

        let sid = completed_session(&storage_root, &topo(), GPU_A, 5);
        apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid,
        )
        .unwrap();

        let cur = backend.current_policy(GPU_A);
        assert_dword(&cur, "DevicePolicy", DEVICE_POLICY_SINGLE_PROCESSOR);
        assert_override_mask(&cur, 1u32 << 5);
        assert_eq!(backend.restart_count(), 1);
        assert_eq!(backend.disable_attempts(), 1);
        assert_eq!(backend.enable_attempts(), 1); // disable 成功後有嘗試 enable
        assert!(!journal.exists(), "成功後日誌應清除");
        assert!(restore.exists(), "一層還原記錄應寫入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GPU 原本沒有 Affinity Policy 值（present=false）：套用成功，
    /// 還原時把兩個值都「刪除」回缺失狀態。
    #[test]
    fn apply_and_restore_missing_registry_values() {
        let dir = temp_dir("missingvals");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        // 不 set_policy → 兩值皆不存在
        assert!(!backend.current_policy(GPU_A).device_policy.present);

        let sid = completed_session(&storage_root, &topo(), GPU_A, 3);
        apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid,
        )
        .unwrap();
        assert_override_mask(&backend.current_policy(GPU_A), 1u32 << 3);

        // 還原 → 回到「不存在」
        restore_previous_affinity(&backend, &NoopSleeper, &restore).unwrap();
        let restored = backend.current_policy(GPU_A);
        assert!(!restored.device_policy.present);
        assert!(!restored.assignment_set_override.present);
        assert!(restored.device_policy.bytes.is_none());
        assert!(!restore.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_requires_completed_session() {
        let dir = temp_dir("notcomp");
        let storage_root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let id = Uuid::new_v4().to_string();
        let mut detail = SessionDetail::default();
        detail.summary.id = id.clone();
        detail.summary.status = SessionStatus::Running;
        detail.summary.gpu_instance_id = GPU_A.into();
        detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
        storage::save_session_at(&storage_root, &detail).unwrap();

        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &dir.join("journal.json"),
            &dir.join("restore.json"),
            &id,
        )
        .unwrap_err();
        assert_eq!(err, codes::BENCHMARK_SESSION_NOT_COMPLETED);
        assert_eq!(backend.restart_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_rejects_cpu_fingerprint_mismatch() {
        let dir = temp_dir("cpumismatch");
        let storage_root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        // 用「不同 CPU」建立 session（8C16T vs 8C 指紋不同）
        let id = Uuid::new_v4().to_string();
        let other_topo = build_topology(
            (0..8u32)
                .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
                .collect(),
        );
        let mut detail = SessionDetail::default();
        detail.summary.id = id.clone();
        detail.summary.status = SessionStatus::Completed;
        detail.summary.gpu_instance_id = GPU_A.into();
        detail.summary.best_lp = Some(3);
        detail.summary.reliability = passed_reliability();
        detail.summary.cpu_fingerprint = cpu_fingerprint_with(&other_topo, &fixed_identity());
        storage::save_session_at(&storage_root, &detail).unwrap();

        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &dir.join("journal.json"),
            &dir.join("restore.json"),
            &id,
        )
        .unwrap_err();
        assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
        assert_eq!(backend.restart_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_rejects_gpu_not_present() {
        let dir = temp_dir("gpumissing");
        let storage_root = dir.join("benchmarks");
        // 本機只有 GPU_B，session 指向 GPU_A
        let backend = FakeBackend::new(vec![device(GPU_B)]);
        let sid = completed_session(&storage_root, &topo(), GPU_A, 3);
        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &dir.join("journal.json"),
            &dir.join("restore.json"),
            &sid,
        )
        .unwrap_err();
        assert_eq!(err, codes::GPU_NOT_FOUND);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_blocks_when_basic_display_disabled() {
        let dir = temp_dir("basic");
        let storage_root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        backend.basic_display_on.store(false, Ordering::SeqCst);
        let sid = completed_session(&storage_root, &topo(), GPU_A, 3);
        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &dir.join("journal.json"),
            &dir.join("restore.json"),
            &sid,
        )
        .unwrap_err();
        assert_eq!(err, codes::GPU_BASIC_DISPLAY_DISABLED);
        assert_eq!(backend.restart_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 64-LP 拓撲（group 0 上限）
    fn topo_64() -> Topology {
        build_topology(
            (0..32u32)
                .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
                .collect(),
        )
    }

    /// LP 0/31/32/63 都能套用：AssignmentSetOverride 是 REG_BINARY 且位元組精確
    #[test]
    fn apply_lp_boundaries_write_reg_binary_masks() {
        let dir = temp_dir("lpbound");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let topo64 = topo_64();
        assert_eq!(topo64.total_lp, 64);

        for lp in [0u32, 31, 32, 63] {
            let sid = completed_session(&storage_root, &topo64, GPU_A, lp);
            apply_best_affinity(
                &backend,
                &NoopSleeper,
                &fixed_identity(),
                &topo64,
                &storage_root,
                &journal,
                &restore,
                &sid,
            )
            .unwrap();
            let cur = backend.current_policy(GPU_A);
            assert_eq!(
                cur.assignment_set_override.value_type,
                Some(REG_BINARY.0),
                "LP {lp} 應為 REG_BINARY"
            );
            assert_eq!(
                cur.assignment_set_override.bytes.as_deref(),
                Some(single_lp_mask_bytes(lp).as_slice()),
                "LP {lp} mask bytes 不符"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_rejects_best_lp_64() {
        let dir = temp_dir("lp64");
        let storage_root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let topo64 = topo_64();
        let sid = completed_session(&storage_root, &topo64, GPU_A, 64);
        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo64,
            &storage_root,
            &dir.join("journal.json"),
            &dir.join("restore.json"),
            &sid,
        )
        .unwrap_err();
        assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
        assert_eq!(backend.restart_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_rejects_best_lp_outside_topology() {
        let dir = temp_dir("lpoutside");
        let storage_root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        // 8-LP 拓撲，best_lp=8 超出實際 LP 範圍
        let sid = completed_session(&storage_root, &topo(), GPU_A, 8);
        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &dir.join("journal.json"),
            &dir.join("restore.json"),
            &sid,
        )
        .unwrap_err();
        assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
        assert_eq!(backend.restart_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 原本策略含非 DWORD 型別：還原必須逐型別、逐位元組還原
    #[test]
    fn restore_rejects_non_dword_device_policy() {
        let dir = temp_dir("non_dword");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        // DevicePolicy=REG_SZ：真實驅動語意只接受 DWORD；非 DWORD 快照一律
        // fail closed（不得讓任意型別/bytes 進入 HKLM）
        let original = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            device_policy: RegistryValueSnapshot {
                present: true,
                value_type: Some(1), // REG_SZ
                bytes: Some(vec![b'g', 0, b'p', 0, 0, 0]),
            },
            assignment_set_override: RegistryValueSnapshot {
                present: true,
                value_type: Some(REG_BINARY.0),
                bytes: Some(vec![0x00, 0x00, 0x00, 0x80]),
            },
        };
        backend.set_policy(original.clone());
        let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
        apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid,
        )
        .unwrap();
        // 快照（非 DWORD）寫回還原記錄成功，但 restore 必須在 write 前被語意
        // 驗證擋下 → 還原失敗，裝置狀態維持在套用後的 DWORD 策略
        assert_eq!(
            backend.current_policy(GPU_A).device_policy.value_type,
            Some(REG_DWORD.0)
        );
        assert!(restore_previous_affinity(&backend, &NoopSleeper, &restore).is_err());
        assert_eq!(
            backend.current_policy(GPU_A).device_policy.value_type,
            Some(REG_DWORD.0),
            "非 DWORD 快照不得被寫入"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_write_failure_restores_and_clears_journal() {
        let dir = temp_dir("writefail");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        backend.set_policy(original.clone());
        let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
        backend.fail_next_write(); // 第一次 write 失敗（apply 的 write）

        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid,
        )
        .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        // 還原成功 → 策略與原快照逐位元組一致，日誌清除
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_restart_disable_failure_restores_and_clears_journal() {
        let dir = temp_dir("restartfail");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 1, 0b11);
        backend.set_policy(original.clone());
        let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
        backend.fail_next_restart.store(true, Ordering::SeqCst);

        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid,
        )
        .unwrap_err();
        assert_eq!(err, codes::GPU_RESTART_FAILED);
        // 套用 restart 失敗後，還原 restart 成功 → 策略還原、日誌清除
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists());
        assert!(!restore.exists(), "套用未完成，不該寫入還原記錄");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_restart_failure_and_restore_failure_keeps_journal() {
        let dir = temp_dir("restorefail");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 1, 0b11);
        backend.set_policy(original.clone());
        let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
        backend.disable_fails.store(true, Ordering::SeqCst); // 一直失敗

        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid,
        )
        .unwrap_err();
        assert_eq!(err, codes::GPU_RESTART_FAILED);
        // 策略已寫回（write 成功），但 restart 失敗 → 還原未驗證 → 保留日誌等啟動重試
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(journal.exists(), "還原失敗時必須保留日誌");
        assert!(!restore.exists(), "還原記錄不該寫入");
        assert_eq!(backend.enable_attempts(), 0, "disable 失敗不該嘗試 enable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_enable_failure_attempts_enable_after_disable() {
        let dir = temp_dir("enablefail");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 1, 0b11);
        backend.set_policy(original.clone());
        let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
        backend.enable_fails.store(true, Ordering::SeqCst);

        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &dir.join("restore.json"),
            &sid,
        )
        .unwrap_err();
        assert_eq!(err, codes::GPU_RESTART_FAILED);
        // 每次 restart：disable 成功 → 必嘗試 enable（總共 apply + restore 兩次）
        assert!(
            backend.enable_attempts() >= 2,
            "disable 成功後必須嘗試 enable"
        );
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(journal.exists(), "還原失敗 → 保留日誌");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_previous_restores_bytes_and_clears_record() {
        let dir = temp_dir("restoreprev");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 0, 0xFFFF);
        backend.set_policy(original.clone());
        let sid = completed_session(&storage_root, &topo(), GPU_A, 5);

        apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid,
        )
        .unwrap();
        // 現在策略已被改寫
        assert_override_mask(&backend.current_policy(GPU_A), 1u32 << 5);

        restore_previous_affinity(&backend, &NoopSleeper, &restore).unwrap();
        assert_eq!(backend.current_policy(GPU_A), original, "必須逐位元組還原");
        assert!(!restore.exists(), "還原後記錄清除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_previous_without_record_errors() {
        let dir = temp_dir("norecord");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let err = restore_previous_affinity(&backend, &NoopSleeper, &dir.join("restore.json"))
            .unwrap_err();
        assert_eq!(err, codes::GPU_RESTORE_FAILED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_twice_is_one_level_record() {
        let dir = temp_dir("onelevel");
        let storage_root = dir.join("benchmarks");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A), device(GPU_B)]);
        let orig_a = policy_on(GPU_A, 0, 0xFFFF);
        let orig_b = policy_on(GPU_B, 0, 0xFF00);
        backend.set_policy(orig_a.clone());
        backend.set_policy(orig_b.clone());

        let sid_a = completed_session(&storage_root, &topo(), GPU_A, 1);
        apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid_a,
        )
        .unwrap();
        let sid_b = completed_session(&storage_root, &topo(), GPU_B, 2);
        apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &journal,
            &restore,
            &sid_b,
        )
        .unwrap();

        // 只保留最新一次的快照
        let rec = std::fs::read_to_string(&restore).unwrap();
        let saved: AffinityPolicy = serde_json::from_str(&rec).unwrap();
        assert_eq!(saved.instance_id, GPU_B);
        assert_eq!(saved, orig_b);

        // 還原 → 回到 GPU_B 原狀（GPU_A 維持套用後）
        restore_previous_affinity(&backend, &NoopSleeper, &restore).unwrap();
        assert_eq!(backend.current_policy(GPU_B), orig_b);
        assert_override_mask(&backend.current_policy(GPU_A), 1u32 << 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 啟動還原 ──

    #[test]
    fn startup_recovery_no_journal_ok() {
        let dir = temp_dir("recovery_none");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        assert!(
            attempt_startup_recovery(&backend, &NoopSleeper, &dir.join("journal.json")).is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_recovery_restores_policy_applied_crash() {
        let dir = temp_dir("recovery_crash");
        let journal = dir.join("journal.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        backend.set_policy(original.clone());

        // 模擬崩潰：快照後已寫入新策略、還未完成（stage=PolicyApplied）
        recovery::begin_at(&journal, &original).unwrap();
        let j = recovery::load_from(&journal).unwrap().unwrap();
        recovery::advance_to_at(&journal, &j, RecoveryStage::PolicyApplied).unwrap();
        backend.set_policy(policy_on(GPU_A, 4, 1u32 << 7)); // 假設套用寫入的新值

        attempt_startup_recovery(&backend, &NoopSleeper, &journal).unwrap();
        assert_eq!(backend.current_policy(GPU_A), original, "必須還原到快照");
        assert!(!journal.exists(), "還原驗證成功後清除日誌");
        assert_eq!(backend.restart_count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_recovery_stage_snapshot_only_clears_without_mutation() {
        let dir = temp_dir("recovery_snapshot");
        let journal = dir.join("journal.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 1, 0b11);
        backend.set_policy(original.clone());
        recovery::begin_at(&journal, &original).unwrap(); // stage=SnapshotTaken，尚未變更

        attempt_startup_recovery(&backend, &NoopSleeper, &journal).unwrap();
        assert_eq!(backend.current_policy(GPU_A), original);
        assert_eq!(backend.restart_count(), 0, "未變更不該重啟裝置");
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_recovery_snapshot_stage_mismatch_is_error() {
        let dir = temp_dir("recovery_mismatch");
        let journal = dir.join("journal.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 1, 0b11);
        recovery::begin_at(&journal, &original).unwrap();
        // 策略被改了（stage 卻停在 SnapshotTaken → 資料異常）
        backend.set_policy(policy_on(GPU_A, 4, 1));

        assert!(attempt_startup_recovery(&backend, &NoopSleeper, &journal).is_err());
        assert!(journal.exists(), "驗證失敗不該清除日誌");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_recovery_restore_failure_keeps_journal() {
        let dir = temp_dir("recovery_restorefail");
        let journal = dir.join("journal.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        recovery::begin_at(&journal, &original).unwrap();
        let j = recovery::load_from(&journal).unwrap().unwrap();
        recovery::advance_to_at(&journal, &j, RecoveryStage::PolicyApplied).unwrap();
        backend.disable_fails.store(true, Ordering::SeqCst); // 還原 restart 一直失敗

        assert!(attempt_startup_recovery(&backend, &NoopSleeper, &journal).is_err());
        assert!(journal.exists(), "還原失敗必須保留日誌供下次啟動");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── BenchmarkManager 狀態骨架 ──

    #[test]
    fn manager_state_default_and_cancel() {
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)])) as Arc<dyn GpuBackend>;
        let m = BenchmarkManager::new(backend);
        let s = m.state_snapshot();
        assert_eq!(s.status, SessionStatus::Pending);
        assert!(!s.recovery_required);
        assert!(!m.cancel_requested());
        assert_eq!(s.cancel_stage, None);
        assert_eq!(s.cancel_progress, None);
        m.request_cancel();
        assert!(m.cancel_requested());
        let sc = m.state_snapshot();
        assert!(sc.cancel_requested);
        // request_cancel 立即在 state 標記 requested/0%，供前端立刻顯示「已收到取消請求」。
        assert_eq!(sc.cancel_stage.as_deref(), Some("requested"));
        assert_eq!(sc.cancel_progress, Some(0));
        // reset_cancel 歸零取消欄位（下一場 session 不再殘留 requested/0%）。
        m.reset_cancel();
        let s2 = m.state_snapshot();
        assert!(!s2.cancel_requested);
        assert_eq!(s2.cancel_stage, None);
        assert_eq!(s2.cancel_progress, None);
    }

    /// exit guard：Running 阻擋退出，Idle/Completed 允許
    #[test]
    fn refuse_exit_blocks_running_allows_idle_and_completed() {
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)])) as Arc<dyn GpuBackend>;
        let m = BenchmarkManager::new(backend);

        // 初始（Idle）→ 允許
        assert!(m.refuse_exit_if_running().is_ok());
        assert!(!m.is_running());

        // Running → 拒絕
        m.state.write().unwrap().status = SessionStatus::Running;
        assert!(m.is_running());
        assert!(m.refuse_exit_if_running().is_err());

        // Completed / Failed / Cancelled → 允許
        for st in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Cancelled,
        ] {
            m.state.write().unwrap().status = st;
            assert!(!m.is_running(), "{st:?} 不該算執行中");
            assert!(m.refuse_exit_if_running().is_ok(), "{st:?} 應允許退出");
        }
    }

    #[test]
    fn validate_config_rejects_bad_settings() {
        let t = topo();
        // sample=0
        let c = BenchmarkConfig {
            sample_secs: 0,
            ..Default::default()
        };
        assert!(runner::validate_config(&c, &t).is_err());
        // repetitions 越界
        let c = BenchmarkConfig {
            repetitions: 8,
            ..Default::default()
        };
        assert!(runner::validate_config(&c, &t).is_err());
        // Vulkan 但無 args
        let c = BenchmarkConfig {
            vulkan_args: vec![],
            ..Default::default()
        };
        assert!(runner::validate_config(&c, &t).is_err());
        // 沒 GPU
        let c = BenchmarkConfig {
            gpu_instance_id: None,
            ..Default::default()
        };
        assert!(runner::validate_config(&c, &t).is_err());
        // 合法預設（gpu 補上）
        let c = BenchmarkConfig {
            gpu_instance_id: Some(GPU_A.to_string()),
            ..Default::default()
        };
        assert!(runner::validate_config(&c, &t).is_ok());
    }

    #[test]
    fn effective_lps_excludes_core_zero() {
        let t = topo(); // 8 LP（core c = LP c，故 core 0 = LP 0）
        let c = BenchmarkConfig::default();
        // 無 SMT 均質拓撲：預設 = 所有 primary LP（= 全部 LP），排除 physical Core 0 的 LP 0
        assert_eq!(runner::effective_lps(&c, &t), vec![1, 2, 3, 4, 5, 6, 7]);
        // 候選過濾 + 去重 + 排序（LP 0 已在候選外，故不變）
        let c = BenchmarkConfig {
            candidate_lps: vec![5, 1, 5, 99],
            ..Default::default()
        };
        assert_eq!(runner::effective_lps(&c, &t), vec![1, 5]);
        // 顯式只含 core 0 的候選 → 過濾後為空（由 validate_config 拒絕）
        let c = BenchmarkConfig {
            candidate_lps: vec![0],
            ..Default::default()
        };
        assert!(runner::effective_lps(&c, &t).is_empty());
    }

    #[test]
    fn effective_lps_default_excludes_smt_siblings() {
        // 8C16T SMT：sibling 與 primary 同一顆物理核心（GPU policy 綁單一 LP），
        // 預設只測 primary，排除 physical Core 0（LP 0,1）
        let cores: Vec<(Vec<u32>, u8, bool)> = (0..8u32)
            .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
            .collect();
        let t = crate::topology::build_topology(cores);
        let c = BenchmarkConfig::default();
        assert_eq!(runner::effective_lps(&c, &t), vec![2, 4, 6, 8, 10, 12, 14]);
    }

    #[test]
    fn effective_lps_default_p_core_only_on_hybrid() {
        // 8P(HT)+8E：預設只測 P-core primary；E-core 不當 GPU 中斷目標，不測
        let mut cores: Vec<(Vec<u32>, u8, bool)> = Vec::new();
        let mut lp = 0u32;
        for _ in 0..8 {
            cores.push((vec![lp, lp + 1], 1, true));
            lp += 2;
        }
        for _ in 0..8 {
            cores.push((vec![lp], 0, false));
            lp += 1;
        }
        let t = crate::topology::build_topology(cores);
        let c = BenchmarkConfig::default();
        assert_eq!(runner::effective_lps(&c, &t), vec![2, 4, 6, 8, 10, 12, 14]);
    }

    #[test]
    fn round_order_balanced_rotation_and_reversal() {
        let lps = vec![0, 2, 4, 6];
        // 每輪起點旋轉、奇數輪反轉：不重複、不遺漏 LP
        assert_eq!(runner::round_order(0, &lps), vec![0, 2, 4, 6]);
        assert_eq!(runner::round_order(1, &lps), vec![2, 0, 6, 4]);
        assert_eq!(runner::round_order(2, &lps), vec![4, 6, 0, 2]);
        assert_eq!(runner::round_order(3, &lps), vec![6, 4, 2, 0]);
    }

    /// list_importable 只回傳「已完成 + CPU 相容 + GPU 存在 + 有 bestLp」的 session
    #[test]
    fn list_importable_filters_by_compatibility() {
        let dir = temp_dir("importable");
        let root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);

        // 相容已完成（GPU_A + fixed identity 指紋 + bestLp）
        let ok = completed_session(&root, &topo(), GPU_A, 3);

        // 不相容 CPU（不同拓撲指紋）
        let other_topo = build_topology(
            (0..8u32)
                .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
                .collect(),
        );
        let bad_cpu = {
            let id = Uuid::new_v4().to_string();
            let detail = crate::benchmark::SessionDetail {
                summary: crate::benchmark::SessionSummary {
                    id: id.clone(),
                    status: SessionStatus::Completed,
                    started_at: "2026-08-11T00:00:00Z".into(),
                    finished_at: Some("2026-08-11T00:01:00Z".into()),
                    gpu_name: "GPU".into(),
                    gpu_instance_id: GPU_A.to_string(),
                    cpu_fingerprint: cpu_fingerprint_with(&other_topo, &fixed_identity()),
                    best_lp: Some(3),
                    reliability: passed_reliability(),
                    severe_lps: vec![],
                    sample_count: 5,
                    total_bytes: 0,
                    config: BenchmarkConfig::default(),
                    error: None,
                    ..Default::default()
                },
                results: vec![],
                samples: vec![],
                ..Default::default()
            };
            storage::save_session_at(&root, &detail).unwrap();
            id
        };

        // GPU 不存在（本機只有 GPU_A，session 指向 GPU_B）
        let bad_gpu = {
            let id = Uuid::new_v4().to_string();
            let mut detail = crate::benchmark::SessionDetail::default();
            detail.summary.id = id.clone();
            detail.summary.status = SessionStatus::Completed;
            detail.summary.gpu_instance_id = GPU_B.to_string();
            detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
            detail.summary.best_lp = Some(1);
            storage::save_session_at(&root, &detail).unwrap();
            id
        };

        // 未完成（Running）
        let running = {
            let id = Uuid::new_v4().to_string();
            let mut detail = crate::benchmark::SessionDetail::default();
            detail.summary.id = id.clone();
            detail.summary.status = SessionStatus::Running;
            detail.summary.gpu_instance_id = GPU_A.to_string();
            detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
            storage::save_session_at(&root, &detail).unwrap();
            id
        };

        // 已完成但無 bestLp
        let no_best = {
            let id = Uuid::new_v4().to_string();
            let mut detail = crate::benchmark::SessionDetail::default();
            detail.summary.id = id.clone();
            detail.summary.status = SessionStatus::Completed;
            detail.summary.gpu_instance_id = GPU_A.to_string();
            detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
            storage::save_session_at(&root, &detail).unwrap();
            id
        };

        let list = list_importable(&backend, &topo(), &fixed_identity(), &root);
        let ids: Vec<String> = list.iter().map(|s| s.id.clone()).collect();
        assert!(ids.contains(&ok), "相容已完成應可匯入");
        assert!(!ids.contains(&bad_cpu));
        assert!(!ids.contains(&bad_gpu));
        assert!(!ids.contains(&running));
        assert!(!ids.contains(&no_best));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// apply 拒絕「已完成 + bestLp 但可靠性 Unassessed（舊 session）」的 session
    #[test]
    fn apply_rejects_unassessed_reliability() {
        let dir = temp_dir("rel_unass");
        let storage_root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let id = Uuid::new_v4().to_string();
        let mut detail = SessionDetail::default();
        detail.summary.id = id.clone();
        detail.summary.status = SessionStatus::Completed;
        detail.summary.gpu_instance_id = GPU_A.into();
        detail.summary.best_lp = Some(3);
        detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
        // reliability 保持預設 Unassessed（模擬舊 session 缺欄位）
        storage::save_session_at(&storage_root, &detail).unwrap();

        let err = apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo(),
            &storage_root,
            &dir.join("journal.json"),
            &dir.join("restore.json"),
            &id,
        )
        .unwrap_err();
        assert_eq!(err, codes::BENCHMARK_RELIABILITY_NOT_PASSED);
        assert_eq!(backend.restart_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// check_apply：Passed → 可套用；Unassessed → 拒絕（穩定代碼）
    #[test]
    fn check_apply_requires_reliability_passed() {
        let dir = temp_dir("check_rel");
        let root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);

        // Passed → can_apply
        let ok = completed_session(&root, &topo(), GPU_A, 3);
        let st = check_apply_at(&backend, &topo(), &fixed_identity(), &root, &ok);
        assert!(st.can_apply);
        assert_eq!(st.reason, None);

        // 已完成 + bestLp + 相容，但 reliability Unassessed → 拒絕
        let id = Uuid::new_v4().to_string();
        let mut detail = SessionDetail::default();
        detail.summary.id = id.clone();
        detail.summary.status = SessionStatus::Completed;
        detail.summary.gpu_instance_id = GPU_A.into();
        detail.summary.best_lp = Some(3);
        detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
        storage::save_session_at(&root, &detail).unwrap();
        let st = check_apply_at(&backend, &topo(), &fixed_identity(), &root, &id);
        assert!(!st.can_apply);
        assert_eq!(
            st.reason.as_deref(),
            Some(codes::BENCHMARK_RELIABILITY_NOT_PASSED)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// list_importable：Completed + bestLp + 相容，但可靠性 Unassessed → 排除
    #[test]
    fn list_importable_excludes_unassessed_reliability() {
        let dir = temp_dir("import_rel");
        let root = dir.join("benchmarks");
        let backend = FakeBackend::new(vec![device(GPU_A)]);

        let ok = completed_session(&root, &topo(), GPU_A, 3); // Passed → 納入

        let id = Uuid::new_v4().to_string();
        let mut detail = SessionDetail::default();
        detail.summary.id = id.clone();
        detail.summary.status = SessionStatus::Completed;
        detail.summary.gpu_instance_id = GPU_A.into();
        detail.summary.best_lp = Some(1);
        detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
        storage::save_session_at(&root, &detail).unwrap();

        let list = list_importable(&backend, &topo(), &fixed_identity(), &root);
        let ids: Vec<String> = list.iter().map(|s| s.id.clone()).collect();
        assert!(ids.contains(&ok));
        assert!(!ids.contains(&id), "Unassessed 不該可匯入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── apply_affinity_to_gpu（共享 mutation 路徑）──

    #[test]
    fn shared_apply_writes_policy_and_records() {
        let dir = temp_dir("shared_ok");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        backend.set_policy(policy_on(GPU_A, 2, 0b1));

        apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 3, &journal, &restore).unwrap();

        let cur = backend.current_policy(GPU_A);
        assert_dword(&cur, "DevicePolicy", DEVICE_POLICY_SINGLE_PROCESSOR);
        assert_override_mask(&cur, 1u32 << 3);
        assert_eq!(backend.restart_count(), 1);
        assert!(!journal.exists(), "成功後日誌應清除");
        assert!(restore.exists(), "一層還原記錄應寫入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_apply_write_failure_restores() {
        let dir = temp_dir("shared_wfail");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        backend.set_policy(original.clone());
        backend.fail_next_write();

        let err = apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── BenchmarkManager.apply_gpu_affinity ──

    fn manager_with_gpu(gpu: &str) -> (BenchmarkManager, Arc<FakeBackend>) {
        let fake = Arc::new(FakeBackend::new(vec![device(gpu)]));
        let backend = fake.clone() as Arc<dyn GpuBackend>;
        (BenchmarkManager::new(backend), fake)
    }

    #[test]
    fn apply_gpu_affinity_success_writes_correct_mask() {
        let dir = temp_dir("mgr_ok");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let (m, fake) = manager_with_gpu(GPU_A);
        fake.set_policy(policy_on(GPU_A, 2, 0b1));

        m.apply_gpu_affinity_at(&topo(), GPU_A, 5, &journal, &restore)
            .unwrap();

        let cur = m.backend.read_affinity_policy(GPU_A).unwrap();
        assert_dword(&cur, "DevicePolicy", DEVICE_POLICY_SINGLE_PROCESSOR);
        assert_override_mask(&cur, 1u32 << 5);
        assert!(!journal.exists(), "成功後日誌應清除");
        assert!(restore.exists(), "一層還原記錄應寫入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_gpu_affinity_rejects_invalid_lp_outside_range() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        // 8-LP 拓撲，lp=8 超出範圍
        let err = m.apply_gpu_affinity(&topo(), GPU_A, 8).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
    }

    #[test]
    fn apply_gpu_affinity_rejects_lp_64() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        let topo64 = topo_64();
        // 64-LP 拓撲，lp=64 超出 group 0 上限
        let err = m.apply_gpu_affinity(&topo64, GPU_A, 64).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
    }

    #[test]
    fn apply_gpu_affinity_rejects_missing_gpu() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        // 本機只有 GPU_A，GPU_B 不存在
        let err = m.apply_gpu_affinity(&topo(), GPU_B, 3).unwrap_err();
        assert_eq!(err, codes::GPU_NOT_FOUND);
    }

    #[test]
    fn apply_gpu_affinity_blocks_when_benchmark_reserved() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        // benchmark reservation 持有中 → manual apply 必須被拒絕（不碰 backend）
        let _guard = m.reserve(OP_BENCHMARK).unwrap();
        let err = m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_ALREADY_RUNNING);
    }

    #[test]
    fn apply_gpu_affinity_blocks_recovery_required() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        m.recovery_required.store(true, Ordering::Relaxed);
        let err = m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_RECOVERY_REQUIRED);
    }

    #[test]
    fn apply_gpu_affinity_blocks_basic_display_disabled() {
        let (m, fake) = manager_with_gpu(GPU_A);
        fake.basic_display_on.store(false, Ordering::SeqCst);
        let err = m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err();
        assert_eq!(err, codes::GPU_BASIC_DISPLAY_DISABLED);
    }

    #[test]
    fn apply_gpu_affinity_write_failure_restores_and_clears_journal() {
        let dir = temp_dir("mgr_writefail");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let (m, fake) = manager_with_gpu(GPU_A);
        let original = policy_on(GPU_A, 2, 0b1);
        fake.set_policy(original.clone());
        fake.fail_next_write();

        let err = m
            .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        // 還原成功 → 策略回原樣，日誌清除
        assert_eq!(m.backend.read_affinity_policy(GPU_A).unwrap(), original);
        assert!(!journal.exists(), "還原成功後日誌應清除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── reservation / 取消污染（concurrency）──

    /// request_cancel 留下 channel=true 後，下一場 session 開始前 reset_cancel
    /// 必須把 channel 實際值與 state.cancel_requested 都歸零，否則新 session 立即 Cancelled。
    #[test]
    fn cancel_reset_clears_channel_for_next_session() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        assert!(!m.cancel_requested());
        m.request_cancel();
        assert!(m.cancel_requested());
        m.reset_cancel();
        assert!(!m.cancel_requested());
        assert!(!m.state_snapshot().cancel_requested);
        // 新 clone 給 runner 的 receiver 也必須讀到 false
        assert!(!*m.cancel_receiver().borrow());
    }

    /// 多執行緒同時 reserve：CAS 保證只會有一個成功（其餘回 BENCHMARK_ALREADY_RUNNING）。
    #[test]
    fn concurrent_reserve_only_one_wins() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        let m = Arc::new(m);
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = m.clone();
            let b = barrier.clone();
            handles.push(std::thread::spawn(move || {
                b.wait();
                let g = m.reserve(OP_BENCHMARK);
                // 贏家持鎖到其他執行緒也完成 attempt，避免釋放過早造成第二個成功
                std::thread::sleep(std::time::Duration::from_millis(50));
                g.is_ok()
            }));
        }
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(wins, 1, "並行 start 只能一個成功");
    }

    /// benchmark 執行期間，apply_best / manual apply / restore_previous 三種 mutation
    /// 都必須被後端拒絕（不碰 backend、不讀 session）。
    #[test]
    fn benchmark_reservation_blocks_all_mutations() {
        let (m, fake) = manager_with_gpu(GPU_A);
        let _guard = m.reserve(OP_BENCHMARK).unwrap();
        let sid = "00000000-0000-0000-0000-000000000000";

        assert_eq!(
            m.apply_best(&topo(), sid).unwrap_err(),
            codes::BENCHMARK_ALREADY_RUNNING
        );
        assert_eq!(
            m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
            codes::BENCHMARK_ALREADY_RUNNING
        );
        assert_eq!(
            m.restore_previous().unwrap_err(),
            codes::BENCHMARK_ALREADY_RUNNING
        );
        assert_eq!(fake.restart_count(), 0, "被拒時不得動到 GPU");
    }

    /// mutation 執行期間，不得開始 benchmark（start 的 reserve）也不得另一 mutation。
    #[test]
    fn mutation_reservation_blocks_benchmark_and_other_mutation() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        let _guard = m.reserve(OP_MUTATION).unwrap();
        assert!(m.reserve(OP_BENCHMARK).is_err(), "start 不得搶 mutation");
        assert!(
            m.reserve(OP_MUTATION).is_err(),
            "mutation 不得並行另一 mutation"
        );
        assert_eq!(
            m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
            codes::BENCHMARK_ALREADY_RUNNING
        );
    }

    /// 前置驗證失敗（LP 越界 / GPU 不存在 / BasicDisplay 停用）不得卡住 reservation。
    #[test]
    fn preflight_failure_releases_reservation() {
        let (m, fake) = manager_with_gpu(GPU_A);

        // 1) LP 越界
        assert_eq!(
            m.apply_gpu_affinity(&topo(), GPU_A, 8).unwrap_err(),
            codes::BENCHMARK_SESSION_INCOMPATIBLE
        );
        drop(m.reserve(OP_BENCHMARK).unwrap());

        // 2) GPU 不存在
        assert_eq!(
            m.apply_gpu_affinity(&topo(), GPU_B, 3).unwrap_err(),
            codes::GPU_NOT_FOUND
        );
        drop(m.reserve(OP_MUTATION).unwrap());

        // 3) BasicDisplay 停用
        fake.basic_display_on.store(false, Ordering::SeqCst);
        assert_eq!(
            m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
            codes::GPU_BASIC_DISPLAY_DISABLED
        );
        drop(m.reserve(OP_BENCHMARK).unwrap());
    }

    // ── 錯誤安全 transaction（rollback / fault injection）──

    /// journal stage advance（PolicyApplied）失敗 → 立即 rollback，策略還原、日誌清除。
    #[test]
    fn apply_journal_advance_failure_rolls_back() {
        let dir = temp_dir("advfail");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        backend.set_policy(original.clone());
        recovery::inject::fail_next_advance();

        let err = apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists(), "advance 失敗 rollback 後日誌應清除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// read-back 讀取失敗（第 2 次 read）→ 立即 rollback，回穩定代碼。
    #[test]
    fn apply_read_back_failure_rolls_back() {
        let dir = temp_dir("readbackfail");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        backend.set_policy(original.clone());
        backend.fail_nth_read(2); // snapshot read=1 成功；read-back read=2 失敗

        let err = apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_REGISTRY_FAILED);
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists(), "read-back 失敗 rollback 後日誌應清除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// read-back 驗證不符（第 2 次 read 回錯 mask）→ 立即 rollback。
    #[test]
    fn apply_read_back_mismatch_rolls_back() {
        let dir = temp_dir("readbackmismatch");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        backend.set_policy(original.clone());
        backend.fail_nth_read_mismatch(2);

        let err = apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists(), "read-back 不符 rollback 後日誌應清除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 還原記錄 atomic write 失敗 → rollback，不留下已套用策略。
    /// 以 restore 路徑的 parent 做成檔案模擬 write 失敗（create_dir_all 失敗），
    /// 只失敗寫入、不影響 clear_restore_record 的 NotFound→Ok，還原乾淨。
    #[test]
    fn apply_restore_record_write_failure_rolls_back() {
        let dir = temp_dir("recfail");
        let journal = dir.join("journal.json");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap(); // parent 為檔案 → atomic_write 的 create_dir_all 失敗
        let restore = blocker.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        backend.set_policy(original.clone());

        let err = apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists(), "還原記錄寫入失敗 rollback 後日誌應清除");
        assert!(!restore.exists(), "還原記錄不該被寫入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// journal clear 失敗（成功路徑）→ rollback，避免 stale 日誌讓下次啟動誤還原。
    #[test]
    fn apply_journal_clear_failure_rolls_back() {
        let dir = temp_dir("clearfail");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let original = policy_on(GPU_A, 2, 0b1);
        backend.set_policy(original.clone());
        recovery::inject::fail_next_clear();

        let err = apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists(), "clear 失敗 rollback 後日誌應清除");
        assert!(!restore.exists(), "rollback 後未完成還原記錄應清除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rollback 失敗（restart 持續失敗）→ 保留 stage=PolicyApplied 日誌、
    /// manager 設 recoveryRequired、封鎖後續 mutation。
    #[test]
    fn apply_rollback_failure_sets_recovery_required() {
        let dir = temp_dir("rollbackfail");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let (m, fake) = manager_with_gpu(GPU_A);
        let original = policy_on(GPU_A, 2, 0b1);
        fake.set_policy(original.clone());
        fake.disable_fails.store(true, Ordering::SeqCst); // restart 持續失敗 → rollback 也失敗

        let err = m
            .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_RESTART_FAILED);
        assert!(m.recovery_required(), "rollback 失敗應設 recoveryRequired");
        assert!(m.state_snapshot().recovery_required);
        assert!(journal.exists(), "rollback 失敗應保留復原日誌");
        let j = recovery::load_from(&journal).unwrap().unwrap();
        assert_eq!(j.stage, RecoveryStage::PolicyApplied, "應強制完整 restore");
        assert_eq!(m.backend.read_affinity_policy(GPU_A).unwrap(), original);
        // 封鎖後續 mutation
        assert_eq!(
            m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
            codes::BENCHMARK_RECOVERY_REQUIRED
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rollback 還原成功但 restore record 清理失敗 → Err 且 manager recoveryRequired，
    /// 不得留下 stale gpu-restore.json 卻 recoveryRequired=false。
    #[test]
    fn rollback_restore_record_cleanup_failure_sets_recovery_required() {
        let dir = temp_dir("rollback_crec");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let (m, fake) = manager_with_gpu(GPU_A);
        let original = policy_on(GPU_A, 2, 0b1);
        fake.set_policy(original.clone());
        fake.fail_next_write(); // apply 的 write 失敗 → rollback
        inject::fail_next_clear_restore_record(); // rollback 的 restore record 清理失敗

        let err = m
            .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        assert!(
            m.recovery_required(),
            "restore record 清理失敗應設 recoveryRequired"
        );
        assert_eq!(m.backend.read_affinity_policy(GPU_A).unwrap(), original);
        assert!(journal.exists(), "清理失敗應保留 dirty journal");
        let j = recovery::load_from(&journal).unwrap().unwrap();
        assert_eq!(j.stage, RecoveryStage::PolicyApplied);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rollback 還原成功但 journal cleanup 失敗 → 不得視為乾淨 rollback。
    #[test]
    fn rollback_journal_cleanup_failure_sets_recovery_required() {
        let dir = temp_dir("rollback_cjournal");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let (m, fake) = manager_with_gpu(GPU_A);
        let original = policy_on(GPU_A, 2, 0b1);
        fake.set_policy(original.clone());
        fake.fail_next_write(); // apply 的 write 失敗 → rollback
        recovery::inject::fail_next_clear(); // rollback 的 journal clear 失敗

        let err = m
            .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        assert!(
            m.recovery_required(),
            "journal cleanup 失敗不得視為乾淨 rollback"
        );
        assert!(journal.exists(), "清理失敗應保留 dirty journal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// journal stage advance（PolicyApplied）失敗後，rollback 的完整復原也失敗 →
    /// 必須 fail-closed：journal 升級為 PolicyApplied（非只驗證的 SnapshotTaken），
    /// manager 設 recoveryRequired 封鎖後續 mutation。
    #[test]
    fn apply_journal_advance_and_restore_failure_fail_closed() {
        let dir = temp_dir("advance_restore_fail");
        let journal = dir.join("journal.json");
        let restore = dir.join("restore.json");
        let (m, fake) = manager_with_gpu(GPU_A);
        let original = policy_on(GPU_A, 2, 0b1);
        fake.set_policy(original.clone());
        fake.disable_fails.store(true, Ordering::SeqCst); // rollback 的 restore restart 持續失敗
        recovery::inject::fail_next_advance(); // PolicyApplied advance 失敗

        let err = m
            .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
            .unwrap_err();
        assert_eq!(err, codes::GPU_APPLY_FAILED);
        assert!(
            m.recovery_required(),
            "advance + restore 失敗必須 fail-closed"
        );
        assert!(journal.exists());
        let j = recovery::load_from(&journal).unwrap().unwrap();
        assert_eq!(
            j.stage,
            RecoveryStage::PolicyApplied,
            "journal 不得停留在 SnapshotTaken"
        );
        assert_eq!(
            m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
            codes::BENCHMARK_RECOVERY_REQUIRED
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// load_restore_record：NotFound → None；有效 → Some；壞 JSON → Err；其他 I/O error → Err。
    #[test]
    fn load_restore_record_distinguishes_notfound_and_errors() {
        let dir = temp_dir("loadrec");
        // NotFound → None
        assert_eq!(
            load_restore_record(&dir.join("missing.json")).unwrap(),
            None
        );
        // 有效（已認證）JSON → Some
        let good = dir.join("good.json");
        let snap = policy_on(GPU_A, 2, 0b1);
        crate::state_auth::auth_write(&good, &serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(load_restore_record(&good).unwrap(), Some(snap.clone()));
        // 未認證的直接寫入（無 MAC 旁檔）→ Err
        let unsigned = dir.join("unsigned.json");
        std::fs::write(&unsigned, serde_json::to_string(&snap).unwrap()).unwrap();
        assert!(load_restore_record(&unsigned).is_err());
        // 壞 JSON → Err
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert!(load_restore_record(&bad).is_err());
        // 其他 I/O error（目錄）→ Err，不是 None
        let as_dir = dir.join("asdir.json");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert!(load_restore_record(&as_dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 等效安全驗證 / 等效套用（Task 3）──

    fn equivalent_reliability() -> ReliabilitySummary {
        ReliabilitySummary {
            status: ReliabilityStatus::Equivalent,
            algorithm_version: 2,
            ..Default::default()
        }
    }

    /// 寫入一個 equivalent-mode Completed session（equivalent_finalist_lps = finalists）。
    fn equivalent_session(
        storage_root: &Path,
        topo: &Topology,
        gpu: &str,
        finalists: &[u32],
    ) -> String {
        let id = Uuid::new_v4().to_string();
        let detail = SessionDetail {
            summary: crate::benchmark::SessionSummary {
                id: id.clone(),
                status: SessionStatus::Completed,
                started_at: "2026-08-11T00:00:00Z".into(),
                finished_at: Some("2026-08-11T00:01:00Z".into()),
                gpu_name: "GPU".into(),
                gpu_instance_id: gpu.to_string(),
                cpu_fingerprint: cpu_fingerprint_with(topo, &fixed_identity()),
                best_lp: None,
                reliability: equivalent_reliability(),
                equivalent_finalist_lps: finalists.to_vec(),
                severe_lps: vec![],
                sample_count: 5,
                total_bytes: 0,
                config: BenchmarkConfig::default(),
                error: None,
                ..Default::default()
            },
            results: vec![],
            samples: vec![],
            ..Default::default()
        };
        storage::save_session_at(storage_root, &detail).unwrap();
        id
    }

    #[test]
    fn mask_bytes_to_lp_roundtrips_and_rejects_non_single_bit() {
        assert_eq!(mask_bytes_to_lp(Some(&single_lp_mask_bytes(0))), Some(0));
        assert_eq!(mask_bytes_to_lp(Some(&single_lp_mask_bytes(3))), Some(3));
        assert_eq!(mask_bytes_to_lp(Some(&single_lp_mask_bytes(63))), Some(63));
        // 非單一位元 / 空 → None
        assert_eq!(mask_bytes_to_lp(None), None);
        assert_eq!(mask_bytes_to_lp(Some(&[])), None);
        assert_eq!(mask_bytes_to_lp(Some(&[0b11])), None);
    }

    #[test]
    fn equivalent_validation_plan_immediate_pass_when_current_in_pair() {
        let dir = temp_dir("plan_imm");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
        let detail = storage::get_at(&storage_root, &sid).unwrap();
        // 目前鎖定 LP 5（在 pair 內）
        let policy = policy_on(GPU_A, 4, 1 << 5);
        match equivalent_validation_plan(&backend, &topo(), &fixed_identity(), &detail, &policy, 3)
        {
            Ok(EquivalentValidationPlan::ImmediatePass { reference_lp }) => {
                assert_eq!(reference_lp, 5);
            }
            other => panic!("應 ImmediatePass: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn equivalent_validation_plan_run_captures_when_current_outside_pair() {
        let dir = temp_dir("plan_run");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
        let detail = storage::get_at(&storage_root, &sid).unwrap();
        // 目前鎖定 LP 2（不在 pair 內）
        let policy = policy_on(GPU_A, 4, 1 << 2);
        match equivalent_validation_plan(&backend, &topo(), &fixed_identity(), &detail, &policy, 3)
        {
            Ok(EquivalentValidationPlan::RunCaptures { reference_lp }) => {
                assert_eq!(reference_lp, 2);
            }
            other => panic!("應 RunCaptures: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn equivalent_validation_plan_rejects_non_pair_selected_and_no_reference() {
        let dir = temp_dir("plan_reject");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
        let detail = storage::get_at(&storage_root, &sid).unwrap();
        let policy = policy_on(GPU_A, 4, 1 << 5);
        // selected 不在 pair → LP_INVALID
        assert_eq!(
            equivalent_validation_plan(&backend, &topo(), &fixed_identity(), &detail, &policy, 9)
                .unwrap_err(),
            codes::BENCHMARK_EQUIVALENT_LP_INVALID
        );
        // 無單一鎖定核心（policy 空）→ NO_REFERENCE
        let empty = AffinityPolicy {
            instance_id: GPU_A.to_string(),
            ..Default::default()
        };
        assert_eq!(
            equivalent_validation_plan(&backend, &topo(), &fixed_identity(), &detail, &empty, 3)
                .unwrap_err(),
            codes::BENCHMARK_EQUIVALENT_NO_REFERENCE
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_equivalent_decision_requires_passed_matching_validation() {
        let dir = temp_dir("apply_decision");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
        // 未驗證 → VALIDATION_REQUIRED
        {
            let detail = storage::get_at(&storage_root, &sid).unwrap();
            let policy = policy_on(GPU_A, 4, 1 << 2);
            assert_eq!(
                apply_equivalent_decision(
                    &backend,
                    &topo(),
                    &fixed_identity(),
                    &detail,
                    &policy,
                    3
                )
                .unwrap_err(),
                codes::BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED
            );
        }
        // 寫入 Passed validation（selected=3、ref mask=LP2）→ 通過
        write_equivalent_validation(
            &storage_root,
            &sid,
            EquivalentSafetyValidation {
                status: EquivalentSafetyStatus::Passed,
                selected_lp: Some(3),
                reference_lp: Some(2),
                rounds: 3,
                reference_policy_mask: Some(single_lp_mask_bytes(2)),
                reason: Some("passed".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let detail = storage::get_at(&storage_root, &sid).unwrap();
        // live policy 仍為 LP2（未變）→ 通過
        let policy = policy_on(GPU_A, 4, 1 << 2);
        assert_eq!(
            apply_equivalent_decision(&backend, &topo(), &fixed_identity(), &detail, &policy, 3)
                .unwrap(),
            3
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_equivalent_decision_rejects_reference_changed() {
        let dir = temp_dir("apply_changed");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
        write_equivalent_validation(
            &storage_root,
            &sid,
            EquivalentSafetyValidation {
                status: EquivalentSafetyStatus::Passed,
                selected_lp: Some(3),
                reference_lp: Some(2),
                rounds: 3,
                reference_policy_mask: Some(single_lp_mask_bytes(2)),
                reason: Some("passed".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let detail = storage::get_at(&storage_root, &sid).unwrap();
        // live policy 變更為 LP4（≠ snapshot LP2）→ REFERENCE_CHANGED
        let policy = policy_on(GPU_A, 4, 1 << 4);
        assert_eq!(
            apply_equivalent_decision(&backend, &topo(), &fixed_identity(), &detail, &policy, 3)
                .unwrap_err(),
            codes::BENCHMARK_EQUIVALENT_REFERENCE_CHANGED
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_apply_equivalent_requires_validation_then_can_apply() {
        let dir = temp_dir("check_equiv");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
        // 尚未驗證 → equivalent_mode=true、requires_safety_validation=true、can_apply=false
        let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
        assert!(st.equivalent_mode);
        assert_eq!(st.allowed_lps, vec![3, 5]);
        assert!(st.requires_safety_validation);
        assert!(!st.can_apply);
        assert_eq!(
            st.reason.as_deref(),
            Some(codes::BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED)
        );
        // 寫入 Passed validation（ref snapshot=LP2），live policy 仍為 LP2 → can_apply=true、requires=false
        write_equivalent_validation(
            &storage_root,
            &sid,
            EquivalentSafetyValidation {
                status: EquivalentSafetyStatus::Passed,
                selected_lp: Some(3),
                reference_lp: Some(2),
                rounds: 3,
                reference_policy_mask: Some(single_lp_mask_bytes(2)),
                reason: Some("passed".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        backend.set_policy(policy_on(GPU_A, 4, 1 << 2));
        let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
        assert!(st.equivalent_mode);
        assert!(st.can_apply);
        assert!(!st.requires_safety_validation);
        assert_eq!(st.reason, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_apply_legacy_path_unchanged_for_passed_session() {
        let dir = temp_dir("check_legacy");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let sid = completed_session(&storage_root, &topo(), GPU_A, 3);
        let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
        assert!(st.can_apply);
        assert!(!st.equivalent_mode);
        assert!(st.allowed_lps.is_empty());
        assert!(!st.requires_safety_validation);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// asset 解析/驗證失敗不得留下永久 Pending；assets 就緒後可重試成功。
    #[test]
    fn begin_equivalent_validation_asset_failure_leaves_no_pending_and_is_retryable() {
        let dir = temp_dir("begin_assets");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);

        // asset 驗證失敗 → 不寫 Pending，session validation 保持 None（可重試）
        let err = begin_equivalent_validation(
            &storage_root,
            &sid,
            3,
            2,
            Some(single_lp_mask_bytes(2)),
            Err(codes::BENCHMARK_ASSETS_MISSING.to_string()),
        )
        .unwrap_err();
        assert_eq!(err, codes::BENCHMARK_ASSETS_MISSING);
        assert!(
            storage::get_at(&storage_root, &sid)
                .unwrap()
                .equivalent_safety_validation
                .is_none(),
            "asset 失敗不得留下 Pending"
        );

        // assets 就緒 → 重試成功寫入 Pending
        let assets = BenchmarkAssets {
            presentmon: PathBuf::from("pm"),
            vulkan_workload: PathBuf::from("vk"),
            d3d9_workload: PathBuf::from("d3d9"),
        };
        let out = begin_equivalent_validation(
            &storage_root,
            &sid,
            3,
            2,
            Some(single_lp_mask_bytes(2)),
            Ok(assets),
        )
        .unwrap();
        assert_eq!(out.presentmon, PathBuf::from("pm"));
        let v = storage::get_at(&storage_root, &sid)
            .unwrap()
            .equivalent_safety_validation
            .unwrap();
        assert_eq!(v.status, EquivalentSafetyStatus::Pending);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// validation_running 以 OP_VALIDATION reservation 辨識（不讀 session status）。
    #[test]
    fn validation_running_reflects_reservation() {
        let (m, _fake) = manager_with_gpu(GPU_A);
        assert!(!m.validation_running());
        let guard = m.reserve(OP_VALIDATION).unwrap();
        assert!(m.validation_running());
        drop(guard);
        assert!(!m.validation_running());
    }

    /// check_apply：live reference policy 變更 → can_apply=false、reason=REFERENCE_CHANGED。
    #[test]
    fn check_apply_equivalent_rejects_reference_changed() {
        let dir = temp_dir("check_ref_changed");
        let storage_root = dir.join("benchmarks");
        std::fs::create_dir_all(&storage_root).unwrap();
        let backend = FakeBackend::new(vec![device(GPU_A)]);
        let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
        write_equivalent_validation(
            &storage_root,
            &sid,
            EquivalentSafetyValidation {
                status: EquivalentSafetyStatus::Passed,
                selected_lp: Some(3),
                reference_lp: Some(2),
                rounds: 3,
                reference_policy_mask: Some(single_lp_mask_bytes(2)),
                reason: Some("passed".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        // live policy 仍是 LP2 → can_apply
        backend.set_policy(policy_on(GPU_A, 4, 1 << 2));
        let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
        assert!(st.can_apply);

        // live policy 變更為 LP4 → 拒絕
        backend.set_policy(policy_on(GPU_A, 4, 1 << 4));
        let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
        assert!(!st.can_apply);
        assert_eq!(
            st.reason.as_deref(),
            Some(codes::BENCHMARK_EQUIVALENT_REFERENCE_CHANGED)
        );
        assert!(st.requires_safety_validation);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn physical_core_apply_rolls_back_faults_and_preserves_pending_recovery() {
        for mode in [
            "write", "restart", "read", "mismatch", "clear", "restore", "missing",
        ] {
            let root = temp_dir(&format!("physical_apply_{mode}"));
            let backend = Arc::new(FakeBackend::new(if mode == "missing" {
                vec![]
            } else {
                vec![device(GPU_A)]
            }));
            let baseline = policy_on(GPU_A, 4, 4); // E-core policy is a valid restore target.
            backend.set_policy(baseline.clone());
            let mut manager = BenchmarkManager::new(backend.clone());
            manager.sleeper = Arc::new(NoopSleeper);
            let topo = build_topology(vec![(vec![0, 1], 1, true), (vec![2], 0, false)]);
            if mode == "write" {
                backend.fail_next_write();
            }
            if mode == "restart" {
                backend.fail_next_restart.store(true, Ordering::SeqCst);
            }
            if mode == "read" {
                backend.fail_nth_read(2);
            }
            if mode == "mismatch" {
                backend.fail_nth_read_mismatch(2);
            }
            if mode == "clear" {
                recovery::inject::fail_next_clear();
            }
            if mode == "restore" {
                backend.disable_fails.store(true, Ordering::SeqCst);
            }
            let journal = root.join("journal.json");
            let restore = root.join("restore.json");
            assert!(
                manager
                    .apply_core_at(
                        &topo,
                        &fixed_identity(),
                        GPU_A,
                        0,
                        None,
                        &root,
                        &journal,
                        &restore
                    )
                    .is_err(),
                "{mode}"
            );
            assert_eq!(backend.current_policy(GPU_A), baseline, "{mode}");
            if mode == "restore" {
                assert!(journal.exists());
                assert!(manager.recovery_required());
            } else {
                assert!(!journal.exists());
                assert!(!manager.recovery_required());
            }
        }
    }

    #[test]
    fn legacy_gpu_restore_remains_valid_and_is_journaled_on_failure() {
        let root = temp_dir("physical_legacy_restore");
        let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
        let original = policy_on(GPU_A, 4, 0x84); // arbitrary multi-LP policy, not a candidate
        let mut manager = BenchmarkManager::new(backend.clone());
        manager.sleeper = Arc::new(NoopSleeper);
        let journal = root.join("journal.json");
        let restore = root.join("restore.json");
        write_restore_record(&restore, &original).unwrap();
        backend.fail_next_restart.store(true, Ordering::SeqCst);
        assert!(manager.restore_previous_at(&journal, &restore).is_err());
        assert!(journal.exists());
        assert!(restore.exists());
        assert!(manager.recovery_required());
        manager.restore_previous_at(&journal, &restore).unwrap();
        assert_eq!(backend.current_policy(GPU_A), original);
        assert!(!journal.exists());
        assert!(!manager.recovery_required());
    }

    #[test]
    fn all_gpu_operations_block_close_and_updates_exclude_mutations() {
        let m = BenchmarkManager::new(Arc::new(FakeBackend::new(vec![])));
        for kind in [OP_BENCHMARK, OP_MUTATION] {
            let guard = m.reserve(kind).unwrap();
            assert!(m.refuse_exit_if_running().is_err());
            assert!(!m.can_exit());
            assert!(m.begin_update().is_err());
            assert!(m.state_snapshot().gpu_busy);
            drop(guard);
        }
        m.begin_update().unwrap();
        assert!(m.reserve(OP_BENCHMARK).is_err());
        assert!(m.reserve(OP_MUTATION).is_err());
        assert!(m.can_exit()); // installer may restart after acquiring exclusion
        m.end_update();
        assert!(!m.state_snapshot().gpu_busy);
        assert!(m.can_exit());
    }
}
