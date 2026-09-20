//! Win32 系統匣與 balloon 共用同一圖示，支援提權及免安裝版。
//! HWND 與 HICON 僅由 Tauri 主執行緒操作。
use tauri::{AppHandle, Manager};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const CALLBACK: u32 = WM_APP + 71;
struct TrayWindow(usize);
struct WindowState {
    app: AppHandle,
    icon: HICON,
    tooltip: String,
    taskbar_created: u32,
}

fn error(e: impl std::fmt::Display) -> tauri::Error {
    std::io::Error::other(e.to_string()).into()
}

fn copy_wide<const N: usize>(target: &mut [u16; N], value: &str) {
    let mut offset = 0;
    for c in value.chars() {
        if offset + c.len_utf16() >= N {
            break;
        }
        offset += c.encode_utf16(&mut target[offset..]).len();
    }
    target[offset] = 0;
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

fn make_icon(image: &tauri::image::Image<'_>) -> tauri::Result<HICON> {
    // CreateIcon 使用 BGRA；AND mask 每行須對齊 16 bits。
    let mut bgra = image.rgba().to_vec();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        let alpha = pixel[3] as u16;
        for channel in &mut pixel[..3] {
            *channel = (*channel as u16 * alpha / 255) as u8;
        }
    }
    let mask = vec![0u8; (image.width().div_ceil(16) * 2 * image.height()) as usize];
    unsafe {
        CreateIcon(
            None,
            image.width() as i32,
            image.height() as i32,
            1,
            32,
            mask.as_ptr(),
            bgra.as_ptr(),
        )
        .map_err(error)
    }
}

unsafe fn add(hwnd: HWND, state: &WindowState) -> bool {
    let mut nid = data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = CALLBACK;
    nid.hIcon = state.icon;
    copy_wide(&mut nid.szTip, &state.tooltip);
    Shell_NotifyIconW(NIM_ADD, &nid).as_bool()
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
    if !ptr.is_null() {
        if message == WM_DESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let state = Box::from_raw(ptr);
            let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd));
            let _ = DestroyIcon(state.icon);
            return LRESULT(0);
        }
        // 不跨過 popup 的巢狀訊息迴圈持有 &mut WindowState。
        if message == (*ptr).taskbar_created {
            if !add(hwnd, &*ptr) {
                log::warn!("Explorer 重啟後重建系統匣失敗");
            }
            return LRESULT(0);
        }
        if message == CALLBACK {
            let app = (*ptr).app.clone();
            match lparam.0 as u32 {
                WM_LBUTTONUP | NIN_BALLOONUSERCLICK => crate::show_main_window(&app),
                WM_RBUTTONUP => super::show_context_menu(&app),
                _ => {}
            }
            return LRESULT(0);
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

pub(super) fn build(app: &AppHandle, image: tauri::image::Image<'_>) -> tauri::Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None).map_err(error)?;
        let class = w!("PaceDock.TrayWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(error(windows::core::Error::from_win32()));
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("PaceDock"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(error)?;
        let icon = match make_icon(&image) {
            Ok(icon) => icon,
            Err(e) => {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }
        };
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        // Explorer 是非提權行程；允許它在重啟後通知此隱藏視窗。
        let _ = ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, None);
        let state = Box::new(WindowState {
            app: app.clone(),
            icon,
            tooltip: format!("PaceDock v{}", app.package_info().version),
            taskbar_created,
        });
        let added = add(hwnd, &state);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
        if !added {
            let _ = DestroyWindow(hwnd);
            return Err(error("Shell_NotifyIconW(NIM_ADD) failed"));
        }
        app.manage(TrayWindow(hwnd.0 as usize));
    }
    Ok(())
}

fn on_tray(
    app: &AppHandle,
    action: impl FnOnce(HWND, &mut WindowState) -> tauri::Result<()> + Send + 'static,
) -> tauri::Result<()> {
    let hwnd = app.state::<TrayWindow>().0;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || unsafe {
        let hwnd = HWND(hwnd as *mut _);
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
        let result = if ptr.is_null() {
            Err(error("Tray window closed"))
        } else {
            action(hwnd, &mut *ptr)
        };
        let _ = tx.send(result);
    })?;
    rx.recv().map_err(error)?
}

pub(super) fn update(
    app: &AppHandle,
    image: tauri::image::Image<'static>,
    tooltip: String,
) -> tauri::Result<()> {
    on_tray(app, move |hwnd, state| unsafe {
        let icon = make_icon(&image)?;
        let mut nid = data(hwnd);
        nid.uFlags = NIF_ICON | NIF_TIP;
        nid.hIcon = icon;
        copy_wide(&mut nid.szTip, &tooltip);
        if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            let _ = DestroyIcon(icon);
            return Err(error("Shell_NotifyIconW(NIM_MODIFY) failed"));
        }
        let _ = DestroyIcon(state.icon);
        state.icon = icon;
        state.tooltip = tooltip;
        Ok(())
    })
}

pub(crate) fn notify(app: &AppHandle, title: &str, body: &str) -> tauri::Result<()> {
    let (title, body) = (title.to_string(), body.to_string());
    on_tray(app, move |hwnd, _| unsafe {
        let mut nid = data(hwnd);
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_WARNING;
        copy_wide(&mut nid.szInfoTitle, &title);
        copy_wide(&mut nid.szInfo, &body);
        if Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            Ok(())
        } else {
            Err(error("Shell_NotifyIconW notification failed"))
        }
    })
}

pub(crate) fn shutdown(app: &AppHandle) {
    if let Some(tray) = app.try_state::<TrayWindow>() {
        unsafe {
            let _ = DestroyWindow(HWND(tray.0 as *mut _));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_warning_icon_can_be_created_and_released() {
        let image = super::super::status_icon(true).unwrap();
        let icon = make_icon(&image).unwrap();
        unsafe {
            DestroyIcon(icon).unwrap();
        }
        image::save_buffer(
            std::env::temp_dir().join("pacedock-drift-icon.png"),
            image.rgba(),
            image.width(),
            image.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }

    #[test]
    fn notification_text_is_terminated_without_splitting_surrogate_pairs() {
        let mut text = [0; 4];
        copy_wide(&mut text, "中𠀀文");
        assert_eq!(String::from_utf16(&text[..3]).unwrap(), "中𠀀");
        assert_eq!(text[3], 0);
    }
}
