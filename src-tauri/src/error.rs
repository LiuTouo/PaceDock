//! 錯誤型別。對前端的錯誤一律轉成穩定代碼字串（前端查 i18n 顯示，PLAN §8）。

use thiserror::Error;

/// 前端 i18n 用的穩定錯誤代碼（errors.<code>）。
/// 部分代碼目前由前端字典保留（對應 errors.* 翻譯），後端尚未全數使用。
#[allow(dead_code)]
pub mod codes {
    pub const ACCESS_DENIED: &str = "ACCESS_DENIED";
    pub const OPEN_FAILED: &str = "OPEN_FAILED";
    pub const SET_AFFINITY_FAILED: &str = "SET_AFFINITY_FAILED";
    pub const SET_PRIORITY_FAILED: &str = "SET_PRIORITY_FAILED";
    pub const IO_FAILED: &str = "IO_FAILED";
    pub const MEM_FAILED: &str = "MEM_FAILED";
    pub const TOPOLOGY_FAILED: &str = "TOPOLOGY_FAILED";
    pub const CONFIG_FAILED: &str = "CONFIG_FAILED";
    pub const AUTOSTART_FAILED: &str = "AUTOSTART_FAILED";
    pub const UPDATE_CHECK_FAILED: &str = "UPDATE_CHECK_FAILED";
    pub const UPDATE_DOWNLOAD_FAILED: &str = "UPDATE_DOWNLOAD_FAILED";
    pub const UPDATE_INSTALL_FAILED: &str = "UPDATE_INSTALL_FAILED";
    // ── GPU 顯示裝置控制 ──
    pub const GPU_ENUM_FAILED: &str = "GPU_ENUM_FAILED";
    pub const GPU_NOT_FOUND: &str = "GPU_NOT_FOUND";
    pub const GPU_REGISTRY_FAILED: &str = "GPU_REGISTRY_FAILED";
    pub const GPU_RESTART_FAILED: &str = "GPU_RESTART_FAILED";
    pub const GPU_BASIC_DISPLAY_DISABLED: &str = "GPU_BASIC_DISPLAY_DISABLED";
    pub const GPU_RESTORE_FAILED: &str = "GPU_RESTORE_FAILED";
    pub const GPU_APPLY_FAILED: &str = "GPU_APPLY_FAILED";
    // ── 基準測試 ──
    pub const BENCHMARK_SESSION_NOT_FOUND: &str = "BENCHMARK_SESSION_NOT_FOUND";
    pub const BENCHMARK_SESSION_NOT_COMPLETED: &str = "BENCHMARK_SESSION_NOT_COMPLETED";
    /// session 可靠性未達 Passed（舊 session 無欄位 → Unassessed，或 Inconclusive）
    pub const BENCHMARK_RELIABILITY_NOT_PASSED: &str = "BENCHMARK_RELIABILITY_NOT_PASSED";
    pub const BENCHMARK_SESSION_INCOMPATIBLE: &str = "BENCHMARK_SESSION_INCOMPATIBLE";
    pub const BENCHMARK_RECOVERY_REQUIRED: &str = "BENCHMARK_RECOVERY_REQUIRED";
    pub const BENCHMARK_NOT_IMPLEMENTED: &str = "BENCHMARK_NOT_IMPLEMENTED";
    pub const BENCHMARK_ALREADY_RUNNING: &str = "BENCHMARK_ALREADY_RUNNING";
    pub const BENCHMARK_NOT_ACTIVE: &str = "BENCHMARK_NOT_ACTIVE";
    pub const BENCHMARK_STORAGE_FAILED: &str = "BENCHMARK_STORAGE_FAILED";
    /// runner 發生未攔截 panic；管理器會清理子程序、還原 GPU 並將 session 寫成 Failed。
    pub const BENCHMARK_RUNNER_PANIC: &str = "BENCHMARK_RUNNER_PANIC";
    pub const BENCHMARK_INVALID_SESSION_ID: &str = "BENCHMARK_INVALID_SESSION_ID";
    // ── 基準測試 runner（Task 2）──
    pub const BENCHMARK_ASSETS_MISSING: &str = "BENCHMARK_ASSETS_MISSING";
    pub const BENCHMARK_ASSETS_HASH_MISMATCH: &str = "BENCHMARK_ASSETS_HASH_MISMATCH";
    pub const BENCHMARK_INVALID_CONFIG: &str = "BENCHMARK_INVALID_CONFIG";
    pub const BENCHMARK_WORKLOAD_FAILED: &str = "BENCHMARK_WORKLOAD_FAILED";
    pub const BENCHMARK_PRESENTMON_FAILED: &str = "BENCHMARK_PRESENTMON_FAILED";
    pub const BENCHMARK_CSV_INVALID: &str = "BENCHMARK_CSV_INVALID";
    /// PresentMon 在 sample+margin 內未退出（卡住 / 停不下來）
    pub const BENCHMARK_PRESENTMON_TIMEOUT: &str = "BENCHMARK_PRESENTMON_TIMEOUT";
    /// PresentMon 正常退出但沒有產出輸出檔案（裝置/swapchain 暫態或 workload 未產生可擷取畫面）
    pub const BENCHMARK_CAPTURE_MISSING: &str = "BENCHMARK_CAPTURE_MISSING";
    /// 輸出檔案存在但沒有有效 frametime 資料（空 / 只剩 header）
    pub const BENCHMARK_CAPTURE_EMPTY: &str = "BENCHMARK_CAPTURE_EMPTY";
    /// PresentMon 回報 ETW events lost 且無有效 CSV（擷取負載過高，如 overlay/監控/錄影）。
    /// 不可重試、不可繼續後續 LP/round，直接進入 cleanup/restore。
    pub const BENCHMARK_CAPTURE_ETW_LOST: &str = "BENCHMARK_CAPTURE_ETW_LOST";
    /// PresentMon 回報 overflowed present events（circular buffer 溢位）；重試（加倍 buffer）
    /// 仍發生即終止 session，不隔離、不接受損壞資料。
    pub const BENCHMARK_CAPTURE_OVERFLOW: &str = "BENCHMARK_CAPTURE_OVERFLOW";
    /// 環境不穩定（AC/電池/電池節能/CPU 使用率/時間漂移）在重試上限內仍無法滿足。
    /// 失敗關閉（fail closed）：終止 session 且無可套用結果。
    pub const BENCHMARK_ENV_UNSTABLE: &str = "BENCHMARK_ENV_UNSTABLE";
    // ── Benchmark 視窗配置（強制視窗模式 + compact 視窗/還原）──
    /// workload 視窗與 FrameAnchor compact 視窗在 rcWork（工作區，排除工作列）內
    /// 無法不重疊（空間不足）。不自動縮放 workload，立即失敗關閉。
    pub const BENCHMARK_WINDOW_SPACE_INSUFFICIENT: &str = "BENCHMARK_WINDOW_SPACE_INSUFFICIENT";
    /// workload 視窗完整性（前景/非最小化/預期位置/topmost/可見/遮擋）在重試上限內
    /// 仍不滿足；該 capture 不進統計，最終失敗關閉並還原。
    pub const BENCHMARK_WINDOW_INTEGRITY: &str = "BENCHMARK_WINDOW_INTEGRITY";
    // ── Equivalent 安全驗證（Task 3）──
    /// session 非 equivalent-mode（未 Completed / algorithmVersion≠2 / 非 Equivalent）。
    pub const BENCHMARK_NOT_EQUIVALENT: &str = "BENCHMARK_NOT_EQUIVALENT";
    /// selected LP 不在 equivalent finalists pair 內。
    pub const BENCHMARK_EQUIVALENT_LP_INVALID: &str = "BENCHMARK_EQUIVALENT_LP_INVALID";
    /// 尚未完成 safety validation（或 validation 非 Passed / 與 selected 不一致）。
    pub const BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED: &str =
        "BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED";
    /// live reference policy 與驗證 snapshot 不一致（需重驗）。
    pub const BENCHMARK_EQUIVALENT_REFERENCE_CHANGED: &str =
        "BENCHMARK_EQUIVALENT_REFERENCE_CHANGED";
    /// 目前 GPU policy 無單一鎖定核心（無 reference 可比對）。
    pub const BENCHMARK_EQUIVALENT_NO_REFERENCE: &str = "BENCHMARK_EQUIVALENT_NO_REFERENCE";
}

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub enum ProcessError {
    #[error("access denied")]
    AccessDenied,
    #[error("win32 error: {0}")]
    Win32(u32),
}

