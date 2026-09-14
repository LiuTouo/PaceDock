//! 短時間 ETW ISR 實測。只歸屬到驅動模組，不把 affinity 設定當成觀測值。
//! 格式依據：Microsoft Learn ETW ISR / Image_Load（64-bit Windows）。
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use serde::Serialize;
use windows::core::{w, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_SUCCESS, HANDLE};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Diagnostics::Etw::*;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const PERF_INFO: GUID = GUID::from_u128(0xce1dbfb4_137e_4da6_87b0_3f59aa102cbc);
const IMAGE: GUID = GUID::from_u128(0x2cb15d1d_5fc1_11d2_abe1_00a0c911f518);
const SAMPLE_SECS: u64 = 3;
/// PerfInfo DPC 事件 opcode：66 = DPC 開始、68 = DPC 結束（67 是 ISR）。
/// 與 ISR 的 50/67 同源（PerfView KernelTraceEventParser 對照）；
/// live 解碼如有出入，調整這兩個常數即可。
const DPC_START_OPCODE: u8 = 66;
const DPC_STOP_OPCODE: u8 = 68;
/// 落點驗證門檻：目標驅動 ISR+DPC 事件落在釘選 LP 的最低佔比
const ON_PINNED_PASS_RATIO: f64 = 0.95;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterruptCpu {
    lp: u16,
    count: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterruptSample {
    instance_id: String,
    driver: String,
    shared_driver: bool,
    sampled_at: String,
    sample_secs: u64,
    cpus: Vec<InterruptCpu>,
    /// dxgkrnl 是所有顯示配接器共用層，必須分開顯示，不能歸屬到選定 GPU。
    graphics_kernel_cpus: Vec<InterruptCpu>,
    events_lost: u32,
}

/// 套用後落點驗證結果：目標驅動（含 dxgkrnl）的 ISR+DPC 事件
/// 實際落在釘選 LP 的佔比。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterruptVerification {
    /// "passed" | "failed" | "inconclusive"
    verdict: String,
    pinned_events: u64,
    total_events: u64,
    on_pinned_pct: f64,
    events_lost: u32,
    sample_secs: u64,
}

/// 全系統 DPC 大戶（按總耗時排序）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DpcOffender {
    driver: String,
    count: u64,
    total_duration_ms: f64,
    max_duration_ms: f64,
}

/// 全系統 DPC 掃描結果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DpcScan {
    offenders: Vec<DpcOffender>,
    events_lost: u32,
    sampled_at: String,
    sample_secs: u64,
}

/// 單一 (routine, lp) 的 DPC 統計
#[derive(Debug, Default, Clone, Copy)]
struct DpcStat {
    count: u64,
    total_dur: u64,
    max_dur: u64,
}

#[tauri::command]
pub async fn sample_gpu_interrupts(
    state: tauri::State<'_, Arc<crate::AppState>>,
    instance_id: String,
) -> Result<InterruptSample, String> {
    // 與 benchmark / 套用 / 更新共用排他權，避免量測到裝置重啟過程。
    let guard = state.benchmark.reserve_mutation()?;
    let backend = state.benchmark.backend.clone();
    if state.topology.processor_groups > 1 {
        return Err("GPU_INTERRUPTS_GROUPS".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        let devices = backend
            .enumerate_present_adapters()
            .map_err(|e| e.code().to_string())?;
        if !devices.iter().any(|d| d.instance_id == instance_id) {
            return Err("GPU_NOT_FOUND".into());
        }
        let driver = device_driver(&instance_id)?;
        let shared_driver = devices
            .iter()
            .filter(|d| d.instance_id != instance_id)
            .any(|d| device_driver(&d.instance_id).map_or(true, |other| other == driver));
        let (data, events_lost) = capture(&driver)?;
        if !data.driver_seen {
            return Err("GPU_INTERRUPTS_DRIVER_UNRESOLVED".into());
        }
        Ok(InterruptSample {
            instance_id,
            driver,
            shared_driver,
            sampled_at: chrono::Utc::now().to_rfc3339(),
            sample_secs: SAMPLE_SECS,
            cpus: data.counts(false),
            graphics_kernel_cpus: data.counts(true),
            events_lost,
        })
    })
    .await
    .map_err(|e| {
        log::error!("ISR worker: {e}");
        "GPU_INTERRUPTS_FAILED".to_string()
    })?
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn registry_string(path: &str, name: &str) -> Result<String, String> {
    let path = wide(path);
    let name = wide(name);
    let mut buffer = vec![0u16; 32768];
    let mut bytes = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS {
        return Err("GPU_INTERRUPTS_DRIVER_UNRESOLVED".into());
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..end]))
}

