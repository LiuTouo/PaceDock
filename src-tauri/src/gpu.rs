//! GPU 顯示裝置控制器（Task 1 基礎建設）。
//!
//! 提供三類能力，全部封在 `GpuBackend` trait 之後，單元測試用 fake backend，
//! 絕不碰真實 HKLM 或真實裝置：
//! - SetupAPI 列舉目前使用的顯示配接器（present），回傳穩定 PnP instance id 與名稱。
//! - 讀寫
//!   `HKLM\SYSTEM\CurrentControlSet\Enum\<instance>\Device Parameters\Interrupt Management\Affinity Policy`
//!   的 DevicePolicy / AssignmentSetOverride，快照精確的 presence + 型別 + 原始位元組。
//! - 用 SetupAPI property change 對指定顯示裝置做 disable/enable 重啟。
//!
//! 刻意只用 `CurrentControlSet`（不用 `ControlSet001`），路徑與 system 一致。

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiCallClassInstaller, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiSetClassInstallParamsW, DICS_DISABLE, DICS_ENABLE, DICS_FLAG_GLOBAL, DIF_PROPERTYCHANGE,
    DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY, HDEVINFO, SETUP_DI_REGISTRY_PROPERTY,
    SETUP_DI_STATE_CHANGE, SPDRP_DEVICEDESC, SPDRP_FRIENDLYNAME, SP_CLASSINSTALL_HEADER,
    SP_DEVINFO_DATA, SP_PROPCHANGE_PARAMS,
};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_LOCAL_MACHINE, KEY_CREATE_SUB_KEY, KEY_READ, KEY_SET_VALUE, REG_BINARY,
    REG_CREATE_KEY_DISPOSITION, REG_DWORD, REG_OPENED_EXISTING_KEY, REG_OPTION_NON_VOLATILE,
    REG_VALUE_TYPE,
};

use crate::error::codes;

/// InterruptPolicy_DevicePolicy_SingleProcessor：親和性鎖定單一 LP
pub const DEVICE_POLICY_SINGLE_PROCESSOR: u32 = 4;

/// 顯示配接器（由 SetupAPI present 列舉取得）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GpuDevice {
    /// 穩定 PnP 身分，如 `PCI\VEN_10DE&DEV_2684&SUBSYS_...&REV_A1\...`
    pub instance_id: String,
    /// 親和名稱（FriendlyName，取不到退回 DeviceDesc）
    pub friendly_name: String,
}

/// 單一註冊表值的精確快照：presence + 型別字串 + 原始位元組。
/// 還原時逐位元組寫回（含型別），缺失的值用刪除還原。
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegistryValueSnapshot {
    #[serde(default)]
    pub present: bool,
    /// 原生 REG_VALUE_TYPE 數值（REG_DWORD=4、REG_BINARY=3 …），
    /// 逐型別無損還原，不把未知型別退化成 DWORD
    #[serde(default)]
    pub value_type: Option<u32>,
    /// 原始位元組（REG_DWORD 即 little-endian 4 bytes）
    #[serde(default)]
    pub bytes: Option<Vec<u8>>,
}

impl RegistryValueSnapshot {
    /// 建一個存在的 DWORD 值快照
    pub fn dword(value: u32) -> Self {
        Self {
            present: true,
            value_type: Some(REG_DWORD.0),
            bytes: Some(value.to_le_bytes().to_vec()),
        }
    }

    /// 建一個存在的 REG_BINARY 值快照（如 64-bit 單 LP mask 的精簡 LE bytes）
    pub fn binary(bytes: Vec<u8>) -> Self {
        Self {
            present: true,
            value_type: Some(REG_BINARY.0),
            bytes: Some(bytes),
        }
    }

