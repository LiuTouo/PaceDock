//! 崩潰還原日誌：`%APPDATA%\PaceDock\benchmark-recovery.json`。
//!
//! 協定：
//! - 第一次變更之前先 `begin`（stage=SnapshotTaken）寫入快照。
//! - 每次實質變更（寫策略 / 重啟裝置）後 `advance_to` 更新 stage。
//! - 只有「還原驗證成功」或「新狀態驗證完成且快照已持久化」後才 `clear`。
//! - 啟動時若存在日誌 → 依 stage 嘗試還原；失敗則由 manager 標記
//!   RecoveryRequired，封鎖新的 test/apply。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::gpu::AffinityPolicy;

/// 進行中變更的進度
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryStage {
    /// 已快照，尚未改寫任何策略（還原時只需驗證，不需動裝置）
    SnapshotTaken,
    /// 已寫入新策略，尚未重啟裝置
    PolicyApplied,
    /// 已重啟裝置，尚未驗證
    DeviceRestarted,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryJournal {
    pub instance_id: String,
    pub stage: RecoveryStage,
    pub created_at: String,
    pub updated_at: String,
    /// 變更前的精確策略快照（還原目標）
    pub snapshot: AffinityPolicy,
}

pub fn recovery_path() -> std::path::PathBuf {
    config::config_dir().join("benchmark-recovery.json")
}

pub fn begin_at(path: &Path, snapshot: &AffinityPolicy) -> Result<(), String> {
    let now = chrono::Local::now().to_rfc3339();
    let journal = RecoveryJournal {
        instance_id: snapshot.instance_id.clone(),
        stage: RecoveryStage::SnapshotTaken,
        created_at: now.clone(),
        updated_at: now,
        snapshot: snapshot.clone(),
    };
    save_at(path, &journal)
}

pub fn advance_to_at(
    path: &Path,
    journal: &RecoveryJournal,
    stage: RecoveryStage,
) -> Result<(), String> {
    #[cfg(test)]
    if inject::consume_advance() {
        return Err("injected advance failure".to_string());
    }
    let mut next = journal.clone();
    next.stage = stage;
    next.updated_at = chrono::Local::now().to_rfc3339();
    save_at(path, &next)
}

/// 清除日誌：只在還原驗證成功後呼叫。不存在視為成功（已清除）。
pub fn clear_at(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if inject::consume_clear() {
        return Err("injected clear failure".to_string());
    }
    match std::fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("清除還原日誌失敗: {e}")),
    }
}

pub fn load_from(path: &Path) -> Result<Option<RecoveryJournal>, String> {
    if !path.exists() {
        return Ok(None);
    }
    // 日誌位於可寫 APPDATA,卻驅動提升權限還原動作 — 內容必須通過 HMAC 認證
    let text = crate::state_auth::auth_read(path)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("還原日誌解析失敗: {e}"))
}

/// 寫入一份「要求完整 restore」的日誌（stage=PolicyApplied），供 rollback 失敗時
/// 確保 startup recovery 走 restore_snapshot（寫回 + 重啟 + 驗證）而非只驗證。
pub fn mark_restore_needed_at(path: &Path, snapshot: &AffinityPolicy) -> Result<(), String> {
    let now = chrono::Local::now().to_rfc3339();
    let journal = RecoveryJournal {
        instance_id: snapshot.instance_id.clone(),
        stage: RecoveryStage::PolicyApplied,
        created_at: now.clone(),
        updated_at: now,
        snapshot: snapshot.clone(),
    };
    save_at(path, &journal)
}

fn save_at(path: &Path, journal: &RecoveryJournal) -> Result<(), String> {
    let text = serde_json::to_string_pretty(journal).map_err(|e| format!("序列化: {e}"))?;
    crate::state_auth::auth_write(path, &text)
}

/// 測試用 fault injection（僅 `#[cfg(test)]`；production 編譯不含）。
/// 採 thread-local，避免測試平行執行時彼此干擾。
#[cfg(test)]
pub mod inject {
    use std::cell::Cell;

    thread_local! {
        static FAIL_ADVANCE: Cell<bool> = const { Cell::new(false) };
        static FAIL_CLEAR: Cell<bool> = const { Cell::new(false) };
    }

    /// 讓下一次 `advance_to_at` 失敗
    pub fn fail_next_advance() {
        FAIL_ADVANCE.with(|c| c.set(true));
    }
    /// 讓下一次 `clear_at` 失敗
    pub fn fail_next_clear() {
        FAIL_CLEAR.with(|c| c.set(true));
    }
    pub(super) fn consume_advance() -> bool {
        FAIL_ADVANCE.with(|c| c.replace(false))
    }
    pub(super) fn consume_clear() -> bool {
        FAIL_CLEAR.with(|c| c.replace(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{AffinityPolicy, RegistryValueSnapshot};
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        dir.join(format!(
            "pacedock_recovery_test_{}_{}.json",
            std::process::id(),
            name
        ))
    }

    fn snapshot(instance: &str) -> AffinityPolicy {
        AffinityPolicy {
            instance_id: instance.to_string(),
            device_policy: RegistryValueSnapshot::dword(2),
            assignment_set_override: RegistryValueSnapshot::dword(0b1000),
        }
    }

    #[test]
    fn lifecycle_begin_advance_clear() {
        let path = temp_path("lifecycle");
        let _ = std::fs::remove_file(&path);

        // 無日誌 → None
        assert!(load_from(&path).unwrap().is_none());

        begin_at(&path, &snapshot("PCI\\DEV1")).unwrap();
        let j = load_from(&path).unwrap().unwrap();
        assert_eq!(j.stage, RecoveryStage::SnapshotTaken);
        assert_eq!(j.snapshot, snapshot("PCI\\DEV1"));

        advance_to_at(&path, &j, RecoveryStage::PolicyApplied).unwrap();
        let j2 = load_from(&path).unwrap().unwrap();
        assert_eq!(j2.stage, RecoveryStage::PolicyApplied);

        clear_at(&path).unwrap();
        assert!(load_from(&path).unwrap().is_none());

        // 清除不存在的日誌 → Ok
        clear_at(&path).unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn journal_roundtrip_preserves_snapshot_bytes() {
        let path = temp_path("bytes");
        let _ = std::fs::remove_file(&path);
        let snap = snapshot("PCI\\DEV2");
        begin_at(&path, &snap).unwrap();
        let j = load_from(&path).unwrap().unwrap();
        assert_eq!(j.snapshot.device_policy.bytes, snap.device_policy.bytes);
        assert_eq!(j.snapshot.assignment_set_override.as_dword(), Some(0b1000));
        assert_eq!(j.instance_id, "PCI\\DEV2");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_journal_is_error_not_silent() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_from(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    /// 其他 read I/O error（目錄）→ Err，不是當作「無日誌」的 None。
    #[test]
    fn load_from_directory_is_error_not_none() {
        let dir =
            std::env::temp_dir().join(format!("pacedock_recovery_dir_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_from(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
