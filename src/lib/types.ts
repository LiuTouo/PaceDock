// 與 PLAN §5 對應的 TS 型別。enum 字串值採 PascalCase（與 serde 序列化一致）。

export type Theme = 'Dark' | 'Light';
export interface Settings {
  language: string; // 'zh-TW' | 'en'
  startWithWindows: boolean;
  startMinimized: boolean;
  closeToTray: boolean;
  pollIntervalMs: number;
  showAdvancedPriorities: boolean;
  theme: Theme;
}

export interface LogicalProcessor {
  index: number;
  coreId: number;
  isSmtSibling: boolean;
  efficiencyClass: number;
}

export interface PhysicalCore {
  id: number;
  lpIndices: number[];
  efficiencyClass: number;
  isPCore: boolean;
}

export interface Topology {
  logicalProcessors: LogicalProcessor[];
  physicalCores: PhysicalCore[];
  hasSmt: boolean;
  hasHybrid: boolean;
  totalLp: number;
  processorGroups: number; // 偵測到的處理器群組數；>1 = 多群組，僅 group 0 列入拓撲
}

// ── 更新相關型別 ──

export type UpdateStatus =
  | 'Idle'
  | 'Checking'
  | 'UpToDate'
  | 'Available'
  | 'Downloading'
  | 'Installing'
  | 'Error';

export interface UpdateState {
  status: UpdateStatus;
  latestVersion: string | null;
  currentVersion: string;
  progress: number | null; // 0..100，僅 Downloading 有意義
  error: string | null;
}

export interface UpdateInfo {
  version: string;
  portable: boolean;
}

// ── GPU 基準測試相關型別 ──

export type SessionStatus = 'Pending' | 'Running' | 'Completed' | 'Failed' | 'Cancelled';
export type BenchmarkStage = 'Init' | 'Warmup' | 'Collecting' | 'Finalizing';
export type WorkloadKind = 'Vulkan' | 'D3D9';
export type ReliabilityStatus = 'Unassessed' | 'Passed' | 'Equivalent' | 'Inconclusive';
export type FpsCapPolicy = 'Adaptive' | 'Fixed';
export type BenchmarkOperation = 'Benchmark' | 'EquivalentValidation';
export type WindowLayout = 'Normal' | 'CompactProgress';

/** 可靠性/信心摘要（camelCase，與後端 ReliabilitySummary 一致） */
export interface ReliabilitySummary {
  status: ReliabilityStatus;
  perRoundWinners: Array<number | null>; // 動態長度對應 evaluatedRounds；缺漏 round 為 null（保留位置）
  candidateLp: number | null; // 穩健候選 LP（跨 round 複合分數中位數最高）
  runnerUpLp: number | null; // 穩健亞軍；單一 LP 時為 null
  candidateWins: number; // 候選在所有預期 round 中的勝場數
  evaluatedRounds: number; // 評估的確認 round 數（3..=5）；舊 session 缺欄為 0
  requiredWins: number; // 已停用（新排程以一致性規則 + bootstrap 穩定性區間判定）；固定 0，保留供向後相容
  compositeAdvantagePct: number | null; // 配對複合分數優勢點估計（%）；不可得為 null
  avgFpsAdvantagePct: number | null; // 護欄：Avg FPS 優勢（%）
  p1LowAdvantagePct: number | null; // 護欄：1% low 優勢（%）
  spikeRateDeltaPp: number | null; // 護欄：spike rate 差（百分點，正 = 候選較差）
  // 舊版聚合改善欄位（保留供向後相容，非穩健證據）：
  avgFpsPct: number | null;
  p1LowPct: number | null;
  p01LowPct: number | null;
  // 新排程（篩選 + 確認）證據欄位：
  screeningRounds: number; // 篩選 round 數（固定 3）；舊 session 缺欄為 0
  confirmationRounds: number; // 確認 round 數（3..=5）；舊 session 缺欄為 0
  ciLowerPct: number | null; // bootstrap 穩定性區間下界（%）；欄位名保留 ciLowerPct 供向後相容，非信賴區間
  ciUpperPct: number | null; // bootstrap 穩定性區間上界（%）；欄位名保留 ciUpperPct 供向後相容，非信賴區間
  stoppingReason: string; // 'passed' | 'equivalent' | 'inconclusive' | ''（舊 session）
  // 前向/反向驗證 phase 證據（新排程；舊 session 缺欄 → 後端 serde default 仍會發出）：
  forwardVerdict?: string; // 'passed' | 'reversal' | 'equivalent' | 'inconclusive' | ''
  reverseRan?: boolean;
  reverseVerdict?: string; // 'passed' | 'inconclusive' | ''
  reverseCandidateLp?: number | null;
  reverseRounds?: number;
  // 演算法版本：新確認演算法（有界 log-ratio + 等效判定）為 2；舊 session 缺欄 → 0
  algorithmVersion?: number;
  // 等效判定的 raw median evidence（僅 Equivalent 判定時有值；% 或 pp）：
  equivalentAvgImprovementPct?: number | null;
  equivalentP1ImprovementPct?: number | null;
  equivalentP01ImprovementPct?: number | null;
  equivalentMadDeltaPp?: number | null;
  equivalentSpikeDeltaPp?: number | null;
}

