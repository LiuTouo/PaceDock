//! 實際遊戲量測：把既有 PresentMon 捕管（`--process_id` 過濾）掛到執行中遊戲，
//! 擷取 N 秒 frametime 並重用 benchmark 的 `LpResult` 指標。
//!
//! 反作弊安全邊界：全程 ETW attach（PresentMon 自行開 ETW session），不對遊戲
//! 行程開 process handle；唯一 `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
//! 是視窗枚舉時讀 exe 路徑（與 Alt-Tab 同級權限），不做任何寫入。
//!
//! 儲存刻意**不**用 benchmark session schema（那套有 HMAC，因為紀錄驅動特權
//! GPU mutation；capture 紀錄不驅動任何特權操作）：
//! `%APPDATA%\PaceDock\captures\<uuid>.csv` + `<uuid>.json`。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, WINDOWPLACEMENT,
    WS_EX_TOOLWINDOW,
};

use crate::config;
use crate::error::codes;

use super::manager::{BenchmarkManager, GpuOperationGuard};
use super::metrics::{compute_lp_result, parse_presentmon_series};
use super::process_win::RealProcessRunner;
use super::runner::{
    assess_capture_integrity, presentmon_command, ProcessRunner, CAPTURE_WAIT_MARGIN_S,
    DIAG_OUTPUT_TAIL_CAP, PRESENTMON_CIRCULAR_BUFFER_SIZE,
};
use super::LpResult;

// ── 領域型別 ─────────────────────────────────────────────────────────────

/// 可量測的遊戲視窗候選（list_game_windows 回傳）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GameWindow {
    pub pid: u32,
    pub title: String,
    pub exe_name: String,
}

/// 單次遊戲量測紀錄（captures/<uuid>.json）。指標型別與 benchmark 完全共用。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct GameCaptureRecord {
    pub id: String,
    pub game_title: String,
    pub exe_name: String,
    pub pid: u32,
    /// capture 當下已套用的鎖定核心 LP（before/after 對照用）；未套用為 None
    pub locked_lp: Option<u32>,
    pub started_at: String,
    pub duration_secs: u32,
    pub metrics: LpResult,
    /// 失敗代碼；成功為 None
    pub error: Option<String>,
}

/// 進度事件（emit `game-capture-progress`）
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GameCaptureProgress {
    pub capture_id: String,
    /// capturing | parsing | done | cancelled
    pub stage: String,
    pub percentage: u32,
}

/// 取消訊號：同一時間只允許一場 capture（GPU reservation 排他），
/// 因此單一 static 即 race-free。
static CANCEL: AtomicBool = AtomicBool::new(false);

/// 請求取消進行中的 capture（冪等；無 capture 進行時為 no-op）。
pub fn request_cancel() {
    CANCEL.store(true, Ordering::Relaxed);
}

// ── 視窗枚舉 ─────────────────────────────────────────────────────────────

/// 排除清單：shell/系統/自身行程（`ponytail:` 名單 + 尺寸下限涵蓋絕大多數雜訊，
/// 有誤報再換真正的分類器）。
const EXE_BLACKLIST: &[&str] = &[
    "pacedock.exe",
    "msedgewebview2.exe",
    "explorer.exe",
    "applicationframehost.exe",
    "searchhost.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "textinputhost.exe",
    "widgets.exe",
    "runtimebroker.exe",
];

/// 最小外框尺寸（實體像素）：排除通知/toast/隱形工具視窗
const MIN_WINDOW_SIZE: i32 = 320;

/// 純分類：給定視窗屬性判斷是否為可量測的遊戲視窗候選。
/// `minimized` 視窗跳過尺寸檢查 — 最小化時 `GetWindowRect` 回報極小的還原
/// 位置尺寸（如 199×31），會誤殺正是節流豁免目標場景的背景遊戲。
#[allow(clippy::too_many_arguments)]
fn is_candidate(
    self_pid: u32,
    pid: u32,
    title: &str,
    exe: &str,
    visible: bool,
    tool_window: bool,
    cloaked: bool,
    minimized: bool,
    width: i32,
    height: i32,
) -> bool {
    if pid == self_pid || pid == 0 {
        return false;
    }
    if !visible || tool_window || cloaked {
        return false;
    }
    if title.trim().is_empty() {
        return false;
    }
    if !minimized && (width < MIN_WINDOW_SIZE || height < MIN_WINDOW_SIZE) {
        return false;
    }
    !is_blacklisted(exe)
}

