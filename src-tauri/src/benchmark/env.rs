//! 環境穩定度探針（Task 3）：vendor-neutral 的注入式抽象 + 生產 Windows 實作。
//!
//! 純決策（環境閘、CPU idle 等待、漂移百分比）與探針分離，測試用 fake 探針
//! 跑完整流程，不碰真實電源/計數器。生產探針只用 Win32 公開 API（不依賴
//! 特定 GPU 廠商），回傳值一律 best-effort：讀取失敗視為「不阻擋」。

use std::sync::Mutex;

use crate::error::codes;
use crate::gpu::Sleep;

/// 每環境量測間隔（毫秒）：CPU idle 要求「五個連續一秒樣本」。
const CPU_SAMPLE_INTERVAL_MS: u64 = 1000;
/// CPU idle 閘：總使用率門檻（%）。
pub const ENV_CPU_MAX_PCT: f64 = 15.0;
/// CPU idle 閘：需要連續通過的樣本數。
pub const ENV_CPU_SAMPLES: u32 = 5;
/// CPU idle 閘：最大等待秒數（含第一筆 baseline）。
pub const ENV_CPU_MAX_WAIT_SECS: u64 = 60;

/// 時間漂移門檻（%）：screening block 中位數 / confirmation pair 中點偏離
/// 參考值超過此比例視為環境漂移。
#[cfg(test)]
pub const DRIFT_THRESHOLD_PCT: f64 = 5.0;
/// 單一 block/pair 的漂移重跑次數上限。
#[cfg(test)]
pub const MAX_DRIFT_RETRIES: u32 = 2;

/// 環境探針抽象：生產用 [`RealEnvironmentProbe`]，測試注入 fake。
pub trait EnvironmentProbe: Send + Sync {
    /// 是否偵測到實體電池（無電池 → false）。
    fn battery_present(&self) -> bool;
    /// 是否目前接在 AC 電源上（true = 已插電）。
    fn on_ac_power(&self) -> bool;
    /// Windows 電池節能 / 省電模式是否啟用。
    fn battery_saver_on(&self) -> bool;
    /// 取樣「自上次呼叫以來的」總 CPU 使用率（0..=100）。首次呼叫（無 baseline）
    /// 回傳 0.0；之後每次呼叫回傳兩次取樣之間的 delta 使用率。
    fn sample_total_cpu(&self) -> f64;
}

/// 生產環境探針。
/// - AC/電池：`GetSystemPowerStatus`（`SYSTEM_POWER_STATUS`）。
/// - 電池節能：`SYSTEM_POWER_STATUS.SystemStatusFlag`（0=off、1=on）。
/// - 總 CPU：`GetSystemTimes` 兩次取樣間的空閒比例。
pub struct RealEnvironmentProbe {
    /// 上一次 GetSystemTimes 的 (idle, kernel, user) FILETIME 100ns 刻度。
    last_cpu: Mutex<Option<(u64, u64, u64)>>,
}

impl RealEnvironmentProbe {
    pub fn new() -> Self {
        Self {
            last_cpu: Mutex::new(None),
        }
    }
}

impl Default for RealEnvironmentProbe {
    fn default() -> Self {
        Self::new()
    }
}

fn filetime_to_u64(ft: &windows::Win32::Foundation::FILETIME) -> u64 {
    (ft.dwLowDateTime as u64) | ((ft.dwHighDateTime as u64) << 32)
}

fn battery_flag_indicates_present(flag: u8) -> bool {
    // 255 = unknown，不能解讀為 bit 7 的「無系統電池」；保守保留 AC 閘。
    flag == u8::MAX || flag & 0x80 == 0
}

impl EnvironmentProbe for RealEnvironmentProbe {
    fn battery_present(&self) -> bool {
        let mut sps = windows::Win32::System::Power::SYSTEM_POWER_STATUS::default();
        // BatteryFlag 位元 7（128）＝「無系統電池」；其餘視為有電池。
        // 讀取失敗（回傳 FALSE）→ 保守視為「有電池」，交由 AC 閘進一步把關。
        if unsafe { windows::Win32::System::Power::GetSystemPowerStatus(&mut sps) }.is_err() {
            return true;
        }
        battery_flag_indicates_present(sps.BatteryFlag)
    }

    fn on_ac_power(&self) -> bool {
        let mut sps = windows::Win32::System::Power::SYSTEM_POWER_STATUS::default();
        // ACLineStatus：0=離線、1=線上、255=未知。未知/讀取失敗 → 視為離線（fail closed）。
        if unsafe { windows::Win32::System::Power::GetSystemPowerStatus(&mut sps) }.is_err() {
            return false;
        }
        sps.ACLineStatus == 1
    }

