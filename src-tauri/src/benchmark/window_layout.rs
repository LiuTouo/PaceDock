//! 基準測試視窗配置：把 workload 固定在 PaceDock 主視窗所在 monitor 的
//! rcWork（工作區，排除工作列）左上，PaceDock 暫時 compact 成 480×300
//! logical px、右下角 16 logical px 邊距（同螢幕）。
//!
//! 空間預檢：以 DPI 換算 compact 尺寸與 workload config client size，檢查兩者在
//! rcWork 內不重疊；不足 → 穩定錯誤碼立即拒絕（禁止自動縮 workload）。workload
//! HWND 建立後再用實際 outer rect 複檢。
//!
//! 主視窗：`MainWindowController` 注入抽象（production 用 Tauri + Win32，
//! 測試注入 fake）。`WindowLayoutGuard` RAII 於正常/失敗/取消/panic unwind 一律
//! 還原主視窗原位置/尺寸/maximized 狀態（`GetWindowPlacement`/`SetWindowPlacement`）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition, PhysicalSize};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowPlacement, WINDOWPLACEMENT};

use crate::error::codes;

use super::window_win::Rect;

/// compact 視窗寬（logical px）。
pub const COMPACT_WIDTH_LOGICAL: u32 = 480;
/// compact 視窗高（logical px）。
pub const COMPACT_HEIGHT_LOGICAL: u32 = 300;
/// compact 視窗距 rcWork 右/下邊的自適應邊距下限（logical px）。
pub const COMPACT_MARGIN_MIN_LOGICAL: u32 = 16;
/// compact 視窗距 rcWork 右/下邊的自適應邊距上限（logical px）。
pub const COMPACT_MARGIN_MAX_LOGICAL: u32 = 32;
/// 自適應邊距基準（logical px）：1080p 工作區較短邊對應值。
pub const COMPACT_MARGIN_BASE_LOGICAL: u32 = 24;
/// 基準對應的工作區較短邊（logical px）：1080p 全高（不含工作列，clamp 吸收差異）。
const COMPACT_MARGIN_REF_DIM_LOGICAL: u32 = 1080;
/// 主視窗還原用的 min-size（logical px，與 tauri.conf.json `minWidth/minHeight` 一致；
/// app 從不修改 min size，故還原到此常數即等於原值）。
pub const MIN_SIZE_DEFAULT_W: f64 = 1280.0;
pub const MIN_SIZE_DEFAULT_H: f64 = 720.0;

/// DPI → 縮放因子（96 = 1.0）。
pub fn scale_from_dpi(dpi: u32) -> f64 {
    dpi as f64 / 96.0
}

/// logical px → 實體 px（四捨五入）。
pub fn logical_to_physical(logical: u32, scale: f64) -> i32 {
    (logical as f64 * scale).round() as i32
}

/// 自適應 compact 邊距（logical px）：以工作區較短邊線性縮放（1080p ≈ 24），
/// clamp 16..32。螢幕愈大 margin 愈大（至多 32），愈小收斂至 16，避免貼邊或過度內縮。
pub fn compact_margin_logical(rc_work: Rect, dpi: u32) -> u32 {
    let scale = scale_from_dpi(dpi);
    let min_dim_logical = (rc_work.width().min(rc_work.height()) as f64 / scale).round() as u32;
    let margin = (min_dim_logical as u64 * COMPACT_MARGIN_BASE_LOGICAL as u64
        / COMPACT_MARGIN_REF_DIM_LOGICAL as u64) as u32;
    margin.clamp(COMPACT_MARGIN_MIN_LOGICAL, COMPACT_MARGIN_MAX_LOGICAL)
}

/// 佈局規劃結果（實體像素）。
#[derive(Clone, Copy, Debug)]
pub struct LayoutPlan {
    /// monitor 工作區（排除工作列）。
    pub rc_work: Rect,
    /// workload 外框估計矩形（rcWork 左上，client size 換算；HWND 建立後以實測為準）。
    pub workload_rect: Rect,
    /// PaceDock compact 矩形（rcWork 右下角、含邊距）。
    pub compact_rect: Rect,
    /// 縮放因子（dpi/96）。
    pub scale: f64,
}