#[cfg(test)]
impl ProcessError {
    /// 將 windows crate 回傳的 HRESULT 或原生 Win32 code 正規化成 raw Win32 code。
    pub fn normalize_win32(code: u32) -> u32 {
        if code & 0xFFFF_0000 == 0x8007_0000 {
            code & 0xFFFF
        } else {
            code
        }
    }

    pub fn from_win32(code: u32) -> Self {
        let code = Self::normalize_win32(code);
        if code == 5 {
            ProcessError::AccessDenied
        } else {
            ProcessError::Win32(code)
        }
    }
}

#[derive(Error, Debug)]
pub enum TopologyError {
    #[error("topology query failed")]
    QueryFailed,
}

#[cfg(test)]
mod tests {
    use super::{codes, ProcessError};

    #[test]
    fn normalizes_hresult_access_denied_to_stable_classification() {
        assert_eq!(ProcessError::normalize_win32(0x8007_0005), 5);
        assert_eq!(
            ProcessError::from_win32(0x8007_0005),
            ProcessError::AccessDenied
        );
        assert_eq!(ProcessError::from_win32(87), ProcessError::Win32(87));
    }

    /// 回歸測試：error.rs 內每個穩定錯誤代碼都必須存在於 en.json 與 zh-TW.json 的
    /// errors.*（前端查 i18n 顯示）。防止新增代碼漏加任一 locale。
    #[test]
    fn every_error_code_is_present_in_both_locales() {
        let src_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/error.rs");
        let src = std::fs::read_to_string(src_path).expect("讀取 error.rs");
        let codes: Vec<String> = src
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix("pub const ")
                    .and_then(|r| r.split_once(':'))
                    .map(|(name, _)| name.trim().to_string())
            })
            .collect();
        assert!(!codes.is_empty(), "應能從 error.rs 抽取錯誤代碼");

        for locale in ["en", "zh-TW"] {
            let path = format!("{}/../src/i18n/{locale}.json", env!("CARGO_MANIFEST_DIR"));
            let text = std::fs::read_to_string(&path).expect("讀取 i18n json");
            let json: serde_json::Value = serde_json::from_str(&text).expect("i18n json 解析");
            let errors = json["errors"]
                .as_object()
                .unwrap_or_else(|| panic!("{locale}.json 缺 errors 區塊"));
            for code in &codes {
                assert!(
                    errors.contains_key(code),
                    "錯誤代碼 {code} 缺於 {locale}.json 的 errors.*"
                );
            }
        }
        // 確保 codes 模組本身有內容（引用避免 unused）
        let _ = codes::TOPOLOGY_FAILED;
    }
}
