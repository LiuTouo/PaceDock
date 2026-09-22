//! 電源微調（可寫）：USB 選擇性暫停與 PCIe ASPM（AC/DC 雙值）。
//! 與 health.rs 對偶：健檢（health.rs）維持唯讀，本模組負責套用/還原。
//! 讀取輔助重用 health.rs 的 pub(crate) 探針；寫入與提交為本模組職責。
//! 還原記錄仿 MSI 模式：單一 HMAC 記錄檔，刻意不接 recovery journal —
//! 單值電源寫入冪等且即時生效，crash 最壞情況是「已套用且記錄仍在」→ 仍可還原。
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::State;
use windows::core::GUID;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Power::{
    PowerSetActiveScheme, PowerWriteACValueIndex, PowerWriteDCValueIndex,
};

use crate::error::codes;
use crate::AppState;

// ── 電源計畫 GUID（與 health.rs 同值，各自檔內定義以保持模組封閉）────────
// USB 設定子群組 / USB 選擇性暫停設定
const GUID_SUB_USB: GUID = GUID::from_u128(0x2a737441_1930_4402_8d77_b2bebba308a3);
const GUID_USB_SELECTIVE_SUSPEND: GUID = GUID::from_u128(0x48e6b7a6_50f5_4782_a5d4_53bb8f07e226);
// PCI Express 子群組 / 連結狀態電源管理（ASPM）設定
const GUID_SUB_PCIEXPRESS: GUID = GUID::from_u128(0x501a4d13_42af_4429_9fd1_a8218c268e20);
const GUID_PCIEXPRESS_ASPM: GUID = GUID::from_u128(0xee12f906_d277_404b_b6da_e5fa1a576df5);

/// 微調種類（PascalCase 序列化對應前端 'Usb' | 'Aspm'）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum PowerTweakKind {
    Usb,
    Aspm,
}

impl PowerTweakKind {
    /// 回 (子群組, 設定)
    fn guids(self) -> (GUID, GUID) {
        match self {
            Self::Usb => (GUID_SUB_USB, GUID_USB_SELECTIVE_SUSPEND),
            Self::Aspm => (GUID_SUB_PCIEXPRESS, GUID_PCIEXPRESS_ASPM),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Usb => "USB 選擇性暫停",
            Self::Aspm => "PCIe ASPM",
        }
    }
}

/// 微調狀態（get_power_tweaks 回傳）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerTweaksStatus {
    pub usb_ac: Option<u32>,
    pub usb_dc: Option<u32>,
    pub pcie_aspm: Option<u32>,
    /// 此前由 PaceDock 套用過、目前記錄仍在（可還原）
    pub usb_restorable: bool,
    pub aspm_restorable: bool,
}

/// 還原記錄：`%APPDATA%\PaceDock\power-tweaks-restore.json`（HMAC 認證）。
/// 內容為套用前的 [AC, DC] 原值（None = 套用前讀不到，還原時跳過該邊）；
/// 兩項都還原後清檔。
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PowerTweaksRecord {
    usb: Option<[Option<u32>; 2]>,
    aspm: Option<[Option<u32>; 2]>,
}

// ── 純函式（單元測試對象）────────────────────────────────────────────────

/// 目標索引：0 = 停用（USB 選擇性暫停「已停用」/ ASPM「關閉」皆為 0）
fn target_index_for_disable() -> u32 {
    0
}

/// 已達標 → 不需寫（套用冪等）
fn needs_write(current: Option<u32>, target: u32) -> bool {
    current != Some(target)
}

/// 首次套用才記錄原值：記錄已存在（重複套用）→ 保留最早原值
fn merge_record(rec: &mut PowerTweaksRecord, kind: PowerTweakKind, before: [Option<u32>; 2]) {
    match kind {
        PowerTweakKind::Usb => {
            if rec.usb.is_none() {
                rec.usb = Some(before);
            }
        }
        PowerTweakKind::Aspm => {
            if rec.aspm.is_none() {
                rec.aspm = Some(before);
            }
        }
    }
}

/// 取出待還原原值；記錄缺該項 → None（呼叫端回 POWER_RESTORE_FAILED）
fn take_from_record(rec: &mut PowerTweaksRecord, kind: PowerTweakKind) -> Option<[Option<u32>; 2]> {
    match kind {
        PowerTweakKind::Usb => rec.usb.take(),
        PowerTweakKind::Aspm => rec.aspm.take(),
    }
}

