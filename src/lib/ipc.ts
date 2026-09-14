import { invoke } from '@tauri-apps/api/core';
import type {
  AffinityPolicy,
  BenchmarkConfig,
  BenchmarkState,
  GpuDevice,
  GameCaptureRecord,
  GameWindow,
  HealthCheck,
  MsiStatus,
  DpcScan,
  InterruptVerification,
  SessionDetail,
  SessionSummary,
  Settings,
  StorageInfo,
  TimerExemptEntry,
  TimerStatus,
  Topology,
  UpdateInfo,
} from './types';

export const getTopology = () => invoke<Topology>('get_topology');
export const getSettings = () => invoke<Settings>('get_settings');
export const saveSettings = (settings: Settings) => invoke<void>('save_settings', { settings });
export const getTimerStatus = () => invoke<TimerStatus>('get_timer_status');
export const getTimerGlobalEnabled = () => invoke<boolean | null>('get_timer_global_enabled');
export const setTimerGlobalEnabled = (enabled: boolean) =>
  invoke<void>('set_timer_global_enabled', { enabled });
export const setTimerExempt = (exeName: string, enabled: boolean) =>
  invoke<void>('set_timer_exempt', { exeName, enabled });
export const listTimerExempts = () => invoke<TimerExemptEntry[]>('list_timer_exempts');
export const openDataFolder = () => invoke<void>('open_data_folder');

// 更新相關
export const getUpdateInfo = () => invoke<UpdateInfo>('get_update_info');
export const checkPortableUpdate = () => invoke<void>('check_portable_update');
export const performPortableUpdate = () => invoke<void>('perform_portable_update');

// GPU 基準測試相關
export const enumerateGpus = () => invoke<GpuDevice[]>('enumerate_gpus');
export const getBenchmarkState = () => invoke<BenchmarkState>('get_benchmark_state');
export const listBenchmarkSessions = () => invoke<SessionSummary[]>('list_benchmark_sessions');
export const getBenchmarkSession = (id: string) =>
  invoke<SessionDetail>('get_benchmark_session', { id });
export const deleteBenchmarkSession = (id: string) =>
  invoke<void>('delete_benchmark_session', { id });
export const getBenchmarkStorageInfo = () => invoke<StorageInfo>('get_benchmark_storage_info');
export const getGpuAffinityPolicy = (instanceId: string) =>
  invoke<AffinityPolicy>('get_gpu_affinity_policy', { instanceId });
export const restorePreviousGpuAffinity = () => invoke<void>('restore_previous_gpu_affinity');
export const startGpuBenchmark = (config: BenchmarkConfig) =>
  invoke<void>('start_gpu_benchmark', { config });
export const cancelBenchmark = () => invoke<void>('cancel_benchmark');
export const getCoreCandidates = () => invoke<import('./types').CoreTarget[]>('get_core_candidates');
export const getQuickSchedule = (config: BenchmarkConfig) => invoke<import('./types').QuickSchedule>('get_quick_schedule', { config });
export const applyGpuCore = (instanceId: string, coreId: number, sessionId: string | null) => invoke<void>('apply_gpu_core', { instanceId, coreId, sessionId });

export const beginUpdate = () => invoke<void>('begin_update');
export const endUpdate = () => invoke<void>('end_update');

// 實際遊戲量測相關
export const listGameWindows = () => invoke<GameWindow[]>('list_game_windows');
export const startGameCapture = (pid: number, title: string, durationSecs: number, gpuInstanceId: string | null) =>
  invoke<GameCaptureRecord>('start_game_capture', { pid, title, durationSecs, gpuInstanceId });
export const cancelGameCapture = () => invoke<void>('cancel_game_capture');
export const listGameCaptures = () => invoke<GameCaptureRecord[]>('list_game_captures');
export const deleteGameCapture = (id: string) => invoke<void>('delete_game_capture', { id });

// 系統環境健檢（唯讀）
export const getSystemHealth = () => invoke<HealthCheck[]>('get_system_health');

// GPU MSI 模式
export const getMsiStatus = (instanceId: string) =>
  invoke<MsiStatus>('get_msi_status', { instanceId });
export const applyMsi = (instanceId: string) => invoke<void>('apply_msi', { instanceId });
export const restoreMsi = (instanceId: string) => invoke<void>('restore_msi', { instanceId });

// 中斷落點驗證與 DPC 掃描
export const verifyInterruptAffinity = (instanceId: string, expectedLps: number[]) =>
  invoke<InterruptVerification>('verify_interrupt_affinity', { instanceId, expectedLps });
export const scanDpcOffenders = (topN?: number) =>
  invoke<DpcScan>('scan_dpc_offenders', { topN: topN ?? null });