    fn battery_saver_on(&self) -> bool {
        // SystemStatusFlag：0 = off（未節能）、1 = on（電池節能/省電中）。
        let mut sps = windows::Win32::System::Power::SYSTEM_POWER_STATUS::default();
        if unsafe { windows::Win32::System::Power::GetSystemPowerStatus(&mut sps) }.is_err() {
            return false;
        }
        sps.SystemStatusFlag == 1
    }

    fn sample_total_cpu(&self) -> f64 {
        use windows::Win32::System::Threading::GetSystemTimes;
        let mut idle = windows::Win32::Foundation::FILETIME::default();
        let mut kernel = windows::Win32::Foundation::FILETIME::default();
        let mut user = windows::Win32::Foundation::FILETIME::default();
        if unsafe {
            GetSystemTimes(
                Some(&mut idle as *mut _),
                Some(&mut kernel as *mut _),
                Some(&mut user as *mut _),
            )
        }
        .is_err()
        {
            return 0.0;
        }
        let (i, k, u) = (
            filetime_to_u64(&idle),
            filetime_to_u64(&kernel),
            filetime_to_u64(&user),
        );
        let mut last = self.last_cpu.lock().unwrap();
        let prev = last.replace((i, k, u));
        let Some((pi, pk, pu)) = prev else {
            // 第一次取樣：只有 baseline，無 delta。
            return 0.0;
        };
        let idle_delta = i.saturating_sub(pi);
        let total_delta = k.saturating_sub(pk).saturating_add(u.saturating_sub(pu));
        if total_delta == 0 {
            return 0.0;
        }
        // CPU 使用率 = 1 - idle / total。
        (1.0 - idle_delta as f64 / total_delta as f64).clamp(0.0, 100.0) * 100.0
    }
}

/// 環境前置閘：有電池時必須接 AC、電池節能不開。任一不滿足 → 穩定代碼失敗關閉。
/// CPU idle 另由 [`wait_for_cpu_idle`] 處理（含等待預算）。
pub fn environment_gate(probe: &dyn EnvironmentProbe) -> Result<(), String> {
    if probe.battery_present() && !probe.on_ac_power() {
        return Err(codes::BENCHMARK_ENV_UNSTABLE.to_string());
    }
    if probe.battery_saver_on() {
        return Err(codes::BENCHMARK_ENV_UNSTABLE.to_string());
    }
    Ok(())
}

/// 等待總 CPU 使用率連續 [`ENV_CPU_SAMPLES`] 個一秒樣本都 ≤ [`ENV_CPU_MAX_PCT`]。
/// 最長等待 [`ENV_CPU_MAX_WAIT_SECS`]；超時 → 穩定代碼失敗關閉。`is_cancelled`
/// 用於在等待期間可被取消中斷。
///
/// 第一個樣本只建立 delta baseline（回傳 0.0），因此總共需要
/// `ENV_CPU_SAMPLES + 1` 次取樣才能取得五個有效 delta。
pub fn wait_for_cpu_idle(
    probe: &dyn EnvironmentProbe,
    sleeper: &dyn Sleep,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    // 先取一筆 baseline。
    probe.sample_total_cpu();
    let mut consecutive = 0u32;
    let mut waited_secs = 0u64;
    while waited_secs < ENV_CPU_MAX_WAIT_SECS {
        if is_cancelled() {
            return Err("cancelled".to_string());
        }
        sleeper.sleep(CPU_SAMPLE_INTERVAL_MS);
        waited_secs += 1;
        let cpu = probe.sample_total_cpu();
        if cpu <= ENV_CPU_MAX_PCT {
            consecutive += 1;
            if consecutive >= ENV_CPU_SAMPLES {
                return Ok(());
            }
        } else {
            consecutive = 0;
        }
    }
    Err(codes::BENCHMARK_ENV_UNSTABLE.to_string())
}