/// 空間預檢：在 rcWork 內配置 workload（左上）與 compact 視窗（右下），兩者不可重疊。
/// 不足 → [`codes::BENCHMARK_WINDOW_SPACE_INSUFFICIENT`]。
pub fn plan_layout(
    rc_work: Rect,
    dpi: u32,
    workload_client_logical: (u32, u32),
) -> Result<LayoutPlan, String> {
    let scale = scale_from_dpi(dpi);
    let compact_w = logical_to_physical(COMPACT_WIDTH_LOGICAL, scale);
    let compact_h = logical_to_physical(COMPACT_HEIGHT_LOGICAL, scale);
    let margin = logical_to_physical(compact_margin_logical(rc_work, dpi), scale);
    let wl_w = logical_to_physical(workload_client_logical.0, scale);
    let wl_h = logical_to_physical(workload_client_logical.1, scale);

    // compact 視窗（含兩側邊距）本身必須放得進 rcWork
    if compact_w + 2 * margin > rc_work.width() || compact_h + 2 * margin > rc_work.height() {
        return Err(codes::BENCHMARK_WINDOW_SPACE_INSUFFICIENT.to_string());
    }

    let workload_rect = Rect::new(
        rc_work.left,
        rc_work.top,
        rc_work.left + wl_w,
        rc_work.top + wl_h,
    );
    let compact_rect = Rect::new(
        rc_work.right - margin - compact_w,
        rc_work.bottom - margin - compact_h,
        rc_work.right - margin,
        rc_work.bottom - margin,
    );

    // workload 必須放得進 rcWork，且與 compact 不重疊（不自動縮 workload）
    if wl_w > rc_work.width() || wl_h > rc_work.height() || workload_rect.overlaps(&compact_rect) {
        return Err(codes::BENCHMARK_WINDOW_SPACE_INSUFFICIENT.to_string());
    }

    Ok(LayoutPlan {
        rc_work,
        workload_rect,
        compact_rect,
        scale,
    })
}

/// workload HWND 建立後，以實際 outer rect 複檢：必須落在 rcWork 內且不與 compact 重疊。
pub fn verify_workload_fits(
    workload_outer: Rect,
    compact_rect: Rect,
    rc_work: Rect,
) -> Result<(), String> {
    if workload_outer.left < rc_work.left
        || workload_outer.top < rc_work.top
        || workload_outer.right > rc_work.right
        || workload_outer.bottom > rc_work.bottom
        || workload_outer.overlaps(&compact_rect)
    {
        return Err(codes::BENCHMARK_WINDOW_SPACE_INSUFFICIENT.to_string());
    }
    Ok(())
}

/// monitor 資訊（rcWork + DPI），供空間預檢與定位。
#[derive(Clone, Copy, Debug)]
pub struct MonitorInfo {
    pub rc_work: Rect,
    pub dpi: u32,
}

/// 主視窗原始狀態快照（還原用）。`WINDOWPLACEMENT` 同時保留 restored bounds
/// 與 maximized 旗標，故最大化狀態也能精確還原。
pub struct MainWindowSnapshot {
    pub placement: WINDOWPLACEMENT,
    /// 測試前主視窗所在 monitor 的 rcWork（置中還原用）。
    pub rc_work: Rect,
}

/// 依 `rc_work` 置中 `placement` 的 rcNormalPosition：重算 left/top，
/// right/bottom 隨之平移，故 width/height 不變；showCmd/maximized 等其餘欄位保留。
/// 支援負座標螢幕。
pub fn center_placement_in(placement: &mut WINDOWPLACEMENT, rc_work: Rect) {
    let w = placement.rcNormalPosition.right - placement.rcNormalPosition.left;
    let h = placement.rcNormalPosition.bottom - placement.rcNormalPosition.top;
    placement.rcNormalPosition.left = rc_work.left + (rc_work.width() - w) / 2;
    placement.rcNormalPosition.top = rc_work.top + (rc_work.height() - h) / 2;
    placement.rcNormalPosition.right = placement.rcNormalPosition.left + w;
    placement.rcNormalPosition.bottom = placement.rcNormalPosition.top + h;
}

