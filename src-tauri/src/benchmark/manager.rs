#[path = "core_apply.rs"]
pub mod core_apply;
// 基準測試管理者（Task 1 骨架）：AppState 持有 `BenchmarkManager`，
// 提供狀態查詢、取消訊號、實體核心套用、先前策略還原與啟動回復。
//
// 所有會動系統的協調都寫成接受注入路徑的 free function，
// 單元測試用 fake backend + 暫存目錄跑完整流程，不碰真實 HKLM/裝置。

use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
    BenchmarkProgress, BenchmarkStage, BenchmarkState, CpuIdentity, DriftStatus, SessionDetail,
    SessionStatus, WindowIntegrity, WindowLayout,
};

/// 一層還原記錄檔：`%APPDATA%\PaceDock\gpu-restore.json`。
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

// ── 政策漂移偵測 ────────────────────────────────────────────────────────

/// 已套用政策記錄：`%APPDATA%\PaceDock\gpu-applied.json`（HMAC 認證）。
/// 記住「目前鎖定哪個 GPU 的哪顆核心與 mask」，供漂移偵測比對；
/// 驅動更新或外部工具改寫 registry 後可偵測並提示重新套用/還原。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AppliedPolicyRecord {
    pub instance_id: String,
    pub core_id: u32,
    /// 套用的完整 LP 清單（含 SMT sibling）
    pub lp_indices: Vec<u16>,
    /// 套用的 AssignmentSetOverride 原始位元組（比對基準，免重組 mask）
    pub override_bytes: Vec<u8>,
    /// RFC3339 套用時間
    pub applied_at: String,
}

/// 已套用政策記錄路徑
pub fn applied_record_path() -> PathBuf {
    config::config_dir().join("gpu-applied.json")
}

fn write_applied_record(path: &Path, record: &AppliedPolicyRecord) -> Result<(), String> {
    let text = serde_json::to_string_pretty(record).map_err(|e| format!("序列化: {e}"))?;
    crate::state_auth::auth_write(path, &text)
}

/// 檢查已套用政策是否仍與 registry 一致（純比對，可注入路徑測試）。
/// 無記錄/記錄損壞/讀取失敗 → DriftStatus::None（診斷面 fail-quiet，不誤報）。
pub fn check_policy_drift_at(backend: &dyn GpuBackend, record_path: &Path) -> DriftStatus {
    let Ok(Some(record)) = load_applied_record(record_path) else {
        return DriftStatus::None;
    };
    let current = match backend.read_affinity_policy(&record.instance_id) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("漂移偵測讀取 registry 失敗: {}", e.code());
            return DriftStatus::None;
        }
    };
    let expected = AffinityPolicy {
        instance_id: record.instance_id.clone(),
        device_policy: RegistryValueSnapshot::dword(DEVICE_POLICY_SINGLE_PROCESSOR),
        assignment_set_override: RegistryValueSnapshot::binary(record.override_bytes.clone()),
    };
    if crate::gpu::policy_matches(&expected, &current) {
        DriftStatus::Match
    } else {
        DriftStatus::Drifted
    }
}

/// 讀取已套用政策記錄（HMAC fail-closed；無記錄 → None；篡改/損壞 → Err）
fn load_applied_record(path: &Path) -> Result<Option<AppliedPolicyRecord>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = crate::state_auth::auth_read(path)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("套用記錄解析失敗: {e}"))
}

/// MSI 還原記錄：`%APPDATA%\PaceDock\gpu-msi-restore.json`（HMAC 認證）。
/// 內容為套用前的 `MSISupported` 值快照（本來不存在 → present:false）。
pub fn msi_record_path() -> PathBuf {
    config::config_dir().join("gpu-msi-restore.json")
}

/// 讀取 MSI 還原記錄（HMAC fail-closed；無記錄 → None）
fn load_msi_record(path: &Path) -> Result<Option<RegistryValueSnapshot>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = crate::state_auth::auth_read(path)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("MSI 還原記錄解析失敗: {e}"))
}