    /// 解成 u32（僅當型別為 DWORD 且位元組數正確）
    pub fn as_dword(&self) -> Option<u32> {
        if self.value_type != Some(REG_DWORD.0) {
            return None;
        }
        let b = self.bytes.as_deref()?;
        (b.len() == 4).then(|| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// 64-bit 單 LP mask 的精簡 little-endian 位元組（尾端零位元組移除），
/// 與 AutoGpuAffinity 的 AssignmentSetOverride 表示一致：
/// LP 0 → [01]、LP 31 → 4 bytes、LP 32 → 5 bytes、LP 63 → 8 bytes。
pub fn single_lp_mask_bytes(lp: u32) -> Vec<u8> {
    debug_assert!(lp < 64);
    let le = (1u64 << lp).to_le_bytes();
    let mut len = le.len();
    while len > 0 && le[len - 1] == 0 {
        len -= 1;
    }
    le[..len].to_vec()
}

/// 驗證即將寫入/還原的策略快照語意（型別/長度上限）。
/// 來源可能是磁碟上可被竄改的 JSON；任何 registry write 前必須通過此檢查：
/// - instance_id 非空且有長度上限
/// - DevicePolicy 存在時必為 REG_DWORD（4 bytes little-endian）
/// - AssignmentSetOverride 存在時必為 REG_BINARY 且 ≤ 8 bytes（64-bit mask）
pub fn validate_policy_snapshot(policy: &AffinityPolicy) -> Result<(), String> {
    const MAX_INSTANCE_ID_LEN: usize = 256;
    const REG_BINARY_TYPE: u32 = 3;
    const MAX_OVERRIDE_BYTES: usize = 8;

    if policy.instance_id.is_empty() || policy.instance_id.len() > MAX_INSTANCE_ID_LEN {
        return Err("策略快照的 instance_id 為空或超出長度上限".to_string());
    }
    if policy.device_policy.present {
        if policy.device_policy.value_type != Some(REG_DWORD.0) {
            return Err("DevicePolicy 型別非法（必為 REG_DWORD）".to_string());
        }
        if policy.device_policy.bytes.as_ref().map(|b| b.len()) != Some(4) {
            return Err("DevicePolicy 位元組長度非法（必為 4 bytes）".to_string());
        }
    }
    if policy.assignment_set_override.present {
        if policy.assignment_set_override.value_type != Some(REG_BINARY_TYPE) {
            return Err("AssignmentSetOverride 型別非法（必為 REG_BINARY）".to_string());
        }
        match policy
            .assignment_set_override
            .bytes
            .as_ref()
            .map(|b| b.len())
        {
            Some(len) if (1..=MAX_OVERRIDE_BYTES).contains(&len) => {}
            _ => {
                return Err(format!(
                    "AssignmentSetOverride 位元組長度非法（1..={MAX_OVERRIDE_BYTES}）"
                ))
            }
        }
    }
    Ok(())
}

/// 中斷親和性策略（DevicePolicy + AssignmentSetOverride 的成對快照）。
/// 同時作為「讀取結果」「寫入輸入」「還原輸入」三種用途。
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AffinityPolicy {
    pub instance_id: String,
    #[serde(default)]
    pub device_policy: RegistryValueSnapshot,
    #[serde(default)]
    pub assignment_set_override: RegistryValueSnapshot,
}

/// GPU 控制錯誤。對前端一律回傳穩定代碼（查 i18n）。
#[derive(thiserror::Error, Debug)]
pub enum GpuError {
    #[error("display enumeration failed: {0}")]
    Enumerate(String),
    #[error("device not found: {0}")]
    NotFound(String),
    #[error("registry operation failed: {0}")]
    Registry(String),
    #[error("device restart failed: {0}")]
    Restart(String),
    #[error("basic display service is disabled")]
    BasicDisplayDisabled,
}

impl GpuError {
    pub fn code(&self) -> &'static str {
        match self {
            GpuError::Enumerate(_) => codes::GPU_ENUM_FAILED,
            GpuError::NotFound(_) => codes::GPU_NOT_FOUND,
            GpuError::Registry(_) => codes::GPU_REGISTRY_FAILED,
            GpuError::Restart(_) => codes::GPU_RESTART_FAILED,
            GpuError::BasicDisplayDisabled => codes::GPU_BASIC_DISPLAY_DISABLED,
        }
    }
}

/// 裝置/註冊表操作的注入邊界。測試用 fake 實作，生產用 `RealGpuBackend`。
pub trait GpuBackend: Send + Sync {
    /// 列舉目前使用的顯示配接器（SetupAPI DIGCF_PRESENT）
    fn enumerate_present_adapters(&self) -> Result<Vec<GpuDevice>, GpuError>;

    /// 讀取目前中斷親和性策略（含缺失狀態與原始位元組）
    fn read_affinity_policy(&self, instance_id: &str) -> Result<AffinityPolicy, GpuError>;

    /// 依快照寫入：present → 寫回原始型別+位元組；absent → 刪除該值
    fn write_affinity_policy(&self, policy: &AffinityPolicy) -> Result<(), GpuError>;

    /// disable→enable 重啟指定顯示裝置。disable 成功後必然嘗試 enable；
    /// 兩個階段之間的等待由注入的 sleeper 控制（見 `restart_sequence`）。
    fn restart_device(&self, instance_id: &str, sleeper: &dyn Sleep) -> Result<(), GpuError>;

    /// BasicDisplay 服務 Start 值 != 4（未停用）
    fn basic_display_enabled(&self) -> Result<bool, GpuError>;

    /// 讀取 GPU 的 `MSISupported` 值（absent → default snapshot）。
    /// 帶預設實作：不支援的 backend（如測試 fake 未更新）回 Err。
    fn read_msi_supported(&self, _instance_id: &str) -> Result<RegistryValueSnapshot, GpuError> {
        Err(GpuError::Registry("MSI 讀取不支援此 backend".into()))
    }

    /// 寫入/刪除 `MSISupported`（absent snapshot → 刪值）。帶預設實作。
    fn write_msi_supported(
        &self,
        _instance_id: &str,
        _value: &RegistryValueSnapshot,
    ) -> Result<(), GpuError> {
        Err(GpuError::Registry("MSI 寫入不支援此 backend".into()))
    }
}

// ── 重啟等待策略 ─────────────────────────────────────────────────────────

/// disable 後、enable 前的間隔（毫秒）：讓中斷重新分配安定
pub const RESTART_GAP_BEFORE_ENABLE_MS: u64 = 2000;
/// enable 後的安定時間（毫秒）
pub const RESTART_SETTLE_AFTER_ENABLE_MS: u64 = 2000;

/// 等待策略。生產用 `RealSleeper` 真的睡；測試注入 no-op，不真的睡。
pub trait Sleep: Send + Sync {
    fn sleep(&self, ms: u64);
}

/// 生產用：真的睡
pub struct RealSleeper;

impl Sleep for RealSleeper {
    fn sleep(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// 測試用：不真的睡（重啟時序由 `restart_sequence` 單獨測試）
#[cfg(test)]
pub struct NoopSleeper;

#[cfg(test)]
impl Sleep for NoopSleeper {
    fn sleep(&self, _ms: u64) {}
}

/// 重啟序列：disable → 停頓 → enable → 停頓。disable 失敗不嘗試 enable。
/// `ops` 是同一裝置 handle 上的 disable/enable 操作閉包，時序因此可獨立測試。
fn restart_sequence(
    ops: impl Fn(SETUP_DI_STATE_CHANGE) -> Result<(), GpuError>,
    sleeper: &dyn Sleep,
) -> Result<(), GpuError> {
    ops(DICS_DISABLE)?;
    sleeper.sleep(RESTART_GAP_BEFORE_ENABLE_MS);
    ops(DICS_ENABLE)?;
    sleeper.sleep(RESTART_SETTLE_AFTER_ENABLE_MS);
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────

/// 生產實作：直接操作 SetupAPI 與 HKLM。
pub struct RealGpuBackend;

impl RealGpuBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealGpuBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuBackend for RealGpuBackend {
    fn enumerate_present_adapters(&self) -> Result<Vec<GpuDevice>, GpuError> {
        unsafe {
            let set = SetupDiGetClassDevsW(
                Some(&GUID_DEVCLASS_DISPLAY),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT,
            )
            .map_err(|e| GpuError::Enumerate(format!("SetupDiGetClassDevsW: {e}")))?;

            let mut out = Vec::new();
            let mut index = 0u32;
            loop {
                let mut data = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInfo(set, index, &mut data).is_err() {
                    break; // 無更多裝置（ERROR_NO_MORE_ITEMS）
                }
                if let Some(dev) = query_device(set, &data) {
                    out.push(dev);
                }
                index += 1;
            }
            let _ = SetupDiDestroyDeviceInfoList(set);
            Ok(out)
        }
    }

    fn read_affinity_policy(&self, instance_id: &str) -> Result<AffinityPolicy, GpuError> {
        unsafe {
            let path = wide(&affinity_policy_path(instance_id));
            let mut hkey = HKEY::default();
            let status = RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                None,
                KEY_READ,
                &mut hkey,
            );
            if status != ERROR_SUCCESS {
                // 金鑰不存在 → 兩個值都不存在（present=false），不是錯誤
                if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
                    return Ok(AffinityPolicy {
                        instance_id: instance_id.to_string(),
                        device_policy: RegistryValueSnapshot::default(),
                        assignment_set_override: RegistryValueSnapshot::default(),
                    });
                }
                return Err(GpuError::Registry(format!("RegOpenKeyExW: {status:?}")));
            }
            let key = OwnedRegistryKey(hkey); // RAII：成功/錯誤路徑都 RegCloseKey
            let device_policy = read_value(key.0, "DevicePolicy")?;
            let assignment_set_override = read_value(key.0, "AssignmentSetOverride")?;
            Ok(AffinityPolicy {
                instance_id: instance_id.to_string(),
                device_policy,
                assignment_set_override,
            })
        }
    }

    fn write_affinity_policy(&self, policy: &AffinityPolicy) -> Result<(), GpuError> {
        // 語意檢查（型別/長度）是 registry write 前的最後一道防線：
        // policy 可能來自磁碟上的狀態檔,不得讓任意 bytes 進入 HKLM
        validate_policy_snapshot(policy).map_err(GpuError::Registry)?;
        unsafe {
            let path = wide(&affinity_policy_path(&policy.instance_id));
            let mut hkey = HKEY::default();
            let mut disposition: REG_CREATE_KEY_DISPOSITION = REG_OPENED_EXISTING_KEY;
            let status = RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_SET_VALUE | KEY_CREATE_SUB_KEY,
                None,
                &mut hkey,
                Some(&mut disposition),
            );
            if status != ERROR_SUCCESS {
                return Err(GpuError::Registry(format!("RegCreateKeyExW: {status:?}")));
            }
            let key = OwnedRegistryKey(hkey); // RAII：任何錯誤路徑都關閉
            write_value(key.0, "DevicePolicy", &policy.device_policy)?;
            write_value(
                key.0,
                "AssignmentSetOverride",
                &policy.assignment_set_override,
            )?;
            Ok(())
        }
    }

    fn restart_device(&self, instance_id: &str, sleeper: &dyn Sleep) -> Result<(), GpuError> {
        unsafe {
            let set = SetupDiGetClassDevsW(
                Some(&GUID_DEVCLASS_DISPLAY),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT,
            )
            .map_err(|e| GpuError::Restart(format!("SetupDiGetClassDevsW: {e}")))?;

            let mut index = 0u32;
            loop {
                let mut data = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInfo(set, index, &mut data).is_err() {
                    let _ = SetupDiDestroyDeviceInfoList(set);
                    return Err(GpuError::NotFound(instance_id.to_string()));
                }
                index += 1;

                let Some(dev_inst) = read_instance_id(set, &data) else {
                    continue;
                };
                if dev_inst.eq_ignore_ascii_case(instance_id) {
                    // 同一 handle：disable → 停頓 → enable → 停頓（任務規格）
                    let result =
                        restart_sequence(|change| prop_change(set, &data, change), sleeper);
                    let _ = SetupDiDestroyDeviceInfoList(set);
                    return result;
                }
            }
        }
    }

    fn basic_display_enabled(&self) -> Result<bool, GpuError> {
        unsafe {
            let path = wide(r"SYSTEM\CurrentControlSet\Services\BasicDisplay");
            let mut hkey = HKEY::default();
            let status = RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                None,
                KEY_READ,
                &mut hkey,
            );
            if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
                // BasicDisplay 不存在 → 沒有被停用的 fallback，視為可動
                return Ok(true);
            }
            if status != ERROR_SUCCESS {
                return Err(GpuError::Registry(format!(
                    "RegOpenKeyExW(BasicDisplay): {status:?}"
                )));
            }
            let key = OwnedRegistryKey(hkey);
            let start = read_value(key.0, "Start")?;
            // Start = 4 (SERVICE_DISABLED) 才算停用；讀不到或非 DWORD 視為啟用
            Ok(start.as_dword() != Some(4))
        }
    }

