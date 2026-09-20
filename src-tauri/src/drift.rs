//! 常駐設定監看：只讀回已管理的設定，不自動重套或重啟裝置。
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use crate::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Setting {
    Gpu,
    Msi,
    Usb,
    Aspm,
    Timer,
    GlobalTimer,
    TimerExempt,
}

impl Setting {
    fn label(self, english: bool) -> &'static str {
        match (self, english) {
            (Self::Gpu, true) => "GPU core affinity",
            (Self::Gpu, false) => "GPU 核心設定",
            (Self::Msi, _) => "GPU MSI",
            (Self::Usb, true) => "USB selective suspend",
            (Self::Usb, false) => "USB 選擇性暫停",
            (Self::Aspm, _) => "PCIe ASPM",
            (Self::Timer, true) => "High precision timer",
            (Self::Timer, false) => "高精度計時器",
            (Self::GlobalTimer, true) => "Global timer requests",
            (Self::GlobalTimer, false) => "全域計時器請求",
            (Self::TimerExempt, true) => "Timer throttling exemptions",
            (Self::TimerExempt, false) => "計時器節流豁免",
        }
    }
}

#[derive(Default)]
struct Monitor {
    active: BTreeSet<Setting>,
}

impl Monitor {
    /// None 表示無法讀取，不算恢復；持續異常不重複通知。
    fn observe(&mut self, readings: &[(Setting, Option<bool>)]) -> bool {
        let mut new_warning = false;
        for &(setting, drifted) in readings {
            match drifted {
                Some(true) => new_warning |= self.active.insert(setting),
                Some(false) => {
                    self.active.remove(&setting);
                }
                None => {}
            }
        }
        new_warning
    }
}

pub(crate) fn disabled_power_drift(ac: Option<u32>, dc: Option<u32>) -> Option<bool> {
    if ac.is_some_and(|v| v != 0) || dc.is_some_and(|v| v != 0) {
        Some(true)
    } else {
        ac.zip(dc).map(|_| false)
    }
}

fn timer_drift(requested: bool, enabled: bool, intervals: Option<(u32, u32, u32)>) -> Option<bool> {
    if requested != enabled {
        return Some(true);
    }
    // 關閉後其他程式仍可請求較細解析度，不應當成 PaceDock 設定漂移。
    if !requested {
        return Some(false);
    }
    intervals.map(|(minimum, _, current)| current > minimum.max(5_000))
}

fn global_timer_path() -> std::path::PathBuf {
    crate::config::config_dir().join("timer-global-applied.json")
}

pub(crate) fn remember_global_timer(enabled: bool) -> Result<(), String> {
    crate::state_auth::auth_write(&global_timer_path(), if enabled { "true" } else { "false" })
}

fn global_timer_drift() -> Option<bool> {
    let path = global_timer_path();
    if !path.try_exists().ok()? {
        return Some(false);
    }
    let expected: bool = serde_json::from_str(&crate::state_auth::auth_read(&path).ok()?).ok()?;
    let actual = crate::timer::global_requests_enabled()
        .ok()?
        .unwrap_or(false);
    Some(actual != expected)
}

type SettingReadings = Vec<(Setting, Option<bool>)>;

fn collect(app: &tauri::AppHandle) -> Option<(SettingReadings, String)> {
    let state = app.state::<Arc<AppState>>();
    // 與所有 GPU/MSI/電源寫入共用 reservation，避免在操作途中讀到暫態。
    let _guard = state.benchmark.reserve_mutation().ok()?;
    let cfg = state.config.read().ok()?;
    let mut readings =
        crate::benchmark::manager::monitored_gpu_settings(state.benchmark.backend.as_ref());
    readings.extend(crate::power::monitored_power_settings());
    readings.push((
        Setting::Timer,
        timer_drift(
            cfg.settings.high_precision_timer,
            crate::timer::enabled(),
            crate::timer::intervals(),
        ),
    ));
    readings.push((Setting::GlobalTimer, global_timer_drift()));
    readings.push((
        Setting::TimerExempt,
        crate::timer::exemption_drift(cfg.settings.high_precision_timer),
    ));
    Some((readings, cfg.settings.language.clone()))
}