/// 額外欄位由舊的快照 reader 忽略，保持還原記錄相容。
fn write_msi_monitor_record(
    path: &Path,
    snapshot: &RegistryValueSnapshot,
    instance_id: &str,
) -> Result<(), String> {
    let mut value = serde_json::to_value(snapshot).map_err(|e| e.to_string())?;
    value["instanceId"] = serde_json::Value::String(instance_id.to_string());
    crate::state_auth::auth_write(path, &value.to_string())
}

/// 呼叫端持有 GPU reservation。讀取失敗與無記錄分開，避免誤清警示。
pub(crate) fn monitored_gpu_settings(
    backend: &dyn GpuBackend,
) -> Vec<(crate::drift::Setting, Option<bool>)> {
    use crate::drift::Setting;
    let affinity = (|| -> Result<bool, String> {
        let path = applied_record_path();
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok(false);
        }
        let Some(record) = load_applied_record(&path)? else {
            return Ok(false);
        };
        let current = backend
            .read_affinity_policy(&record.instance_id)
            .map_err(|e| e.code().to_string())?;
        let expected = AffinityPolicy {
            instance_id: record.instance_id,
            device_policy: RegistryValueSnapshot::dword(DEVICE_POLICY_SINGLE_PROCESSOR),
            assignment_set_override: RegistryValueSnapshot::binary(record.override_bytes),
        };
        Ok(!policy_matches(&expected, &current))
    })();
    let msi = (|| -> Result<bool, String> {
        let path = msi_record_path();
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok(false);
        }
        let record: serde_json::Value = serde_json::from_str(&crate::state_auth::auth_read(&path)?)
            .map_err(|e| e.to_string())?;
        let Some(id) = record.get("instanceId").and_then(|v| v.as_str()) else {
            // 舊記錄沒有裝置識別，不能以目前選取的 GPU 猜測。
            return Err("MSI record has no device identity".into());
        };
        let actual = backend
            .read_msi_supported(id)
            .map_err(|e| e.code().to_string())?;
        Ok(actual.as_dword() != Some(1))
    })();
    vec![(Setting::Gpu, affinity.ok()), (Setting::Msi, msi.ok())]
}

fn clear_msi_record(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("清除 MSI 還原記錄失敗: {e}")),
    }
}

/// 統一 MSI rollback：寫回快照（含裝置重啟與驗證）並清記錄。
/// 語意與 [`rollback`] 一致：還原成功且記錄清除成功 → clean=true。
fn rollback_msi(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    instance_id: &str,
    snapshot: &RegistryValueSnapshot,
    record_path: &Path,
    error_code: &str,
) -> ApplyError {
    match restore_msi_snapshot(backend, sleeper, instance_id, snapshot) {
        Ok(()) => {
            let cleared = clear_msi_record(record_path);
            if cleared.is_ok() {
                ApplyError {
                    code: error_code.to_string(),
                    clean: true,
                }
            } else {
                log::error!("MSI rollback 清除記錄失敗: {cleared:?}");
                ApplyError {
                    code: error_code.to_string(),
                    clean: false,
                }
            }
        }
        Err(e) => {
            log::error!("MSI mutation 還原失敗: {e}");
            let _ = clear_msi_record(record_path);
            ApplyError {
                code: error_code.to_string(),
                clean: false,
            }
        }
    }
}

/// 寫回 MSI 快照 + 重啟 + 回讀驗證（快照 absent → 刪值）
fn restore_msi_snapshot(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    instance_id: &str,
    snapshot: &RegistryValueSnapshot,
) -> Result<(), String> {
    backend
        .write_msi_supported(instance_id, snapshot)
        .map_err(|e| e.code().to_string())?;
    backend
        .restart_device(instance_id, sleeper)
        .map_err(|e| e.code().to_string())?;
    let read_back = backend
        .read_msi_supported(instance_id)
        .map_err(|e| e.code().to_string())?;
    if read_back != *snapshot {
        return Err(codes::GPU_RESTORE_FAILED.to_string());
    }
    Ok(())
}

