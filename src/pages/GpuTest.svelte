<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
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
    CoreCapture,
    CoreTarget,
    GameCaptureProgress,
    GameCaptureRecord,
    GameWindow,
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
  // 遊戲量測狀態
  let games = $state<GameWindow[]>([]);
  let captureGame = $state("");
  let captureDuration = $state(30);
  let capturing = $state(false),
    capturePct = $state(0);
  let captures = $state<GameCaptureRecord[]>([]);
  let pickA = $state(""),
    pickB = $state("");
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
    let unlisten: (() => void) | null = null;
    void listen<GameCaptureProgress>("game-capture-progress", (e) => {
      if (e.payload.stage === "done") {
        capturing = false;
        void refreshCaptures();
      } else if (e.payload.stage === "cancelled") capturing = false;
      else capturePct = e.payload.percentage;
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      unlisten?.();
    };
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
      await Promise.all([refreshGames(), refreshCaptures()]);
    } catch (e) {
      error = String(e);
    }
  }
  async function refreshGames() {
    try {
      games = await ipc.listGameWindows();
      if (!games.some((g) => String(g.pid) === captureGame))
        captureGame = games[0] ? String(games[0].pid) : "";
    } catch (e) {
      error = String(e);
    }
  }
  async function refreshCaptures() {
    try {
      captures = await ipc.listGameCaptures();
    } catch (e) {
      error = String(e);
    }
  }
  async function startCapture() {
    const game = games.find((g) => String(g.pid) === captureGame);
    if (!game || capturing || locked) return;
    capturing = true;
    capturePct = 0;
    error = "";
    notice = "";
    try {
      const record = await ipc.startGameCapture(
        game.pid,
        game.title,
        captureDuration,
        gpu || null,
      );
      await refreshCaptures();
      pickA = record.id;
      if (!pickB || pickB === record.id)
        pickB = captures.find((c) => c.id !== record.id)?.id ?? "";
      notice = $t("quick.done");
    } catch (e) {
      if (String(e) !== "cancelled") error = String(e);
    } finally {
      capturing = false;
    }
  }
  type MetricRow = {
    label: string;
    a: number | null;
    b: number | null;
    relative: boolean;
  };
  const metricRows = $derived.by<MetricRow[]>(() => {
    const a = captures.find((c) => c.id === pickA);
    const b = captures.find((c) => c.id === pickB);
    if (!a || !b || a.id === b.id) return [];
    return [
      {
        label: "Avg FPS",
        a: a.metrics.avgFps,
        b: b.metrics.avgFps,
        relative: true,
      },
      {
        label: "1% low",
        a: a.metrics.p1Low,
        b: b.metrics.p1Low,
        relative: true,
      },
      {
        label: "0.1% low",
        a: a.metrics.p01Low,
        b: b.metrics.p01Low,
        relative: true,
      },
      {
        label: "MAD %",
        a: a.metrics.frametimeMadPct,
        b: b.metrics.frametimeMadPct,
        relative: false,
      },
      {
        label: "Spike %",
        a: a.metrics.spikeRatePct,
        b: b.metrics.spikeRatePct,
        relative: false,
      },
      {
        label: $t("measure.rowDisplayP99"),
        a: a.metrics.displayLatencyP99Ms,
        b: b.metrics.displayLatencyP99Ms,
        relative: false,
      },
      {
        label: $t("measure.rowDropped"),
        a: a.metrics.droppedPct,
        b: b.metrics.droppedPct,
        relative: false,
      },
      {
        label: "Frames",
        a: a.metrics.sampleCount,
        b: b.metrics.sampleCount,
        relative: false,
      },
    ];
  });
  function fmtVal(v: number | null) {
    return v == null ? "—" : Number.isInteger(v) ? String(v) : v.toFixed(1);
  }
  function fmtDelta(row: MetricRow) {
    if (row.a == null || row.b == null || row.b === 0) return "—";
    return row.relative
      ? `${(((row.a - row.b) / row.b) * 100).toFixed(1)}%`
      : `${(row.a - row.b).toFixed(1)}`;
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
      if (pending === "delCapture" && pickA) {
        await ipc.deleteGameCapture(pickA);
        if (pickB === pickA) pickB = "";
        pickA = "";
        await refreshCaptures();
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
  function number(value: number | null | undefined) {
    return value == null ? "—" : value.toFixed(3);
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
        onclick={() => {
          section = "history";
          void refreshGames();
        }}>{$t("quick.history")}</button
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
      <details open={!captures.length}>
        <summary>{$t("measure.tab")}</summary>
        <section class="panel">
          <p class="hint">{$t("measure.hint")}</p>
          <div class="form-grid">
            <label class="field"
              >{$t("measure.game")}
              <select bind:value={captureGame} disabled={locked || capturing}>
                <option value="" disabled hidden>{$t("measure.noGames")}</option
                >
                {#each games as game}<option value={String(game.pid)}
                    >{game.title} — {game.exeName} (PID {game.pid})</option
                  >{/each}
              </select>
            </label>
            <label class="field"
              >{$t("measure.duration")}
              <input
                type="number"
                min="5"
                max="600"
                bind:value={captureDuration}
                disabled={locked || capturing}
              />
            </label>
          </div>
          <div class="start-row">
            <button
              disabled={locked || capturing}
              onclick={() => void refreshGames()}
              >{$t("measure.refresh")}</button
            >
            <button
              disabled={capturing}
              onclick={() => void ipc.cancelGameCapture().catch(() => {})}
              >{$t("measure.cancel")}</button
            >
            <button
              class="primary"
              disabled={locked || recovery || capturing || !captureGame}
              onclick={startCapture}>{$t("measure.start")}</button
            >
          </div>
          {#if capturing}
            <p role="status">{$t("measure.running")}</p>
            <progress max="100" value={capturePct}></progress>
          {/if}
        </section>
        <section class="panel">
          <h2>{$t("measure.compare")}</h2>
          <p class="hint">{$t("measure.compareHint")}</p>
          {#if !captures.length}
            <p>{$t("measure.noCaptures")}</p>
          {:else}
            <div class="form-grid">
              <label class="field"
                >A
                <select bind:value={pickA} disabled={locked || capturing}>
                  {#each captures as c}<option value={c.id}
                      >{c.startedAt} — {c.gameTitle} ({c.durationSecs}s){#if c.lockedLp != null}
                        · LP {c.lockedLp}{/if}</option
                    >{/each}
                </select>
              </label>
              <label class="field"
                >B
                <select bind:value={pickB} disabled={locked || capturing}>
                  <option value="" hidden>—</option>
                  {#each captures as c}<option value={c.id}
                      >{c.startedAt} — {c.gameTitle} ({c.durationSecs}s){#if c.lockedLp != null}
                        · LP {c.lockedLp}{/if}</option
                    >{/each}
                </select>
              </label>
            </div>
            {#if metricRows.length}
              <div class="table-wrap">
                <table>
                  <thead><tr><th></th><th>A</th><th>B</th><th>Δ</th></tr></thead
                  >
                  <tbody
                    >{#each metricRows as row}<tr
                        ><td>{row.label}</td><td>{fmtVal(row.a)}</td><td
                          >{fmtVal(row.b)}</td
                        ><td>{fmtDelta(row)}</td></tr
                      >{/each}</tbody
                  >
                </table>
              </div>
            {/if}
            <button
              class="danger"
              disabled={locked || capturing || !pickA}
              onclick={() => (action = "delCapture")}
              >{$t("measure.delete")}</button
            >
          {/if}
        </section>
      </details>
      <section class="panel">
        <label class="field"
          >{$t("quick.history")}<select
            disabled={locked}
            value={detail?.summary.id ?? ""}
            onchange={(e) => loadResult(e.currentTarget.value)}
            ><option value="" disabled>{$t("quick.chooseHistory")}</option
            >{#each history as session}<option value={session.id}
                >{session.startedAt} — {session.gpuName} — {$t(
                  `quick.session.${session.status}`,
                )}</option
              >{/each}</select
          ></label
        >
        {#if detail}
          <h2>{detail.summary.gpuName}</h2>
          <p>{$t(`quick.session.${detail.summary.status}`)}</p>
          {#if detail.summary.error}<p class="error" role="alert">
              {$t(`errors.${detail.summary.error}`, {
                default: detail.summary.error,
              })}
            </p>{/if}
          {#if quick}
            <h2>{$t(`quick.ranking.${quick.status}`)}</h2>
            <p>
              {$t("quick.gapHint")}{#if quick.relativeGapPct != null}
                ({number(quick.relativeGapPct)}%){/if}
            </p>
            {#if quick.status === "Close" || quick.status === "Reversed"}<p>
                {$t("quick.manualChoice")}
              </p>{/if}
            <h3>{$t("quick.retestResults")}</h3>
            {@render resultTable(quick.retest, true)}
            {#if quick.baseline}
              <section class="baseline-banner">
                <p>
                  <strong
                    >{$t(`quick.baseline.${quick.baseline.verdict}`)}</strong
                  >{#if quick.baseline.p1ImprovementPct != null}
                    · {quick.baseline.p1ImprovementPct >= 0
                      ? "+"
                      : ""}{quick.baseline.p1ImprovementPct.toFixed(1)}% 1% low{/if}{#if quick.baseline.avgImprovementPct != null}
                    · {quick.baseline.avgImprovementPct >= 0
                      ? "+"
                      : ""}{quick.baseline.avgImprovementPct.toFixed(1)}% Avg
                    FPS{/if}
                </p>
                <p class="hint">{$t("quick.baseline.hint")}</p>
              </section>
            {:else}
              <p class="hint">{$t("quick.baseline.none")}</p>
            {/if}
            <button
              class="primary"
              disabled={locked || !eligible || chosen === null}
              onclick={() => (action = "apply")}
              >{$t("quick.applySelected")}</button
            >
            <details>
              <summary>{$t("quick.screenResults")}</summary
              >{@render resultTable(quick.screening, false)}
            </details>
            <details>
              <summary>{$t("quick.order")}</summary>
              <p>Seed: {quick.seed}</p>
              <p>
                {$t("quick.screenResults")}: {quick.screeningOrder
                  .map((id) => quick.candidates.find((c) => c.coreId === id))
                  .filter((c): c is CoreTarget => !!c)
                  .map(label)
                  .join(" → ")}
              </p>
              <p>
                {$t("quick.retestResults")}: {quick.retestOrder
                  .map((id) => quick.candidates.find((c) => c.coreId === id))
                  .filter((c): c is CoreTarget => !!c)
                  .map(label)
                  .join(" → ")}
              </p>
            </details>
          {:else}
            <p>{$t("quick.legacy")}</p>
            <div class="table-wrap">
              <table>
                <thead
                  ><tr><th>LP</th><th>Avg FPS</th><th>1% low</th></tr></thead
                ><tbody
                  >{#each detail.results as row}<tr
                      ><td>LP {row.lp}</td><td>{number(row.avgFps)}</td><td
                        >{number(row.p1Low)}</td
                      ></tr
                    >{/each}</tbody
                >
              </table>
            </div>
          {/if}
          <button
            class="danger"
            disabled={locked}
            onclick={() => (action = "delete")}>{$t("quick.delete")}</button
          >
        {:else}<p>{$t("quick.chooseHistory")}</p>{/if}
      </section>
    {/if}
  {/if}
</div>

{#snippet resultTable(rows: CoreCapture[], selectable: boolean)}
  <div class="table-wrap">
    <table>
      <thead
        ><tr
          ><th>{$t("quick.core")}</th><th>{$t("quick.score")}</th><th
            >Avg FPS</th
          ><th>1% low</th><th>0.1% low</th><th>MAD %</th><th>Spike %</th><th
            >{$t("quick.colDisplayP99")}</th
          ><th>{$t("quick.colDropped")}</th></tr
        ></thead
      ><tbody
        >{#each rows as row}<tr
            ><td
              >{#if selectable}<label
                  ><input
                    type="radio"
                    name="result-core"
                    value={row.target.coreId}
                    bind:group={chosen}
                    disabled={locked || !eligible}
                  />{label(row.target)}</label
                >{:else}{label(row.target)}{/if}</td
            ><td>{number(row.score)}</td><td>{number(row.metrics.avgFps)}</td
            ><td>{number(row.metrics.p1Low)}</td><td
              >{number(row.metrics.p01Low)}</td
            ><td>{number(row.metrics.frametimeMadPct)}</td><td
              >{number(row.metrics.spikeRatePct)}</td
            ><td>{number(row.metrics.displayLatencyP99Ms)}</td><td
              >{number(row.metrics.droppedPct)}</td
            ></tr
          >{/each}</tbody
      >
    </table>
  </div>
{/snippet}
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
  h3 {
    font-size: 15px;
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
  .table-wrap {
    overflow-x: auto;
    margin: 14px 0;
  }
  table {
    border-collapse: collapse;
    width: 100%;
    font-variant-numeric: tabular-nums;
  }
  th,
  td {
    padding: 10px;
    border-bottom: 1px solid var(--border-default);
    text-align: right;
    white-space: nowrap;
  }
  th:first-child,
  td:first-child {
    text-align: left;
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