// ── 記錄檔 IO（HMAC fail-closed，仿 manager.rs MSI 記錄）────────────────

fn record_path() -> PathBuf {
    crate::config::config_dir().join("power-tweaks-restore.json")
}

fn load_record(path: &Path) -> Result<Option<PowerTweaksRecord>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = crate::state_auth::auth_read(path)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("電源微調還原記錄解析失敗: {e}"))
}

fn save_record(path: &Path, rec: &PowerTweaksRecord) -> Result<(), String> {
    let text = serde_json::to_string_pretty(rec).map_err(|e| format!("序列化: {e}"))?;
    crate::state_auth::auth_write(path, &text)
}

fn clear_record(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("清除電源微調還原記錄失敗: {e}")),
    }
}

// ── Win32 寫入與提交（讀取重用 health.rs 探針）──────────────────────────

/// 寫入單一電源設定索引（ac=true → AC 值，否則 DC 值）
fn write_index(
    scheme: GUID,
    subgroup: GUID,
    setting: GUID,
    ac: bool,
    index: u32,
) -> Result<(), String> {
    // windows crate 不對稱：AC 版回 WIN32_ERROR、DC 版回 u32 — 統一成 raw code 比較
    let result = unsafe {
        if ac {
            PowerWriteACValueIndex(None, &scheme, Some(&subgroup), Some(&setting), index).0
        } else {
            PowerWriteDCValueIndex(None, &scheme, Some(&subgroup), Some(&setting), index)
        }
    };
    if result == ERROR_SUCCESS.0 {
        Ok(())
    } else {
        log::error!(
            "PowerWrite{}ValueIndex 失敗: {result:?}",
            if ac { "AC" } else { "DC" }
        );
        Err(codes::POWER_APPLY_FAILED.to_string())
    }
}

/// 對目前計畫重發 PowerSetActiveScheme，讓剛寫入的索引即時生效（免重開機）
fn commit(scheme: GUID) -> Result<(), String> {
    let result = unsafe { PowerSetActiveScheme(None, Some(&scheme)) };
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        log::error!("PowerSetActiveScheme 失敗: {result:?}");
        Err(codes::POWER_APPLY_FAILED.to_string())
    }
}

// ── 套用 / 還原 ─────────────────────────────────────────────────────────

/// 套用微調：記錄原值 → 寫 0（AC+DC）→ commit → 回讀驗證。
/// 已達標 → no-op 成功（冪等，不重複寫記錄）。
fn apply_tweak(kind: PowerTweakKind, path: &Path) -> Result<(), String> {
    let (sub, setting) = kind.guids();
    let scheme =
        crate::health::active_scheme().ok_or_else(|| codes::POWER_APPLY_FAILED.to_string())?;
    let before = [
        crate::health::ac_dc_value_index(scheme, sub, setting, true),
        crate::health::ac_dc_value_index(scheme, sub, setting, false),
    ];
    if !needs_write(before[0], target_index_for_disable())
        && !needs_write(before[1], target_index_for_disable())
    {
        return Ok(());
    }
    // 先寫還原記錄（HMAC），再動電源設定；記錄已存在 → 保留最早原值
    let mut rec = load_record(path)?.unwrap_or_default();
    merge_record(&mut rec, kind, before);
    save_record(path, &rec).map_err(|_| codes::POWER_APPLY_FAILED.to_string())?;
    write_index(scheme, sub, setting, true, 0)?;
    write_index(scheme, sub, setting, false, 0)?;
    commit(scheme)?;
    // 回讀驗證；未生效 → 嘗試還原後回報失敗
    if let Err(detail) = verify_disabled(scheme, sub, setting) {
        log::error!("{}套用後驗證失敗（{detail}），嘗試還原", kind.label());
        let _ = restore_tweak(kind, path);
        return Err(codes::POWER_APPLY_FAILED.to_string());
    }
    log::info!("{}已停用（套用前 {:?}）", kind.label(), before);
    Ok(())
}

