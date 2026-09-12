<script lang="ts">
  import { onDestroy } from 'svelte';
  import { locale, t } from 'svelte-i18n';
  import { invoke } from '@tauri-apps/api/core';
  import { gpuOperationBusy, topology } from '../lib/stores';

  let { instanceId, locked }: { instanceId: string; locked: boolean } = $props();
  type Cpu = { lp: number; count: number };
  type Sample = {
    instanceId: string; driver: string; sharedDriver: boolean; sampledAt: string;
    sampleSecs: number; cpus: Cpu[]; graphicsKernelCpus: Cpu[]; eventsLost: number;
  };
  let sample = $state<Sample | null>(null);
  let measuring = $state(false);
  let error = $state('');
  let alive = true;
  const en = $derived($locale === 'en');
  const text = $derived(en ? {
    title: 'Observed interrupt CPUs', measure: 'Measure for 3 seconds', measuring: 'Measuring…',
    hint: 'Run a GPU workload while sampling. Results show logical processors (LPs) that handled ISR events during this sample; they are not affinity settings or a continuous monitor.',
    empty: 'No ISR events observed for this driver. Run a GPU workload and measure again.',
    initial: 'Not measured yet.', core: 'Physical core', count: 'ISR count',
    shared: 'Multiple adapters may share this driver. These events cannot be attributed to the selected GPU alone.',
    kernel: 'Shared graphics kernel (dxgkrnl.sys)', kernelHint: 'System-wide graphics events; cannot be attributed to the selected GPU.',
    lost: 'Trace data was lost. Counts are incomplete; measure again.',
    driverHint: 'Attribution is by driver module. Interrupts handled through other shared modules may not appear here.',
    at: 'Sample completed',
  } : {
    title: '實測中斷 CPU 核心', measure: '量測 3 秒', measuring: '量測中…',
    hint: '取樣時請執行 GPU 負載。結果顯示這次取樣期間實際處理 ISR 的邏輯處理器（LP），並非親和性設定或持續監控。',
    empty: '未觀測到此驅動的 ISR 事件。請執行 GPU 負載後重新量測。',
    initial: '尚未量測。', core: '實體核心', count: 'ISR 次數',
    shared: '多張顯示配接器可能共用此驅動，無法將這些事件單獨歸屬到選定的 GPU。',
    kernel: '共用顯示核心（dxgkrnl.sys）', kernelHint: '全系統的顯示中斷事件，無法歸屬到選定的 GPU。',
    lost: '追蹤資料有遺失，次數不完整，請重新量測。',
    driverHint: '以驅動模組辨識來源；經由其他共用模組處理的中斷可能不會列出。',
    at: '取樣完成時間',
  });
  $effect(() => {
    const id = instanceId;
    if (sample?.instanceId !== id || (locked && !measuring)) sample = null;
  });
  $effect(() => { instanceId; error = ''; });
  onDestroy(() => { alive = false; });
  async function measure() {
    if (locked || measuring || !instanceId) return;
    const id = instanceId;
    measuring = true; sample = null; error = ''; gpuOperationBusy.set(true);
    try {
      const result = await invoke<Sample>('sample_gpu_interrupts', { instanceId: id });
      if (alive && instanceId === id) sample = result;
    } catch (e) { if (alive && instanceId === id) error = String(e); }
    finally { measuring = false; gpuOperationBusy.set(false); }
  }
</script>

<section class="interrupt-panel" aria-busy={measuring}>
  <div class="heading">
    <h2>{text.title}</h2>
    <button class="large" disabled={locked || measuring || !instanceId} aria-busy={measuring} onclick={measure}>{measuring ? text.measuring : text.measure}</button>
  </div>
  <p class="hint">{text.hint}</p>
  <div role="status" aria-live="polite">
    {#if measuring}<p>{text.measuring}</p>
    {:else if error}<p class="error">{$t(`errors.${error}`, { default: error })}</p>
    {:else if sample}
      <p><strong>{sample.driver}</strong> · {text.at}: {new Date(sample.sampledAt).toLocaleTimeString(en ? 'en-US' : 'zh-TW')} ({sample.sampleSecs}s)</p>
      {#if sample.sharedDriver}<p>{text.shared}</p>{/if}
      {#if sample.eventsLost}<p class="error">{text.lost}</p>{/if}
      {#if sample.cpus.length}{@render cpuTable(sample.cpus)}{:else}<p>{text.empty}</p>{/if}
      <p class="hint">{text.driverHint}</p>
      {#if sample.graphicsKernelCpus.length}
        <details open={sample.cpus.length === 0}><summary>{text.kernel}</summary><p class="hint">{text.kernelHint}</p>{@render cpuTable(sample.graphicsKernelCpus)}</details>
      {/if}
    {:else}<p>{text.initial}</p>{/if}
  </div>
</section>

{#snippet cpuTable(cpus: Cpu[])}
  <table>
    <thead><tr><th>LP</th><th>{text.core}</th><th>{text.count}</th></tr></thead>
    <tbody>{#each cpus as cpu}<tr><td>LP {cpu.lp}</td><td>{$topology?.logicalProcessors.find(lp => lp.index === cpu.lp)?.coreId ?? '—'}</td><td>{cpu.count.toLocaleString()}</td></tr>{/each}</tbody>
  </table>
{/snippet}

<style>
  .interrupt-panel { background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: 16px; }
  .heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
  h2 { margin: 0; font-size: 1rem; }
  p { margin: 10px 0; }
  .hint { color: var(--text-secondary); font-size: 0.85rem; }
  table { width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums; }
  th, td { text-align: left; padding: 8px; border-bottom: 1px solid var(--border); }
  summary { cursor: pointer; padding: 8px 0; }
  .error { color: var(--danger, #f38ba8); }
</style>
