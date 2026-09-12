//! 開機自啟動：schtasks ONLOGON 工作（登入不跳 UAC），僅移除可辨識的舊版工作。
//! 任務不帶參數 — 啟動時是否最小化由 settings.start_minimized 在執行期決定。
//! 執行檔位於受保護目錄（Program Files 樹）用 /RL HIGHEST；可寫位置（可攜版等）
//! 一律降級 /RL LIMITED——HIGHEST 工作保存的是路徑字串，登入時會無 UAC 執行
//! 該路徑的內容，可寫位置可被同帳戶未提升程序置換（安全審計 #05）。
use serde::Serialize;
use std::{
    os::windows::process::CommandExt,
    process::Command,
    sync::{Mutex, OnceLock},
};

const CREATE_NO_WINDOW: u32 = 0x08000000;
const TASK_NAME: &str = "FrameAnchor";

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

static CLEANUP: OnceLock<Mutex<Option<Result<(), String>>>> = OnceLock::new();
pub fn cleanup_legacy_autostart() -> Result<(), String> {
    let mut cached = CLEANUP
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|e| e.to_string())?;
    if let Some(result) = cached.as_ref() {
        return result.clone();
    }
    let result = cleanup_task();
    *cached = Some(result.clone());
    result
}
fn cleanup_task() -> Result<(), String> {
    let script = r#"
$ErrorActionPreference = 'Stop'
try {
  $service = New-Object -ComObject 'Schedule.Service'
  $service.Connect()
  $folder = $service.GetFolder('\')
  try { $task = $folder.GetTask('FrameAnchor') }
  catch { if (($_.Exception.GetBaseException()).HResult -eq -2147024894) { exit 0 }; throw }
  if (-not (Test-LegacyTask $task.Xml)) {
    throw 'Task FrameAnchor is not a recognized legacy FrameAnchor task; it was preserved.'
  }
  $folder.DeleteTask('FrameAnchor', 0)
  exit 0
} catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
"#;
    let script = format!("{LEGACY_MATCHER}\n{script}");
    let out = std::process::Command::new(crate::syspath::powershell_exe()?)
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationStatus {
    pub notice_required: bool,
    pub cleanup_error: Option<String>,
}
fn marker() -> std::path::PathBuf {
    crate::config::config_dir().join("gpu-only-migration.ack")
}
#[tauri::command]
pub async fn get_migration_status() -> Result<MigrationStatus, String> {
    tauri::async_runtime::spawn_blocking(|| MigrationStatus {
        notice_required: !marker().exists(),
        cleanup_error: cleanup_legacy_autostart().err(),
    })
    .await
    .map_err(|e| e.to_string())
}
#[tauri::command]
pub fn acknowledge_migration() -> Result<(), String> {
    std::fs::create_dir_all(crate::config::config_dir()).map_err(|e| e.to_string())?;
    std::fs::write(marker(), b"3").map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn retry_autostart_cleanup() -> Result<(), String> {
    *CLEANUP
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|e| e.to_string())? = None;
    tauri::async_runtime::spawn_blocking(cleanup_legacy_autostart)
        .await
        .map_err(|e| e.to_string())?
}

const LEGACY_MATCHER: &str = r#"
function Test-LegacyTask([xml]$definition) {
  $ns = New-Object System.Xml.XmlNamespaceManager($definition.NameTable)
  $ns.AddNamespace('t', 'http://schemas.microsoft.com/windows/2004/02/mit/task')
  $actions = @($definition.SelectNodes('//t:Actions/*', $ns))
  $triggers = @($definition.SelectNodes('//t:Triggers/*', $ns))
  return ($actions.Count -eq 1 -and $actions[0].LocalName -eq 'Exec' -and
      $triggers.Count -eq 1 -and $triggers[0].LocalName -eq 'LogonTrigger' -and
      [IO.Path]::GetFileName(([string]$actions[0].Command).Trim('"')) -ieq 'FrameAnchor.exe' -and
      ([string]$actions[0].Arguments).Trim() -eq '--minimized')
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_recognizes_only_legacy_task_definition() {
        // 僅呼叫 XML predicate，絕不連線或修改真實 Task Scheduler。
        let xml = r#"<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><Triggers><LogonTrigger/></Triggers><Actions><Exec><Command>C:\Apps\FrameAnchor.exe</Command><Arguments>--minimized</Arguments></Exec></Actions></Task>"#;
        for (fixture, expected) in [
            (xml.to_string(), true),
            (xml.replace("FrameAnchor.exe", "Other.exe"), false),
            (xml.replace("--minimized", "--other"), false),
            (xml.replace("LogonTrigger", "BootTrigger"), false),
            (
                xml.replace(
                    "</Actions>",
                    "<Exec><Command>Other.exe</Command></Exec></Actions>",
                ),
                false,
            ),
        ] {
            let script = format!(
                "{}\nif ((Test-LegacyTask '{}') -ne ${}) {{ exit 1 }}",
                LEGACY_MATCHER, fixture, expected
            );
            let status = std::process::Command::new(crate::syspath::powershell_exe().unwrap())
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .creation_flags(0x08000000)
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
}
