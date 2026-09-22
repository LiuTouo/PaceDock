//! FPS cap 自動校準與原始策略基線 capture(自 runner.rs 機械搬移)。
//! 校準依 tier 序列選定最高安全 cap + circular buffer;基線 capture
//! 不套用任何 GPU policy。兩者皆不變更 GPU affinity。

use std::path::Path;

use super::{
    emit, prepare_workload_window, run_capture, warmup_with_integrity, workload_command,
    CapturedCsv, FpsCapPolicy, RunContext, SessionDetail, CALIBRATION_SAMPLE_SECS,
    CALIBRATION_TIERS, CALIBRATION_WARMUP_SECS, PRESENTMON_CIRCULAR_BUFFER_SIZE,
    WORKLOAD_STARTUP_MS,
};
use crate::benchmark::metrics::compute_lp_result;
use crate::error::codes;

/// 校準：下一 tier 相對目前 clean tier 的 FPS 增益低於此值即停（選目前 clean tier）。
const CALIBRATION_GAIN_MIN_PCT: f64 = 10.0;

const CALIBRATION_ROUND_BASE: u32 = 300;

fn next_power_of_two(v: u64) -> u64 {
    if v == 0 {
        return 1;
    }
    let mut p = 1u64;
    while p < v {
        p <<= 1;
    }
    p
}

/// 依 cap 決定 circular buffer：`max(8192, next_power_of_two(cap*8))`。
pub(super) fn calibration_buffer(cap: u32) -> u32 {
    let needed = (cap as u64).saturating_mul(8);
    let pow2 = next_power_of_two(needed);
    (pow2.max(8192)).min(u32::MAX as u64) as u32
}

/// 校準 tier 決策：剛測試的新 tier 相對前一 clean tier 的 FPS 增益 < 門檻 → 停
/// （選剛測試的新 tier，其 clean 且仍 ≥ 前一 tier）。
fn calibration_stop(clean_fps: f64, next_fps: f64) -> bool {
    if clean_fps <= 0.0 || !clean_fps.is_finite() || !next_fps.is_finite() {
        return true;
    }
    ((next_fps - clean_fps) / clean_fps * 100.0) < CALIBRATION_GAIN_MIN_PCT
}

/// 純決策：剛測試的新 tier clean 時，決定「選新 tier 停」或「繼續下一 tier」。
/// 回傳 `Some(cap)` = 選定、`None` = 繼續。規則：
/// - 最後一個 tier clean → 選它（不管增益）。
/// - 相對前一 clean tier 增益 < [`CALIBRATION_GAIN_MIN_PCT`] → 選新 tier（非前一 tier）。
/// - 首 tier（無前一 clean FPS）→ 繼續。
pub(super) fn calibration_clean_decision(
    clean_fps: Option<f64>,
    tier: u32,
    fps: f64,
    is_last_tier: bool,
) -> Option<u32> {
    if is_last_tier {
        return Some(tier);
    }
    match clean_fps {
        Some(cf) if calibration_stop(cf, fps) => Some(tier),
        _ => None,
    }
}

/// 單一校準 capture 的結果。
enum CalibrationCapture {
    Clean(f64),
    Overflow,
}

/// 校準：依 tier 序列選定最高安全 cap + buffer。Adaptive 才執行；Fixed 沿用 fps_cap。
/// 全程不變更 GPU affinity（workload 在既有策略下執行）。
pub(super) fn calibrate(
    ctx: &mut RunContext,
    session_dir: &Path,
    detail: &SessionDetail,
) -> Result<(u32, u32), String> {
    if ctx.config.fps_cap_policy != FpsCapPolicy::Adaptive {
        return Ok((ctx.config.fps_cap, PRESENTMON_CIRCULAR_BUFFER_SIZE));
    }
    let mut clean_tier: Option<u32> = None;
    let mut clean_fps: Option<f64> = None;
    let n_tiers = CALIBRATION_TIERS.len();
    for (i, &tier) in CALIBRATION_TIERS.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let buffer = calibration_buffer(tier);
        match calibration_capture(ctx, tier, buffer, session_dir, detail)? {
            CalibrationCapture::Overflow => match clean_tier {
                Some(prev) => return Ok((prev, calibration_buffer(prev))),
                None => return Err(codes::BENCHMARK_CAPTURE_OVERFLOW.to_string()),
            },
            CalibrationCapture::Clean(fps) => {
                if let Some(selected) =
                    calibration_clean_decision(clean_fps, tier, fps, i + 1 == n_tiers)
                {
                    return Ok((selected, calibration_buffer(selected)));
                }
                clean_tier = Some(tier);
                clean_fps = Some(fps);
            }
        }
    }
    match clean_tier {
        Some(ct) => Ok((ct, calibration_buffer(ct))),
        None => Err(codes::BENCHMARK_CAPTURE_OVERFLOW.to_string()),
    }
}

