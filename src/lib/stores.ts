import { writable } from 'svelte/store';
import type {
  AffinityPolicy,
  BenchmarkProgress,
  BenchmarkState,
  GpuDevice,
  SessionSummary,
  Settings,
  Topology,
  UpdateState,
} from './types';

export const topology = writable<Topology | null>(null);
export const settings = writable<Settings | null>(null);
/// 更新狀態
export const updateState = writable<UpdateState | null>(null);
/// 是否為可攜版（由 get_update_info 設定）
export const isPortable = writable<boolean>(false);

// ── GPU 基準測試（Task 4 UI）────────────────────────────────────────────

/// 執行期狀態（後端持有；reload 後由 get_benchmark_state 重建）
export const benchmarkState = writable<BenchmarkState | null>(null);
/// 最近一次 progress 事件（round/lp/eta 等即時資訊）
export const benchmarkProgress = writable<BenchmarkProgress | null>(null);
/// 目前顯示配接器清單
export const gpuDevices = writable<GpuDevice[]>([]);
/// 歷史 session 摘要列表
export const benchmarkSessions = writable<SessionSummary[]>([]);
/// 目前選取 GPU 的中斷親和性策略
export const gpuPolicy = writable<AffinityPolicy | null>(null);

export const gpuOperationBusy = writable(false);
