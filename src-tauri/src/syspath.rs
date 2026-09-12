//! 受信任系統工具絕對路徑：提升權限程序以裸名稱啟動子程序時，
//! 應用程式目錄／目前目錄／PATH 中較早的可寫目錄可能被放置同名 PE 並繼承
//! administrator token（CWE-426）。此處一律以 GetSystemDirectoryW 組出
//! 絕對路徑，不依賴環境變數搜尋；解析失敗即 fail closed。

use std::path::{Path, PathBuf};

use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

/// 判斷目前執行檔是否位於 medium-integrity 程序不可寫的受保護目錄
/// (Program Files 樹)。其他位置(可攜版目錄、使用者目錄)一律視為可寫。
/// 以 SHGetKnownFolderPath 查詢,不依賴可被使用者環境變數覆蓋的 %ProgramFiles%。
pub fn in_protected_program_dir() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy().to_lowercase();
    [
        windows::Win32::UI::Shell::FOLDERID_ProgramFiles,
        windows::Win32::UI::Shell::FOLDERID_ProgramFilesX86,
    ]
    .iter()
    .filter_map(|fid| known_folder_path(fid).ok())
    .any(|root| {
        let root = root.trim_end_matches('\\').to_lowercase();
        exe.starts_with(&format!("{root}\\"))
    })
}

fn known_folder_path(fid: &windows::core::GUID) -> Result<String, String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::SHGetKnownFolderPath;

    unsafe {
        use windows::Win32::UI::Shell::KNOWN_FOLDER_FLAG;
        let path =
            SHGetKnownFolderPath(fid, KNOWN_FOLDER_FLAG(0), None).map_err(|e| e.to_string())?;
        let converted = path.to_string().map_err(|e| e.to_string());
        CoTaskMemFree(Some(path.as_ptr().cast()));
        converted
    }
}

/// %SystemRoot%\System32（以 Win32 API 查詢，不讀環境變數）。
pub fn system32_dir() -> Result<PathBuf, String> {
    let mut buf = [0u16; 260]; // MAX_PATH
                               // 回傳不含結尾反斜線的路徑長度；0 表示失敗
    let len = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if len == 0 || len >= buf.len() {
        return Err("GetSystemDirectoryW 查詢失敗".to_string());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// System32 下的系統工具絕對路徑（如 `schtasks.exe`），不存在時回傳錯誤。
pub fn system32_tool(name: &str) -> Result<PathBuf, String> {
    let path = system32_dir()?.join(name);
    if !path.is_file() {
        return Err(format!("找不到系統工具: {}", path.display()));
    }
    Ok(path)
}

/// Windows PowerShell 5.1（`System32\WindowsPowerShell\v1.0\powershell.exe`）絕對路徑。
pub fn powershell_exe() -> Result<PathBuf, String> {
    system32_tool("WindowsPowerShell\\v1.0\\powershell.exe")
}

/// `explorer.exe` 絕對路徑（位於 %SystemRoot%，不在 System32）。
pub fn explorer_exe() -> Result<PathBuf, String> {
    let windows_dir = system32_dir()?
        .parent()
        .ok_or_else(|| "無法解析 Windows 目錄".to_string())?
        .to_path_buf();
    let path = windows_dir.join("explorer.exe");
    if !path.is_file() {
        return Err(format!("找不到系統工具: {}", path.display()));
    }
    Ok(path)
}

/// 以「僅 Administrators 與 SYSTEM」的保護型 DACL 建立目錄。
/// 同帳戶 medium-integrity 程序不在 DACL 內，無法讀寫此目錄內容。
/// （已存在時回傳 Ok——呼叫端自行決定後續語意。）
pub fn create_admin_only_dir(dir: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;

    if dir.exists() {
        return Ok(());
    }

    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;FA;;;BA)(A;;FA;;;SY)"),
            1, // SDDL_REVISION_1
            &mut sd,
            None,
        )
        .map_err(|e| format!("SDDL 轉換失敗: {e}"))?;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        // 任何錯誤（含 ALREADY_EXISTS 競態——可能是我們沒建、DACL 未知的目錄）
        // 一律 fail closed
        let result = CreateDirectoryW(PCWSTR(wide.as_ptr()), Some(&sa));
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        result.map_err(|e| format!("建立受保護目錄失敗: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system32_dir_resolves_to_real_directory() {
        let dir = system32_dir().expect("System32 解析不應失敗");
        assert!(dir.is_dir(), "System32 不存在: {}", dir.display());
        // Windows API 可能回傳 system32 小寫
        assert!(
            dir.to_string_lossy().to_lowercase().ends_with("system32"),
            "應指向 System32: {}",
            dir.display()
        );
    }

    #[test]
    fn system_tools_resolve_to_absolute_existing_paths() {
        let schtasks = system32_tool("schtasks.exe").expect("schtasks 應存在");
        assert!(schtasks.is_absolute());

        let ps = powershell_exe().expect("powershell 應存在");
        assert!(ps.is_absolute() && ps.is_file());

        let explorer = explorer_exe().expect("explorer 應存在");
        assert!(explorer.is_absolute() && explorer.is_file());
        assert!(!explorer.ends_with("System32"), "explorer 不在 System32");
    }

    #[test]
    fn missing_tool_fails_closed() {
        assert!(system32_tool("no-such-tool-xyz.exe").is_err());
    }

    #[test]
    fn test_binary_is_not_in_protected_dir() {
        // 測試執行檔來自 target/debug,不在 Program Files 樹
        assert!(!in_protected_program_dir());
    }
}