/// 執行單一 tier 的校準 capture（無 GPU mutation），回傳 Clean(avg_fps) 或 Overflow。
fn calibration_capture(
    ctx: &mut RunContext,
    tier: u32,
    buffer: u32,
    session_dir: &Path,
    detail: &SessionDetail,
) -> Result<CalibrationCapture, String> {
    emit(ctx, detail, "calibrating", None, Some(tier), 0, None, None);
    let (wl_exe, wl_args) = workload_command(&ctx.assets, &ctx.config, tier);
    let wl_pid = match ctx.processes.spawn(&wl_exe, &wl_args) {
        Ok(pid) => {
            ctx.owned_processes.push(pid);
            pid
        }
        Err(_) => return Err(codes::BENCHMARK_WORKLOAD_FAILED.to_string()),
    };
    let expected = prepare_workload_window(ctx, wl_pid)?;
    warmup_with_integrity(
        ctx,
        wl_pid,
        expected,
        WORKLOAD_STARTUP_MS + (CALIBRATION_WARMUP_SECS as u64) * 1000,
    )?;
    let csv = session_dir.join(format!("calib-tier-{tier}.csv"));
    let result = run_capture(
        ctx,
        CALIBRATION_ROUND_BASE,
        tier,
        wl_pid,
        &csv,
        1,
        tier,
        buffer,
        CALIBRATION_SAMPLE_SECS,
        expected,
    );
    if ctx.cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    match result {
        Err(e) if e == codes::BENCHMARK_CAPTURE_OVERFLOW => Ok(CalibrationCapture::Overflow),
        Err(e) => Err(e),
        Ok(captured) => {
            let frames = captured.read()?;
            let res = compute_lp_result(tier, &frames)?;
            match res.avg_fps {
                Some(fps) if fps.is_finite() && fps > 0.0 => Ok(CalibrationCapture::Clean(fps)),
                _ => Err(codes::BENCHMARK_CSV_INVALID.to_string()),
            }
        }
    }
}

/// 原始策略基線 capture 的 round 編號 namespace（與正式 round 隔離）
pub(super) const BASELINE_ROUND_BASE: u32 = 900;

/// 執行單次「原始策略基線」capture：不套用任何 GPU policy，workload 在
/// 既有（OS 預設或先前套用）策略下執行。形狀同 [`calibration_capture`]，
/// 但 warmup/取樣秒數由呼叫端指定（與 retest 同條件）。
#[allow(clippy::too_many_arguments)]
pub(super) fn baseline_capture(
    ctx: &mut RunContext,
    round: u32,
    fps_cap: u32,
    buffer: u32,
    warmup_secs: u32,
    sample_secs: u32,
    session_dir: &Path,
    detail: &SessionDetail,
) -> Result<CapturedCsv, String> {
    emit(ctx, detail, "calibrating", Some(round), None, 0, None, None);
    if ctx.cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    let (wl_exe, wl_args) = workload_command(&ctx.assets, &ctx.config, fps_cap);
    let wl_pid = match ctx.processes.spawn(&wl_exe, &wl_args) {
        Ok(pid) => {
            ctx.owned_processes.push(pid);
            pid
        }
        Err(_) => return Err(codes::BENCHMARK_WORKLOAD_FAILED.to_string()),
    };
    let expected = prepare_workload_window(ctx, wl_pid)?;
    warmup_with_integrity(
        ctx,
        wl_pid,
        expected,
        WORKLOAD_STARTUP_MS + (warmup_secs as u64) * 1000,
    )?;
    let csv = session_dir.join(format!("round-{round}-lp-0.csv"));
    let result = run_capture(
        ctx,
        round,
        0,
        wl_pid,
        &csv,
        1,
        fps_cap,
        buffer,
        sample_secs,
        expected,
    );
    if ctx.cancel.is_cancelled() {
        return Err("cancelled".to_string());
    }
    result
}