export interface GpuDevice {
  instanceId: string; // 穩定 PnP 身分
  friendlyName: string;
}

/** 基準測試參數 */
export interface BenchmarkConfig {
  candidateCoreIds?: number[];
  methodVersion?: number;
  retestWarmUpSecs?: number;
  retestSampleSecs?: number;
  candidateLps: number[]; // 要逐一測試的候選 LP；空 = 全部支援 LP
  gpuInstanceId: string | null;
  workload: WorkloadKind;
  warmUpSecs: number; // 預設 5
  sampleSecs: number; // 預設 30
  repetitions: number; // 已停用（新版兩階段排程忽略此欄位）；保留供舊 session 向後相容
  syncWorkloadAffinity: boolean; // 已棄用；固定 false。保留供舊 session 向後相容。
  fullscreen: boolean; // 預設 false
  width: number; // 1280
  height: number; // 720
  fpsCap: number; // 0 = 不限
  fpsCapPolicy?: FpsCapPolicy; // 校準策略：Adaptive（預設，忽略 fpsCap，依校準選定）| Fixed（沿用 fpsCap）
  tripleBuffer: boolean;
  vulkanArgs: string[]; // workload=Vulkan 時必須非空
  gamePath: string | null; // 相容舊欄位
  windowTitle: string | null;
}

/** 單一候選 LP 的最終測試結果 */
export interface LpResult {
  lp: number;
  avgFps: number | null;
  maxFps: number | null;
  minFps: number | null;
  stdevFps: number | null; // Bessel（n-1）
  frametimeMadPct: number | null; // frametime MAD 正規化為中位數百分比（越低越穩）
  spikeRatePct: number | null; // 慢幀 spike rate：frametime 超 2×中位數幀佔比（%，越低越好）
  p1Low: number | null; // 1% low（最慢 1% 個 instantaneous FPS 平均）
  p01Low: number | null; // 0.1% low
  p001Low: number | null; // 0.01% low
  p0005Low: number | null; // 0.005% low
  p1Percentile: number | null; // 1% percentile（最慢 1% 分位數）
  p01Percentile: number | null; // 0.1% percentile
  p001Percentile: number | null; // 0.01% percentile
  p0005Percentile: number | null; // 0.005% percentile
  sampleCount: number;
  avgFrameTimeMs: number | null;
  completed: boolean;
  error: string | null; // 錯誤代碼，查 i18n errors.*
}

/** 執行期 progress 事件（`gpu-benchmark-progress`） */
export type BenchmarkPhase =
  | 'Screening'
  | 'Refinement'
  | 'Confirmation'
  | 'ReverseConfirmation'
  | 'EquivalentValidation';

export interface BenchmarkProgress {
  target?: CoreTarget | null;
  sessionId: string;
  stage: string; // starting/applying/launching/collecting/collected/finalizing
  round: number | null;
  phase?: BenchmarkPhase | null;
  phaseRound?: number | null;
  lp: number | null;
  percentage: number;
  etaSecs: number | null;
  error: string | null;
  cancelStage?: string | null; // 取消專用階段（requested/stopping/restoring/finalizing）；非取消為 null
  cancelProgress?: number | null; // 取消專用百分比 0..100（單調）；非取消為 null
  windowIntegrity?: WindowIntegrity | null; // workload 視窗完整性快照（狀態改變才附帶；None = 未回報）
}

