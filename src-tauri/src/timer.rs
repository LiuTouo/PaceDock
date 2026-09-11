//! 高精度計時器：以 ntdll `NtSetTimerResolution` 常駐請求 0.5 ms timer resolution。
//!
//! 誠實限制（設定頁 hint 須如實告知）：
//! - Windows 10 2004+ 起請求為 per-process：FrameAnchor 的請求只保證自身 timer
//!   精度；背景/隱藏視窗行程在 Win11 起其請求會被節流。
//! - 全域生效需 `GlobalTimerResolutionRequests=1`（本功能不寫此鍵，僅提示）。

use std::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Power::{
    PowerRegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD,
};
use windows::Win32::System::Threading::{
    OpenProcess, SetProcessInformation, ProcessPowerThrottling,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_SET_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS;

use crate::error::codes;

/// 請求值（100 ns 單位）：5,000 = 0.5 ms。核心授予「≤ 請求值的最近支援粒度」，
/// 不會自動給更細值 — 請求 10,000 只會拿到 1 ms，必須直接請求 5,000 才有 0.5 ms。
const REQUEST_HUNDRED_NS: u32 = 5_000;

/// 是否持有請求（與 Settings.high_precision_timer 同步）
static KEEP: AtomicBool = AtomicBool::new(false);

/// 電源通知已註冊（冪等防重複註冊）
static POWER_WATCH: AtomicBool = AtomicBool::new(false);

type NtSetTimerResolutionFn = unsafe extern "system" fn(u32, u8, *mut u32) -> i32;
type NtQueryTimerResolutionFn = unsafe extern "system" fn(*mut u32, *mut u32, *mut u32) -> i32;

/// 解析 ntdll 符號為函式指標（ntdll 不在 windows crate 內，走 GetProcAddress）。
fn resolve_ntdll(name: windows::core::PCSTR, label: &str) -> Result<*const c_void, String> {
    unsafe {
        let module = GetModuleHandleW(w!("ntdll.dll"))
            .map_err(|e| format!("GetModuleHandleW(ntdll.dll) 失敗: {e}"))?;
        let proc = GetProcAddress(module, name);
        let Some(proc) = proc else {
            return Err(format!("ntdll 缺少 {label}"));
        };
        Ok(proc as *const c_void)
    }
}

/// 請求 0.5 ms timer resolution。回傳 (結果, 核心授予值)（授予值為 100 ns 單位）。
pub fn request() -> (Result<(), String>, Option<u32>) {
    match resolve_ntdll(s!("NtSetTimerResolution"), "NtSetTimerResolution") {
        Ok(ptr) => {
            let f: NtSetTimerResolutionFn = unsafe { std::mem::transmute(ptr) };
            let mut current = 0u32;
            // 第二參數為 BOOLEAN：1 = 使請求生效（Disable = FALSE）
            let status = unsafe { f(REQUEST_HUNDRED_NS, 1, &mut current) };
            if status < 0 {
                (
                    Err(format!("NtSetTimerResolution 失敗: NTSTATUS {status:#010x}")),
                    None,
                )
            } else {
                (Ok(()), Some(current))
            }
        }
        Err(e) => (Err(e), None),
    }
}

/// 釋放請求。KEEP 旗標由 apply 管理。
pub fn release() -> Result<(), String> {
    match resolve_ntdll(s!("NtSetTimerResolution"), "NtSetTimerResolution") {
        Ok(ptr) => {
            let f: NtSetTimerResolutionFn = unsafe { std::mem::transmute(ptr) };
            let mut current = 0u32;
            let status = unsafe { f(REQUEST_HUNDRED_NS, 0, &mut current) };
            if status < 0 {
                Err(format!(
                    "NtSetTimerResolution(FALSE) 失敗: NTSTATUS {status:#010x}"
                ))
            } else {
                Ok(())
            }
        }
        Err(e) => Err(e),
    }
}

/// 查詢 timer resolution 三元組（100 ns 單位）：`(min, max, current)`。
/// 命名跟隨 NtQueryTimerResolution：`min` = 最細支援解析（如 5,000 = 0.5 ms）、
/// `max` = 最粗（如 156,250 = 15.625 ms）、`current` = 目前的全域生效值。
/// 與 Sysinternals Clockres 的對應：min→"Minimum timer interval"、
/// max→"Maximum timer interval"、current→"Current timer interval"。
pub fn intervals() -> Option<(u32, u32, u32)> {
    let ptr = resolve_ntdll(
        s!("NtQueryTimerResolution"),
        "NtQueryTimerResolution",
    )
    .ok()?;
    let f: NtQueryTimerResolutionFn = unsafe { std::mem::transmute(ptr) };
    let mut min = 0u32;
    let mut max = 0u32;
    let mut cur = 0u32;
    let status = unsafe { f(&mut min, &mut max, &mut cur) };
    if status < 0 {
        None
    } else {
        Some((min, max, cur))
    }
}

/// 套用開關：與 KEEP 同步。冪等（同值重套不重發請求）。
pub fn apply(enabled: bool) -> Result<(), String> {
    if KEEP.swap(enabled, Ordering::Relaxed) == enabled {
        return Ok(());
    }
    if enabled {
        request().0
    } else {
        release()
    }
}

/// 目前是否持有請求。
pub fn enabled() -> bool {
    KEEP.load(Ordering::Relaxed)
}

/// 註冊睡眠/喚醒通知：喚醒後若 KEEP 仍在，重新送出請求（睡眠會重置 per-process
/// 請求）。註冊一次、不 unregister（行程生命週期資源，退出時 OS 回收）。
/// 註冊失敗僅 log 降級：喚醒後需手動重開；`ponytail:` 需重試/監控再升級。
pub fn init_power_watch() {
    if POWER_WATCH.swap(true, Ordering::Relaxed) {
        return;
    }
    // 訂閱參數會在 callback 內被系統讀取 → 必須活得比本函式久，Box::leak 固定
    let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS = Box::leak(Box::new(
        DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(power_callback),
            Context: std::ptr::null_mut(),
        },
    ));
    let mut handle: *mut c_void = std::ptr::null_mut();
    // recipient 為參數結構指標（以 HANDLE 傳遞）；flags = DEVICE_NOTIFY_CALLBACK (2)
    let result = unsafe {
        PowerRegisterSuspendResumeNotification(
            REGISTER_NOTIFICATION_FLAGS(2),
            HANDLE(params as *mut _ as *mut c_void),
            &mut handle,
        )
    };
    if result.is_err() {
        log::warn!(
            "PowerRegisterSuspendResumeNotification 失敗（{result:?}）；喚醒後需手動重開高精度計時器"
        );
        POWER_WATCH.store(false, Ordering::Relaxed);
    }
}