fn filename(path: &str) -> String {
    path.trim_matches('"')
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn device_driver(instance: &str) -> Result<String, String> {
    let service = registry_string(
        &format!(r"SYSTEM\CurrentControlSet\Enum\{instance}"),
        "Service",
    )?;
    if service.is_empty() || service.contains(['\\', '/', '\0']) {
        return Err("GPU_INTERRUPTS_DRIVER_UNRESOLVED".into());
    }
    let driver = filename(&registry_string(
        &format!(r"SYSTEM\CurrentControlSet\Services\{service}"),
        "ImagePath",
    )?);
    if !driver.ends_with(".sys") {
        return Err("GPU_INTERRUPTS_DRIVER_UNRESOLVED".into());
    }
    Ok(driver)
}

struct ProfilePrivilege {
    token: HANDLE,
    previous: TOKEN_PRIVILEGES,
}
impl ProfilePrivilege {
    fn enable() -> Result<Self, String> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
            .map_err(|_| "GPU_INTERRUPTS_PERMISSION".to_string())?;
            let mut owned = Self {
                token,
                previous: TOKEN_PRIVILEGES::default(),
            };
            let mut luid = Default::default();
            LookupPrivilegeValueW(None, w!("SeSystemProfilePrivilege"), &mut luid)
                .map_err(|_| "GPU_INTERRUPTS_PERMISSION".to_string())?;
            let requested = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut length = 0;
            AdjustTokenPrivileges(
                token,
                false,
                Some(&requested),
                size_of::<TOKEN_PRIVILEGES>() as u32,
                Some(&mut owned.previous),
                Some(&mut length),
            )
            .map_err(|_| "GPU_INTERRUPTS_PERMISSION".to_string())?;
            if GetLastError() != ERROR_SUCCESS {
                return Err("GPU_INTERRUPTS_PERMISSION".into());
            }
            Ok(owned)
        }
    }
}
impl Drop for ProfilePrivilege {
    fn drop(&mut self) {
        unsafe {
            if self.previous.PrivilegeCount != 0 {
                let _ =
                    AdjustTokenPrivileges(self.token, false, Some(&self.previous), 0, None, None);
            }
            let _ = CloseHandle(self.token);
        }
    }
}