/** 執行期間的原始取樣 */
export interface CoreSample {
  lp: number;
  fps: number;
  frameTimeMs: number;
}

export interface SessionSummary {
  quick?: QuickResult | null;
  id: string;
  status: SessionStatus;
  startedAt: string;
  finishedAt: string | null;
  gpuName: string;
  gpuInstanceId: string;
  cpuFingerprint: string;
  bestLp: number | null;
  reliability: ReliabilitySummary; // 可靠性判定；舊 session 缺欄位時後端解讀為 Unassessed
  severeLps: number[]; // 嚴重 LP（後端判定）
  sampleCount: number;
  totalBytes: number; // 整個 session 資料夾位元組數（即時計算）
  config: BenchmarkConfig;
  error: string | null; // 終結失敗原因（i18n errors.* 代碼）；成功/取消為 null
  // 新 schema（舊 session 缺欄 → 後端 serde default 仍會發出）：
  screeningCandidateLp?: number | null; // 篩選（1 輪）+ refinement（2 輪）後的 Top 1 候選 LP
  screeningRunnerUpLp?: number | null; // 篩選（1 輪）+ refinement（2 輪）後的 Top 2 亞軍 LP
  confirmationWinnerLp?: number | null; // 前向確認 phase 的勝者（Passed=候選；Reversal=亞軍；否則 null）
  verifiedBestLp?: number | null; // 反向驗證確認的最終最佳 LP（只有反向 Passed 才設置）
  captureQuality?: CaptureQuality; // capture 完整性摘要
  environmentStability?: EnvironmentStability; // 環境穩定度摘要
  equivalentFinalistLps?: number[]; // Equivalent 判定的等效 finalists（[candidate, runner]）；非 Equivalent → 空
}

/** 歷史 session 的「可否套用」狀態（相容性判定只在後端） */
export interface ApplyStatus {
  canApply: boolean;
  reason: string | null; // null = 可套用；否則為 i18n errors.* 代碼
  equivalentMode: boolean; // 是否為 equivalent-mode session
  allowedLps: number[]; // 允許套用的 LP（equivalent finalists）；非 equivalent → 空
  requiresSafetyValidation: boolean; // 套用前是否需先完成 safety validation
}

/** Session 層的 capture 完整性摘要（camelCase，與後端 CaptureQuality 一致） */
export interface CaptureQuality {
  totalCaptures: number; // 所有 capture attempt（含校準/overflow retry/drift rerun）
  validCaptures: number;
  invalidCaptures: number;
  windowInvalidCaptures?: number; // 因視窗完整性失敗而 invalid 的 capture 數
  windowRetryCaptures?: number; // 因視窗完整性失敗觸發的重跑次數
  overflowedPresentEvents: number;
  etwEventsLost: number;
  integrityPassed: boolean; // 全部正式用於結果的 capture 完整且 session 完成才 true
  effectiveFpsCap: number; // 校準/正式鎖定的有效 FPS cap
  circularBufferSize: number;
}

/** Session 層的環境穩定度摘要（camelCase，與後端 EnvironmentStability 一致） */
export interface EnvironmentStability {
  passed: boolean;
  driftReruns: number;
  error: string | null; // 不穩定時的穩定錯誤碼（= BENCHMARK_ENV_UNSTABLE）；穩定為 null
}

/** 等效安全驗證狀態（serde PascalCase；default None） */
export type EquivalentSafetyStatus = 'None' | 'Pending' | 'Passed' | 'Failed' | 'Cancelled';