/// 主視窗 compact/還原注入抽象。runner 在 spawn_blocking 執行緒呼叫；
/// production 用 [`RealMainWindowController`]，測試注入 fake。
pub trait MainWindowController: Send + Sync {
    /// 主視窗所在 monitor 的 rcWork 與有效 DPI。
    fn monitor_info(&self) -> Result<MonitorInfo, String>;
    /// 快照主視窗目前位置/尺寸/maximized（`GetWindowPlacement`）。
    fn snapshot(&self) -> Result<MainWindowSnapshot, String>;
    /// 解除最大化並切到 compact（解除 min-size 限制 + 設尺寸/位置）。
    fn apply_compact(&self, rect: Rect) -> Result<(), String>;
    /// 還原快照（min-size + `SetWindowPlacement`）。
    fn restore(&self, snap: &MainWindowSnapshot) -> Result<(), String>;
    /// 要求本次 restore 置中於測試前 monitor 的 rcWork（workload 失去前景時呼叫）。
    /// 一次性：下次 restore 消費後清除，不影響後續 benchmark。
    fn request_center_restore(&self);
}

/// production 實作：Tauri `AppHandle` + Win32。
pub struct RealMainWindowController {
    app: AppHandle,
    center_restore: AtomicBool,
}

impl RealMainWindowController {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            center_restore: AtomicBool::new(false),
        }
    }

    fn main_window(&self) -> Result<tauri::WebviewWindow, String> {
        self.app
            .get_webview_window("main")
            .ok_or_else(|| "main window not found".to_string())
    }
}

impl MainWindowController for RealMainWindowController {
    fn monitor_info(&self) -> Result<MonitorInfo, String> {
        let win = self.main_window()?;
        let hwnd = win.hwnd().map_err(|e| format!("hwnd: {e}"))?;
        unsafe {
            let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                rcMonitor: Default::default(),
                rcWork: Default::default(),
                dwFlags: 0,
            };
            if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
                return Err("GetMonitorInfoW 失敗".to_string());
            }
            let (mut dx, mut dy) = (0u32, 0u32);
            GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy)
                .map_err(|e| format!("GetDpiForMonitor: {e}"))?;
            let dpi = dx.max(96);
            Ok(MonitorInfo {
                rc_work: Rect::new(
                    mi.rcWork.left,
                    mi.rcWork.top,
                    mi.rcWork.right,
                    mi.rcWork.bottom,
                ),
                dpi,
            })
        }
    }

    fn snapshot(&self) -> Result<MainWindowSnapshot, String> {
        let win = self.main_window()?;
        let hwnd = win.hwnd().map_err(|e| format!("hwnd: {e}"))?;
        let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        unsafe {
            GetWindowPlacement(hwnd, &mut wp).map_err(|e| format!("GetWindowPlacement: {e}"))?;
        }
        let rc_work = self.monitor_info()?.rc_work;
        Ok(MainWindowSnapshot {
            placement: wp,
            rc_work,
        })
    }

    fn apply_compact(&self, rect: Rect) -> Result<(), String> {
        let win = self.main_window()?;
        if let Ok(hd) = win.hwnd() {
            log::info!("apply_compact: hwnd=0x{:x}", hd.0 as usize);
        }
        let _ = win.unmaximize();
        // 解除預設 min-size（900×600 logical）限制，否則無法縮到 compact
        let _ = win.set_min_size(Some(LogicalSize::new(1.0, 1.0)));
        win.set_size(PhysicalSize::new(rect.width() as u32, rect.height() as u32))
            .map_err(|e| format!("set_size: {e}"))?;
        win.set_position(PhysicalPosition::new(rect.left, rect.top))
            .map_err(|e| format!("set_position: {e}"))?;
        log::debug!(
            "主視窗切 compact: {}x{} at ({},{})",
            rect.width(),
            rect.height(),
            rect.left,
            rect.top
        );
        Ok(())
    }

    fn restore(&self, snap: &MainWindowSnapshot) -> Result<(), String> {
        let win = self.main_window()?;
        let mut placement = snap.placement;
        // 一次性旗標：本次 restore 若要求置中則重算 left/top，其餘（含 showCmd）保留。
        if self.center_restore.swap(false, Ordering::SeqCst) {
            center_placement_in(&mut placement, snap.rc_work);
        }
        let p = placement.rcNormalPosition;
        let w = (p.right - p.left) as u32;
        let h = (p.bottom - p.top) as u32;
        let x = p.left;
        let y = p.top;
        let show_max = placement.showCmd == 3u32; // SW_SHOWMAXIMIZED
        if let Ok(hd) = win.hwnd() {
            log::info!(
                "restore: hwnd=0x{:x} target {w}x{h} at ({x},{y})",
                hd.0 as usize
            );
        }
        // 主執行緒套用還原；之後 1.5/3/5 秒各重套一次並實測。實測發現 tao 內部
        // 快取會在 restore 約 2 秒後以舊 compact 幾何蓋回（非本 repo 任何路徑），
        // 重套可壓回目標幾何；冪等操作，重複無副作用。
        let app = self.app.clone();
        for delay_ms in [0u64, 1500, 3000, 5000] {
            let app = app.clone();
            std::thread::Builder::new()
                .name("layout-restore".into())
                .spawn(move || {
                    if delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                    let app_inner = app.clone();
                    let _ = app.run_on_main_thread(move || {
                        if let Some(win) = app_inner.get_webview_window("main") {
                            if delay_ms == 0 {
                                let _ = win.unmaximize();
                            }
                            let _ = win.set_min_size(Some(LogicalSize::new(
                                MIN_SIZE_DEFAULT_W,
                                MIN_SIZE_DEFAULT_H,
                            )));
                            if let Err(e) = win.set_size(PhysicalSize::new(w, h)) {
                                log::error!("restore set_size 失敗: {e}");
                            }
                            if let Err(e) = win.set_position(PhysicalPosition::new(x, y)) {
                                log::error!("restore set_position 失敗: {e}");
                            }
                            if show_max && delay_ms == 0 {
                                let _ = win.maximize();
                            }
                            if let Ok(hd) = win.hwnd() {
                                let mut rect = Default::default();
                                if unsafe {
                                    windows::Win32::UI::WindowsAndMessaging::GetWindowRect(
                                        hd, &mut rect,
                                    )
                                }
                                .is_ok()
                                {
                                    log::info!(
                                        "restore[{delay_ms}ms] 實測: hwnd=0x{:x} rect {}x{} at ({},{})",
                                        hd.0 as usize,
                                        rect.right - rect.left,
                                        rect.bottom - rect.top,
                                        rect.left,
                                        rect.top
                                    );
                                }
                            }
                        } else {
                            log::error!("restore: main window not found（主執行緒）");
                        }
                    });
                })
                .map_err(|e| format!("restore retry spawn: {e}"))?;
        }
        log::info!(
            "主視窗已還原: {w}x{h} at ({x},{y}) showCmd={}",
            placement.showCmd
        );
        Ok(())
    }

    fn request_center_restore(&self) {
        self.center_restore.store(true, Ordering::SeqCst);
    }
}