/// 回讀驗證 AC/DC 皆為 0
fn verify_disabled(scheme: GUID, sub: GUID, setting: GUID) -> Result<(), String> {
    let ac = crate::health::ac_dc_value_index(scheme, sub, setting, true);
    let dc = crate::health::ac_dc_value_index(scheme, sub, setting, false);
    if ac == Some(0) && dc == Some(0) {
        Ok(())
    } else {
        Err(format!("ac={ac:?},dc={dc:?}"))
    }
}

/// 還原微調：寫回記錄中的原值 → commit；兩項都還原 → 清記錄。
/// 記錄不存在或缺該項 → POWER_RESTORE_FAILED（無原值可回）。
fn restore_tweak(kind: PowerTweakKind, path: &Path) -> Result<(), String> {
    let (sub, setting) = kind.guids();
    let mut rec = load_record(path)?.ok_or_else(|| codes::POWER_RESTORE_FAILED.to_string())?;
    let before =
        take_from_record(&mut rec, kind).ok_or_else(|| codes::POWER_RESTORE_FAILED.to_string())?;
    let scheme =
        crate::health::active_scheme().ok_or_else(|| codes::POWER_RESTORE_FAILED.to_string())?;
    // 只寫回套用前讀得到的那幾邊（None = 套用前即讀不到，跳過）
    if let Some(ac) = before[0] {
        write_index(scheme, sub, setting, true, ac)
            .map_err(|_| codes::POWER_RESTORE_FAILED.to_string())?;
    }
    if let Some(dc) = before[1] {
        write_index(scheme, sub, setting, false, dc)
            .map_err(|_| codes::POWER_RESTORE_FAILED.to_string())?;
    }
    commit(scheme).map_err(|_| codes::POWER_RESTORE_FAILED.to_string())?;
    log::info!("{}已還原為 {:?}", kind.label(), before);
    if rec.usb.is_none() && rec.aspm.is_none() {
        clear_record(path)?;
    } else {
        save_record(path, &rec).map_err(|_| codes::POWER_RESTORE_FAILED.to_string())?;
    }
    Ok(())
}

// ── IPC commands ────────────────────────────────────────────────────────

/// 僅比對有套用記錄的項目；AC/DC 都檢查，讀取失敗保留未知狀態。
pub(crate) fn monitored_power_settings() -> Vec<(crate::drift::Setting, Option<bool>)> {
    use crate::drift::Setting;
    let path = record_path();
    let record = path
        .try_exists()
        .map_err(|e| e.to_string())
        .and_then(|_| load_record(&path));
    let scheme = crate::health::active_scheme();
    [
        (PowerTweakKind::Usb, Setting::Usb),
        (PowerTweakKind::Aspm, Setting::Aspm),
    ]
    .into_iter()
    .map(|(kind, setting)| {
        let drift = match &record {
            Err(_) => None,
            Ok(record) => {
                let managed = record.as_ref().is_some_and(|r| match kind {
                    PowerTweakKind::Usb => r.usb.is_some(),
                    PowerTweakKind::Aspm => r.aspm.is_some(),
                });
                if !managed {
                    Some(false)
                } else {
                    let (sub, key) = kind.guids();
                    let ac =
                        scheme.and_then(|s| crate::health::ac_dc_value_index(s, sub, key, true));
                    let dc =
                        scheme.and_then(|s| crate::health::ac_dc_value_index(s, sub, key, false));
                    crate::drift::disabled_power_drift(ac, dc)
                }
            }
        };
        (setting, drift)
    })
    .collect()
}

/// 查詢 USB 選擇性暫停與 PCIe ASPM 目前值（唯讀；不需排他權）。
/// 值讀不到 → 該欄位 null；還原記錄被篡改 → Err（fail-closed）。
#[tauri::command]
pub async fn get_power_tweaks() -> Result<PowerTweaksStatus, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let scheme = crate::health::active_scheme();
        let (usb_ac, usb_dc) = match scheme {
            Some(s) => (
                crate::health::ac_dc_value_index(s, GUID_SUB_USB, GUID_USB_SELECTIVE_SUSPEND, true),
                crate::health::ac_dc_value_index(
                    s,
                    GUID_SUB_USB,
                    GUID_USB_SELECTIVE_SUSPEND,
                    false,
                ),
            ),
            None => (None, None),
        };
        let pcie_aspm = scheme.and_then(|s| {
            crate::health::ac_dc_value_index(s, GUID_SUB_PCIEXPRESS, GUID_PCIEXPRESS_ASPM, true)
        });
        let rec = load_record(&record_path())?.unwrap_or_default();
        Ok(PowerTweaksStatus {
            usb_ac,
            usb_dc,
            pcie_aspm,
            usb_restorable: rec.usb.is_some(),
            aspm_restorable: rec.aspm.is_some(),
        })
    })
    .await
    .map_err(|e| {
        log::error!("電源微調狀態 worker: {e}");
        codes::POWER_APPLY_FAILED.to_string()
    })?
}