/// 中位數漂移百分比：`abs(current - reference) / reference * 100`。
/// reference ≤0 或非有限 → 0.0（無漂移）。
#[cfg(test)]
pub fn drift_pct(reference: f64, current: f64) -> f64 {
    if !reference.is_finite() || reference <= 0.0 || !current.is_finite() {
        return 0.0;
    }
    ((current - reference).abs() / reference) * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::NoopSleeper;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 可程式化的 fake 探針：依序回傳預設的 CPU 樣本序列。
    struct FakeProbe {
        battery_present: bool,
        on_ac: bool,
        saver_on: bool,
        cpu_samples: Vec<f64>,
        calls: AtomicUsize,
    }

    impl FakeProbe {
        fn new(battery_present: bool, on_ac: bool, saver_on: bool) -> Self {
            Self {
                battery_present,
                on_ac,
                saver_on,
                cpu_samples: Vec::new(),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl EnvironmentProbe for FakeProbe {
        fn battery_present(&self) -> bool {
            self.battery_present
        }
        fn on_ac_power(&self) -> bool {
            self.on_ac
        }
        fn battery_saver_on(&self) -> bool {
            self.saver_on
        }
        fn sample_total_cpu(&self) -> f64 {
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            // 第一個樣本是 baseline；之後依序回傳腳本值，不足補 0。
            if i == 0 {
                0.0
            } else {
                self.cpu_samples.get(i - 1).copied().unwrap_or(0.0)
            }
        }
    }

    #[test]
    fn unknown_battery_flag_is_treated_as_present() {
        assert!(battery_flag_indicates_present(255));
        assert!(!battery_flag_indicates_present(0x80));
        assert!(battery_flag_indicates_present(0x01));
    }

    #[test]
    fn environment_gate_requires_ac_when_battery_present() {
        // 有電池 + 離線 → 失敗
        assert_eq!(
            environment_gate(&FakeProbe::new(true, false, false)).unwrap_err(),
            codes::BENCHMARK_ENV_UNSTABLE
        );
        // 有電池 + 插電 → 通過
        assert!(environment_gate(&FakeProbe::new(true, true, false)).is_ok());
        // 無電池（桌上機）→ 不需 AC，直接通過
        assert!(environment_gate(&FakeProbe::new(false, false, false)).is_ok());
    }

    #[test]
    fn environment_gate_rejects_battery_saver() {
        assert_eq!(
            environment_gate(&FakeProbe::new(true, true, true)).unwrap_err(),
            codes::BENCHMARK_ENV_UNSTABLE
        );
    }

    #[test]
    fn wait_for_cpu_idle_requires_five_consecutive_low_samples() {
        let never_cancel = || false;
        // 全低 → 快速通過
        let mut p = FakeProbe::new(false, false, false);
        p.cpu_samples = vec![5.0; 5];
        assert!(wait_for_cpu_idle(&p, &NoopSleeper, &never_cancel).is_ok());

        // 前兩筆高、之後低 → 連續數歸零重算，仍通過（第 1 個低樣本之後要 5 個連續）
        let mut p = FakeProbe::new(false, false, false);
        p.cpu_samples = vec![50.0, 50.0, 5.0, 5.0, 5.0, 5.0, 5.0];
        assert!(wait_for_cpu_idle(&p, &NoopSleeper, &never_cancel).is_ok());
    }

    #[test]
    fn wait_for_cpu_idle_fails_closed_on_exhaustion() {
        let never_cancel = || false;
        // 一直高使用率：跑滿 60 秒樣本後失敗關閉。
        let mut p = FakeProbe::new(false, false, false);
        p.cpu_samples = vec![100.0; (ENV_CPU_MAX_WAIT_SECS + 1) as usize];
        assert_eq!(
            wait_for_cpu_idle(&p, &NoopSleeper, &never_cancel).unwrap_err(),
            codes::BENCHMARK_ENV_UNSTABLE
        );
    }

    #[test]
    fn wait_for_cpu_idle_is_cancellable() {
        let cancelled = std::cell::Cell::new(false);
        let cancel = || cancelled.get();
        let mut p = FakeProbe::new(false, false, false);
        p.cpu_samples = vec![5.0; 10];
        let sleeper = crate::gpu::RealSleeper;
        cancelled.set(true);
        assert_eq!(
            wait_for_cpu_idle(&p, &sleeper, &cancel).unwrap_err(),
            "cancelled"
        );
    }

    #[test]
    fn drift_pct_computes_percentage_and_guards_zero() {
        assert!((drift_pct(100.0, 105.0) - 5.0).abs() < 1e-9);
        assert!((drift_pct(100.0, 95.0) - 5.0).abs() < 1e-9);
        assert_eq!(drift_pct(0.0, 100.0), 0.0);
        assert_eq!(drift_pct(f64::NAN, 100.0), 0.0);
        assert_eq!(drift_pct(100.0, f64::INFINITY), 0.0);
    }
}
