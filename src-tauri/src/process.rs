//! 僅保留本工具的 WebView2 孤兒清理。
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, TerminateProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, PROCESS_VM_READ,
};
fn utf16_slice_to_string(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

pub fn kill_orphan_webviews() {
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::{
        PEB, PROCESS_BASIC_INFORMATION, RTL_USER_PROCESS_PARAMETERS,
    };

    let our_dir = format!(
        "{}\\com.frameanchor.app\\EBWebView",
        std::env::var("LOCALAPPDATA").unwrap_or_default()
    );
    let self_pid = std::process::id();

    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut webviews: Vec<u32> = Vec::new();
        let mut other_host_alive = false;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let name = utf16_slice_to_string(&entry.szExeFile).to_lowercase();
                if name == "frameanchor.exe" && entry.th32ProcessID != self_pid {
                    other_host_alive = true;
                } else if name == "msedgewebview2.exe" {
                    webviews.push(entry.th32ProcessID);
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        if other_host_alive {
            return; // 有其他實例在跑：它的 webview 是活的，不該動
        }
        for pid in webviews {
            // 讀 cmdline 比對 user-data-dir
            let cmdline = (|| {
                let Ok(h) = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid)
                else {
                    return None;
                };
                let mut pbi = PROCESS_BASIC_INFORMATION::default();
                let status = NtQueryInformationProcess(
                    h,
                    PROCESSINFOCLASS(0), // ProcessBasicInformation
                    &mut pbi as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                );
                if status.0 < 0 || pbi.PebBaseAddress.is_null() {
                    let _ = CloseHandle(h);
                    return None;
                }
                let mut peb = PEB::default();
                let mut read: usize = 0;
                if ReadProcessMemory(
                    h,
                    pbi.PebBaseAddress as *const _,
                    &mut peb as *mut _ as *mut _,
                    std::mem::size_of::<PEB>(),
                    Some(&mut read),
                )
                .is_err()
                {
                    let _ = CloseHandle(h);
                    return None;
                }
                let mut rtl = RTL_USER_PROCESS_PARAMETERS::default();
                if ReadProcessMemory(
                    h,
                    peb.ProcessParameters as *const _,
                    &mut rtl as *mut _ as *mut _,
                    std::mem::size_of::<RTL_USER_PROCESS_PARAMETERS>(),
                    Some(&mut read),
                )
                .is_err()
                {
                    let _ = CloseHandle(h);
                    return None;
                }
                let len = rtl.CommandLine.Length as usize;
                if rtl.CommandLine.Buffer.is_null() || len == 0 {
                    let _ = CloseHandle(h);
                    return None;
                }
                let mut buf = vec![0u16; len / 2];
                if ReadProcessMemory(
                    h,
                    rtl.CommandLine.Buffer.0 as *const core::ffi::c_void,
                    buf.as_mut_ptr() as *mut _,
                    len,
                    Some(&mut read),
                )
                .is_err()
                {
                    let _ = CloseHandle(h);
                    return None;
                }
                let _ = CloseHandle(h);
                Some(String::from_utf16_lossy(&buf))
            })();
            if let Some(cmd) = cmdline {
                if cmd.contains(&our_dir) {
                    if let Ok(h) = OpenProcess(
                        PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                        false,
                        pid,
                    ) {
                        let _ = TerminateProcess(h, 0);
                        let _ = CloseHandle(h);
                    }
                    log::info!("已清理孤兒 WebView2 子程序 PID {pid}");
                }
            }
        }
    }
}
