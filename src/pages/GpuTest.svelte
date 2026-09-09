<script lang="ts">
  import { onMount } from 'svelte';
  import { locale, t } from 'svelte-i18n';
  import * as ipc from '../lib/ipc';
  import { benchmarkProgress, benchmarkState, topology, gpuOperationBusy } from '../lib/stores';
  import { coreLabel, policyIndices } from '../lib/core';
  import type { AffinityPolicy, BenchmarkConfig, CoreCapture, CoreTarget, GpuDevice, QuickSchedule, SessionDetail, SessionSummary } from '../lib/types';
  import ConfirmDialog from '../components/ConfirmDialog.svelte';
  import GpuInterrupts from '../components/GpuInterrupts.svelte';

  let devices = $state<GpuDevice[]>([]);
  let targets = $state<CoreTarget[]>([]);
  let selected = $state<number[]>([]);
  let gpu = $state('');
  let workload = $state<'Vulkan' | 'D3D9'>('Vulkan');
  let warmup = $state(3), sample = $state(10), retestWarmup = $state(5), retestSample = $state(20);
  let width = $state(1280), height = $state(720), fpsCap = $state(0);
  let adaptive = $state(true), triple = $state(false);
  let schedule = $state<QuickSchedule | null>(null);
  let history = $state<SessionSummary[]>([]);
  let detail = $state<SessionDetail | null>(null);
  let policy = $state<AffinityPolicy | null>(null);
  let manual = $state<number | null>(null), chosen = $state<number | null>(null);
  let section = $state<'test' | 'results'>('test');
  let busy = $state(false), cancelSent = $state(false);
  let error = $state(''), notice = $state('');
  let action = $state<'start' | 'apply' | 'manual' | 'restore' | 'delete' | null>(null);
  let handled = $state('');
  const running = $derived($benchmarkState?.status === 'Running');
  const compact = $derived($benchmarkState?.windowLayout === 'CompactProgress');
  const locked = $derived(busy || running || $gpuOperationBusy || $benchmarkState?.gpuBusy);
  const recovery = $derived($benchmarkState?.recoveryRequired ?? false);
  const quick = $derived(detail?.summary.quick);
  const eligible = $derived(detail?.summary.status === 'Completed' && quick?.methodVersion === 3 && quick.status !== 'Insufficient' && detail.summary.captureQuality?.integrityPassed && !recovery);
  const current = $derived($benchmarkProgress?.target ?? $benchmarkState?.currentTarget);
  const cancelling = $derived(cancelSent || $benchmarkState?.cancelRequested);
  const label = (target: CoreTarget) => coreLabel(target, $locale === 'en');
  const config = $derived<BenchmarkConfig>({
    candidateLps: [], candidateCoreIds: selected, methodVersion: 3, gpuInstanceId: gpu,
    workload, warmUpSecs: warmup, sampleSecs: sample, retestWarmUpSecs: retestWarmup,
    retestSampleSecs: retestSample, repetitions: 1, syncWorkloadAffinity: false, fullscreen: false,
    width, height, fpsCap, fpsCapPolicy: adaptive ? 'Adaptive' : 'Fixed', tripleBuffer: triple,
    vulkanArgs: ['--fullscreen=0', `--width=${width}`, `--height=${height}`, `--fps_cap=${fpsCap}`, `--triple_buffering=${triple ? 1 : 0}`],
    gamePath: null, windowTitle: null,
  });
  const policyText = $derived.by(() => {
    if (!policy) return $t('quick.unknown');
    if (!policy.assignmentSetOverride.present) return $t('quick.noOverride');
    const indices = policyIndices(policy.assignmentSetOverride.bytes);
    const core = $topology?.physicalCores.find(c => c.lpIndices.length === indices.length && c.lpIndices.every(i => indices.includes(i)));
    if (core) return label({ coreId: core.id, lpIndices: indices });
    return indices.length ? `LP ${indices.join($locale === 'en' ? ', ' : '、')}` : $t('quick.emptyMask');
  });
  const confirmTarget = $derived(targets.find(c => c.coreId === manual));
  const resultTarget = $derived(quick?.retest.find(c => c.target.coreId === chosen)?.target);
  const confirmMessage = $derived(action === 'start' ? $t('quick.startConfirm') : action === 'restore' ? $t('quick.restoreConfirm') : action === 'delete' ? $t('quick.deleteConfirm') : `${$t('quick.applyConfirm')} ${action === 'manual' && confirmTarget ? label(confirmTarget) : resultTarget ? label(resultTarget) : ''}${action === 'manual' ? ` — ${$t('quick.untested')}` : ''}`);

  $effect(() => {
    const request = config;
    let active = true;
    schedule = null;
    if (selected.length && gpu) {
      const timer = setTimeout(() => { ipc.getQuickSchedule(request).then(v => { if (active) schedule = v; }).catch(() => { if (active) schedule = null; }); }, 150);
      return () => { active = false; clearTimeout(timer); };
    }
  });
  $effect(() => {
    const instance = gpu;
    let active = true;
    policy = null;
    if (instance) ipc.getGpuAffinityPolicy(instance).then(v => { if (active) policy = v; }).catch(e => { if (active) error = String(e); });
    return () => { active = false; };
  });
  $effect(() => {
    const state = $benchmarkState;
    if (state?.status === 'Running') { section = 'test'; return; }
    if (state?.sessionId && ['Completed', 'Failed', 'Cancelled'].includes(state.status) && state.sessionId !== handled) {
      handled = state.sessionId;
      cancelSent = false;
      void loadResult(state.sessionId);
      void refreshHistory();
    }
  });
  onMount(() => { void initialize(); });
  async function initialize() {
    try {
      [devices, targets] = await Promise.all([ipc.enumerateGpus(), ipc.getCoreCandidates()]);
      gpu = devices[0]?.instanceId ?? '';
      selected = targets.map(c => c.coreId);
      manual = targets[0]?.coreId ?? null;
      await refreshHistory();
    } catch (e) { error = String(e); }
  }
  async function refreshHistory() { try { history = await ipc.listBenchmarkSessions(); } catch (e) { error = String(e); } }
  let loadSerial = 0;
  async function loadResult(id: string) {
    const serial = ++loadSerial;
    chosen = null; detail = null;
    try {
      const result = await ipc.getBenchmarkSession(id);
      if (serial !== loadSerial) return;
      detail = result;
      section = 'results';
      if (devices.some(g => g.instanceId === result.summary.gpuInstanceId)) gpu = result.summary.gpuInstanceId;
      if (result.summary.status === 'Completed' && ['Consistent', 'SingleCandidate'].includes(result.summary.quick?.status ?? '')) chosen = result.summary.quick?.retest[0]?.target.coreId ?? null;
    } catch (e) { if (serial === loadSerial) error = String(e); }
  }
  async function confirm() {
    const pending = action;
    action = null;
    if (!pending || locked) return;
    busy = true; gpuOperationBusy.set(true); error = ''; notice = '';
    try {
      if (pending === 'start') { cancelSent = false; await ipc.startGpuBenchmark(config); benchmarkState.set(await ipc.getBenchmarkState()); }
      if (pending === 'apply' && detail && chosen !== null) await ipc.applyGpuCore(detail.summary.gpuInstanceId, chosen, detail.summary.id);
      if (pending === 'manual' && manual !== null) await ipc.applyGpuCore(gpu, manual, null);
      if (pending === 'restore') await ipc.restorePreviousGpuAffinity();
      if (pending === 'delete' && detail) { await ipc.deleteBenchmarkSession(detail.summary.id); detail = null; chosen = null; await refreshHistory(); }
      if (pending !== 'start') notice = $t('quick.done');
      if (gpu) policy = await ipc.getGpuAffinityPolicy(gpu);
    } catch (e) { error = String(e); }
    finally { try { benchmarkState.set(await ipc.getBenchmarkState()); } finally { busy = false; gpuOperationBusy.set(false); } }
  }
  async function cancel() {
    cancelSent = true;
    try { await ipc.cancelBenchmark(); } catch (e) { cancelSent = false; error = String(e); }
  }
  function toggle(id: number, checked: boolean) { selected = checked ? [...selected, id] : selected.filter(c => c !== id); }
  function number(value: number | null | undefined) { return value == null ? '—' : value.toFixed(3); }
