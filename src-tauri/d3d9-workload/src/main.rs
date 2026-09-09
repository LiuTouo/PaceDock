//! FrameAnchor GPU 基準測試的 D3D9 workload。
//! 與內建 liblava Vulkan workload（lava-triangle.exe）同定位：
//! 全螢幕、無 vsync、不設上限，黑/白交替 Clear+Present 的確定性負載。
//! 被 runner 啟動後持續渲染，直到被終止。
//!
//! CLI：`--fullscreen=<0|1> --width=N --height=N --fps-cap=N --triple-buffer=<0|1>`

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D9::{
    Direct3DCreate9, IDirect3DDevice9, D3DCLEAR_TARGET, D3DCREATE_HARDWARE_VERTEXPROCESSING,
    D3DDEVTYPE_HAL, D3DFMT_UNKNOWN, D3DMULTISAMPLE_NONE, D3DPRESENT_INTERVAL_IMMEDIATE,
    D3DPRESENT_PARAMETERS, D3DSWAPEFFECT_DISCARD, D3D_SDK_VERSION,
};
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW, ShowWindow,
    TranslateMessage, CS_OWNDC, MSG, PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WM_QUIT,
    WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

struct Options {
    fullscreen: bool,
    width: u32,
    height: u32,
    fps_cap: u32,
    triple_buffer: bool,
}

fn parse_args() -> Options {
    let mut o = Options {
        fullscreen: true,
        width: 640,
        height: 480,
        fps_cap: 0,
        triple_buffer: false,
    };
    for arg in std::env::args().skip(1) {
        let Some((k, v)) = arg.split_once('=') else {
            continue;
        };
        match k {
            "--fullscreen" => o.fullscreen = v != "0",
            "--width" => o.width = v.parse().unwrap_or(640),
            "--height" => o.height = v.parse().unwrap_or(480),
            "--fps-cap" => o.fps_cap = v.parse().unwrap_or(0),
            "--triple-buffer" => o.triple_buffer = v != "0",
            _ => {}
        }
    }
    o
}

const CLASS_NAME: &[u16] = &[
    'F' as u16, 'r' as u16, 'a' as u16, 'm' as u16, 'e' as u16, 'D' as u16, '3' as u16, 'D' as u16,
    '9' as u16, 0,
];

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DESTROY {
        std::process::exit(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() -> ExitCode {
    let opts = parse_args();
    let title = wide(&format!(
        "FrameAnchor D3D9 workload {}x{}",
        opts.width, opts.height
    ));

    unsafe {
        let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or(HMODULE(std::ptr::null_mut()));
        let hinstance: HINSTANCE = hmodule.into();
        let wc = WNDCLASSW {
            style: CS_OWNDC,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: Default::default(),
            hCursor: Default::default(),
            hbrBackground: Default::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(CLASS_NAME.as_ptr()),
        };
        if RegisterClassW(&wc) == 0 {
            return ExitCode::from(2);
        }
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(CLASS_NAME.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            opts.width as i32,
            opts.height as i32,
            None,
            None,
            Some(hinstance),
            None,
        ) {
            Ok(h) => h,
            Err(_) => return ExitCode::from(3),
        };
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        let Some(d3d) = Direct3DCreate9(D3D_SDK_VERSION) else {
            return ExitCode::from(4);
        };
        let params = D3DPRESENT_PARAMETERS {
            BackBufferWidth: if opts.fullscreen { opts.width } else { 0 },
            BackBufferHeight: if opts.fullscreen { opts.height } else { 0 },
            BackBufferFormat: D3DFMT_UNKNOWN,
            BackBufferCount: if opts.triple_buffer { 3 } else { 1 },
            MultiSampleType: D3DMULTISAMPLE_NONE,
            MultiSampleQuality: 0,
            SwapEffect: D3DSWAPEFFECT_DISCARD,
            hDeviceWindow: hwnd,
            Windowed: BOOL((!opts.fullscreen) as _),
            EnableAutoDepthStencil: BOOL(0),
            AutoDepthStencilFormat: D3DFMT_UNKNOWN,
            Flags: 0,
            FullScreen_RefreshRateInHz: 0,
            // 無 vsync、不設上限
            PresentationInterval: D3DPRESENT_INTERVAL_IMMEDIATE as u32,
        };
        let mut device: Option<IDirect3DDevice9> = None;
        if d3d
            .CreateDevice(
                0,
                D3DDEVTYPE_HAL,
                hwnd,
                D3DCREATE_HARDWARE_VERTEXPROCESSING as u32,
                &params as *const D3DPRESENT_PARAMETERS as *mut _,
                &mut device,
            )
            .is_err()
        {
            return ExitCode::from(5);
        }
        let device = device.unwrap();

        let frame_period = frame_period(opts.fps_cap);
        let mut next_frame = std::time::Instant::now();
        let mut black = true;
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
                if msg.message == WM_QUIT {
                    return ExitCode::SUCCESS;
                }
            }
            let color = if black { 0x00000000u32 } else { 0x00FF_FFFFu32 };
            if device
                .Clear(0, std::ptr::null(), D3DCLEAR_TARGET as u32, color, 1.0, 0)
                .is_err()
            {
                return ExitCode::from(6);
            }
            if device
                .Present(
                    std::ptr::null(),
                    std::ptr::null(),
                    HWND::default(),
                    std::ptr::null(),
                )
                .is_err()
            {
                // 裝置遺失（Alt-Tab 等）→ 結束，讓 runner 判定失敗
                return ExitCode::from(7);
            }
            black = !black;
            if let Some(period) = frame_period {
                next_frame += period;
                while let Some(remaining) =
                    next_frame.checked_duration_since(std::time::Instant::now())
                {
                    if remaining > std::time::Duration::from_millis(2) {
                        std::thread::sleep(remaining - std::time::Duration::from_millis(1));
                    } else {
                        std::hint::spin_loop();
                    }
                }
                if next_frame + period < std::time::Instant::now() {
                    next_frame = std::time::Instant::now();
                }
            }
        }
    }
}

fn frame_period(fps_cap: u32) -> Option<std::time::Duration> {
    (fps_cap > 0).then(|| {
        std::time::Duration::from_secs_f64(1.0 / fps_cap as f64)
            .max(std::time::Duration::from_nanos(1))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn high_fps_caps_keep_submillisecond_periods() {
        assert_eq!(frame_period(0), None);
        assert_eq!(
            frame_period(2000).unwrap(),
            std::time::Duration::from_micros(500)
        );
        assert_eq!(
            frame_period(4000).unwrap(),
            std::time::Duration::from_micros(250)
        );
        assert!(!frame_period(u32::MAX).unwrap().is_zero());
    }
}
