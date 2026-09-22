//! 開機自啟動：schtasks ONLOGON 工作（登入不跳 UAC）。
//! 任務不帶參數 — 啟動時是否最小化由 settings.start_minimized 在執行期決定。
//! 執行檔位於受保護目錄（Program Files 樹）用 /RL HIGHEST；可寫位置（可攜版等）
//! 一律降級 /RL LIMITED——HIGHEST 工作保存的是路徑字串，登入時會無 UAC 執行
//! 該路徑的內容，可寫位置可被同帳戶未提升程序置換（安全審計 #05）。
use std::{os::windows::process::CommandExt, process::Command};

const CREATE_NO_WINDOW: u32 = 0x08000000;
const TASK_NAME: &str = "PaceDock";

/// 以 System32 絕對路徑建構 schtasks(避免 untrusted search path 解析到可寫目錄的同名 PE)。
fn schtasks_command() -> Result<Command, String> {
    Ok(Command::new(crate::syspath::system32_tool("schtasks.exe")?))
}

/// 設定/取消開機自啟動排程工作。
pub fn set_autostart(enable: bool) -> Result<(), String> {
    if enable {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let protected = crate::syspath::in_protected_program_dir();
        let run_level = if protected { "HIGHEST" } else { "LIMITED" };
        if !protected {
            log::warn!(
                "執行檔位於可寫位置({}),開機自啟降級為 LIMITED,登入時可能出現 UAC 提示",
                exe.display()
            );
        }
        let tr = format!("\"{}\"", exe.display());
        let out = schtasks_command()?
            .args([
                "/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", run_level, "/TR", &tr, "/F",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("schtasks: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            log::error!(
                "schtasks /Create 失敗: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Err(crate::error::codes::AUTOSTART_FAILED.to_string())
        }
    } else {
        // 工作原本就不存在時，目標狀態已達成；若確實存在，刪除失敗不可靜默忽略。
        if !is_enabled() {
            return Ok(());
        }
        let out = schtasks_command()?
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("schtasks: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            log::error!(
                "schtasks /Delete 失敗: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Err(crate::error::codes::AUTOSTART_FAILED.to_string())
        }
    }
}

/// 查詢排程工作是否存在（查詢失敗一律視為未啟用，fail-closed）
pub fn is_enabled() -> bool {
    let Ok(mut cmd) = schtasks_command() else {
        return false;
    };
    cmd.args(["/Query", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