/// exe 檔名是否在排除清單（timer 節流豁免的開 handle 防線也用這份）。
/// 另外涵蓋目前執行檔自身的檔名：更名版 exe 改名後，清單內的舊名擋不到自身。
pub(crate) fn is_blacklisted(exe: &str) -> bool {
    EXE_BLACKLIST.iter().any(|b| b.eq_ignore_ascii_case(exe))
        || crate::process::self_exe_name().eq_ignore_ascii_case(exe)
}

unsafe extern "system" fn enum_trampoline(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<HWND>);
    list.push(hwnd);
    true.into()
}

/// 讀視窗所屬行程的 exe 檔名（小寫；失敗為空字串）。
fn exe_name_of_pid(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if result.is_err() || len == 0 {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Path::new(&full)
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

/// 列舉目前可量測的遊戲視窗（每個 PID 取第一個符合的視窗，依 Z-order）。
pub fn list_game_windows() -> Result<Vec<GameWindow>, String> {
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe {
        EnumWindows(Some(enum_trampoline), LPARAM(&mut hwnds as *mut _ as isize))
            .map_err(|e| format!("EnumWindows 失敗: {e}"))?;
    }
    let self_pid = std::process::id();
    let mut out: Vec<GameWindow> = Vec::new();
    let mut seen_pids: Vec<u32> = Vec::new();
    for hwnd in hwnds {
        // 先讀出全部屬性（unsafe 區塊），分類與組裝在安全側
        let (pid, title, visible, tool_window, cloaked, minimized, w, h) = unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 || seen_pids.contains(&pid) {
                continue;
            }
            let mut buf = [0u16; 256];
            let len = GetWindowTextW(hwnd, &mut buf);
            let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
            let visible = IsWindowVisible(hwnd).as_bool();
            let minimized = IsIconic(hwnd).as_bool();
            let tool_window =
                (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0) != 0;
            let mut cloaked: u32 = 0;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
            // 最小化時 GetWindowRect 是極小還原位置 → 改用 placement 的正常位置尺寸
            let (w, h) = if minimized {
                let mut placement = WINDOWPLACEMENT::default();
                if GetWindowPlacement(hwnd, &mut placement).is_ok() {
                    let r = placement.rcNormalPosition;
                    ((r.right - r.left).max(0), (r.bottom - r.top).max(0))
                } else {
                    (0, 0)
                }
            } else {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    (rect.right - rect.left, rect.bottom - rect.top)
                } else {
                    (0, 0)
                }
            };
            (
                pid,
                title,
                visible,
                tool_window,
                cloaked != 0,
                minimized,
                w,
                h,
            )
        };
        let exe_name = exe_name_of_pid(pid);
        if !is_candidate(
            self_pid,
            pid,
            &title,
            &exe_name,
            visible,
            tool_window,
            cloaked,
            minimized,
            w,
            h,
        ) {
            continue;
        }
        seen_pids.push(pid);
        out.push(GameWindow {
            pid,
            title,
            exe_name,
        });
    }
    Ok(out)
}

// ── capture 儲存 ─────────────────────────────────────────────────────────

/// captures 根目錄
pub fn captures_dir() -> PathBuf {
    config::config_dir().join("captures")
}

/// 驗證 id（只接受合法 UUID，杜絕穿越）並回傳對應 json 路徑。
fn capture_json_path(id: &str) -> Result<PathBuf, String> {
    Uuid::parse_str(id).map_err(|_| codes::BENCHMARK_INVALID_SESSION_ID.to_string())?;
    Ok(captures_dir().join(format!("{id}.json")))
}

