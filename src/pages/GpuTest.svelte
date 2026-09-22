<script lang="ts">
  import { onMount } from "svelte";
  import { locale, t } from "svelte-i18n";
  import * as ipc from "../lib/ipc";
  import {
    benchmarkProgress,
    benchmarkState,
    gpuOperationBusy,
    settings,
    topology,
  } from "../lib/stores";
  import { coreLabel, policyIndices } from "../lib/core";
  import type {
    AffinityPolicy,
    BenchmarkConfig,
    CoreTarget,
    GpuDevice,
    InterruptVerification,
    MsiStatus,
    QuickSchedule,
    SessionDetail,
    SessionSummary,
  } from "../lib/types";
  import ConfirmDialog from "../components/ConfirmDialog.svelte";
  import GpuInterrupts from "../components/GpuInterrupts.svelte";
  import SystemHealth from "../components/SystemHealth.svelte";
  import DpcScan from "../components/DpcScan.svelte";
  import PowerTweaks from "../components/PowerTweaks.svelte";
  import GameCapture from "./game/GameCapture.svelte";
  import HistoryTab from "./gpu/HistoryTab.svelte";

  let devices = $state<GpuDevice[]>([]);
  let targets = $state<CoreTarget[]>([]);
  let selected = $state<number[]>([]);
  let gpu = $state("");
  let workload = $state<"Vulkan" | "D3D9">("Vulkan");
  let warmup = $state(3),
    sample = $state(10),
    retestWarmup = $state(5),
    retestSample = $state(20);
  let width = $state(1280),
    height = $state(720),
    fpsCap = $state(0);
  let adaptive = $state(true),
    triple = $state(false);
  let schedule = $state<QuickSchedule | null>(null);
  let history = $state<SessionSummary[]>([]);
  let detail = $state<SessionDetail | null>(null);
  let policy = $state<AffinityPolicy | null>(null);
  let msi = $state<MsiStatus | null>(null);
  let verification = $state<InterruptVerification | null>(null);
  let manual = $state<number | null>(null),
    chosen = $state<number | null>(null);
  let section = $state<"status" | "test" | "diagnostics" | "history">("status");
  let busy = $state(false),
    cancelSent = $state(false);
  let error = $state(""),
    notice = $state("");
  let action = $state<
    | "start"
    | "apply"
    | "manual"
    | "restore"
    | "delete"
    | "delCapture"
    | "msi"
    | "msiRestore"
    | null
  >(null);
  // 遊戲量測區塊 ref(僅 history 分頁掛載)與待刪除的量測紀錄 id
  let gameCapture = $state<GameCapture>();
  let captureDeleteId = $state("");
  let handled = $state("");
  const running = $derived($benchmarkState?.status === "Running");
  const compact = $derived($benchmarkState?.windowLayout === "CompactProgress");
  const locked = $derived(
    busy || running || $gpuOperationBusy || $benchmarkState?.gpuBusy,
  );
  // 進階模式（Settings 全域開關）：關閉時隱藏測試進階設定、狀態原始值與診斷分頁
  const advanced = $derived($settings?.advancedMode ?? false);
  const recovery = $derived($benchmarkState?.recoveryRequired ?? false);
  const quick = $derived(detail?.summary.quick);
  const eligible = $derived(
    detail?.summary.status === "Completed" &&
      quick?.methodVersion === 3 &&
      quick.status !== "Insufficient" &&
      detail.summary.captureQuality?.integrityPassed &&
      !recovery,
  );
  const current = $derived(
    $benchmarkProgress?.target ?? $benchmarkState?.currentTarget,
  );
  const cancelling = $derived(cancelSent || $benchmarkState?.cancelRequested);
  const label = (target: CoreTarget) => coreLabel(target, $locale === "en");
  const config = $derived<BenchmarkConfig>({
    candidateLps: [],
    candidateCoreIds: selected,
    methodVersion: 3,
    gpuInstanceId: gpu,
    workload,
    warmUpSecs: warmup,
    sampleSecs: sample,
    retestWarmUpSecs: retestWarmup,
    retestSampleSecs: retestSample,
    repetitions: 1,
    syncWorkloadAffinity: false,
    fullscreen: false,
    width,
    height,
    fpsCap,
    fpsCapPolicy: adaptive ? "Adaptive" : "Fixed",
    tripleBuffer: triple,
    vulkanArgs: [
      "--fullscreen=0",
      `--width=${width}`,
      `--height=${height}`,
      `--fps_cap=${fpsCap}`,
      `--triple_buffering=${triple ? 1 : 0}`,
    ],
    gamePath: null,
    windowTitle: null,
  });
  const policyText = $derived.by(() => {
    if (!policy) return $t("quick.unknown");
    if (!policy.assignmentSetOverride.present) return $t("quick.noOverride");
    const indices = policyIndices(policy.assignmentSetOverride.bytes);
    const core = $topology?.physicalCores.find(
      (c) =>
        c.lpIndices.length === indices.length &&
        c.lpIndices.every((i) => indices.includes(i)),
    );
    if (core) return label({ coreId: core.id, lpIndices: indices });
    return indices.length
      ? `LP ${indices.join($locale === "en" ? ", " : "、")}`
      : $t("quick.emptyMask");
  });
  const en = $derived($locale === "en");
  const driftAlert = $derived($benchmarkState?.policyDrift === "Drifted");
  const msiText = $derived.by(() => {
    if (!msi) return en ? "Not detected." : "尚未偵測。";
    if (msi.value === 1)
      return en
        ? "Message Signaled Interrupts enabled (lower interrupt latency)."
        : "訊息號誌中斷（MSI）已啟用，中斷延遲較低。";
    if (msi.value === 0)
      return en
        ? "Line-based interrupts in use. Enabling MSI can reduce interrupt latency on older GPUs."
        : "目前為行線中斷（line-based）。舊款 GPU 啟用 MSI 可降低中斷延遲（套用後會重啟裝置）。";
    return en
      ? "Not configured; the driver default applies."
      : "未設定，使用驅動預設值。";
  });
  const confirmTarget = $derived(targets.find((c) => c.coreId === manual));
  const resultTarget = $derived(
    quick?.retest.find((c) => c.target.coreId === chosen)?.target,
  );
  const confirmMessage = $derived(
    action === "start"
      ? $t("quick.startConfirm")
      : action === "restore"
        ? $t("quick.restoreConfirm")
        : action === "delete"
          ? $t("quick.deleteConfirm")
          : action === "delCapture"
            ? $t("measure.deleteConfirm")
            : action === "msi"
              ? $t("quick.msiConfirm")
              : action === "msiRestore"
                ? $t("quick.msiRestoreConfirm")
                : `${$t("quick.applyConfirm")} ${action === "manual" && confirmTarget ? label(confirmTarget) : resultTarget ? label(resultTarget) : ""}${action === "manual" ? ` — ${$t("quick.untested")}` : ""}`,
  );

  $effect(() => {
    const request = config;
    let active = true;
    schedule = null;
    if (selected.length && gpu) {
      const timer = setTimeout(() => {
        ipc
          .getQuickSchedule(request)
          .then((v) => {
            if (active) schedule = v;
          })
          .catch(() => {
            if (active) schedule = null;
          });
      }, 150);
      return () => {
        active = false;
        clearTimeout(timer);
      };
    }
  });
  $effect(() => {
    const instance = gpu;
    let active = true;
    policy = null;
    msi = null;
    if (instance)
      ipc
        .getGpuAffinityPolicy(instance)
        .then((v) => {
          if (active) policy = v;
        })
        .catch((e) => {
          if (active) error = String(e);
        });
    if (instance)
      ipc
        .getMsiStatus(instance)
        .then((v) => {
          if (active) msi = v;
        })
        .catch(() => {
          if (active) msi = null;
        });
    return () => {
      active = false;
    };
  });
  // 進階模式關閉時不得停留在診斷分頁
  $effect(() => {
    if (!advanced && section === "diagnostics") section = "status";
  });
  $effect(() => {
    const state = $benchmarkState;
    if (
      state?.sessionId &&
      ["Completed", "Failed", "Cancelled"].includes(state.status) &&
      state.sessionId !== handled
    ) {
      handled = state.sessionId;
      cancelSent = false;
      void loadResult(state.sessionId);
      void refreshHistory();
    }
  });
  onMount(() => {
    void initialize();
  });
  // 進入 history 分頁時刷新遊戲清單(區塊掛載後才可呼叫)
  $effect(() => {
    if (section === "history") void gameCapture?.refreshGames();
  });
  async function initialize() {
    try {
      [devices, targets] = await Promise.all([
        ipc.enumerateGpus(),
        ipc.getCoreCandidates(),
      ]);
      gpu = devices[0]?.instanceId ?? "";
      selected = targets.map((c) => c.coreId);
      manual = targets[0]?.coreId ?? null;
      await refreshHistory();
    } catch (e) {
      error = String(e);
    }
  }
  async function refreshHistory() {
    try {
      history = await ipc.listBenchmarkSessions();
    } catch (e) {
      error = String(e);
    }
  }
  let loadSerial = 0;
  async function loadResult(id: string) {
    const serial = ++loadSerial;
    chosen = null;
    detail = null;
    try {
      const result = await ipc.getBenchmarkSession(id);
      if (serial !== loadSerial) return;
      detail = result;
      section = "history";
      if (devices.some((g) => g.instanceId === result.summary.gpuInstanceId))
        gpu = result.summary.gpuInstanceId;
      if (
        result.summary.status === "Completed" &&
        ["Consistent", "SingleCandidate"].includes(
          result.summary.quick?.status ?? "",
        )
      )
        chosen = result.summary.quick?.retest[0]?.target.coreId ?? null;
    } catch (e) {
      if (serial === loadSerial) error = String(e);
    }
  }
  async function confirm() {
    const pending = action;
    action = null;
    if (!pending || locked) return;
    busy = true;
    gpuOperationBusy.set(true);
    error = "";
    notice = "";
    try {
      if (pending === "start") {
        cancelSent = false;
        await ipc.startGpuBenchmark(config);
        benchmarkState.set(await ipc.getBenchmarkState());
      }
      if (pending === "apply" && detail && chosen !== null) {
        await ipc.applyGpuCore(
          detail.summary.gpuInstanceId,
          chosen,
          detail.summary.id,
        );
        void runVerification(detail.summary.gpuInstanceId, chosen);
      }
      if (pending === "manual" && manual !== null) {
        await ipc.applyGpuCore(gpu, manual, null);
        void runVerification(gpu, manual);
      }
      if (pending === "restore") await ipc.restorePreviousGpuAffinity();
      if (pending === "msi" && gpu) await ipc.applyMsi(gpu);
      if (pending === "msiRestore" && gpu) await ipc.restoreMsi(gpu);
      if (pending === "delete" && detail) {
        await ipc.deleteBenchmarkSession(detail.summary.id);
        detail = null;
        chosen = null;
        await refreshHistory();
      }
      if (pending === "delCapture" && captureDeleteId) {
        await ipc.deleteGameCapture(captureDeleteId);
        await gameCapture?.captureDeleted(captureDeleteId);
        captureDeleteId = "";
      }
      if (pending !== "start") notice = $t("quick.done");
      if (pending === "apply" || pending === "manual") section = "status";
      if (gpu) policy = await ipc.getGpuAffinityPolicy(gpu);
      if (gpu && (pending === "msi" || pending === "msiRestore"))
        msi = await ipc.getMsiStatus(gpu);
    } catch (e) {
      error = String(e);
    } finally {
      try {
        benchmarkState.set(await ipc.getBenchmarkState());
      } finally {
        busy = false;
        gpuOperationBusy.set(false);
      }
    }
  }
  async function cancel() {
    cancelSent = true;
    try {
      await ipc.cancelBenchmark();
    } catch (e) {
      cancelSent = false;
      error = String(e);
    }
  }
  // 套用後背景落點驗證：實測 ISR+DPC 是否落在釘選 LP（不阻塞套用流程）
  async function runVerification(instanceId: string, coreId: number | null) {
    const fromDetail = detail?.summary.quick?.candidates.find(
      (c) => c.coreId === coreId,
    );
    const lps =
      fromDetail?.lpIndices ??
      targets.find((c) => c.coreId === coreId)?.lpIndices ??
      [];
    if (!instanceId || !lps.length) return;
    verification = null;
    try {
      verification = await ipc.verifyInterruptAffinity(instanceId, lps);
    } catch (e) {
      error = String(e);
    }
  }
  const verificationText = $derived.by(() => {
    if (!verification) return "";
    const en0 = en;
    const pct = verification.onPinnedPct.toFixed(1);
    const base = `${verification.pinnedEvents.toLocaleString()} / ${verification.totalEvents.toLocaleString()} · ${pct}%`;
    if (verification.verdict === "passed")
      return en0
        ? `Verified: ${base} of driver interrupt/DPC events landed on the pinned core.`
        : `已驗證：${base} 的驅動中斷/DPC 事件落在釘選核心。`;
    if (verification.verdict === "failed")
      return en0
        ? `Only ${base} of events landed on the pinned core; the policy may not be effective.`
        : `僅 ${base} 的事件落在釘選核心，政策可能未生效。`;
    return en0
      ? "No interrupt events observed during sampling; cannot verify."
      : "取樣期間未觀測到中斷事件，無法驗證。";
  });
  function toggle(id: number, checked: boolean) {
    selected = checked ? [...selected, id] : selected.filter((c) => c !== id);
  }
  function requestCaptureDelete(captureId: string) {
    captureDeleteId = captureId;
    action = "delCapture";
  }