// repr(C) 保證 properties / 名稱 buffer 的配置及對齊，避免 Vec<u8> cast 的未定義行為。
#[repr(C)]
struct Properties {
    header: EVENT_TRACE_PROPERTIES,
    name: [u16; 128],
    path: [u16; 1024],
}
struct TraceSession {
    handle: CONTROLTRACE_HANDLE,
    properties: Box<Properties>,
    path: PathBuf,
    active: bool,
}
impl TraceSession {
    fn stop(&mut self) -> Result<(), String> {
        let status = unsafe {
            ControlTraceW(
                self.handle,
                None,
                &mut self.properties.header,
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(trace_error(status.0));
        }
        self.active = false;
        Ok(())
    }
}
impl Drop for TraceSession {
    fn drop(&mut self) {
        if self.active {
            let _ = self.stop();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
fn trace_error(code: u32) -> String {
    log::warn!("GPU ISR ETW failed: Win32 {code}");
    match code {
        5 | 1314 => "GPU_INTERRUPTS_PERMISSION",
        _ => "GPU_INTERRUPTS_FAILED",
    }
    .into()
}

fn capture(driver: &str) -> Result<(Capture, u32), String> {
    let _privilege = ProfilePrivilege::enable()?;
    let id = uuid::Uuid::new_v4();
    let name = wide(&format!("PaceDock-ISR-{id}"));
    let path = std::env::temp_dir().join(format!("PaceDock-ISR-{id}.etl"));
    let path_w = wide(&path.to_string_lossy());
    let mut properties = Box::new(Properties {
        header: Default::default(),
        name: [0; 128],
        path: [0; 1024],
    });
    if path_w.len() > properties.path.len() {
        return Err("GPU_INTERRUPTS_FAILED".into());
    }
    properties.name[..name.len()].copy_from_slice(&name);
    properties.path[..path_w.len()].copy_from_slice(&path_w);
    let h = &mut properties.header;
    h.Wnode.BufferSize = size_of::<Properties>() as u32;
    h.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    h.Wnode.ClientContext = 1;
    h.Wnode.Guid = GUID::from_u128(id.as_u128());
    h.BufferSize = 64;
    h.MinimumBuffers = 16;
    h.MaximumBuffers = 128;
    h.MaximumFileSize = 32;
    h.LogFileMode = EVENT_TRACE_SYSTEM_LOGGER_MODE | EVENT_TRACE_FILE_MODE_SEQUENTIAL;
    h.EnableFlags = EVENT_TRACE_FLAG_INTERRUPT | EVENT_TRACE_FLAG_DPC | EVENT_TRACE_FLAG_IMAGE_LOAD;
    h.LoggerNameOffset = std::mem::offset_of!(Properties, name) as u32;
    h.LogFileNameOffset = std::mem::offset_of!(Properties, path) as u32;
    let mut session = TraceSession {
        handle: Default::default(),
        properties,
        path,
        active: false,
    };
    let status = unsafe {
        StartTraceW(
            &mut session.handle,
            PCWSTR(name.as_ptr()),
            &mut session.properties.header,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(trace_error(status.0));
    }
    session.active = true;
    std::thread::sleep(Duration::from_secs(SAMPLE_SECS));
    session.stop()?;
    let lost = session
        .properties
        .header
        .EventsLost
        .saturating_add(session.properties.header.LogBuffersLost);
    let mut data = Capture::new(driver);
    let mut log = EVENT_TRACE_LOGFILEW::default();
    let mut file = path_w;
    log.LogFileName = PWSTR(file.as_mut_ptr());
    log.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD;
    log.Anonymous2.EventRecordCallback = Some(event_callback);
    log.Context = (&mut data as *mut Capture).cast();
    // ProcessTrace 同步執行；data 與 file 活到 CloseTrace 之後。
    unsafe {
        let handle = OpenTraceW(&mut log);
        if handle.Value == u64::MAX {
            return Err(trace_error(GetLastError().0));
        }
        let result = ProcessTrace(&[handle], None, None);
        let _ = CloseTrace(handle);
        if result != ERROR_SUCCESS {
            return Err(trace_error(result.0));
        }
    }
    if data.invalid {
        return Err("GPU_INTERRUPTS_FAILED".into());
    }
    Ok((data, lost))
}

#[derive(Default)]
struct Capture {
    driver: String,
    driver_seen: bool,
    /// Image_Load 解析出的所有 .sys 模組（base, size, 檔名）；
    /// 驅動與 dxgkrnl 供 ISR 歸屬，其餘供 DPC 大戶歸屬。
    modules: Vec<(u64, u64, String)>,
    /// (routine, lp) → ISR 次數
    events: BTreeMap<(u64, u16), u64>,
    /// (routine, lp) → DPC 統計（配對 start/stop 後）
    dpc: BTreeMap<(u64, u16), DpcStat>,
    /// lp → (routine, start timestamp)：進行中的 DPC（per-LP DPC queue 序列化，
    /// 同 LP 不會並行兩個 DPC，可用 lp 配對 start/stop）
    open_dpc: BTreeMap<u16, (u64, u64)>,
    invalid: bool,
}
impl Capture {
    fn new(driver: &str) -> Self {
        Self {
            driver: driver.into(),
            ..Default::default()
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn event(
        &mut self,
        provider: GUID,
        opcode: u8,
        version: u8,
        pointer_size: usize,
        lp: u16,
        ts: u64,
        bytes: &[u8],
    ) {
        // PerfView KernelTraceEventParser 將 50（MSI）與 67 都解成 ISRTraceData。
        if provider == PERF_INFO && matches!(opcode, 50 | 67) {
            // InitialTime = 8 bytes，Routine = pointer，ReturnValue = claimed interrupt。
            if let (Some(routine), Some(&claimed)) = (
                read_pointer(bytes, 8, pointer_size),
                bytes.get(8 + pointer_size),
            ) {
                if claimed != 0 {
                    if self.events.len() >= 65536 {
                        self.invalid = true;
                        return;
                    }
                    *self.events.entry((routine, lp)).or_default() += 1;
                }
            } else {
                self.invalid = true;
            }
        } else if provider == PERF_INFO && opcode == DPC_START_OPCODE {
            // DPC 開始：InitialTime(8) + Routine(pointer)。
            if let Some(routine) = read_pointer(bytes, 8, pointer_size) {
                self.open_dpc.insert(lp, (routine, ts));
            } else {
                self.invalid = true;
            }
        } else if provider == PERF_INFO && opcode == DPC_STOP_OPCODE {
            // DPC 結束：以 lp 配對進行中的 start；duration = ts 差（100ns 單位）。
            if let Some((routine, start)) = self.open_dpc.remove(&lp) {
                let dur = ts.saturating_sub(start);
                let stat = self.dpc.entry((routine, lp)).or_default();
                stat.count += 1;
                stat.total_dur += dur;
                stat.max_dur = stat.max_dur.max(dur);
            }
        } else if provider == IMAGE && matches!(opcode, 3 | 4 | 10) {
            // Image_Load v2/v3: 3 pointers + 8 DWORDs，後接 UTF-16 FileName。
            if !matches!(version, 2 | 3) {
                return;
            }
            let offset = 3 * pointer_size + 32;
            let Some(raw_name) = bytes.get(offset..) else {
                return;
            };
            let name: Vec<u16> = raw_name
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .take_while(|&c| c != 0)
                .collect();
            let name = filename(&String::from_utf16_lossy(&name));
            if let (Some(base), Some(size)) = (
                read_pointer(bytes, 0, pointer_size),
                read_pointer(bytes, pointer_size, pointer_size),
            ) {
                if base == 0 || size == 0 {
                    return;
                }
                if name == self.driver {
                    self.driver_seen = true;
                }
                let module = (base, size, name);
                if !self.modules.contains(&module) {
                    self.modules.push(module);
                }
            }
        } else if provider == IMAGE && opcode == 2 {
            // 模組卸載使時間外位址歸屬不可靠；此取樣不回傳猜測值。
            if let Some(base) = read_pointer(bytes, 0, pointer_size) {
                if self.modules.iter().any(|m| m.0 == base) {
                    self.invalid = true;
                }
            }
        }
    }
    /// ISR 事件按 LP 聚合（`kernel=true` 只計 dxgkrnl，否則只計目標驅動）
    fn counts(&self, kernel: bool) -> Vec<InterruptCpu> {
        let mut counts = BTreeMap::<u16, u64>::new();
        for (&(routine, lp), &count) in &self.events {
            if self.module_name(routine).is_some_and(|name| {
                if kernel {
                    name == "dxgkrnl.sys"
                } else {
                    name == self.driver
                }
            }) {
                *counts.entry(lp).or_default() += count;
            }
        }
        counts
            .into_iter()
            .map(|(lp, count)| InterruptCpu { lp, count })
            .collect()
    }
    /// DPC 事件按 LP 聚合（同 counts 語意）
    fn dpc_counts(&self, kernel: bool) -> BTreeMap<u16, u64> {
        let mut counts = BTreeMap::<u16, u64>::new();
        for (&(routine, lp), stat) in &self.dpc {
            if self.module_name(routine).is_some_and(|name| {
                if kernel {
                    name == "dxgkrnl.sys"
                } else {
                    name == self.driver
                }
            }) {
                *counts.entry(lp).or_default() += stat.count;
            }
        }
        counts
    }
    /// 位址 → 模組檔名（落在任何已知 .sys 模組範圍內）
    fn module_name(&self, address: u64) -> Option<&str> {
        self.modules
            .iter()
            .find(|&&(base, size, _)| address >= base && address - base < size)
            .map(|(_, _, name)| name.as_str())
    }
    /// 目標驅動（含 dxgkrnl）ISR+DPC 事件中，落在釘選 LP 的（命中, 總數）
    fn verification(&self, expected_lps: &[u16]) -> (u64, u64) {
        let pinned: BTreeSet<u16> = expected_lps.iter().copied().collect();
        let mut total = 0u64;
        let mut hit = 0u64;
        let mut add = |lp: &u16, count: &u64| {
            total += count;
            if pinned.contains(lp) {
                hit += count;
            }
        };
        for c in self.counts(false) {
            add(&c.lp, &c.count);
        }
        for c in self.counts(true) {
            add(&c.lp, &c.count);
        }
        for (lp, count) in self.dpc_counts(false) {
            add(&lp, &count);
        }
        for (lp, count) in self.dpc_counts(true) {
            add(&lp, &count);
        }
        (hit, total)
    }
    /// 全系統 DPC 大戶（按總耗時降序；未知模組歸 "unknown"）
    fn offenders(&self, top_n: usize) -> Vec<DpcOffender> {
        let mut per_driver: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        for (&(routine, _lp), stat) in &self.dpc {
            let name = self.module_name(routine).unwrap_or("unknown").to_string();
            let e = per_driver.entry(name).or_default();
            e.0 += stat.count;
            e.1 += stat.total_dur;
            e.2 = e.2.max(stat.max_dur);
        }
        let mut out: Vec<DpcOffender> = per_driver
            .into_iter()
            .map(|(driver, (count, total_dur, max_dur))| DpcOffender {
                driver,
                count,
                total_duration_ms: total_dur as f64 / 10_000.0,
                max_duration_ms: max_dur as f64 / 10_000.0,
            })
            .collect();
        out.sort_by(|a, b| {
            b.total_duration_ms
                .partial_cmp(&a.total_duration_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.count.cmp(&a.count))
        });
        out.truncate(top_n);
        out
    }
}
fn read_pointer(bytes: &[u8], offset: usize, size: usize) -> Option<u64> {
    match size {
        4 => Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as u64),
        8 => Some(u64::from_le_bytes(
            bytes.get(offset..offset + 8)?.try_into().ok()?,
        )),
        _ => None,
    }
}
unsafe extern "system" fn event_callback(record: *mut EVENT_RECORD) {
    let Some(record) = record.as_ref() else {
        return;
    };
    let Some(data) = (record.UserContext as *mut Capture).as_mut() else {
        return;
    };
    if record.UserData.is_null() {
        return;
    }
    // 不讓 panic 穿越 Windows FFI 邊界。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let flags = record.EventHeader.Flags as u32;
        let size = if flags & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 {
            4
        } else {
            8
        };
        let lp = if flags & EVENT_HEADER_FLAG_PROCESSOR_INDEX != 0 {
            record.BufferContext.Anonymous.ProcessorIndex
        } else {
            record.BufferContext.Anonymous.Anonymous.ProcessorNumber as u16
        };
        data.event(
            record.EventHeader.ProviderId,
            record.EventHeader.EventDescriptor.Opcode,
            record.EventHeader.EventDescriptor.Version,
            size,
            lp,
            record.EventHeader.TimeStamp as u64,
            std::slice::from_raw_parts(record.UserData.cast(), record.UserDataLength as usize),
        );
    }));
    if result.is_err() {
        data.invalid = true;
    }
}

/// 套用後落點驗證：取樣 3 秒，檢查目標驅動（含 dxgkrnl）的 ISR+DPC
/// 事件是否 ≥95% 落在釘選 LP。機制驗證（registry 回讀之外的實測證據）。
#[tauri::command]
pub async fn verify_interrupt_affinity(
    state: tauri::State<'_, Arc<crate::AppState>>,
    instance_id: String,
    expected_lps: Vec<u16>,
) -> Result<InterruptVerification, String> {
    // 與 benchmark / 套用共用排他權（同 sample_gpu_interrupts）
    let guard = state.benchmark.reserve_mutation()?;
    let backend = state.benchmark.backend.clone();
    if state.topology.processor_groups > 1 {
        return Err("GPU_INTERRUPTS_GROUPS".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        let devices = backend
            .enumerate_present_adapters()
            .map_err(|e| e.code().to_string())?;
        if !devices.iter().any(|d| d.instance_id == instance_id) {
            return Err("GPU_NOT_FOUND".into());
        }
        let driver = device_driver(&instance_id)?;
        let (data, lost) = capture(&driver)?;
        if !data.driver_seen {
            return Err("GPU_INTERRUPTS_DRIVER_UNRESOLVED".into());
        }
        let (pinned, total) = data.verification(&expected_lps);
        let pct = if total > 0 {
            pinned as f64 / total as f64
        } else {
            0.0
        };
        let verdict = if total == 0 {
            "inconclusive"
        } else if pct >= ON_PINNED_PASS_RATIO {
            "passed"
        } else {
            "failed"
        };
        Ok(InterruptVerification {
            verdict: verdict.into(),
            pinned_events: pinned,
            total_events: total,
            on_pinned_pct: pct * 100.0,
            events_lost: lost,
            sample_secs: SAMPLE_SECS,
        })
    })
    .await
    .map_err(|e| {
        log::error!("ISR verify worker: {e}");
        "GPU_INTERRUPTS_FAILED".to_string()
    })?
}

/// 全系統 DPC 大戶掃描（3 秒取樣；按總耗時排序，找出干擾幀格的裝置驅動）
#[tauri::command]
pub async fn scan_dpc_offenders(
    state: tauri::State<'_, Arc<crate::AppState>>,
    top_n: Option<u8>,
) -> Result<DpcScan, String> {
    let guard = state.benchmark.reserve_mutation()?;
    if state.topology.processor_groups > 1 {
        return Err("GPU_INTERRUPTS_GROUPS".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        // 掃全系統，不歸屬到特定 GPU 驅動（Image_Load 仍記錄所有 .sys 模組）
        let (data, lost) = capture("")?;
        let top_n = top_n.unwrap_or(5).clamp(1, 20) as usize;
        Ok(DpcScan {
            offenders: data.offenders(top_n),
            events_lost: lost,
            sampled_at: chrono::Utc::now().to_rfc3339(),
            sample_secs: SAMPLE_SECS,
        })
    })
    .await
    .map_err(|e| {
        log::error!("DPC scan worker: {e}");
        "GPU_INTERRUPTS_FAILED".to_string()
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires administrator and a real display adapter; captures ETW for 3 seconds"]
    fn live_interrupt_sample() {
        use crate::gpu::GpuBackend;
        let devices = crate::gpu::RealGpuBackend
            .enumerate_present_adapters()
            .unwrap();
        let device = devices.first().expect("present GPU");
        let driver = device_driver(&device.instance_id).unwrap();
        let (data, lost) = capture(&driver).unwrap();
        eprintln!(
            "driver={driver}, resolved={}, cpus={:?}, shared={:?}, lost={lost}",
            data.driver_seen,
            data.counts(false),
            data.counts(true)
        );
        assert!(
            data.driver_seen,
            "ETW image rundown must resolve selected GPU driver"
        );
    }
    fn image(data: &mut Capture, name: &str, base: u64) {
        let mut bytes = vec![0u8; 56];
        bytes[..8].copy_from_slice(&base.to_le_bytes());
        bytes[8..16].copy_from_slice(&256u64.to_le_bytes());
        for c in name.encode_utf16().chain(Some(0)) {
            bytes.extend(c.to_le_bytes());
        }
        data.event(IMAGE, 3, 2, 8, 0, 0, &bytes);
    }
    fn isr(data: &mut Capture, address: u64, lp: u16, claimed: u8) {
        let mut bytes = vec![0; 8];
        bytes.extend(address.to_le_bytes());
        bytes.push(claimed);
        data.event(PERF_INFO, 67, 2, 8, lp, 0, &bytes);
    }
    fn dpc_start(data: &mut Capture, address: u64, lp: u16, ts: u64) {
        let mut bytes = vec![0u8; 8];
        bytes.extend(address.to_le_bytes());
        data.event(PERF_INFO, DPC_START_OPCODE, 2, 8, lp, ts, &bytes);
    }
    fn dpc_stop(data: &mut Capture, lp: u16, ts: u64) {
        data.event(PERF_INFO, DPC_STOP_OPCODE, 2, 8, lp, ts, &[0u8; 16]);
    }
    #[test]
    fn resolves_rundown_after_events_and_separates_shared_kernel() {
        let mut data = Capture::new("gpu.sys");
        isr(&mut data, 0x1010, 7, 1);
        isr(&mut data, 0x1010, 7, 1);
        isr(&mut data, 0x1010, 2, 0);
        isr(&mut data, 0x2010, 3, 1);
        isr(&mut data, 0x3010, 0, 1);
        image(&mut data, r"\SystemRoot\gpu.sys", 0x1000);
        image(&mut data, "dxgkrnl.sys", 0x2000);
        assert!(data.driver_seen);
        assert_eq!(data.counts(false).len(), 1);
        assert_eq!(
            (data.counts(false)[0].lp, data.counts(false)[0].count),
            (7, 2)
        );
        assert_eq!(data.counts(true)[0].lp, 3);
    }
    #[test]
    fn malformed_isr_and_module_unload_invalidate_sample() {
        let mut data = Capture::new("gpu.sys");
        data.event(PERF_INFO, 67, 2, 8, 0, 0, &[0; 8]);
        assert!(data.invalid);
        let mut data = Capture::new("gpu.sys");
        image(&mut data, "gpu.sys", 0x1000);
        data.event(IMAGE, 2, 2, 8, 0, 0, &0x1000u64.to_le_bytes());
        assert!(data.invalid);
    }
    #[test]
    fn no_observations_does_not_infer_core_zero() {
        let mut data = Capture::new("gpu.sys");
        image(&mut data, "gpu.sys", 0x1000);
        assert!(data.counts(false).is_empty());
        assert_eq!(read_pointer(&[1, 0, 0, 0], 0, 4), Some(1));
        assert_eq!(read_pointer(&[1, 0, 0, 0], 0, 8), None);
    }

    #[test]
    fn message_interrupt_opcode_and_32_bit_pointer_are_supported() {
        let mut data = Capture::new("gpu.sys");
        image(&mut data, "gpu.sys", 0x1000);
        let mut bytes = vec![0; 8];
        bytes.extend(0x1010u32.to_le_bytes());
        bytes.push(1);
        data.event(PERF_INFO, 50, 2, 4, 5, 0, &bytes);
        data.event(PERF_INFO, DPC_STOP_OPCODE, 2, 4, 6, 0, &bytes); // DPC 不列為 ISR。
        assert_eq!(data.counts(false).len(), 1);
        assert_eq!(
            (data.counts(false)[0].lp, data.counts(false)[0].count),
            (5, 1)
        );
    }

    #[test]
    fn dpc_start_stop_pairs_accumulate_duration_per_lp() {
        let mut data = Capture::new("gpu.sys");
        image(&mut data, "gpu.sys", 0x1000);
        // LP 7：兩個 DPC（2ms、6ms）；LP 3：一個 1ms 的未知模組 DPC
        dpc_start(&mut data, 0x1010, 7, 1_000_000);
        dpc_stop(&mut data, 7, 1_000_000 + 20_000); // 2ms
        dpc_start(&mut data, 0x1010, 7, 2_000_000);
        dpc_stop(&mut data, 7, 2_000_000 + 60_000); // 6ms
        dpc_start(&mut data, 0x9010, 3, 3_000_000);
        dpc_stop(&mut data, 3, 3_000_000 + 10_000); // 1ms
        // 無配對的 stop 不計、不 invalid
        dpc_stop(&mut data, 9, 5_000_000);
        assert!(!data.invalid);
        // 目標驅動 DPC 落在 LP 7，共 8ms
        let counts = data.dpc_counts(false);
        assert_eq!(counts.get(&7), Some(&2));
        assert_eq!(counts.get(&3), None);
        let offenders = data.offenders(10);
        assert_eq!(offenders[0].driver, "gpu.sys");
        assert_eq!(offenders[0].count, 2);
        assert!((offenders[0].total_duration_ms - 8.0).abs() < 1e-9);
        assert!((offenders[0].max_duration_ms - 6.0).abs() < 1e-9);
        assert!(offenders.iter().any(|o| o.driver != "gpu.sys"));
    }

    #[test]
    fn verification_counts_driver_and_kernel_events_on_pinned_lps() {
        let mut data = Capture::new("gpu.sys");
        image(&mut data, "gpu.sys", 0x1000);
        image(&mut data, "dxgkrnl.sys", 0x2000);
        // ISR：目標驅動 LP7×2、LP2×1；dxgkrnl LP3×1
        isr(&mut data, 0x1010, 7, 1);
        isr(&mut data, 0x1010, 7, 1);
        isr(&mut data, 0x1010, 2, 1);
        isr(&mut data, 0x2010, 3, 1);
        // DPC：目標驅動 LP7×2
        dpc_start(&mut data, 0x1010, 7, 0);
        dpc_stop(&mut data, 7, 20_000);
        dpc_start(&mut data, 0x1010, 7, 30_000);
        dpc_stop(&mut data, 7, 40_000);
        // 總事件 6，落在 LP7 = 2+2 = 4
        let (hit, total) = data.verification(&[7]);
        assert_eq!((hit, total), (4, 6));
        // 全部釘選 → 100%
        let (hit, total) = data.verification(&[2, 3, 7]);
        assert_eq!((hit, total), (6, 6));
        // 無事件 → (0,0)，由呼叫端判 inconclusive
        let empty = Capture::new("gpu.sys");
        assert_eq!(empty.verification(&[7]), (0, 0));
    }
}