/// 歷史 capture 列表（依 startedAt 降冪）
pub fn list_captures() -> Result<Vec<GameCaptureRecord>, String> {
    let dir = captures_dir();
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in
        std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))?
    {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Ok(record) = serde_json::from_str::<GameCaptureRecord>(&text) {
            out.push(record);
        }
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(out)
}

/// 刪除單一 capture（json + csv）
pub fn delete_capture(id: &str) -> Result<(), String> {
    let json = capture_json_path(id)?;
    let csv = json.with_extension("csv");
    for path in [json, csv] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED)),
        }
    }
    Ok(())
}

// ── capture 執行 ─────────────────────────────────────────────────────────

fn emit_progress(app: &AppHandle, capture_id: &str, stage: &str, percentage: u32) {
    let _ = app.emit(
        "game-capture-progress",
        GameCaptureProgress {
            capture_id: capture_id.to_string(),
            stage: stage.to_string(),
            percentage,
        },
    );
}

/// 執行一次遊戲量測（在 spawn_blocking 內執行）。`guard` 持有 GPU 操作排他鎖，
/// return/drop 時釋放。任何失敗路徑都會確保 PresentMon 被終止。
#[allow(clippy::too_many_arguments)]
pub fn run_game_capture(
    app: &AppHandle,
    manager: &BenchmarkManager,
    _guard: GpuOperationGuard,
    pid: u32,
    title: &str,
    duration_secs: u32,
    gpu_instance_id: Option<String>,
) -> Result<GameCaptureRecord, String> {
    CANCEL.store(false, Ordering::Relaxed);
    let started_at = chrono::Local::now().to_rfc3339();
    let id = Uuid::new_v4().to_string();
    let exe_name = exe_name_of_pid(pid);

    // 遊戲必須仍在執行（PID 消失 = 選單過期）
    let pm_runner = RealProcessRunner::new();
    if !pm_runner.is_alive(pid) {
        return Err(codes::CAPTURE_GAME_NOT_FOUND.to_string());
    }

    // 內建資源（PresentMon）解析 + digest 驗證，與 benchmark 同一條信任路徑
    let assets = super::manager::resolve_assets(app)?;
    crate::benchmark::assets::verify(&assets).map_err(|e| e.code().to_string())?;

    let dir = captures_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))?;
    let csv = dir.join(format!("{id}.csv"));

    // stale 輸出清除（與 run_capture 同語意：舊檔絕不能被當成當前輸出）
    if let Err(e) = std::fs::remove_file(&csv) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED));
        }
    }

    emit_progress(app, &id, "capturing", 0);
    let session_name = format!("PaceDock-capture-{id}");
    let args = presentmon_command(
        duration_secs,
        PRESENTMON_CIRCULAR_BUFFER_SIZE,
        pid,
        &csv,
        &session_name,
    );
    let pm_pid = pm_runner.spawn(&assets.presentmon, &args).map_err(|e| {
        log::warn!("PresentMon 啟動失敗: {e}");
        codes::BENCHMARK_PRESENTMON_FAILED.to_string()
    })?;

    // 等待 PresentMon 自停（--timed + --terminate_after_timed）；可取消、有 deadline
    let started = Instant::now();
    let deadline = started + Duration::from_secs(duration_secs as u64 + CAPTURE_WAIT_MARGIN_S);
    let (mut cancelled, mut timed_out) = (false, false);
    loop {
        if !pm_runner.is_alive(pm_pid) {
            break;
        }
        if CANCEL.swap(false, Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        let pct = (started.elapsed().as_secs_f64() / duration_secs as f64 * 100.0) as u32;
        emit_progress(app, &id, "capturing", pct.min(99));
        std::thread::sleep(Duration::from_millis(500));
    }

    // 終止前先取 exit code / output tail（kill 會 reap 並丟掉 pipe 內容）
    let pm_exit_code = if !cancelled && !timed_out {
        pm_runner.exit_code(pm_pid)
    } else {
        None
    };
    let pm_stderr = pm_runner
        .output_tail(pm_pid, DIAG_OUTPUT_TAIL_CAP)
        .map(|o| o.stderr)
        .unwrap_or_default();
    let _ = pm_runner.kill(pm_pid);

    if cancelled {
        let _ = std::fs::remove_file(&csv);
        emit_progress(app, &id, "cancelled", 0);
        return Err("cancelled".to_string());
    }
    if timed_out {
        log::error!("PresentMon 逾時未退出（遊戲量測 {duration_secs}s + margin）");
        return Err(codes::BENCHMARK_PRESENTMON_TIMEOUT.to_string());
    }
    // 非零結束 = capture 生產者未成功（與 run_capture 同判定）
    if pm_exit_code.is_some_and(|c| c != 0) {
        return Err(codes::BENCHMARK_PRESENTMON_FAILED.to_string());
    }

    emit_progress(app, &id, "parsing", 99);
    let overflowed = super::runner::parse_overflowed_present_events(&pm_stderr).unwrap_or(0);
    let etw_loss = super::runner::stderr_has_etw_loss(&pm_stderr);
    let integ = assess_capture_integrity(&csv, duration_secs, overflowed, etw_loss);
    if let Some(code) = integ.code {
        return Err(code);
    }

    let series = std::fs::read_to_string(&csv)
        .ok()
        .and_then(|t| parse_presentmon_series(&t).ok())
        .ok_or_else(|| codes::BENCHMARK_CSV_INVALID.to_string())?;
    let mut metrics = compute_lp_result(0, &series.frames).map_err(|e| {
        log::warn!("遊戲量測指標計算失敗: {e}");
        codes::BENCHMARK_CSV_INVALID.to_string()
    })?;
    super::metrics::attach_display_metrics(&mut metrics, &series.display);

    // capture 當下的鎖定核心（before/after 對照用；best-effort，失敗不擋結果）
    let locked_lp = gpu_instance_id.as_deref().and_then(|instance| {
        match manager.backend.read_affinity_policy(instance) {
            Ok(policy) => {
                super::manager::mask_bytes_to_lp(policy.assignment_set_override.bytes.as_deref())
            }
            Err(e) => {
                log::warn!("讀取 GPU policy 失敗（lockedLp 記為未知）: {}", e.code());
                None
            }
        }
    });

    let record = GameCaptureRecord {
        id: id.clone(),
        game_title: title.to_string(),
        exe_name,
        pid,
        locked_lp,
        started_at,
        duration_secs,
        metrics,
        error: None,
    };
    let json = capture_json_path(&id)?;
    let text = serde_json::to_string_pretty(&record)
        .map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))?;
    config::atomic_write(&json, &text)
        .map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))?;

    emit_progress(app, &id, "done", 100);
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(args: (u32, u32, &str, &str, bool, bool, bool, bool, i32, i32)) -> bool {
        let (self_pid, pid, title, exe, visible, tool, cloaked, minimized, w, h) = args;
        is_candidate(
            self_pid, pid, title, exe, visible, tool, cloaked, minimized, w, h,
        )
    }

    #[test]
    fn candidate_matrix_accepts_normal_game_window() {
        assert!(candidate((
            1,
            42,
            "Cyberpunk 2077",
            "cyberpunk2077.exe",
            true,
            false,
            false,
            false,
            1920,
            1080
        )));
    }

    /// 最小化視窗跳過尺寸檢查（GetWindowRect 回報極小還原位置會誤殺背景遊戲）；
    /// 正常視窗仍受尺寸下限約束。
    #[test]
    fn candidate_matrix_minimized_bypasses_size_floor() {
        // 最小化 + 極小 GetWindowRect → 仍為候選（用 placement 正常尺寸 2560x1440）
        assert!(candidate((
            1,
            42,
            "鬥陣特攻",
            "overwatch.exe",
            true,
            false,
            false,
            true,
            199,
            31
        )));
        // 未最小化 + 極小 → 拒絕（通知/toast）
        assert!(!candidate((
            1, 42, "Toast", "game.exe", true, false, false, false, 199, 31
        )));
        // 最小化但其他屬性不合格 → 仍拒絕
        assert!(!candidate((
            1,
            42,
            "鬥陣特攻",
            "overwatch.exe",
            false,
            false,
            false,
            true,
            199,
            31
        )));
        assert!(!candidate((
            1,
            42,
            "鬥陣特攻",
            "explorer.exe",
            true,
            false,
            false,
            true,
            199,
            31
        )));
    }

    #[test]
    fn blacklist_covers_current_exe_name() {
        // 更名版：目前執行檔（測試中即測試 binary）的名稱必須被排除，大小寫不敏感
        let self_name = crate::process::self_exe_name();
        assert!(!self_name.is_empty());
        assert!(is_blacklisted(&self_name));
        assert!(is_blacklisted(&self_name.to_uppercase()));
    }

    #[test]
    fn candidate_matrix_rejects_noise() {
        // 自身行程
        assert!(!candidate((
            42,
            42,
            "PaceDock",
            "pacedock.exe",
            true,
            false,
            false,
            false,
            800,
            600
        )));
        // 黑名單（大小寫不敏感）
        assert!(!candidate((
            1,
            7,
            "Settings",
            "explorer.exe",
            true,
            false,
            false,
            false,
            1200,
            800
        )));
        assert!(!candidate((
            1,
            7,
            "x",
            "MSEdgeWebView2.exe",
            true,
            false,
            false,
            false,
            1200,
            800
        )));
        // 隱藏 / toolwindow / cloaked
        assert!(!candidate((
            1, 9, "Game", "game.exe", false, false, false, false, 1200, 800
        )));
        assert!(!candidate((
            1, 9, "Game", "game.exe", true, true, false, false, 1200, 800
        )));
        assert!(!candidate((
            1, 9, "Game", "game.exe", true, false, true, false, 1200, 800
        )));
        // 空標題
        assert!(!candidate((
            1, 9, "  ", "game.exe", true, false, false, false, 1200, 800
        )));
        // 太小（通知/toast）
        assert!(!candidate((
            1, 9, "Toast", "game.exe", true, false, false, false, 300, 200
        )));
        // PID 0
        assert!(!candidate((
            1, 0, "Game", "game.exe", true, false, false, false, 1200, 800
        )));
    }

    /// GameCaptureRecord serde camelCase roundtrip（含 lockedLp None、metrics 巢狀）。
    #[test]
    fn capture_record_serializes_camel_case() {
        let record = GameCaptureRecord {
            id: "abc".into(),
            game_title: "Game".into(),
            exe_name: "game.exe".into(),
            pid: 42,
            locked_lp: Some(3),
            started_at: "t".into(),
            duration_secs: 30,
            metrics: LpResult {
                lp: 0,
                avg_fps: Some(120.0),
                p1_low: Some(90.0),
                ..Default::default()
            },
            error: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"gameTitle\""));
        assert!(json.contains("\"exeName\""));
        assert!(json.contains("\"lockedLp\":3"));
        assert!(json.contains("\"durationSecs\":30"));
        assert!(json.contains("\"avgFps\":120.0"));
        let back: GameCaptureRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.locked_lp, Some(3));
        assert_eq!(back.metrics.avg_fps, Some(120.0));
        // 舊 json 缺 lockedLp → None（向後相容）
        let old: GameCaptureRecord =
            serde_json::from_str(r#"{"id":"x","gameTitle":"g","exeName":"g.exe","pid":1,"startedAt":"t","durationSecs":10,"metrics":{}}"#).unwrap();
        assert_eq!(old.locked_lp, None);
    }

    /// UUID 驗證拒絕穿越與非法 id。
    #[test]
    fn capture_json_path_validates_uuid() {
        assert!(capture_json_path("../../evil").is_err());
        assert!(capture_json_path("not-a-uuid").is_err());
        let id = Uuid::new_v4().to_string();
        let path = capture_json_path(&id).unwrap();
        assert!(path.ends_with(format!("{id}.json")));
    }
}