/// 電源通知回呼：喚醒後重新送出請求（僅當 KEEP 仍在）。
unsafe extern "system" fn power_callback(
    _context: *const c_void,
    kind: u32,
    _setting: *const c_void,
) -> u32 {
    // PBT_APMRESUMEAUTOMATIC = 18：從睡眠喚醒
    const PBT_APMRESUMEAUTOMATIC: u32 = 18;
    if kind == PBT_APMRESUMEAUTOMATIC && KEEP.load(Ordering::Relaxed) {
        let (result, granted) = request();
        match result {
            Ok(()) => log::info!(
                "喚醒後重發高精度計時器請求，授予 {} × 100ns",
                granted.unwrap_or(0)
            ),
            Err(e) => log::warn!("喚醒後重發高精度計時器請求失敗: {e}"),
        }
    }
    0
}

// ── 全域請求政策（登錄值）────────────────────────────────────────────────
//
// Win11 24H2 起 timer resolution 全面 per-process；`GlobalTimerResolutionRequests=1`
// 恢復舊全域語意（FrameAnchor 的請求才會抬升全系統 tick）。此值本身只是政策門、
// 不是計時請求：程式關閉後請求隨行程消失、tick 自動回落，登錄值留著也無害。

const GLOBAL_TIMER_SUBKEY: &str = "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\kernel";
const GLOBAL_TIMER_VALUE: &str = "GlobalTimerResolutionRequests";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 讀全域請求政策登錄值。None = 值未設定（系統預設 = 無全域語意）。
pub fn global_requests_enabled() -> Result<Option<bool>, String> {
    unsafe {
        let subkey = wide(GLOBAL_TIMER_SUBKEY);
        let mut hkey = HKEY::default();
        let status = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut hkey,
        );
        if status.is_err() {
            return Err(format!("RegOpenKeyExW(kernel): {status:?}"));
        }
        let key = OwnedKey(hkey);
        let mut data = 0u32;
        let mut size = 4u32;
        let mut kind = REG_DWORD;
        let status = RegQueryValueExW(
            key.0,
            PCWSTR(wide(GLOBAL_TIMER_VALUE).as_ptr()),
            None,
            Some(&mut kind),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status.is_err() {
            return Err(format!("RegQueryValueExW(GlobalTimerResolutionRequests): {status:?}"));
        }
        Ok(Some(data != 0))
    }
}