/** 等效安全驗證 contract（camelCase，與後端 EquivalentSafetyValidation 一致） */
export interface EquivalentSafetyValidation {
  status: EquivalentSafetyStatus;
  selectedLp: number | null;
  referenceLp: number | null;
  rounds: number;
  avgImprovementPct: number | null;
  p1ImprovementPct: number | null;
  p01ImprovementPct: number | null;
  madDeltaPp: number | null;
  spikeDeltaPp: number | null;
  captureQuality: CaptureQuality;
  environmentStability: EnvironmentStability;
  validatedAt: string | null;
  referencePolicyMask: number[] | null; // 精簡 LE 單 LP mask bytes
  reason: string | null; // 失敗原因或 "passed"
}

export interface SessionDetail {
  summary: SessionSummary;
  results: LpResult[];
  samples: CoreSample[];
  // 分相結果（新 schema；舊 session 缺欄）：
  screeningResults?: LpResult[]; // 篩選階段（3 round 全 LP）逐 LP 聚合結果
  refinementResults?: LpResult[]; // 新版前兩名的獨立複測結果；舊格式保留原 refinement 語意
  confirmationResults?: LpResult[]; // 前向確認階段（Top 2，3..=5 round）逐 LP 聚合結果
  equivalentSafetyValidation?: EquivalentSafetyValidation | null; // 等效安全驗證（後續 task 填值）
}

/** workload 視窗完整性快照（capture/warmup 期間輪詢回報） */
export interface WindowIntegrity {
  foreground: boolean; // true = 前景（ok）
  minimized: boolean; // true = 最小化（異常）
  position: boolean; // true = 位置正確（ok）
  occlusion: boolean; // true = 被遮擋（異常）
  retries: number; // 累計視窗完整性重跑次數
  error: string | null; // 穩定錯誤碼（重試用盡）；null = 無
}

/** 執行期狀態（get_benchmark_state） */
export interface BenchmarkState {
  currentPhase?: BenchmarkPhase | null;
  gpuBusy?: boolean;
  currentTarget?: CoreTarget | null;
  status: SessionStatus;
  sessionId: string | null;
  currentLp: number | null;
  stage: BenchmarkStage;
  progressPct: number;
  elapsedSecs: number;
  cancelRequested: boolean;
  recoveryRequired: boolean; // 啟動還原失敗 → 封鎖 test/apply
  operation?: BenchmarkOperation | null; // 目前背景操作（Benchmark | EquivalentValidation | null）
  windowLayout?: WindowLayout; // 主視窗版面（Normal | CompactProgress）
  windowIntegrity?: WindowIntegrity; // workload 視窗完整性快照
  cancelStage?: string | null; // 取消專用階段（requested/stopping/restoring/finalizing）；無取消為 null
  cancelProgress?: number | null; // 取消專用百分比 0..100；無取消為 null
}

/** 單一註冊表值的精確快照（presence + 型別 + 原始位元組） */
export interface RegistryValueSnapshot {
  present: boolean;
  valueType: number | null; // 原生 REG_VALUE_TYPE 數值（REG_DWORD=4、REG_BINARY=3 …）
  bytes: number[] | null; // 原始位元組（little-endian）
}

/** GPU 中斷親和性策略（DevicePolicy + AssignmentSetOverride） */
export interface AffinityPolicy {
  instanceId: string;
  devicePolicy: RegistryValueSnapshot;
  assignmentSetOverride: RegistryValueSnapshot;
}

export interface StorageInfo {
  totalBytes: number;
  sessionCount: number;
}

export interface CoreTarget { coreId: number; lpIndices: number[] }
export interface QuickSchedule {
  screeningWarmupSecs: number; screeningSampleSecs: number;
  retestWarmupSecs: number; retestSampleSecs: number;
  candidateCaptures: number; estimatedMinSecs: number; estimatedMaxSecs: number;
}
export interface CoreCapture { target: CoreTarget; metrics: LpResult; score: number }
export type RankingStatus = 'Consistent' | 'Close' | 'Reversed' | 'SingleCandidate' | 'Insufficient';
export interface QuickResult {
  methodVersion: number; candidates: CoreTarget[]; seed: number;
  screeningOrder: number[]; retestOrder: number[]; schedule: QuickSchedule;
  screening: CoreCapture[]; retest: CoreCapture[];
  status: RankingStatus; relativeGapPct: number | null;
}
export interface MigrationStatus { noticeRequired: boolean; cleanupError: string | null }