    fn read_msi_supported(&self, instance_id: &str) -> Result<RegistryValueSnapshot, GpuError> {
        unsafe {
            let path = wide(&msi_policy_path(instance_id));
            let mut hkey = HKEY::default();
            let status = RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                None,
                KEY_READ,
                &mut hkey,
            );
            if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
                // 金鑰不存在 → 驅動未設定 MSI 屬性（present=false）
                return Ok(RegistryValueSnapshot::default());
            }
            if status != ERROR_SUCCESS {
                return Err(GpuError::Registry(format!(
                    "RegOpenKeyExW(MSI): {status:?}"
                )));
            }
            let key = OwnedRegistryKey(hkey);
            read_value(key.0, MSI_SUPPORTED)
        }
    }

    fn write_msi_supported(
        &self,
        instance_id: &str,
        value: &RegistryValueSnapshot,
    ) -> Result<(), GpuError> {
        unsafe {
            let path = wide(&msi_policy_path(instance_id));
            let mut hkey = HKEY::default();
            let mut disposition: REG_CREATE_KEY_DISPOSITION = REG_OPENED_EXISTING_KEY;
            let status = RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(path.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_SET_VALUE | KEY_CREATE_SUB_KEY,
                None,
                &mut hkey,
                Some(&mut disposition),
            );
            if status != ERROR_SUCCESS {
                return Err(GpuError::Registry(format!(
                    "RegCreateKeyExW(MSI): {status:?}"
                )));
            }
            let key = OwnedRegistryKey(hkey);
            write_value(key.0, MSI_SUPPORTED, value)
        }
    }
}

