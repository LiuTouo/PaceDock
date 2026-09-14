//! 系統環境健檢（唯讀）：電源計畫、PCIe ASPM、GameDVR、HAGS。
//! 只讀：僅使用 PowerRead* 與 RegGetValueW，絕不寫入任何系統狀態、
//! 不提供任何切換（toggle）面；建議文案由前端 i18n 依 id+status 呈現。
use serde::Serialize;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Power::{PowerGetActiveScheme, PowerReadACValueIndex};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD,
};

// 已知電源計畫 GUID（Windows 內建三方案）
const GUID_BALANCED: GUID = GUID::from_u128(0x381b4222_f694_41f0_9685_ff5bb260df2e);
const GUID_HIGH_PERFORMANCE: GUID = GUID::from_u128(0x8c5e7fda_e8bf_4a96_9a85_a6e23a8c635c);
const GUID_ULTIMATE: GUID = GUID::from_u128(0xe9a42b02_d5df_448d_aa00_03f14749eb61);
// PCI Express 電源子群組與 ASPM 設定 GUID
const GUID_SUB_PCIEXPRESS: GUID = GUID::from_u128(0x501a4d13_42af_4429_9fd1_a8218c268e20);
const GUID_PCIEXPRESS_ASPM: GUID = GUID::from_u128(0xee12f906_d277_404b_b6da_e5fa1a576df5);

/// 健檢結果狀態（PascalCase 序列化）
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum HealthStatus {
    Ok,
    Warn,
    Info,
    Unknown,
}

/// 單一健檢項目（detail 為機器可讀原始值；建議文案由前端 i18n 依 id+status 呈現）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheck {
    pub id: &'static str,
    pub status: HealthStatus,
    /// 機器可讀的原始值（如 "balanced"、"aspm=2"）；讀取失敗為空字串
    pub detail: String,
}

#[tauri::command]
pub async fn get_system_health() -> Result<Vec<HealthCheck>, String> {
    // 全為唯讀探針；任何一項讀取失敗 → 該項 Unknown，整批永不 Err
    tauri::async_runtime::spawn_blocking(collect_system_health)
        .await
        .map_err(|e| {
            log::error!("健檢 worker: {e}");
            "HEALTH_FAILED".to_string()
        })
}

fn collect_system_health() -> Vec<HealthCheck> {
    vec![
        check_power_plan(),
        check_pcie_aspm(),
        check_game_dvr(),
        check_hags(),
    ]
}

// ── 純判定函式（單元測試對象）─────────────────────────────────────────

/// 電源計畫：Balanced → Info（建議高效能/終極效能）；其他方案 → Ok；讀不到 → Unknown。
fn power_plan_status(scheme: Option<GUID>) -> (HealthStatus, String) {
    match scheme {
        Some(g) if g == GUID_BALANCED => (HealthStatus::Info, "balanced".into()),
        Some(g) if g == GUID_HIGH_PERFORMANCE || g == GUID_ULTIMATE => (HealthStatus::Ok, "high-performance".into()),
        Some(_) => (HealthStatus::Ok, "custom".into()),
        None => (HealthStatus::Unknown, String::new()),
    }
}

/// PCIe ASPM：0 = 關（Ok）；非 0 = 鏈路電源管理開啟（Warn：會增加延遲）；讀不到 → Unknown。
fn aspm_status(value: Option<u32>) -> (HealthStatus, String) {
    match value {
        Some(0) => (HealthStatus::Ok, "aspm=0".into()),
        Some(v) => (HealthStatus::Warn, format!("aspm={v}")),
        None => (HealthStatus::Unknown, String::new()),
    }
}

/// GameDVR：兩個登錄值皆為 0 → Ok；任一非 0 → Warn；皆讀不到 → Unknown（單邊讀到即判定）。
fn game_dvr_status(
    app_capture: Option<u32>,
    dvr_enabled: Option<u32>,
) -> (HealthStatus, String) {
    let on = app_capture.is_some_and(|v| v != 0) || dvr_enabled.is_some_and(|v| v != 0);
    let status = if on {
        HealthStatus::Warn
    } else {
        match (app_capture, dvr_enabled) {
            (None, None) => HealthStatus::Unknown,
            _ => HealthStatus::Ok,
        }
    };
    let detail = format!("appCapture={app_capture:?},gameDvr={dvr_enabled:?}");
    (status, detail)
}

/// HAGS：HwSchMode=2 → Info（中性資訊：GPU 棧改由 HAGS 排程）；其他/預設 → Ok。
fn hags_status(value: Option<u32>) -> (HealthStatus, String) {
    match value {
        Some(2) => (HealthStatus::Info, "hags=on".into()),
        Some(v) => (HealthStatus::Ok, format!("hags={v}")),
        None => (HealthStatus::Ok, "hags=default".into()),
    }
}

