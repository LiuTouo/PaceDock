//! 依 PID 找 workload 的 top-level visible window，做兩件事：
//! 1. 停用關閉能力（`guard_close`）：僅內建 Vulkan workload（windowed 與
//!    fullscreen 都做）。用 `GetSystemMenu` + `EnableMenuItem(SC_CLOSE,
//!    MF_GRAYED)` 停用關閉鈕，保留 minimize/maximize，再 `DrawMenuBar` 重繪。
//! 2. 把 client area 調整成使用者設定的 width×height（`find_and_resize`，
//!    僅 Vulkan windowed）。fullscreen 或 D3D9 不調整尺寸。
//!
//! 尺寸調整作法：用 `GetWindowRect` / `GetClientRect` 量出 non-client frame 差異，
//! 再以 `SetWindowPos` 把 outer window 設成 `(width + dx, height + dy)`。
//! 此 delta 法與 DPI 無關，PerMonitorV2 或跨 monitor 都正確；不依賴
//! `AdjustWindowRect*ForDpi`（需另猜 DPI）。
//!
//! `SetWindowPos` 成功不代表 client 真的達標（frame 可能在調整後改變），
//! 因此調整後一律 remeasure client；未達標做 bounded 補正，最終仍不符回 Err
//! 由 caller（runner）log warn。

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawMenuBar, EnableMenuItem, EnumWindows, GetClientRect, GetForegroundWindow, GetSystemMenu,
    GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, SetForegroundWindow, SetWindowPos, ShowWindow, GWL_EXSTYLE, GW_HWNDPREV,
    HWND_TOP, HWND_TOPMOST, MF_BYCOMMAND, MF_GRAYED, SC_CLOSE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_RESTORE, WS_EX_TOPMOST,
};

/// 實體像素矩形（virtual screen 座標，left/top 可為負）。純資料，供配置與完整性測試。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// 兩矩形是否相交（含邊界相鄰亦視為不重疊，僅嚴格重疊才 true）。
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }
}

/// workload 視窗完整性觀測值（純資料，測試注入 fake 不必碰 Win32）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowIntegritySnapshot {
    /// 是否為前景視窗（GetForegroundWindow == hwnd）。
    pub foreground: bool,
    /// 是否最小化（IsIconic）。
    pub minimized: bool,
    /// 外框矩形是否仍在預期位置（含容忍誤差）。
    pub position_ok: bool,
    /// 是否帶 WS_EX_TOPMOST。
    pub topmost: bool,
    /// 是否可見（IsWindowVisible）。
    pub visible: bool,
    /// 是否被遮擋/cloaked（DWMWA_CLOAKED != 0）。
    pub occluded: bool,
}

/// 完整性「良好」判定：前景、非最小化、位置正確、topmost、可見、未遮擋。
pub fn integrity_ok(snap: &WindowIntegritySnapshot) -> bool {
    snap.foreground
        && !snap.minimized
        && snap.position_ok
        && snap.topmost
        && snap.visible
        && !snap.occluded
}

/// 單一「z-order 上方視窗」的遮擋觀測值（純資料，測試不碰 Win32）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OccluderSnapshot {
    pub visible: bool,
    pub minimized: bool,
    pub cloaked: bool,
    pub rect: Rect,
}

/// 是否存在真正「可見、未最小化、未 cloaked、且與 workload 外框相交」的上方視窗。
/// 純函式（抽離判斷供單元測試）：不可見/最小化/cloaked 不構成可見遮擋；
/// 邊界相鄰不算（`Rect::overlaps` 嚴格重疊）。
pub fn covered_by_above(workload_rect: Rect, above: &[OccluderSnapshot]) -> bool {
    above
        .iter()
        .any(|w| w.visible && !w.minimized && !w.cloaked && w.rect.overlaps(&workload_rect))
}

/// workload 視窗操作的注入介面。runner 在內建 Vulkan 時呼叫。
pub trait WorkloadWindow: Send + Sync {
    /// 單次嘗試：依 pid 找 visible top-level window，把 client area 調成
    /// `width`×`height`。
    /// - `Ok(true)`：找到並調整成功。
    /// - `Ok(false)`：尚未找到（window 可能還在建立，可稍後重試）。
    /// - `Err(e)`：找到但調整失敗（重試無益）。
    fn find_and_resize(&self, pid: u32, width: u32, height: u32) -> Result<bool, String>;