pub(crate) fn start(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut monitor = Monitor::default();
        let mut displayed = None;
        loop {
            let handle = app.clone();
            match tauri::async_runtime::spawn_blocking(move || collect(&handle)).await {
                Ok(Some((readings, language))) => {
                    let notify = monitor.observe(&readings);
                    let english = language.starts_with("en");
                    let labels = monitor
                        .active
                        .iter()
                        .map(|s| s.label(english))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let warning = !monitor.active.is_empty();
                    let tooltip = if warning {
                        format!(
                            "PaceDock — {}: {labels}",
                            if english {
                                "Settings changed"
                            } else {
                                "設定不一致"
                            }
                        )
                    } else {
                        format!("PaceDock v{}", app.package_info().version)
                    };
                    if displayed.as_ref() != Some(&(warning, tooltip.clone())) {
                        match crate::tray::set_drift_warning(&app, warning, &tooltip) {
                            Ok(()) => displayed = Some((warning, tooltip)),
                            Err(e) => log::warn!("更新設定漂移圖示失敗: {e}"),
                        }
                    }
                    if notify {
                        let title = if english {
                            "PaceDock: settings changed"
                        } else {
                            "PaceDock：設定與套用值不一致"
                        };
                        let body = if english {
                            format!("{labels}. Open PaceDock to review and reapply or restore these settings.")
                        } else {
                            format!("{labels}。請開啟 PaceDock 檢查，並重新套用或還原設定。")
                        };
                        if let Err(e) = crate::tray::notify(&app, title, &body) {
                            log::warn!("發送設定漂移通知失敗: {e}");
                        }
                    }
                }
                Ok(None) => {} // busy：保留既有警示，下一輪再讀。
                Err(e) => log::warn!("設定漂移監看失敗: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_are_deduplicated_and_rearmed_after_recovery() {
        let mut monitor = Monitor::default();
        assert!(!monitor.observe(&[(Setting::Gpu, Some(false))]));
        assert!(monitor.observe(&[(Setting::Gpu, Some(true))]));
        assert!(!monitor.observe(&[(Setting::Gpu, Some(true))]));
        assert!(!monitor.observe(&[(Setting::Gpu, None)]));
        assert!(monitor.active.contains(&Setting::Gpu));
        assert!(monitor.observe(&[(Setting::Usb, Some(true))]));
        assert!(!monitor.observe(&[(Setting::Gpu, Some(false))]));
        assert!(monitor.active.contains(&Setting::Usb));
        assert!(monitor.observe(&[(Setting::Gpu, Some(true))]));
        monitor.observe(&[(Setting::Gpu, Some(false)), (Setting::Usb, Some(false))]);
        assert!(monitor.active.is_empty());
    }

    #[test]
    fn power_checks_both_ac_and_dc_without_treating_unknown_as_recovered() {
        assert_eq!(disabled_power_drift(Some(0), Some(1)), Some(true));
        assert_eq!(disabled_power_drift(Some(1), None), Some(true));
        assert_eq!(disabled_power_drift(Some(0), Some(0)), Some(false));
        assert_eq!(disabled_power_drift(Some(0), None), None);
    }

    #[test]
    fn timer_respects_supported_resolution_and_other_processes() {
        assert_eq!(
            timer_drift(false, false, Some((5_000, 156_250, 5_000))),
            Some(false)
        );
        assert_eq!(timer_drift(true, false, None), Some(true));
        assert_eq!(timer_drift(true, true, None), None);
        assert_eq!(
            timer_drift(true, true, Some((5_000, 156_250, 156_250))),
            Some(true)
        );
        assert_eq!(
            timer_drift(true, true, Some((10_000, 156_250, 10_000))),
            Some(false)
        );
    }
}