/// 設定/移除全域請求政策登錄值。disabled = 刪除值（回復系統預設）。
pub fn set_global_requests(enabled: bool) -> Result<(), String> {
    unsafe {
        let subkey = wide(GLOBAL_TIMER_SUBKEY);
        let mut hkey = HKEY::default();
        let status = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if status.is_err() {
            return Err(format!("RegOpenKeyExW(kernel): {status:?}"));
        }
        let key = OwnedKey(hkey);
        if enabled {
            let status = RegSetValueExW(
                key.0,
                PCWSTR(wide(GLOBAL_TIMER_VALUE).as_ptr()),
                None,
                REG_DWORD,
                Some(&1u32.to_le_bytes()),
            );
            if status.is_err() {
                return Err(format!("RegSetValueExW: {status:?}"));
            }
        } else {
            let status = RegDeleteValueW(key.0, PCWSTR(wide(GLOBAL_TIMER_VALUE).as_ptr()));
            if status.is_err() && status != ERROR_FILE_NOT_FOUND {
                return Err(format!("RegDeleteValueW: {status:?}"));
            }
        }
        Ok(())
    }
}

/// RAII 註冊表 key（與 gpu.rs 同模式：drop 時 RegCloseKey）
struct OwnedKey(HKEY);

impl Drop for OwnedKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

// ── 遊戲 timer 節流豁免（持久化名單 + per-PID runtime 施加）────────────────
//
// Win11 起「背景/被遮蔽視窗」行程的 timer 解析會被節流（打回 15.6 ms）。
// 對選定遊戲行程清掉 IGNORE_TIMER_RESOLUTION = 其解析不被前景狀態節流。
// 持久化：名單以小寫 exe 檔名為鍵存於 config.json（settings.timerExemptPrograms），
// 本模組內 PROGRAMS 為執行時鏡像；遊戲重啟（新 PID）由輪詢自動重套，
// FrameAnchor 重啟後從 config 灌回續用；正常退出時批次還原 runtime 施加。

/// 持久化豁免名單的執行時鏡像（小寫 exe 檔名；事實源為 config.json）
static PROGRAMS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// 啟動時灌入持久化名單（單次呼叫；之後由 set_exempt_program 維護）
pub fn init_programs(list: Vec<String>) {
    *PROGRAMS.write().unwrap_or_else(|p| p.into_inner()) = list;
}

/// 目前豁免名單（小寫 exe 檔名）
pub fn programs() -> Vec<String> {
    PROGRAMS.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// 已施加豁免的行程（pid → 小寫 exe 名），供列表與退出還原
static EXEMPTS: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();

fn exempts() -> &'static Mutex<HashMap<u32, String>> {
    EXEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 豁免清單項目（list_timer_exempts 回傳）：以程式為鍵；pids 空 = 名單內但未執行
#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TimerExemptEntry {
    pub exe_name: String,
    pub pids: Vec<u32>,
}

/// 對行程施加/還原 timer 節流豁免（即時生效）。`exe_name` 僅在 enabled 時作
/// 黑名單把關用（不得對 shell/自身行程開 handle）。
pub fn set_exempt(pid: u32, exe_name: &str, enabled: bool) -> Result<(), String> {
    if enabled && (pid == std::process::id() || crate::benchmark::capture::is_blacklisted(exe_name))
    {
        return Err(codes::TIMER_EXEMPT_BLOCKED.to_string());
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SET_LIMITED_INFORMATION, false, pid) else {
            log::warn!("timer 豁免：OpenProcess 失敗 pid={pid}（行程不存在或受保護）");
            return Err(codes::TIMER_EXEMPT_FAILED.to_string());
        };
        // ControlMask = 我接管此層面；StateMask = 0 → 「忽略 timer 解析」關閉 =
        // 行程的解析不被背景化節流。還原 = ControlMask 0（交還系統預設）。
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: if enabled {
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION
            } else {
                0
            },
            StateMask: 0,
        };
        let result = SetProcessInformation(
            h,
            ProcessPowerThrottling,
            &state as *const _ as *const c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
        let _ = CloseHandle(h);
        if result.is_err() {
            log::warn!("SetProcessInformation(ProcessPowerThrottling) 失敗 pid={pid}: {result:?}");
            return Err(codes::TIMER_EXEMPT_FAILED.to_string());
        }
    }
    let mut map = exempts().lock().unwrap_or_else(|p| p.into_inner());
    if enabled {
        map.insert(pid, exe_name.to_string());
    } else {
        map.remove(&pid);
    }
    Ok(())
}

/// 目前豁免清單（以程式為鍵聚合；名單內未執行者 pids 為空；順手清掉已死的 pid）。
pub fn list_exempts() -> Vec<TimerExemptEntry> {
    let procs = crate::process::enumerate_processes();
    let mut map = exempts().lock().unwrap_or_else(|p| p.into_inner());
    map.retain(|pid, _| procs.iter().any(|(p, _)| p == pid));
    let mut out: Vec<TimerExemptEntry> = programs()
        .into_iter()
        .map(|exe_name| TimerExemptEntry {
            exe_name,
            pids: Vec::new(),
        })
        .collect();
    for (pid, exe) in map.iter() {
        match out.iter_mut().find(|e| &e.exe_name == exe) {
            Some(entry) => entry.pids.push(*pid),
            None => out.push(TimerExemptEntry {
                exe_name: exe.clone(),
                pids: vec![*pid],
            }),
        }
    }
    out
}