// ── 探針（Win32 唯讀）─────────────────────────────────────────────────

/// 目前作用中電源計畫 GUID（失敗 → None）
fn active_scheme() -> Option<GUID> {
    unsafe {
        let mut scheme: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut scheme) != ERROR_SUCCESS {
            return None;
        }
        scheme.as_ref().map(|g| *g)
    }
}

/// 讀作用中計畫的 AC 電源設定索引（失敗 → None）
fn ac_value_index(scheme: GUID, subgroup: GUID, setting: GUID) -> Option<u32> {
    unsafe {
        let mut index = 0u32;
        if PowerReadACValueIndex(None, Some(&scheme), Some(&subgroup), Some(&setting), &mut index)
            != ERROR_SUCCESS
        {
            return None;
        }
        Some(index)
    }
}

/// 讀 REG_DWORD（任一 hive）；失敗 → None
fn read_dword(root: HKEY, subkey: &str, name: &str) -> Option<u32> {
    let subkey_w: Vec<u16> = subkey.encode_utf16().chain(Some(0)).collect();
    let name_w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut value: u32 = 0;
    let mut cb = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(name_w.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as _),
            Some(&mut cb),
        )
    };
    (status == ERROR_SUCCESS).then_some(value)
}

fn check_power_plan() -> HealthCheck {
    let scheme = active_scheme();
    let (status, detail) = power_plan_status(scheme);
    HealthCheck { id: "powerPlan", status, detail }
}

fn check_pcie_aspm() -> HealthCheck {
    let value = active_scheme().and_then(|s| ac_value_index(s, GUID_SUB_PCIEXPRESS, GUID_PCIEXPRESS_ASPM));
    let (status, detail) = aspm_status(value);
    HealthCheck { id: "pcieAspm", status, detail }
}

fn check_game_dvr() -> HealthCheck {
    let app_capture = read_dword(
        HKEY_CURRENT_USER,
        r"Software\Microsoft\Windows\CurrentVersion\GameDVR",
        "AppCaptureEnabled",
    );
    let dvr_enabled = read_dword(
        HKEY_CURRENT_USER,
        r"System\GameConfigStore",
        "GameDVR_Enabled",
    );
    let (status, detail) = game_dvr_status(app_capture, dvr_enabled);
    HealthCheck { id: "gameDvr", status, detail }
}

fn check_hags() -> HealthCheck {
    let value = read_dword(
        HKEY_LOCAL_MACHINE,
        r"SYSTEM\CurrentControlSet\Control\GraphicsDrivers",
        "HwSchMode",
    );
    let (status, detail) = hags_status(value);
    HealthCheck { id: "hags", status, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_plan_balanced_is_info_others_ok() {
        let (st, d) = power_plan_status(Some(GUID_BALANCED));
        assert_eq!(st, HealthStatus::Info);
        assert_eq!(d, "balanced");
        let (st, _) = power_plan_status(Some(GUID_HIGH_PERFORMANCE));
        assert_eq!(st, HealthStatus::Ok);
        let (st, d) = power_plan_status(Some(GUID::from_u128(0)));
        assert_eq!(st, HealthStatus::Ok);
        assert_eq!(d, "custom");
        let (st, _) = power_plan_status(None);
        assert_eq!(st, HealthStatus::Unknown);
    }

    #[test]
    fn aspm_nonzero_warns() {
        assert_eq!(aspm_status(Some(0)).0, HealthStatus::Ok);
        assert_eq!(aspm_status(Some(1)).0, HealthStatus::Warn);
        assert_eq!(aspm_status(Some(2)).0, HealthStatus::Warn);
        assert_eq!(aspm_status(None).0, HealthStatus::Unknown);
    }

    #[test]
    fn game_dvr_any_nonzero_warns() {
        let (st, _) = game_dvr_status(Some(0), Some(0));
        assert_eq!(st, HealthStatus::Ok);
        let (st, _) = game_dvr_status(Some(1), Some(0));
        assert_eq!(st, HealthStatus::Warn);
        let (st, _) = game_dvr_status(Some(0), Some(1));
        assert_eq!(st, HealthStatus::Warn);
        let (st, _) = game_dvr_status(None, None);
        assert_eq!(st, HealthStatus::Unknown);
        // 單邊讀到 0 → Ok（該邊視為已確認關閉）
        let (st, _) = game_dvr_status(Some(0), None);
        assert_eq!(st, HealthStatus::Ok);
    }

    #[test]
    fn hags_only_on_is_info() {
        assert_eq!(hags_status(Some(2)).0, HealthStatus::Info);
        assert_eq!(hags_status(Some(1)).0, HealthStatus::Ok);
        assert_eq!(hags_status(None).0, HealthStatus::Ok);
    }
}