    /// 單次嘗試：依 pid 找 visible top-level window，停用其關閉能力
    /// （SC_CLOSE），防使用者誤關 workload。
    /// - `Ok(true)`：找到並已停用關閉。
    /// - `Ok(false)`：尚未找到（window 可能還在建立，可稍後重試）。
    /// - `Err(e)`：找到但停用失敗（重試無益）。
    fn guard_close(&self, pid: u32) -> Result<bool, String>;

    /// 單次嘗試：依 pid 找 visible top-level window，`ShowWindow(SW_RESTORE)` →
    /// `SetWindowPos(HWND_TOPMOST, x, y)`（保留尺寸）→ `SetForegroundWindow`。
    /// 適用所有 spawned workload（內建 Vulkan/D3D9 與自訂 visible top-level），
    /// 但「不」調整尺寸（自訂 exe 不可擅自 resize）。
    /// - `Ok(true)`：找到並已定位。
    /// - `Ok(false)`：尚未找到。
    /// - `Err(e)`：找到但定位失敗（重試無益）。
    fn position_topmost(&self, pid: u32, x: i32, y: i32) -> Result<bool, String>;

    /// 依 pid 找 visible top-level window 的實際外框矩形（實體像素）。
    /// `Ok(None)` = 尚未找到；`Err` = 量測失敗。
    fn outer_rect(&self, pid: u32) -> Result<Option<Rect>, String>;

    /// 觀測 workload 視窗完整性（前景/最小化/位置/topmost/可見/遮擋）。
    /// 找不到 window 時回傳全「不良」快照（foreground=false、visible=false、
    /// position_ok=false、occluded=true），由 caller 的 `integrity_ok` 判定失敗。
    fn integrity(&self, pid: u32, expected: Rect) -> WindowIntegritySnapshot;
}

/// 完整性「位置」比對容忍誤差（實體像素）：DWM/DPI 捨入可能造成 1–2px 偏差。
pub const INTEGRITY_POSITION_TOLERANCE: i32 = 2;

/// 由目前 outer/client 尺寸算出「使 client area 變成 (width,height)」所需
/// 的 outer window 尺寸。純函式，獨立測試。
pub fn outer_size_for_client(
    window_w: i32,
    window_h: i32,
    client_w: i32,
    client_h: i32,
    width: u32,
    height: u32,
) -> (i32, i32) {
    let frame_w = window_w - client_w;
    let frame_h = window_h - client_h;
    (width as i32 + frame_w, height as i32 + frame_h)
}

/// 生產實作：EnumWindows + GetClientRect/SetWindowPos。
pub struct RealWorkloadWindow;

impl RealWorkloadWindow {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealWorkloadWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkloadWindow for RealWorkloadWindow {
    fn find_and_resize(&self, pid: u32, width: u32, height: u32) -> Result<bool, String> {
        let Some(hwnd) = find_top_level_window(pid) else {
            return Ok(false);
        };
        resize_to_client(hwnd, width, height)?;
        Ok(true)
    }

    fn guard_close(&self, pid: u32) -> Result<bool, String> {
        let Some(hwnd) = find_top_level_window(pid) else {
            return Ok(false);
        };
        disable_close(hwnd)?;
        Ok(true)
    }

    fn position_topmost(&self, pid: u32, x: i32, y: i32) -> Result<bool, String> {
        let Some(hwnd) = find_top_level_window(pid) else {
            return Ok(false);
        };
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_SHOWWINDOW,
            )
            .map_err(|e| format!("SetWindowPos(topmost): {e}"))?;
            let _ = SetForegroundWindow(hwnd);
        }
        Ok(true)
    }

    fn outer_rect(&self, pid: u32) -> Result<Option<Rect>, String> {
        let Some(hwnd) = find_top_level_window(pid) else {
            return Ok(None);
        };
        let mut wr = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut wr).map_err(|e| format!("GetWindowRect: {e}"))?;
        }
        Ok(Some(Rect::new(wr.left, wr.top, wr.right, wr.bottom)))
    }

    fn integrity(&self, pid: u32, expected: Rect) -> WindowIntegritySnapshot {
        let Some(hwnd) = find_top_level_window(pid) else {
            return WindowIntegritySnapshot {
                occluded: true,
                ..Default::default()
            };
        };
        let mut wr = RECT::default();
        let (position_ok, workload_rect) = unsafe {
            if GetWindowRect(hwnd, &mut wr).is_err() {
                (false, None)
            } else {
                let rect = Rect::new(wr.left, wr.top, wr.right, wr.bottom);
                let t = INTEGRITY_POSITION_TOLERANCE;
                let ok = (wr.left - expected.left).abs() <= t
                    && (wr.top - expected.top).abs() <= t
                    && (wr.right - expected.right).abs() <= t
                    && (wr.bottom - expected.bottom).abs() <= t;
                (ok, Some(rect))
            }
        };
        unsafe {
            let foreground = GetForegroundWindow() == hwnd;
            let minimized = IsIconic(hwnd).as_bool();
            let visible = IsWindowVisible(hwnd).as_bool();
            let topmost = (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0) != 0;
            let mut cloaked: u32 = 0;
            // DWM cloaking（最小化/shell 隱藏）— 只反映隱藏，不反映被其他視窗蓋住。
            let self_cloaked = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .map(|_| cloaked != 0)
            .unwrap_or(false);
            // 實際遮擋：z-order 位於 workload 之上的可見視窗與其外框相交。
            let covered = workload_rect
                .map(|r| covered_by_above(r, &collect_above_windows(hwnd)))
                .unwrap_or(false);
            WindowIntegritySnapshot {
                foreground,
                minimized,
                position_ok,
                topmost,
                visible,
                occluded: self_cloaked || covered,
            }
        }
    }
}