/// 套用微調（Usb → 停用 USB 選擇性暫停；Aspm → 關閉 PCIe ASPM）。
/// 共用 GPU 操作排他鎖（與 benchmark/MSI 同一 CAS）；衝突回 BENCHMARK_ALREADY_RUNNING。
#[tauri::command]
pub async fn apply_power_tweak(
    state: State<'_, Arc<AppState>>,
    kind: PowerTweakKind,
) -> Result<(), String> {
    let manager = state.benchmark.clone();
    let guard = manager.reserve_mutation()?;
    tauri::async_runtime::spawn_blocking(move || {
        let _op = guard; // 持有 mutation 排他權至操作結束
        apply_tweak(kind, &record_path())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 還原微調（寫回套用前原值）
#[tauri::command]
pub async fn restore_power_tweak(
    state: State<'_, Arc<AppState>>,
    kind: PowerTweakKind,
) -> Result<(), String> {
    let manager = state.benchmark.clone();
    let guard = manager.reserve_mutation()?;
    tauri::async_runtime::spawn_blocking(move || {
        let _op = guard;
        restore_tweak(kind, &record_path())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ── 測試（純邏輯；Power API 呼叫需真機驗證）────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_write_is_idempotent() {
        assert!(!needs_write(Some(0), 0)); // 已達標
        assert!(needs_write(Some(1), 0));
        assert!(needs_write(Some(2), 0));
        assert!(needs_write(None, 0)); // 讀不到 → 保守起見重寫
    }

    #[test]
    fn record_merge_keeps_first_original() {
        let mut rec = PowerTweaksRecord::default();
        merge_record(&mut rec, PowerTweakKind::Usb, [Some(1), Some(1)]);
        merge_record(&mut rec, PowerTweakKind::Usb, [Some(0), Some(0)]); // 重複套用不覆蓋
        assert_eq!(rec.usb, Some([Some(1), Some(1)]));
        assert_eq!(rec.aspm, None);
    }

    #[test]
    fn record_take_and_clear_semantics() {
        let mut rec = PowerTweaksRecord {
            usb: Some([Some(1), Some(0)]),
            aspm: Some([Some(2), None]), // None = 套用前讀不到,還原時跳過該邊
        };
        assert_eq!(
            take_from_record(&mut rec, PowerTweakKind::Usb),
            Some([Some(1), Some(0)])
        );
        assert_eq!(take_from_record(&mut rec, PowerTweakKind::Usb), None); // 已取出
        assert_eq!(
            take_from_record(&mut rec, PowerTweakKind::Aspm),
            Some([Some(2), None])
        );
        // 兩項都取出 → 呼叫端應清檔
        assert!(rec.usb.is_none() && rec.aspm.is_none());
    }

    #[test]
    fn record_roundtrip_and_fail_closed() {
        let path =
            std::env::temp_dir().join(format!("pacedock-power-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // 無記錄 → None
        assert_eq!(load_record(&path).unwrap(), None);
        // 寫 → 讀 round-trip(含 None 邊)
        let rec = PowerTweaksRecord {
            usb: Some([Some(0), None]),
            aspm: None,
        };
        save_record(&path, &rec).unwrap();
        assert_eq!(load_record(&path).unwrap(), Some(rec));
        // 清除冪等
        clear_record(&path).unwrap();
        clear_record(&path).unwrap();
        assert_eq!(load_record(&path).unwrap(), None);
        // 篡改（無 HMAC）→ Err fail-closed
        std::fs::write(&path, r#"{ "usb": [0, 0] }"#).unwrap();
        assert!(load_record(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
