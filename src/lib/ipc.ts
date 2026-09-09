import { invoke } from '@tauri-apps/api/core';
import type {
  AffinityPolicy,
  BenchmarkConfig,
  BenchmarkState,
  GpuDevice,
  SessionDetail,
  SessionSummary,
  Settings,
  StorageInfo,
  Topology,
  UpdateInfo,
} from './types';

export const getTopology = () => invoke<Topology>('get_topology');
export const getSettings = () => invoke<Settings>('get_settings');
export const saveSettings = (settings: Settings) => invoke<void>('save_settings', { settings });
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
export const getMigrationStatus = () => invoke<import('./types').MigrationStatus>('get_migration_status');
export const acknowledgeMigration = () => invoke<void>('acknowledge_migration');
export const retryAutostartCleanup = () => invoke<void>('retry_autostart_cleanup');

export const beginUpdate = () => invoke<void>('begin_update');
export const endUpdate = () => invoke<void>('end_update');