/// 啟用 GPU MSI 模式：`MSISupported=1`（REG_DWORD）+ 裝置重啟 + 回讀驗證。
/// 前置：呼叫端已取得 mutation 排他權、驗證 GPU 存在。
/// `ponytail:` 刻意不接 recovery journal：寫入+重啟後即為期望終態，crash 中斷
/// 最壞情況是「已啟用但記錄殘留」（記錄僅作還原指標，不會被啟動流程誤套用）；
/// 若要求 crash-safe 回滾，再為 MSI 接 journal stage（升級路徑）。
pub fn apply_msi_to_gpu(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    instance_id: &str,
    record_path: &Path,
) -> Result<(), ApplyError> {
    // 1) 快照目前值；已啟用 → no-op 成功（避免無謂裝置重啟）
    let snapshot = backend
        .read_msi_supported(instance_id)
        .map_err(|e| ApplyError::clean(e.code()))?;
    if snapshot.as_dword() == Some(1) {
        // 明確對此裝置重新套用時，可補齊舊版記錄的識別，保留原始還原值。
        if let Some(original) =
            load_msi_record(record_path).map_err(|_| ApplyError::clean(codes::GPU_APPLY_FAILED))?
        {
            write_msi_monitor_record(record_path, &original, instance_id)
                .map_err(|_| ApplyError::clean(codes::GPU_APPLY_FAILED))?;
        }
        return Ok(());
    }
    // 2) 先寫還原記錄（HMAC），再動 registry
    write_msi_monitor_record(record_path, &snapshot, instance_id).map_err(|e| {
        log::error!("MSI 還原記錄寫入失敗: {e}");
        ApplyError::clean(codes::GPU_APPLY_FAILED)
    })?;
    // 3) 寫入 MSISupported=1
    if let Err(_e) = backend.write_msi_supported(instance_id, &RegistryValueSnapshot::dword(1)) {
        return Err(rollback_msi(
            backend,
            sleeper,
            instance_id,
            &snapshot,
            record_path,
            codes::GPU_APPLY_FAILED,
        ));
    }
    // 4) 重啟裝置 + 5) 回讀驗證
    if let Err(_e) = backend.restart_device(instance_id, sleeper) {
        return Err(rollback_msi(
            backend,
            sleeper,
            instance_id,
            &snapshot,
            record_path,
            codes::GPU_RESTART_FAILED,
        ));
    }
    let read_back = backend
        .read_msi_supported(instance_id)
        .map_err(|e| ApplyError::clean(e.code()))?;
    if read_back.as_dword() != Some(1) {
        return Err(rollback_msi(
            backend,
            sleeper,
            instance_id,
            &snapshot,
            record_path,
            codes::GPU_APPLY_FAILED,
        ));
    }
    Ok(())
}