struct EnumCtx {
    pid: u32,
    found: Option<HWND>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumCtx);
    if ctx.found.is_some() {
        return BOOL(0);
    }
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == ctx.pid {
        ctx.found = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

/// 找 pid 的「第一個」visible top-level window。
fn find_top_level_window(pid: u32) -> Option<HWND> {
    let mut ctx = EnumCtx { pid, found: None };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.found
}

/// 遮擋掃描的 z-order 上方視窗數上限：避免病態 z-order 拖慢每 100ms 的完整性輪詢。
const OCCLUSION_SCAN_MAX: usize = 64;

/// 收集 z-order 位於 `hwnd` 之上（GW_HWNDPREV 鏈）的 top-level window 遮擋觀測值。
/// workload 為 HWND_TOPMOST 時，其上方只會有其他 topmost（或更高）視窗；普通視窗
/// 與 PaceDock compact 視窗都在下方，不會被納入。
fn collect_above_windows(hwnd: HWND) -> Vec<OccluderSnapshot> {
    let mut out = Vec::new();
    unsafe {
        let mut cur = GetWindow(hwnd, GW_HWNDPREV).unwrap_or_default();
        while !cur.0.is_null() && out.len() < OCCLUSION_SCAN_MAX {
            let visible = IsWindowVisible(cur).as_bool();
            let minimized = IsIconic(cur).as_bool();
            let mut cloaked: u32 = 0;
            let cloaked_flag = DwmGetWindowAttribute(
                cur,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .map(|_| cloaked != 0)
            .unwrap_or(false);
            let mut wr = RECT::default();
            let rect = if GetWindowRect(cur, &mut wr).is_ok() {
                Rect::new(wr.left, wr.top, wr.right, wr.bottom)
            } else {
                Rect::default()
            };
            out.push(OccluderSnapshot {
                visible,
                minimized,
                cloaked: cloaked_flag,
                rect,
            });
            cur = GetWindow(cur, GW_HWNDPREV).unwrap_or_default();
        }
    }
    out
}

/// 停用 `SC_CLOSE`：`GetSystemMenu` + `EnableMenuItem(SC_CLOSE, MF_GRAYED)` 停用
/// 關閉鈕（保留 minimize/maximize），再 `DrawMenuBar` 重繪標題列。idempotent：
/// 重複停用無害。不可移除整個 `WS_SYSMENU`。
fn disable_close(hwnd: HWND) -> Result<(), String> {
    unsafe {
        let menu = GetSystemMenu(hwnd, false);
        if menu.0.is_null() {
            return Err("GetSystemMenu 傳回 null（視窗無系統選單）".to_string());
        }
        let prev = EnableMenuItem(menu, SC_CLOSE, MF_BYCOMMAND | MF_GRAYED);
        if prev.0 == -1 {
            return Err("EnableMenuItem(SC_CLOSE) 失敗".to_string());
        }
        // 重繪讓停用狀態反映到標題列 X；失敗不影響已完成的停用（cosmetic）
        let _ = DrawMenuBar(hwnd);
        Ok(())
    }
}

/// 初次調整後 client area 仍未達指定值時，最多再補正的次數（bounded correction）。
const RESIZE_MAX_CORRECTIONS: u32 = 3;

/// 驅動「量測 → 調整 → 驗證」的收斂迴圈（純邏輯，不含 Win32）。`measure` 回傳
/// `(outer_w, outer_h, client_w, client_h)`；`set` 設定 outer 尺寸。初次量測若
/// 已達標即成功；否則調整後 remeasure，未達標依最新 frame delta 補正，最多
/// `max_corrections` 次；最終仍不符回 Err。
fn resize_verify_loop(
    measure: &dyn Fn() -> Result<(i32, i32, i32, i32), String>,
    set: &dyn Fn(i32, i32) -> Result<(), String>,
    width: u32,
    height: u32,
    max_corrections: u32,
) -> Result<(), String> {
    let target_w = width as i32;
    let target_h = height as i32;
    // 初次調整 + max_corrections 次補正 = 最多 max_corrections+1 次 set
    let mut sets_left = max_corrections.saturating_add(1);
    loop {
        let (ow, oh, cw, ch) = measure()?;
        if cw == target_w && ch == target_h {
            return Ok(());
        }
        if sets_left == 0 {
            return Err(format!(
                "client area 未達 {width}×{height}（實際 {cw}×{ch}）"
            ));
        }
        let (new_ow, new_oh) = outer_size_for_client(ow, oh, cw, ch, width, height);
        set(new_ow, new_oh)?;
        sets_left -= 1;
    }
}

fn resize_to_client(hwnd: HWND, width: u32, height: u32) -> Result<(), String> {
    let measure = || -> Result<(i32, i32, i32, i32), String> {
        let mut wr = RECT::default();
        let mut cr = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut wr).map_err(|e| format!("GetWindowRect: {e}"))?;
            GetClientRect(hwnd, &mut cr).map_err(|e| format!("GetClientRect: {e}"))?;
        }
        Ok((
            wr.right - wr.left,
            wr.bottom - wr.top,
            cr.right - cr.left,
            cr.bottom - cr.top,
        ))
    };
    let set = |w: i32, h: i32| -> Result<(), String> {
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                w,
                h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .map_err(|e| format!("SetWindowPos: {e}"))
        }
    };
    resize_verify_loop(&measure, &set, width, height, RESIZE_MAX_CORRECTIONS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_overlaps_detects_intersection_and_separation() {
        let a = Rect::new(0, 0, 100, 100);
        // 嚴格重疊
        assert!(a.overlaps(&Rect::new(50, 50, 150, 150)));
        // 邊界相鄰（不重疊）
        assert!(!a.overlaps(&Rect::new(100, 0, 200, 100)));
        assert!(!a.overlaps(&Rect::new(0, 100, 100, 200)));
        // 完全分離
        assert!(!a.overlaps(&Rect::new(200, 200, 300, 300)));
    }

    #[test]
    fn integrity_ok_requires_all_good() {
        let good = WindowIntegritySnapshot {
            foreground: true,
            minimized: false,
            position_ok: true,
            topmost: true,
            visible: true,
            occluded: false,
        };
        assert!(integrity_ok(&good));
        // 任一異常即失敗
        assert!(!integrity_ok(&WindowIntegritySnapshot {
            foreground: false,
            ..good
        }));
        assert!(!integrity_ok(&WindowIntegritySnapshot {
            minimized: true,
            ..good
        }));
        assert!(!integrity_ok(&WindowIntegritySnapshot {
            position_ok: false,
            ..good
        }));
        assert!(!integrity_ok(&WindowIntegritySnapshot {
            topmost: false,
            ..good
        }));
        assert!(!integrity_ok(&WindowIntegritySnapshot {
            visible: false,
            ..good
        }));
        assert!(!integrity_ok(&WindowIntegritySnapshot {
            occluded: true,
            ..good
        }));
    }

    #[test]
    fn covered_by_above_flags_only_real_occluders() {
        let wr = Rect::new(0, 0, 1280, 720);
        let occ = OccluderSnapshot {
            visible: true,
            minimized: false,
            cloaked: false,
            rect: Rect::new(100, 100, 200, 200),
        };
        // 可見、未最小化、未 cloaked、相交 → 遮擋
        assert!(covered_by_above(wr, &[occ]));
        // 不相交 → 不遮擋
        assert!(!covered_by_above(
            wr,
            &[OccluderSnapshot {
                rect: Rect::new(1300, 0, 1400, 100),
                ..occ
            }]
        ));
        // 邊界相鄰（不嚴格重疊）→ 不遮擋
        assert!(!covered_by_above(
            wr,
            &[OccluderSnapshot {
                rect: Rect::new(1280, 0, 1400, 100),
                ..occ
            }]
        ));
        // 不可見 / 最小化 / cloaked → 不遮擋
        assert!(!covered_by_above(
            wr,
            &[OccluderSnapshot {
                visible: false,
                ..occ
            }]
        ));
        assert!(!covered_by_above(
            wr,
            &[OccluderSnapshot {
                minimized: true,
                ..occ
            }]
        ));
        assert!(!covered_by_above(
            wr,
            &[OccluderSnapshot {
                cloaked: true,
                ..occ
            }]
        ));
        // 空清單 → 不遮擋
        assert!(!covered_by_above(wr, &[]));
    }

    #[test]
    fn outer_size_preserves_frame_delta() {
        // 假設 outer 800×600、client 784×561（frame 寬 16、高 39）
        let (w, h) = outer_size_for_client(800, 600, 784, 561, 640, 480);
        assert_eq!(w, 640 + 16);
        assert_eq!(h, 480 + 39);
    }

    #[test]
    fn outer_size_no_frame_returns_exact() {
        // borderless：client == outer
        assert_eq!(
            outer_size_for_client(640, 480, 640, 480, 640, 480),
            (640, 480)
        );
    }

    /// 依序餵給 `resize_verify_loop` 的 measure 值，回傳 (result, set 呼叫紀錄)。
    fn run_resize(
        measures: Vec<(i32, i32, i32, i32)>,
        max_corrections: u32,
    ) -> (Result<(), String>, Vec<(i32, i32)>) {
        let idx = std::cell::Cell::new(0usize);
        let sets = std::cell::RefCell::new(Vec::new());
        let measure = || -> Result<(i32, i32, i32, i32), String> {
            let i = idx.get();
            idx.set(i + 1);
            measures
                .get(i)
                .copied()
                .ok_or_else(|| "measure exhausted".to_string())
        };
        let set = |w: i32, h: i32| -> Result<(), String> {
            sets.borrow_mut().push((w, h));
            Ok(())
        };
        let r = resize_verify_loop(&measure, &set, 640, 480, max_corrections);
        (r, sets.into_inner())
    }

    #[test]
    fn resize_verify_exact_success_no_set() {
        // 初次量測 client 即達標：不呼叫 set
        let (r, sets) = run_resize(vec![(800, 600, 640, 480)], 3);
        assert_eq!(r, Ok(()));
        assert!(sets.is_empty());
    }

    #[test]
    fn resize_verify_first_set_achieves_target() {
        // 初次 mismatch → set 一次 → remeasure 達標
        let (r, sets) = run_resize(vec![(800, 600, 784, 561), (656, 519, 640, 480)], 3);
        assert_eq!(r, Ok(()));
        assert_eq!(sets, vec![(656, 519)]);
    }

    #[test]
    fn resize_verify_corrects_after_first_mismatch() {
        // 第一次 set 後 client 仍 mismatch（frame delta 改變）→ 依最新 delta 補正成功
        let measures = vec![
            (800, 600, 784, 561), // frame 16×39 → set(656, 519)
            (656, 519, 624, 441), // frame 變 32×78 → set(672, 558)
            (672, 558, 640, 480), // 達標
        ];
        let (r, sets) = run_resize(measures, 3);
        assert_eq!(r, Ok(()));
        assert_eq!(sets, vec![(656, 519), (672, 558)]);
    }

    #[test]
    fn resize_verify_bounded_correction_failure() {
        // 無補正（max_corrections=0）：初次 set 後仍 mismatch → Err
        let (r, sets) = run_resize(vec![(800, 600, 784, 561), (656, 519, 624, 441)], 0);
        assert!(r.is_err(), "補正用盡仍不符必須回 Err");
        assert_eq!(sets, vec![(656, 519)]);
    }

    #[test]
    fn resize_verify_exhausts_corrections_then_errs() {
        // max_corrections=2：初次 + 2 補正 = 3 次 set，之後仍 mismatch → Err
        let measures = vec![
            (800, 600, 784, 561),
            (656, 519, 624, 441),
            (672, 558, 624, 441),
            (672, 558, 624, 441),
        ];
        let (r, sets) = run_resize(measures, 2);
        assert!(r.is_err());
        assert_eq!(sets.len(), 3, "初次 + 2 次補正共 3 次 set");
    }
}