/// 對程式（小寫 exe 檔名為鍵）開啟/關閉持久化豁免：同步名單鏡像，並立即對
/// 執行中同名行程施加/還原。之後由輪詢對新啟動的同名行程自動重套。
pub fn set_exempt_program(exe_name: &str, enabled: bool) -> Result<(), String> {
    let exe = exe_name.to_lowercase();
    if enabled && (exe == "frameanchor.exe" || crate::benchmark::capture::is_blacklisted(&exe)) {
        return Err(codes::TIMER_EXEMPT_BLOCKED.to_string());
    }
    // 先同步名單再動手：輪詢 thread 看到一致狀態
    {
        let mut list = PROGRAMS.write().unwrap_or_else(|p| p.into_inner());
        if enabled {
            if !list.contains(&exe) {
                list.push(exe.clone());
            }
        } else {
            list.retain(|p| *p != exe);
        }
    }
    if enabled {
        for (pid, name) in crate::process::enumerate_processes() {
            if name == exe {
                // 單一 PID 施加失敗（受保護行程）不阻斷：輪詢會持續重試
                let _ = set_exempt(pid, &exe, true);
            }
        }
    } else {
        let pids: Vec<u32> = exempts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, name)| name.as_str() == exe)
            .map(|(pid, _)| *pid)
            .collect();
        for pid in pids {
            let _ = set_exempt(pid, "", false);
        }
    }
    Ok(())
}

/// 輪詢一輪：對名單內執行中、尚未豁免的行程施加豁免；清掉已死 PID（設置隨
/// 行程消失，無需還原）。名單空時直接返回（零掃描開銷）。
pub fn apply_exempts_once() {
    let list = programs();
    if list.is_empty() {
        return;
    }
    let procs = crate::process::enumerate_processes();
    let live: std::collections::HashSet<u32> = procs.iter().map(|(pid, _)| *pid).collect();
    for (pid, name) in &procs {
        if !list.contains(name) {
            continue;
        }
        let known = exempts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(pid);
        if !known {
            let _ = set_exempt(*pid, name, true);
        }
    }
    exempts()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|pid, _| live.contains(pid));
}

/// 豁免輪詢：每 3 秒自動對名單內新啟動的行程套用豁免。
/// 立即先跑一輪（啟動即套用執行中遊戲），之後進入輪詢。
pub fn init_exempt_watch() {
    static WATCH: AtomicBool = AtomicBool::new(false);
    if WATCH.swap(true, Ordering::Relaxed) {
        return;
    }
    apply_exempts_once();
    let spawned = std::thread::Builder::new()
        .name("timer-exempt-watch".into())
        .spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            apply_exempts_once();
        });
    if let Err(e) = spawned {
        log::warn!("豁免輪詢 thread 啟動失敗: {e}（遊戲重啟後需手動重套）");
    }
}

/// 退出時批次還原所有豁免（掛 RunEvent::Exit）。崩潰路徑不保證 — 豁免隨
/// 目標行程自然消失，屬可接受的降級。
pub fn revert_all_exempts() {
    let map: HashMap<u32, String> =
        std::mem::take(&mut *exempts().lock().unwrap_or_else(|p| p.into_inner()));
    for (pid, _) in map {
        let _ = set_exempt(pid, "", false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 純狀態機：apply 冪等，且 KEEP 與 enabled() 同步。ntdll 呼叫為 best-effort：
    /// NT 失敗時 apply 回 Err 但旗標仍翻轉 — 測試只驗證旗標同步，不影響其他測試。
    #[test]
    fn apply_toggles_keep_flag() {
        let original = enabled();
        // 冪等：重複同值回 Ok 且不變
        assert!(apply(original).is_ok());
        assert_eq!(enabled(), original);
        // 翻轉後回復原狀
        let _ = apply(!original);
        let _ = apply(original);
        assert_eq!(enabled(), original);
    }

    /// 回讀 API 形狀：Some(v) 時 v 必落在合法 100 ns 解析範圍。
    #[test]
    fn intervals_return_none_or_valid_range() {
        if let Some((min, max, cur)) = intervals() {
            assert!((100..=1_000_000).contains(&min), "異常解析值 {min}");
            assert!((100..=1_000_000).contains(&max), "異常解析值 {max}");
            assert!((100..=1_000_000).contains(&cur), "異常解析值 {cur}");
        }
    }
}