</script>

<div class="gpu-page" class:compact>
  {#if error}<div class="panel error" role="alert">{$t(`errors.${error}`, { default: error })}</div>{/if}
  {#if notice && !compact}<p role="status">{notice}</p>{/if}
  {#if recovery && !compact}<div class="panel error" role="alert">{$t('quick.recovery')}</div>{/if}
  {#if running || compact}
    <section class="panel progress-panel" aria-live="polite">
      <h2>{cancelling ? $t('quick.cancelling') : $t('quick.running')}</h2>
      <p>{current ? label(current) : $t('quick.calibrating')}</p>
      <p>{cancelling ? $t(`quick.cancelStage.${$benchmarkState?.cancelStage ?? 'requested'}`, { default: $t('quick.cancelling') }) : $t(`quick.phase.${$benchmarkProgress?.phase ?? $benchmarkState?.currentPhase ?? 'Calibration'}`)} · {$t(`quick.stage.${$benchmarkProgress?.stage ?? 'starting'}`)}</p>
      <progress max="100" value={cancelling ? $benchmarkState?.cancelProgress ?? 0 : $benchmarkState?.progressPct ?? 0}></progress>
      <p>{$t('quick.keepWindow')}</p>
      <button class="danger" disabled={cancelling} onclick={cancel}>{$t('quick.cancel')}</button>
    </section>
  {:else}
    <header><h1>{$t('quick.title')}</h1><p class="hint">{$t('quick.scope')}</p></header>
    <div class="tabs"><button class:active={section === 'test'} disabled={locked} onclick={() => section = 'test'}>{$t('quick.test')}</button><button class:active={section === 'results'} disabled={locked} onclick={() => section = 'results'}>{$t('quick.history')}</button></div>
    <section class="panel policy-row">
      <div><strong>{$t('quick.currentPolicy')} — {devices.find(d => d.instanceId === gpu)?.friendlyName ?? gpu}</strong><p>{policyText}</p><small>DevicePolicy: {policy?.devicePolicy.bytes?.join(', ') ?? '—'}</small></div>
      <button disabled={locked} onclick={() => action = 'restore'}>{$t('quick.restore')}</button>
    </section>
    <GpuInterrupts instanceId={gpu} locked={!!locked} />
    {#if section === 'test'}
      <section class="panel">
        <label class="field">GPU<select bind:value={gpu} disabled={locked}>{#each devices as device}<option value={device.instanceId}>{device.friendlyName}</option>{/each}</select></label>
        <h2>{$t('quick.candidates')}</h2>
        <p class="hint">{$t('quick.candidateHint')}</p>
        {#if !targets.length}<p role="alert">{$t('quick.noCandidates')}</p>{/if}
        <div class="core-grid">{#each targets as target}<label class="core-choice"><input type="checkbox" checked={selected.includes(target.coreId)} disabled={locked} onchange={e => toggle(target.coreId, e.currentTarget.checked)} />{label(target)}</label>{/each}</div>
        <div class="start-row"><p>{#if schedule}{$t('quick.estimate', { values: { min: Math.ceil(schedule.estimatedMinSecs / 60), max: Math.ceil(schedule.estimatedMaxSecs / 60), captures: schedule.candidateCaptures } })}{:else}{$t('quick.chooseCandidates')}{/if}<br/><small>{$t('quick.estimateHint')}</small></p><button class="primary" disabled={locked || recovery || !schedule || !selected.length || !gpu} onclick={() => action = 'start'}>{$t('quick.start')}</button></div>
      </section>
      <details class="panel"><summary>{$t('quick.advanced')}</summary>
        <div class="form-grid">
          <label>{$t('quick.screenWarmup')}<input type="number" min="0" max="60" bind:value={warmup} /></label>
          <label>{$t('quick.screenSample')}<input type="number" min="1" max="120" bind:value={sample} /></label>
          <label>{$t('quick.retestWarmup')}<input type="number" min="0" max="60" bind:value={retestWarmup} /></label>
          <label>{$t('quick.retestSample')}<input type="number" min="1" max="120" bind:value={retestSample} /></label>
          <label>Workload<select bind:value={workload}><option>Vulkan</option><option>D3D9</option></select></label>
          <label>{$t('quick.width')}<input type="number" min="1" bind:value={width} /></label>
          <label>{$t('quick.height')}<input type="number" min="1" bind:value={height} /></label>
          <label>FPS cap<input type="number" min="0" max="10000" bind:value={fpsCap} disabled={adaptive} /></label>
          <label><input type="checkbox" bind:checked={adaptive} />{$t('quick.adaptive')}</label>
          <label><input type="checkbox" bind:checked={triple} />{$t('quick.triple')}</label>
        </div>
        <h2>{$t('quick.manual')}</h2><p class="hint">{$t('quick.untested')}</p>
        <div class="start-row"><select aria-label={$t('quick.manual')} bind:value={manual}>{#each targets as target}<option value={target.coreId}>{label(target)}</option>{/each}</select><button disabled={locked || recovery || manual === null || !gpu} onclick={() => action = 'manual'}>{$t('quick.apply')}</button></div>
      </details>
    {:else}
      <section class="panel">
        <label class="field">{$t('quick.history')}<select disabled={locked} value={detail?.summary.id ?? ''} onchange={e => loadResult(e.currentTarget.value)}><option value="" disabled>{$t('quick.chooseHistory')}</option>{#each history as session}<option value={session.id}>{session.startedAt} — {session.gpuName} — {$t(`quick.session.${session.status}`)}</option>{/each}</select></label>
        {#if detail}
          <h2>{detail.summary.gpuName}</h2><p>{$t(`quick.session.${detail.summary.status}`)}</p>
          {#if detail.summary.error}<p class="error" role="alert">{$t(`errors.${detail.summary.error}`, { default: detail.summary.error })}</p>{/if}
          {#if quick}
            <h2>{$t(`quick.ranking.${quick.status}`)}</h2>
            <p>{$t('quick.gapHint')}{#if quick.relativeGapPct != null} ({number(quick.relativeGapPct)}%){/if}</p>
            {#if quick.status === 'Close' || quick.status === 'Reversed'}<p>{$t('quick.manualChoice')}</p>{/if}
            <h3>{$t('quick.retestResults')}</h3>
            {@render resultTable(quick.retest, true)}
            <button class="primary" disabled={locked || !eligible || chosen === null} onclick={() => action = 'apply'}>{$t('quick.applySelected')}</button>
            <details><summary>{$t('quick.screenResults')}</summary>{@render resultTable(quick.screening, false)}</details>
            <details><summary>{$t('quick.order')}</summary><p>Seed: {quick.seed}</p><p>{$t('quick.screenResults')}: {quick.screeningOrder.map(id => quick.candidates.find(c => c.coreId === id)).filter((c): c is CoreTarget => !!c).map(label).join(' → ')}</p><p>{$t('quick.retestResults')}: {quick.retestOrder.map(id => quick.candidates.find(c => c.coreId === id)).filter((c): c is CoreTarget => !!c).map(label).join(' → ')}</p></details>
          {:else}
            <p>{$t('quick.legacy')}</p>
            <div class="table-wrap"><table><thead><tr><th>LP</th><th>Avg FPS</th><th>1% low</th></tr></thead><tbody>{#each detail.results as row}<tr><td>LP {row.lp}</td><td>{number(row.avgFps)}</td><td>{number(row.p1Low)}</td></tr>{/each}</tbody></table></div>
          {/if}
          <button class="danger" disabled={locked} onclick={() => action = 'delete'}>{$t('quick.delete')}</button>
        {:else}<p>{$t('quick.chooseHistory')}</p>{/if}
      </section>
    {/if}
  {/if}
</div>

{#snippet resultTable(rows: CoreCapture[], selectable: boolean)}
<div class="table-wrap"><table><thead><tr><th>{$t('quick.core')}</th><th>{$t('quick.score')}</th><th>Avg FPS</th><th>1% low</th><th>0.1% low</th><th>MAD %</th><th>Spike %</th></tr></thead><tbody>{#each rows as row}<tr><td>{#if selectable}<label><input type="radio" name="result-core" value={row.target.coreId} bind:group={chosen} disabled={locked || !eligible} />{label(row.target)}</label>{:else}{label(row.target)}{/if}</td><td>{number(row.score)}</td><td>{number(row.metrics.avgFps)}</td><td>{number(row.metrics.p1Low)}</td><td>{number(row.metrics.p01Low)}</td><td>{number(row.metrics.frametimeMadPct)}</td><td>{number(row.metrics.spikeRatePct)}</td></tr>{/each}</tbody></table></div>
{/snippet}
<ConfirmDialog open={action !== null} title={$t('quick.confirm')} message={confirmMessage} confirmLabel={$t('quick.confirm')} cancelLabel={$t('quick.back')} busy={busy} danger={action === 'delete'} onconfirm={confirm} oncancel={() => action = null} />

<style>
  .gpu-page { display: flex; flex-direction: column; gap: 16px; }
  header h1 { margin: 0 0 8px; font-size: 24px; }
  h2 { font-size: 17px; margin: 12px 0; } h3 { font-size: 15px; }
  p { line-height: 1.6; } .hint, small { color: var(--text-secondary); }
  .tabs, .start-row, .policy-row { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  .tabs { justify-content: flex-start; } .active { border-color: var(--accent); }
  .field, .form-grid label { display: flex; gap: 8px; flex-direction: column; }
  .core-grid, .form-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(230px, 1fr)); gap: 12px; margin: 16px 0; }
  .core-choice { display: flex; align-items: center; gap: 8px; min-height: 44px; }
  summary { cursor: pointer; padding: 8px 0; } details { margin: 12px 0; }
  select { max-width: 100%; } .table-wrap { overflow-x: auto; margin: 14px 0; }
  table { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; }
  th, td { padding: 10px; border-bottom: 1px solid var(--border-default); text-align: right; white-space: nowrap; }
  th:first-child, td:first-child { text-align: left; }
  progress { width: 100%; height: 10px; accent-color: var(--accent); }
  .error { color: var(--danger); } .compact { gap: 4px; } .compact .panel { padding: 12px; }
  .compact p { margin: 6px 0; font-size: 12px; } .compact h2 { margin: 4px 0; }
  @media (max-width: 700px) { .start-row, .policy-row { flex-wrap: wrap; } }
</style>