</script>

<div class="gpu-page" class:compact>
  {#if error}<div class="panel error" role="alert">
      {$t(`errors.${error}`, { default: error })}
    </div>{/if}
  {#if notice && !compact}<p role="status">{notice}</p>{/if}
  {#if recovery && !compact}<div class="panel error" role="alert">
      {$t("quick.recovery")}
    </div>{/if}
  {#if running || compact}
    <section
      class="panel progress-panel"
      class:sticky={!compact}
      aria-live="polite"
    >
      <div class="progress-info">
        <h2>{cancelling ? $t("quick.cancelling") : $t("quick.running")}</h2>
        <p>{current ? label(current) : $t("quick.calibrating")}</p>
        <p>
          {cancelling
            ? $t(
                `quick.cancelStage.${$benchmarkState?.cancelStage ?? "requested"}`,
                { default: $t("quick.cancelling") },
              )
            : $t(
                `quick.phase.${$benchmarkProgress?.phase ?? $benchmarkState?.currentPhase ?? "Calibration"}`,
              )} · {$t(
            `quick.stage.${$benchmarkProgress?.stage ?? "starting"}`,
          )}
        </p>
        <progress
          max="100"
          value={cancelling
            ? ($benchmarkState?.cancelProgress ?? 0)
            : ($benchmarkState?.progressPct ?? 0)}
        ></progress>
        <p>{$t("quick.keepWindow")}</p>
      </div>
      <button
        class="danger small"
        disabled={cancelling}
        aria-busy={!!cancelling}
        onclick={cancel}
        >{cancelling ? $t("quick.cancelling") : $t("quick.cancel")}</button
      >
    </section>
  {/if}
  {#if !compact}
    <header>
      <h1>{$t("quick.title")}</h1>
      <p class="hint">{$t("quick.scope")}</p>
    </header>
    <div class="tabs">
      <button
        class="ghost"
        aria-pressed={section === "status"}
        onclick={() => (section = "status")}>{$t("quick.tabStatus")}</button
      >
      <button
        class="ghost"
        aria-pressed={section === "test"}
        onclick={() => (section = "test")}>{$t("quick.test")}</button
      >
      {#if advanced}<button
          class="ghost"
          aria-pressed={section === "diagnostics"}
          onclick={() => (section = "diagnostics")}
          >{$t("quick.tabDiagnostics")}</button
        >{/if}
      <button
        class="ghost"
        aria-pressed={section === "history"}
        onclick={() => (section = "history")}>{$t("quick.history")}</button
      >
    </div>
    {#if section === "status"}
      <div class="cards">
        <section
          class="panel card"
          class:ok={!driftAlert && policy?.assignmentSetOverride.present}
          class:drift={driftAlert}
        >
          <h2>
            {$t("quick.currentPolicy")} — {devices.find(
              (d) => d.instanceId === gpu,
            )?.friendlyName ?? gpu}
          </h2>
          {#if driftAlert}
            <p class="card-status warn-text">
              <strong>{$t("quick.statusDrift")}</strong>
            </p>
            <p class="hint">
              {en
                ? `Applied core no longer matches the registry (core ${$benchmarkState?.appliedCore ?? "—"}). Re-apply or restore.`
                : `已套用核心與 registry 不符（核心 ${$benchmarkState?.appliedCore ?? "—"}）。請重新套用或還原。`}
            </p>
            <div class="start-row">
              {#if $benchmarkState?.appliedCore != null}
                <button
                  disabled={locked}
                  onclick={() => {
                    manual = $benchmarkState?.appliedCore ?? null;
                    action = "manual";
                  }}>{$t("quick.apply")}</button
                >
              {/if}
              <button disabled={locked} onclick={() => (action = "restore")}
                >{$t("quick.restore")}</button
              >
            </div>
          {:else}
            <p
              class="card-status"
              class:ok-text={policy?.assignmentSetOverride.present}
            >
              {policy?.assignmentSetOverride.present
                ? $t("quick.statusOk")
                : $t("quick.statusInfo")}
            </p>
            <p><strong>{policyText}</strong></p>
            {#if advanced}<small
                >DevicePolicy: {policy?.devicePolicy.bytes?.join(", ") ??
                  "—"}</small
              >{/if}
            <button disabled={locked} onclick={() => (action = "restore")}
              >{$t("quick.restore")}</button
            >
          {/if}
        </section>
        <section
          class="panel card"
          class:ok={msi?.value === 1}
          class:warn={msi?.value === 0}
        >
          <h2>MSI</h2>
          <p class="card-status">{msiText}</p>
          {#if msi?.value === 0}
            <button disabled={locked} onclick={() => (action = "msi")}
              >{$t("quick.msiEnable")}</button
            >
          {:else if msi?.restorable}
            <button disabled={locked} onclick={() => (action = "msiRestore")}
              >{$t("quick.msiRestore")}</button
            >
          {/if}
        </section>
        <PowerTweaks {locked} />
        {#if verification}
          <section
            class="panel card"
            class:ok={verification.verdict === "passed"}
            class:drift={verification.verdict === "failed"}
          >
            <h2>{$t("quick.cardVerify")}</h2>
            <p class="card-status">{verificationText}</p>
            {#if verification.eventsLost}<small
                >{en
                  ? "Sampling reported lost events; re-run for a definitive result."
                  : "取樣有遺失事件，建議重新驗證。"}</small
              >{/if}
          </section>
        {/if}
      </div>
    {:else if section === "test"}
      <section class="panel">
        <label class="field"
          >GPU<select bind:value={gpu} disabled={locked}
            >{#each devices as device}<option value={device.instanceId}
                >{device.friendlyName}</option
              >{/each}</select
          ></label
        >
        {#if advanced}
          <h2>{$t("quick.candidates")}</h2>
          <p class="hint">{$t("quick.candidateHint")}</p>
          {#if !targets.length}<p role="alert">
              {$t("quick.noCandidates")}
            </p>{/if}
          <div class="core-grid">
            {#each targets as target}<label class="core-choice"
                ><input
                  type="checkbox"
                  checked={selected.includes(target.coreId)}
                  disabled={locked}
                  onchange={(e) =>
                    toggle(target.coreId, e.currentTarget.checked)}
                />{label(target)}</label
              >{/each}
          </div>
        {/if}
        <div class="start-row">
          <p>
            {#if schedule}{$t("quick.estimate", {
                values: {
                  min: Math.ceil(schedule.estimatedMinSecs / 60),
                  max: Math.ceil(schedule.estimatedMaxSecs / 60),
                  captures: schedule.candidateCaptures,
                },
              })}{:else}{$t("quick.chooseCandidates")}{/if}<br /><small
              >{$t("quick.estimateHint")}</small
            >
          </p>
          <button
            class="primary"
            disabled={locked ||
              recovery ||
              !schedule ||
              !selected.length ||
              !gpu}
            onclick={() => (action = "start")}>{$t("quick.start")}</button
          >
        </div>
      </section>
      {#if advanced}
        <details class="panel">
          <summary>{$t("quick.advanced")}</summary>
          <div class="form-grid">
            <label
              >{$t("quick.screenWarmup")}<input
                type="number"
                min="0"
                max="60"
                bind:value={warmup}
              /></label
            >
            <label
              >{$t("quick.screenSample")}<input
                type="number"
                min="1"
                max="120"
                bind:value={sample}
              /></label
            >
            <label
              >{$t("quick.retestWarmup")}<input
                type="number"
                min="0"
                max="60"
                bind:value={retestWarmup}
              /></label
            >
            <label
              >{$t("quick.retestSample")}<input
                type="number"
                min="1"
                max="120"
                bind:value={retestSample}
              /></label
            >
            <label
              >Workload<select bind:value={workload}
                ><option>Vulkan</option><option>D3D9</option></select
              ></label
            >
            <label
              >{$t("quick.width")}<input
                type="number"
                min="1"
                bind:value={width}
              /></label
            >
            <label
              >{$t("quick.height")}<input
                type="number"
                min="1"
                bind:value={height}
              /></label
            >
            <label
              >FPS cap<input
                type="number"
                min="0"
                max="10000"
                bind:value={fpsCap}
                disabled={adaptive}
              /></label
            >
            <label
              ><input type="checkbox" bind:checked={adaptive} />{$t(
                "quick.adaptive",
              )}</label
            >
            <label
              ><input type="checkbox" bind:checked={triple} />{$t(
                "quick.triple",
              )}</label
            >
          </div>
          <h2>{$t("quick.manual")}</h2>
          <p class="hint">{$t("quick.untested")}</p>
          <div class="start-row">
            <select aria-label={$t("quick.manual")} bind:value={manual}
              >{#each targets as target}<option value={target.coreId}
                  >{label(target)}</option
                >{/each}</select
            ><button
              disabled={locked || recovery || manual === null || !gpu}
              onclick={() => (action = "manual")}>{$t("quick.apply")}</button
            >
          </div>
        </details>
      {/if}
    {:else if section === "diagnostics"}
      <GpuInterrupts instanceId={gpu} locked={!!locked} />
      <DpcScan />
      <SystemHealth />
    {:else}
      <GameCapture
        bind:this={gameCapture}
        {locked}
        {recovery}
        {gpu}
        onError={(message) => (error = message)}
        onNotice={(message) => (notice = message)}
        requestDelete={requestCaptureDelete}
      />
      <HistoryTab
        {locked}
        {eligible}
        {quick}
        {history}
        {detail}
        bind:chosen
        {label}
        onSelect={(id) => void loadResult(id)}
        onApply={() => (action = "apply")}
        onDelete={() => (action = "delete")}
      />
    {/if}
  {/if}
</div>

<ConfirmDialog
  open={action !== null}
  title={$t("quick.confirm")}
  message={confirmMessage}
  confirmLabel={$t("quick.confirm")}
  cancelLabel={$t("quick.back")}
  {busy}
  danger={action === "delete" || action === "delCapture"}
  onconfirm={confirm}
  oncancel={() => (action = null)}
/>

<style>
  .gpu-page {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  header h1 {
    margin: 0 0 8px;
    font-size: 24px;
  }
  h2 {
    font-size: 17px;
    margin: 12px 0;
  }
  p {
    line-height: 1.6;
  }
  .hint,
  small {
    color: var(--text-secondary);
  }
  .tabs,
  .start-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }
  .tabs {
    justify-content: flex-start;
  }
  .start-row > p {
    flex: 1 1 240px;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .start-row > select {
    flex: 1 1 220px;
    min-width: 0;
  }
  .field,
  .form-grid label {
    display: flex;
    gap: 8px;
    flex-direction: column;
  }
  .core-grid,
  .form-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(230px, 1fr));
    gap: 12px;
    margin: 16px 0;
  }
  .core-choice {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 44px;
  }
  summary {
    cursor: pointer;
    padding: 8px 0;
  }
  details {
    margin: 12px 0;
  }
  select {
    max-width: 100%;
  }
  progress {
    width: 100%;
    height: 10px;
    accent-color: var(--accent);
  }
  .error {
    color: var(--danger);
  }
  /* 狀態卡：左緣色條表健康度 */
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
    gap: 12px;
  }
  .card {
    border-left: 3px solid var(--border-default);
  }
  .card h2 {
    margin: 0 0 4px;
    font-size: 15px;
  }
  .card-status {
    margin: 0;
  }
  .card.ok {
    border-left-color: var(--success);
  }
  .card.warn {
    border-left-color: var(--warning);
  }
  .card.drift {
    border-left-color: var(--danger);
  }
  .ok-text {
    color: var(--success);
  }
  .warn-text {
    color: var(--warning);
  }
  /* 執行中 sticky 進度列：任何分頁可見 */
  .progress-panel.sticky {
    position: sticky;
    top: 0;
    z-index: 10;
    background: var(--surface-1);
  }
  .compact {
    gap: 4px;
    height: 100%;
    min-height: 0;
  }
  .compact .panel {
    padding: 12px;
  }
  .compact .progress-panel {
    display: flex;
    flex-direction: column;
    gap: 8px;
    flex: 1;
    min-height: 0;
  }
  .compact .progress-info {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overflow-wrap: anywhere;
    padding: 2px;
  }
  .compact .progress-panel > button {
    align-self: flex-end;
  }
  .compact p {
    margin: 6px 0;
    font-size: 12px;
  }
  .compact h2 {
    margin: 4px 0;
  }
  @media (max-width: 700px) {
    .start-row {
      flex-wrap: wrap;
    }
  }
</style>