/// 組出 Affinity Policy 金鑰路徑。刻意用 CurrentControlSet。
fn affinity_policy_path(instance_id: &str) -> String {
    format!(
        r"SYSTEM\CurrentControlSet\Enum\{instance_id}\Device Parameters\Interrupt Management\Affinity Policy"
    )
}

/// 組出 MSI（Message Signaled Interrupt）屬性金鑰路徑。
/// `MSISupported`（REG_DWORD）= 1 表示 GPU 以 MSI 模式發中斷（低延遲）。
pub fn msi_policy_path(instance_id: &str) -> String {
    format!(
        r"SYSTEM\CurrentControlSet\Enum\{instance_id}\Device Parameters\Interrupt Management\MessageSignaledInterruptProperties"
    )
}

/// 讀取 `MSISupported` 值的常數名
pub const MSI_SUPPORTED: &str = "MSISupported";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// RAII 註冊表 key：drop 時自動 RegCloseKey，確保成功與錯誤路徑都不漏 handle
struct OwnedRegistryKey(HKEY);

impl Drop for OwnedRegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// 列舉單一裝置：instance id + friendly name（fallback DeviceDesc / instance id）
unsafe fn query_device(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Option<GpuDevice> {
    let instance_id = read_instance_id(set, data)?;
    let friendly_name = read_registry_string(set, data, SPDRP_FRIENDLYNAME)
        .or_else(|| read_registry_string(set, data, SPDRP_DEVICEDESC))
        .unwrap_or_else(|| instance_id.clone());
    Some(GpuDevice {
        instance_id,
        friendly_name,
    })
}

/// 讀裝置 instance id（SetupDiGetDeviceInstanceIdW，先取長度再取資料）
unsafe fn read_instance_id(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Option<String> {
    let mut needed = 0u32;
    let _ = SetupDiGetDeviceInstanceIdW(set, data, None, Some(&mut needed));
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u16; (needed + 1) as usize];
    if SetupDiGetDeviceInstanceIdW(set, data, Some(buf.as_mut_slice()), Some(&mut needed)).is_err()
    {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(needed as usize);
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// 讀 SPDRP_* 字串屬性
unsafe fn read_registry_string(
    set: HDEVINFO,
    data: &SP_DEVINFO_DATA,
    prop: SETUP_DI_REGISTRY_PROPERTY,
) -> Option<String> {
    let mut needed = 0u32;
    let _ = SetupDiGetDeviceRegistryPropertyW(set, data, prop, None, None, Some(&mut needed));
    if needed == 0 {
        // 有些驅動不回長度，改用固定 buffer 再試
        needed = 1024;
    }
    let mut buf = vec![0u8; (needed + 2) as usize];
    let mut cb = needed;
    if SetupDiGetDeviceRegistryPropertyW(
        set,
        data,
        prop,
        None,
        Some(buf.as_mut_slice()),
        Some(&mut cb),
    )
    .is_err()
    {
        return None;
    }
    let u16s: Vec<u16> = buf[..cb as usize]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16_lossy(&u16s)
        .trim_end_matches('\0')
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 讀單一註冊表值：存在 → 型別+位元組；缺失 → present=false
unsafe fn read_value(hkey: HKEY, name: &str) -> Result<RegistryValueSnapshot, GpuError> {
    let name_w = wide(name);
    let mut value_type: REG_VALUE_TYPE = REG_DWORD;
    let mut cb = 0u32;
    let status = RegQueryValueExW(
        hkey,
        PCWSTR(name_w.as_ptr()),
        None,
        Some(&mut value_type),
        None,
        Some(&mut cb),
    );
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(RegistryValueSnapshot::default());
    }
    if status != ERROR_SUCCESS {
        return Err(GpuError::Registry(format!(
            "RegQueryValueExW({name}): {status:?}"
        )));
    }
    let mut buf = vec![0u8; cb as usize];
    let status = RegQueryValueExW(
        hkey,
        PCWSTR(name_w.as_ptr()),
        None,
        Some(&mut value_type),
        Some(buf.as_mut_ptr()),
        Some(&mut cb),
    );
    if status != ERROR_SUCCESS {
        return Err(GpuError::Registry(format!(
            "RegQueryValueExW({name}) data: {status:?}"
        )));
    }
    buf.truncate(cb as usize);
    Ok(RegistryValueSnapshot {
        present: true,
        value_type: Some(value_type.0),
        bytes: Some(buf),
    })
}

/// 依快照寫入單一值：present → 寫回型別+位元組；absent → 刪除
unsafe fn write_value(
    hkey: HKEY,
    name: &str,
    snap: &RegistryValueSnapshot,
) -> Result<(), GpuError> {
    let name_w = wide(name);
    if snap.present {
        // 用快照的數值型別（無損還原任何 REG_VALUE_TYPE）
        let value_type = REG_VALUE_TYPE(snap.value_type.unwrap_or(REG_DWORD.0));
        let bytes: &[u8] = snap.bytes.as_deref().unwrap_or(&[]);
        let status = RegSetValueExW(hkey, PCWSTR(name_w.as_ptr()), None, value_type, Some(bytes));
        if status != ERROR_SUCCESS {
            return Err(GpuError::Registry(format!(
                "RegSetValueExW({name}): {status:?}"
            )));
        }
    } else {
        let status = RegDeleteValueW(hkey, PCWSTR(name_w.as_ptr()));
        // 本來就不存在的值 → 刪除失敗也算達成目標
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            return Err(GpuError::Registry(format!(
                "RegDeleteValueW({name}): {status:?}"
            )));
        }
    }
    Ok(())
}

/// DICS_DISABLE / DICS_ENABLE 的 property change 包裝
unsafe fn prop_change(
    set: HDEVINFO,
    data: &SP_DEVINFO_DATA,
    change: SETUP_DI_STATE_CHANGE,
) -> Result<(), GpuError> {
    let params = SP_PROPCHANGE_PARAMS {
        ClassInstallHeader: SP_CLASSINSTALL_HEADER {
            cbSize: std::mem::size_of::<SP_CLASSINSTALL_HEADER>() as u32,
            InstallFunction: DIF_PROPERTYCHANGE,
        },
        StateChange: change,
        Scope: DICS_FLAG_GLOBAL,
        HwProfile: 0,
    };
    SetupDiSetClassInstallParamsW(
        set,
        Some(data),
        Some(&params.ClassInstallHeader as *const SP_CLASSINSTALL_HEADER),
        std::mem::size_of::<SP_PROPCHANGE_PARAMS>() as u32,
    )
    .map_err(|e| GpuError::Restart(format!("SetupDiSetClassInstallParamsW: {e}")))?;
    SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set, Some(data))
        .map_err(|e| GpuError::Restart(format!("SetupDiCallClassInstaller: {e}")))
}

/// 查詢目前策略的「驗證」：read 回傳是否與快照逐位元組一致
pub fn policy_matches(snapshot: &AffinityPolicy, current: &AffinityPolicy) -> bool {
    current.device_policy == snapshot.device_policy
        && current.assignment_set_override == snapshot.assignment_set_override
}

/// 寫回快照 + 重啟裝置 + 驗證逐位元組一致。manager 與 runner 共用。
/// 前置：語意驗證 + 目標必須是**目前存在**的 display adapter —
/// recovery/restore 的快照來自磁碟,任何 registry write 前先驗證 target。
pub fn restore_snapshot(
    backend: &dyn GpuBackend,
    sleeper: &dyn Sleep,
    snapshot: &AffinityPolicy,
) -> Result<(), String> {
    validate_policy_snapshot(snapshot).map_err(|_| crate::error::codes::GPU_RESTORE_FAILED)?;
    let present = backend
        .enumerate_present_adapters()
        .map_err(|e| e.code().to_string())?
        .iter()
        .any(|d| d.instance_id.eq_ignore_ascii_case(&snapshot.instance_id));
    if !present {
        return Err(crate::error::codes::GPU_NOT_FOUND.to_string());
    }
    backend
        .write_affinity_policy(snapshot)
        .map_err(|e| e.code().to_string())?;
    backend
        .restart_device(&snapshot.instance_id, sleeper)
        .map_err(|e| e.code().to_string())?;
    let current = backend
        .read_affinity_policy(&snapshot.instance_id)
        .map_err(|e| e.code().to_string())?;
    if !policy_matches(snapshot, &current) {
        return Err(crate::error::codes::GPU_RESTORE_FAILED.to_string());
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// 測試用 fake backend：記憶體模擬 registry 狀態與重啟行為，
// 讓「套用/還原」流程不需要真實 HKLM 或真實裝置。

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;

    /// 可注入的記憶體 backend。`policies` 即「目前的註冊表狀態」。
    pub struct FakeBackend {
        pub devices: Vec<GpuDevice>,
        pub basic_display_on: AtomicBool,
        /// 下一次 restart 的 disable 階段失敗（一次性，之後恢復）
        pub fail_next_restart: AtomicBool,
        /// 持續讓 disable 失敗（enable 永不嘗試）
        pub disable_fails: AtomicBool,
        /// 持續讓 enable 失敗（disable 成功後必嘗試 enable 再失敗）
        pub enable_fails: AtomicBool,
        /// 下一次 write 失敗（一次性）
        fail_next_write: AtomicBool,
        /// 讓第 N 次 read（1-based）失敗；0 = 永不。供 read-back 失敗注入。
        fail_read_at: AtomicU32,
        /// 讓第 N 次 read（1-based）回傳不符的 mask（read-back mismatch）；0 = 永不。
        mismatch_read_at: AtomicU32,
        /// 已發生的 read 次數（配合 fail_read_at / mismatch_read_at）
        read_count: AtomicU32,
        policies: Mutex<HashMap<String, AffinityPolicy>>,
        /// MSI（MSISupported）模擬狀態：instance → snapshot；缺項 = absent
        msi: Mutex<HashMap<String, RegistryValueSnapshot>>,
        restart_count: AtomicU32,
        disable_attempts: AtomicU32,
        enable_attempts: AtomicU32,
    }

    impl FakeBackend {
        pub fn new(devices: Vec<GpuDevice>) -> Self {
            Self {
                devices,
                basic_display_on: AtomicBool::new(true),
                fail_next_restart: AtomicBool::new(false),
                disable_fails: AtomicBool::new(false),
                enable_fails: AtomicBool::new(false),
                fail_next_write: AtomicBool::new(false),
                fail_read_at: AtomicU32::new(0),
                mismatch_read_at: AtomicU32::new(0),
                read_count: AtomicU32::new(0),
                policies: Mutex::new(HashMap::new()),
                msi: Mutex::new(HashMap::new()),
                restart_count: AtomicU32::new(0),
                disable_attempts: AtomicU32::new(0),
                enable_attempts: AtomicU32::new(0),
            }
        }

        /// 預置目前策略（模擬真實系統既有值）
        pub fn set_policy(&self, policy: AffinityPolicy) {
            self.policies
                .lock()
                .unwrap()
                .insert(policy.instance_id.clone(), policy);
        }

        pub fn fail_next_write(&self) {
            self.fail_next_write.store(true, Ordering::SeqCst);
        }

        /// 讓第 N 次 read（1-based）失敗（模擬 read-back 失敗）。
        pub fn fail_nth_read(&self, n: u32) {
            self.fail_read_at.store(n, Ordering::SeqCst);
        }

        /// 讓第 N 次 read（1-based）回傳不符的 mask（模擬 read-back mismatch）。
        pub fn fail_nth_read_mismatch(&self, n: u32) {
            self.mismatch_read_at.store(n, Ordering::SeqCst);
        }

        pub fn restart_count(&self) -> u32 {
            self.restart_count.load(Ordering::SeqCst)
        }
        pub fn disable_attempts(&self) -> u32 {
            self.disable_attempts.load(Ordering::SeqCst)
        }
        pub fn enable_attempts(&self) -> u32 {
            self.enable_attempts.load(Ordering::SeqCst)
        }
        pub fn current_policy(&self, instance_id: &str) -> AffinityPolicy {
            self.policies
                .lock()
                .unwrap()
                .get(instance_id)
                .cloned()
                .unwrap_or_else(|| AffinityPolicy {
                    instance_id: instance_id.to_string(),
                    ..Default::default()
                })
        }

        /// 預置 MSI 狀態（模擬驅動既有 MSISupported 值）
        pub fn set_msi(&self, instance_id: &str, value: Option<u32>) {
            let mut msi = self.msi.lock().unwrap();
            match value {
                Some(v) => {
                    msi.insert(instance_id.to_string(), RegistryValueSnapshot::dword(v));
                }
                None => {
                    msi.remove(instance_id);
                }
            }
        }

        pub fn msi_value(&self, instance_id: &str) -> Option<u32> {
            self.msi
                .lock()
                .unwrap()
                .get(instance_id)
                .and_then(|s| s.as_dword())
        }
    }

    impl GpuBackend for FakeBackend {
        fn enumerate_present_adapters(&self) -> Result<Vec<GpuDevice>, GpuError> {
            Ok(self.devices.clone())
        }

        fn read_msi_supported(&self, instance_id: &str) -> Result<RegistryValueSnapshot, GpuError> {
            Ok(self
                .msi
                .lock()
                .unwrap()
                .get(instance_id)
                .cloned()
                .unwrap_or_default())
        }

        fn write_msi_supported(
            &self,
            instance_id: &str,
            value: &RegistryValueSnapshot,
        ) -> Result<(), GpuError> {
            let mut msi = self.msi.lock().unwrap();
            if value.present {
                msi.insert(instance_id.to_string(), value.clone());
            } else {
                msi.remove(instance_id);
            }
            Ok(())
        }

        fn read_affinity_policy(&self, instance_id: &str) -> Result<AffinityPolicy, GpuError> {
            let n = self.read_count.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_read_at.load(Ordering::SeqCst) == n {
                return Err(GpuError::Registry("fake: read fail".into()));
            }
            if self.mismatch_read_at.load(Ordering::SeqCst) == n {
                return Ok(AffinityPolicy {
                    instance_id: instance_id.to_string(),
                    device_policy: RegistryValueSnapshot::dword(DEVICE_POLICY_SINGLE_PROCESSOR),
                    assignment_set_override: RegistryValueSnapshot::binary(vec![0xFF]),
                });
            }
            Ok(self.current_policy(instance_id))
        }

        fn write_affinity_policy(&self, policy: &AffinityPolicy) -> Result<(), GpuError> {
            if self.fail_next_write.swap(false, Ordering::SeqCst) {
                return Err(GpuError::Registry("fake: write fail".into()));
            }
            self.policies
                .lock()
                .unwrap()
                .insert(policy.instance_id.clone(), policy.clone());
            Ok(())
        }

        fn restart_device(&self, instance_id: &str, _sleeper: &dyn Sleep) -> Result<(), GpuError> {
            let _ = instance_id;
            self.disable_attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail_next_restart.swap(false, Ordering::SeqCst)
                || self.disable_fails.load(Ordering::SeqCst)
            {
                return Err(GpuError::Restart("fake: disable fail".into()));
            }
            self.enable_attempts.fetch_add(1, Ordering::SeqCst);
            if self.enable_fails.load(Ordering::SeqCst) {
                return Err(GpuError::Restart("fake: enable fail".into()));
            }
            self.restart_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn basic_display_enabled(&self) -> Result<bool, GpuError> {
            Ok(self.basic_display_on.load(Ordering::SeqCst))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn single_lp_mask_trimmed_little_endian() {
        // AutoGpuAffinity 相容的精簡 LE 表示
        assert_eq!(single_lp_mask_bytes(0), vec![0x01]);
        assert_eq!(single_lp_mask_bytes(31), vec![0x00, 0x00, 0x00, 0x80]);
        assert_eq!(single_lp_mask_bytes(32), vec![0x00, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!(
            single_lp_mask_bytes(63),
            vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80]
        );
    }

    #[test]
    fn snapshot_roundtrip_preserves_arbitrary_type_and_bytes() {
        // 任意非 DWORD 型別：REG_SZ（值 1）附 UTF-16 位元組
        let snap = RegistryValueSnapshot {
            present: true,
            value_type: Some(1), // REG_SZ
            bytes: Some(vec![b'h', 0, b'i', 0, 0, 0]),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: RegistryValueSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.value_type, Some(1));
        assert_eq!(back.bytes, snap.bytes);
        assert!(back.present);
    }

    #[test]
    fn as_dword_requires_dword_type() {
        assert_eq!(RegistryValueSnapshot::binary(vec![0x01]).as_dword(), None);
        assert_eq!(RegistryValueSnapshot::dword(4).as_dword(), Some(4));
    }

    /// 記錄 sleep 呼叫，驗證重啟時序而不真的睡
    struct RecordingSleeper {
        calls: Mutex<Vec<u64>>,
    }

    impl RecordingSleeper {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Sleep for RecordingSleeper {
        fn sleep(&self, ms: u64) {
            self.calls.lock().unwrap().push(ms);
        }
    }

    #[test]
    fn restart_sequence_disable_gap_enable_settle() {
        let sleeper = RecordingSleeper::new();
        let log = Mutex::new(Vec::new());
        let ops = |change: SETUP_DI_STATE_CHANGE| -> Result<(), GpuError> {
            log.lock().unwrap().push(change);
            Ok(())
        };
        restart_sequence(ops, &sleeper).unwrap();
        assert_eq!(*log.lock().unwrap(), vec![DICS_DISABLE, DICS_ENABLE]);
        assert_eq!(
            *sleeper.calls.lock().unwrap(),
            vec![RESTART_GAP_BEFORE_ENABLE_MS, RESTART_SETTLE_AFTER_ENABLE_MS]
        );
    }

    #[test]
    fn restart_sequence_disable_failure_skips_enable_and_sleep() {
        let sleeper = RecordingSleeper::new();
        let ops = |change: SETUP_DI_STATE_CHANGE| -> Result<(), GpuError> {
            if change == DICS_DISABLE {
                Err(GpuError::Restart("fail".into()))
            } else {
                Ok(())
            }
        };
        assert!(restart_sequence(ops, &sleeper).is_err());
        assert!(
            sleeper.calls.lock().unwrap().is_empty(),
            "disable 失敗不該睡，也不該嘗試 enable"
        );
    }
}