/// 關閉 GPU MSI 模式（寫回套用前快照：absent → 刪值）+ 重啟 + 驗證。
/// 前置：呼叫端已取得 mutation 排他權、驗證 GPU 存在。
pub fn restore_msi_to_gpu(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    instance_id: &str,
    record_path: &Path,
) -> Result<(), String> {
    let snapshot =
        load_msi_record(record_path)?.ok_or_else(|| codes::GPU_RESTORE_FAILED.to_string())?;
    restore_msi_snapshot(backend, sleeper, instance_id, &snapshot)?;
    clear_msi_record(record_path)?;
    Ok(())
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
/// 遊戲量測（capture.rs）：PresentMon ETW attach，與其他 GPU 操作互斥
/// （`--stop_existing_session` 會毀掉執行中 benchmark 的 session）。
const OP_CAPTURE: u8 = 5;

/// 政策漂移偵測的最小間隔（毫秒）：搭 get_benchmark_state 既有輪詢節流，
/// 避免每秒重讀 HKLM registry。
const DRIFT_CHECK_INTERVAL_MS: u64 = 15_000;

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
    /// 漂移偵測上次檢查時間（毫秒 epoch；0 = 立即檢查）
    drift_checked_at: AtomicU64,
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
            drift_checked_at: AtomicU64::new(0),
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
    pub(crate) fn reserve_capture(&self) -> Result<GpuOperationGuard, String> {
        self.reserve(OP_CAPTURE)
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
        // 有 pending 日誌的啟動還原會把策略回滾到套用前 → 套用記錄失效
        let had_journal = recovery::recovery_path().exists();
        match attempt_startup_recovery(
            self.backend.as_ref(),
            self.sleeper.as_ref(),
            &recovery::recovery_path(),
        ) {
            Ok(()) => {
                log::info!("啟動還原：無 pending 或已還原");
                if had_journal {
                    let _ = std::fs::remove_file(applied_record_path());
                }
            }
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
        self.refresh_drift(&mut s);
        s
    }

    /// 政策漂移偵測（節流 [`DRIFT_CHECK_INTERVAL_MS`]，GPU 閒置才查）。
    /// 只在真正檢查時更新時間；快取供下一次前景輪詢使用。
    fn refresh_drift(&self, s: &mut BenchmarkState) {
        if self.reservation.load(Ordering::Acquire) != OP_IDLE {
            return; // mutation/benchmark 進行中不讀 registry（剛寫入的值會誤判）
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = self.drift_checked_at.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < DRIFT_CHECK_INTERVAL_MS {
            return;
        }
        let Ok(_guard) = self.reserve_mutation() else {
            return;
        };
        self.drift_checked_at.store(now, Ordering::Relaxed);
        s.policy_drift = Some(check_policy_drift_at(
            self.backend.as_ref(),
            &applied_record_path(),
        ));
        s.applied_core = load_applied_record(&applied_record_path())
            .ok()
            .flatten()
            .map(|r| r.core_id);
        if let Ok(mut cached) = self.state.write() {
            cached.policy_drift = s.policy_drift;
            cached.applied_core = s.applied_core;
        }
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

    /// 啟用 GPU MSI 模式（生產入口；guard 由 ipc 取得）。
    /// MSI 的失敗不設 recovery_required——該旗標屬於 affinity 管線的
    /// 啟動還原流程；MSI rollback 自帶還原與錯誤碼，不應永久封鎖套用。
    pub(crate) fn apply_msi_reserved(
        &self,
        _guard: GpuOperationGuard,
        instance_id: &str,
    ) -> Result<(), String> {
        let present = self
            .backend
            .enumerate_present_adapters()
            .map_err(|e| e.code().to_string())?
            .iter()
            .any(|d| d.instance_id.eq_ignore_ascii_case(instance_id));
        if !present {
            return Err(codes::GPU_NOT_FOUND.to_string());
        }
        apply_msi_to_gpu(
            self.backend.as_ref(),
            self.sleeper.as_ref(),
            instance_id,
            &msi_record_path(),
        )
        .map_err(|e| e.code)
    }

    /// 關閉 GPU MSI 模式（寫回套用前快照；生產入口）
    pub(crate) fn restore_msi_reserved(
        &self,
        _guard: GpuOperationGuard,
        instance_id: &str,
    ) -> Result<(), String> {
        let present = self
            .backend
            .enumerate_present_adapters()
            .map_err(|e| e.code().to_string())?
            .iter()
            .any(|d| d.instance_id.eq_ignore_ascii_case(instance_id));
        if !present {
            return Err(codes::GPU_NOT_FOUND.to_string());
        }
        restore_msi_to_gpu(
            self.backend.as_ref(),
            self.sleeper.as_ref(),
            instance_id,
            &msi_record_path(),
        )
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
        if result.is_ok() {
            // 已還原原始策略 → 套用記錄失效
            let _ = std::fs::remove_file(applied_record_path());
        }
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
        let validation_dir = std::env::temp_dir().join(format!("pacedock_equiv_{session_id}"));
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
pub(crate) fn resolve_assets(app: &AppHandle) -> Result<BenchmarkAssets, String> {
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
#[path = "manager_tests.rs"]
mod tests;