/// RAII 還原守衛：drop 時（正常回傳/失敗/取消/panic unwind）還原主視窗。
pub struct WindowLayoutGuard {
    controller: Arc<dyn MainWindowController>,
    snapshot: MainWindowSnapshot,
    /// 本次佈局規劃（供 workload 定位與完整性比對）。
    pub plan: LayoutPlan,
}

impl Drop for WindowLayoutGuard {
    fn drop(&mut self) {
        if let Err(e) = self.controller.restore(&self.snapshot) {
            log::error!("主視窗 compact 還原失敗: {e}");
        }
    }
}

/// 空間預檢 + 快照 + 切 compact + 回傳 RAII guard。任一步失敗即還原並回穩定錯誤碼。
pub fn prepare_window_layout(
    controller: Arc<dyn MainWindowController>,
    workload_client_logical: (u32, u32),
) -> Result<WindowLayoutGuard, String> {
    let mon = controller.monitor_info()?;
    let plan = plan_layout(mon.rc_work, mon.dpi, workload_client_logical)?;
    let snapshot = controller.snapshot()?;
    if let Err(e) = controller.apply_compact(plan.compact_rect) {
        let _ = controller.restore(&snapshot);
        return Err(e);
    }
    Ok(WindowLayoutGuard {
        controller,
        snapshot,
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100% DPI（scale=1.0）：1920×1080 rcWork、1280×720 client。
    fn rc1080() -> Rect {
        Rect::new(0, 0, 1920, 1080)
    }

    #[test]
    fn plan_100pct_simple_layout() {
        let plan = plan_layout(rc1080(), 96, (1280, 720)).unwrap();
        // workload 左上 = rcWork 左上
        assert_eq!(plan.workload_rect, Rect::new(0, 0, 1280, 720));
        // compact 480×300、右下 24px 自適應邊距
        assert_eq!(
            plan.compact_rect,
            Rect::new(1920 - 24 - 480, 1080 - 24 - 300, 1920 - 24, 1080 - 24)
        );
        assert!(!plan.workload_rect.overlaps(&plan.compact_rect));
    }

    #[test]
    fn plan_125pct_scales_compact_and_client() {
        // 125% → scale 1.25；rcWork 2560×1440（實體）
        let rc = Rect::new(0, 0, 2560, 1440);
        let plan = plan_layout(rc, 120, (1280, 720)).unwrap();
        assert_eq!(plan.scale, 1.25);
        // client 1280×720 logical → 1600×900 實體
        assert_eq!(plan.workload_rect, Rect::new(0, 0, 1600, 900));
        // compact 480×300 logical → 600×375 實體；margin 25 logical → 31 實體
        assert_eq!(
            plan.compact_rect,
            Rect::new(2560 - 31 - 600, 1440 - 31 - 375, 2529, 1409)
        );
        assert!(!plan.workload_rect.overlaps(&plan.compact_rect));
    }

    #[test]
    fn plan_negative_origin_monitor() {
        // 副螢幕在左上（負座標）：rcWork.left=-1920，物理座標正確平移
        let rc = Rect::new(-1920, 0, 0, 1080);
        let plan = plan_layout(rc, 96, (1280, 720)).unwrap();
        assert_eq!(plan.workload_rect, Rect::new(-1920, 0, -640, 720));
        assert_eq!(
            plan.compact_rect,
            Rect::new(-24 - 480, 1080 - 24 - 300, -24, 1080 - 24)
        );
    }

    #[test]
    fn plan_insufficient_space_rejects() {
        // rcWork 太小：compact 480×300 + 邊距放不下 → 拒絕
        let tiny = Rect::new(0, 0, 400, 300);
        assert_eq!(
            plan_layout(tiny, 96, (320, 200)).unwrap_err(),
            codes::BENCHMARK_WINDOW_SPACE_INSUFFICIENT
        );
    }

    #[test]
    fn plan_overlap_rejects_without_shrinking() {
        // workload client 太大（幾乎填滿 rcWork）→ 與 compact 重疊 → 拒絕（不縮 workload）
        let rc = Rect::new(0, 0, 1920, 1080);
        let err = plan_layout(rc, 96, (1900, 1060)).unwrap_err();
        assert_eq!(err, codes::BENCHMARK_WINDOW_SPACE_INSUFFICIENT);
    }

    #[test]
    fn plan_tall_narrow_fits_when_horizontally_separated() {
        // 高但不寬的 workload：與 compact 垂直分離（不重疊）即通過
        let rc = Rect::new(0, 0, 1920, 1080);
        let plan = plan_layout(rc, 96, (400, 800)).unwrap();
        assert!(!plan.workload_rect.overlaps(&plan.compact_rect));
    }

    #[test]
    fn verify_workload_fits_accepts_and_rejects() {
        let rc = rc1080();
        let compact = Rect::new(1424, 764, 1904, 1064);
        // 符合預期 → 通過
        assert!(verify_workload_fits(Rect::new(0, 0, 1280, 720), compact, rc).is_ok());
        // 外框超出 rcWork 右緣 → 拒絕
        assert!(verify_workload_fits(Rect::new(0, 0, 2000, 720), compact, rc).is_err());
        // 與 compact 重疊 → 拒絕
        assert!(verify_workload_fits(Rect::new(0, 0, 1500, 800), compact, rc).is_err());
    }

    #[test]
    fn scale_and_logical_rounding() {
        assert_eq!(scale_from_dpi(96), 1.0);
        assert_eq!(scale_from_dpi(150), 1.5625);
        assert_eq!(logical_to_physical(480, 1.0), 480);
        assert_eq!(logical_to_physical(480, 1.25), 600);
        assert_eq!(logical_to_physical(16, 1.5), 24);
    }

    #[test]
    fn compact_margin_scales_with_work_area_and_clamps() {
        // 100% DPI
        assert_eq!(compact_margin_logical(Rect::new(0, 0, 1920, 1080), 96), 24); // 1080p ≈ 24
        assert_eq!(compact_margin_logical(Rect::new(0, 0, 2560, 1440), 96), 32); // 1440p → 32（clamp 上限）
        assert_eq!(compact_margin_logical(Rect::new(0, 0, 3840, 2160), 96), 32); // 4K → 32
        assert_eq!(compact_margin_logical(Rect::new(0, 0, 1280, 720), 96), 16); // 720p → 16（clamp 下限）
        assert_eq!(compact_margin_logical(Rect::new(0, 0, 800, 600), 96), 16); // 小屏 → 16
                                                                               // 125% DPI：physical 較短邊 1440 → logical 1152 → 25
        assert_eq!(compact_margin_logical(Rect::new(0, 0, 2560, 1440), 120), 25);
        // 負座標不影響（只看寬高）
        assert_eq!(compact_margin_logical(Rect::new(-1920, 0, 0, 1080), 96), 24);
    }

    /// 建立 rcNormalPosition 為 (left,top,w,h)、其餘欄位歸零的 placement。
    fn placement_at(left: i32, top: i32, w: i32, h: i32) -> WINDOWPLACEMENT {
        let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        wp.rcNormalPosition.left = left;
        wp.rcNormalPosition.top = top;
        wp.rcNormalPosition.right = left + w;
        wp.rcNormalPosition.bottom = top + h;
        wp
    }

    /// fake controller：記錄 snapshot/apply/restore 呼叫次數，回傳可程式化 monitor，
    /// 並把 restore 實際套用的 placement 記下供置中/精確還原斷言。
    struct FakeController {
        mon: MonitorInfo,
        snap_placement: WINDOWPLACEMENT,
        apply_fails: std::sync::atomic::AtomicBool,
        snapshot_count: std::sync::atomic::AtomicU32,
        apply_count: std::sync::atomic::AtomicU32,
        restore_count: std::sync::atomic::AtomicU32,
        center_requested: std::sync::atomic::AtomicBool,
        restored: std::sync::Mutex<Option<WINDOWPLACEMENT>>,
    }

    impl FakeController {
        fn new(mon: MonitorInfo) -> Self {
            Self {
                mon,
                snap_placement: placement_at(0, 0, 1000, 700),
                apply_fails: Default::default(),
                snapshot_count: Default::default(),
                apply_count: Default::default(),
                restore_count: Default::default(),
                center_requested: Default::default(),
                restored: std::sync::Mutex::new(None),
            }
        }
    }

    impl MainWindowController for FakeController {
        fn monitor_info(&self) -> Result<MonitorInfo, String> {
            Ok(self.mon)
        }
        fn snapshot(&self) -> Result<MainWindowSnapshot, String> {
            self.snapshot_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(MainWindowSnapshot {
                placement: self.snap_placement,
                rc_work: self.mon.rc_work,
            })
        }
        fn apply_compact(&self, _rect: Rect) -> Result<(), String> {
            self.apply_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.apply_fails.load(std::sync::atomic::Ordering::SeqCst) {
                Err("apply fails".to_string())
            } else {
                Ok(())
            }
        }
        fn restore(&self, snap: &MainWindowSnapshot) -> Result<(), String> {
            self.restore_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut placement = snap.placement;
            if self
                .center_requested
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                center_placement_in(&mut placement, snap.rc_work);
            }
            *self.restored.lock().unwrap() = Some(placement);
            Ok(())
        }
        fn request_center_restore(&self) {
            self.center_requested
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn guard_restores_on_drop() {
        let c = Arc::new(FakeController::new(MonitorInfo {
            rc_work: rc1080(),
            dpi: 96,
        }));
        {
            let _g = prepare_window_layout(c.clone(), (1280, 720)).unwrap();
            assert_eq!(c.apply_count.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(c.restore_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
        // drop 後還原一次
        assert_eq!(c.restore_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn prepare_failure_restores_and_errors() {
        let c = Arc::new(FakeController::new(MonitorInfo {
            rc_work: rc1080(),
            dpi: 96,
        }));
        c.apply_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = prepare_window_layout(c.clone(), (1280, 720))
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err, "apply fails");
        // apply 失敗 → 已 snapshot，restore 被呼叫一次以還原
        assert_eq!(c.restore_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_restores_on_panic_unwind() {
        let c = Arc::new(FakeController::new(MonitorInfo {
            rc_work: rc1080(),
            dpi: 96,
        }));
        let r = std::panic::catch_unwind(|| {
            let _g = prepare_window_layout(c.clone(), (1280, 720)).unwrap();
            panic!("boom");
        });
        assert!(r.is_err());
        // panic unwind 仍觸發 guard Drop → restore
        assert_eq!(c.restore_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn center_placement_normal_monitor_preserves_size() {
        let mut wp = placement_at(100, 80, 900, 600);
        center_placement_in(&mut wp, Rect::new(0, 0, 1920, 1080));
        assert_eq!(wp.rcNormalPosition.left, (1920 - 900) / 2);
        assert_eq!(wp.rcNormalPosition.top, (1080 - 600) / 2);
        // width/height 不變
        assert_eq!(wp.rcNormalPosition.right - wp.rcNormalPosition.left, 900);
        assert_eq!(wp.rcNormalPosition.bottom - wp.rcNormalPosition.top, 600);
    }

    #[test]
    fn center_placement_negative_monitor_preserves_size() {
        let mut wp = placement_at(-1800, -200, 900, 600);
        center_placement_in(&mut wp, Rect::new(-1920, 0, 0, 1080));
        assert_eq!(wp.rcNormalPosition.left, -1920 + (1920 - 900) / 2);
        assert_eq!(wp.rcNormalPosition.top, (1080 - 600) / 2);
        assert_eq!(wp.rcNormalPosition.right - wp.rcNormalPosition.left, 900);
        assert_eq!(wp.rcNormalPosition.bottom - wp.rcNormalPosition.top, 600);
    }

    #[test]
    fn guard_foreground_loss_centers_restore() {
        let c = Arc::new(FakeController::new(MonitorInfo {
            rc_work: rc1080(),
            dpi: 96,
        }));
        {
            let _g = prepare_window_layout(c.clone(), (1280, 720)).unwrap();
            c.request_center_restore(); // 模擬 runner 收到 foreground=false
        }
        let restored = c.restored.lock().unwrap().unwrap();
        assert_eq!(restored.rcNormalPosition.left, (1920 - 1000) / 2);
        assert_eq!(restored.rcNormalPosition.top, (1080 - 700) / 2);
        assert_eq!(
            restored.rcNormalPosition.right - restored.rcNormalPosition.left,
            1000
        );
        assert_eq!(
            restored.rcNormalPosition.bottom - restored.rcNormalPosition.top,
            700
        );
    }

    #[test]
    fn guard_no_foreground_loss_restores_exact_placement() {
        let c = Arc::new(FakeController::new(MonitorInfo {
            rc_work: rc1080(),
            dpi: 96,
        }));
        {
            let _g = prepare_window_layout(c.clone(), (1280, 720)).unwrap();
        }
        let restored = c.restored.lock().unwrap().unwrap();
        // 未要求置中 → 精確還原原 placement（1000×700 at 0,0）
        assert_eq!(restored.rcNormalPosition.left, 0);
        assert_eq!(restored.rcNormalPosition.top, 0);
        assert_eq!(restored.rcNormalPosition.right, 1000);
        assert_eq!(restored.rcNormalPosition.bottom, 700);
    }

    #[test]
    fn center_flag_is_one_shot_not_polluting_next_restore() {
        let c = Arc::new(FakeController::new(MonitorInfo {
            rc_work: rc1080(),
            dpi: 96,
        }));
        {
            let _g = prepare_window_layout(c.clone(), (1280, 720)).unwrap();
            c.request_center_restore();
        }
        let first = c.restored.lock().unwrap().unwrap();
        assert_eq!(first.rcNormalPosition.left, (1920 - 1000) / 2);
        // 第二個 guard 未要求置中 → 精確還原（旗標已被第一次 restore 消費）
        {
            let _g = prepare_window_layout(c.clone(), (1280, 720)).unwrap();
        }
        let second = c.restored.lock().unwrap().unwrap();
        assert_eq!(second.rcNormalPosition.left, 0);
        assert_eq!(second.rcNormalPosition.top, 0);
    }
}
