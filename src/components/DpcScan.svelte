<script lang="ts">
  import { onDestroy } from 'svelte';
  import { locale, t } from 'svelte-i18n';
  import { invoke } from '@tauri-apps/api/core';
  import { gpuOperationBusy } from '../lib/stores';
  import type { DpcScan } from '../lib/types';

  let scan = $state<DpcScan | null>(null);
  let scanning = $state(false);
  let error = $state('');
  let alive = true;
  const en = $derived($locale === 'en');
  const text = $derived(en ? {
    title: 'DPC noise scan', measure: 'Scan for 3 seconds', measuring: 'Scanning…',
    hint: 'Samples all driver DPC activity for 3 seconds and ranks drivers by total time spent. High DPC time from network, storage, or USB drivers can disrupt frame pacing even when the GPU is not the cause.',
    empty: 'No DPC activity observed. Try again under load.',
    initial: 'Not scanned yet.', driver: 'Driver', count: 'DPC count',
    total: 'Total time (ms)', max: 'Longest (ms)',
    lost: 'Trace data was lost. Results are incomplete; scan again.',
    at: 'Scan completed',
  } : {
    title: 'DPC 噪音掃描', measure: '掃描 3 秒', measuring: '掃描中…',
    hint: '取樣 3 秒全系統驅動 DPC 活動，依總耗時排序。網卡、儲存或 USB 驅動的高 DPC 耗時會干擾幀格穩定，即使 GPU 不是元兇。',
    empty: '未觀測到 DPC 活動，請在負載下重試。',
    initial: '尚未掃描。', driver: '驅動', count: 'DPC 次數',
    total: '總耗時 (ms)', max: '最長 (ms)',
    lost: '追蹤資料有遺失，結果不完整，請重新掃描。',
    at: '掃描完成時間',
  });
  onDestroy(() => { alive = false; });
  async function scan_now() {
    if (scanning) return;
    scanning = true; scan = null; error = ''; gpuOperationBusy.set(true);
    try {
      const result = await invoke<DpcScan>('scan_dpc_offenders', { topN: 5 });
      if (alive) scan = result;
    } catch (e) { if (alive) error = String(e); }
    finally { scanning = false; gpuOperationBusy.set(false); }
  }
</script>

<section class="dpc-panel" aria-busy={scanning}>
  <div class="heading">
    <h2>{text.title}</h2>
    <button class="large" disabled={scanning} aria-busy={scanning} onclick={scan_now}>{scanning ? text.measuring : text.measure}</button>
  </div>
  <p class="hint">{text.hint}</p>
  <div role="status" aria-live="polite">
    {#if scanning}<p>{text.measuring}</p>
    {:else if error}<p class="error">{$t(`errors.${error}`, { default: error })}</p>
    {:else if scan}
      <p>{text.at}: {new Date(scan.sampledAt).toLocaleTimeString(en ? 'en-US' : 'zh-TW')} ({scan.sampleSecs}s)</p>
      {#if scan.eventsLost}<p class="error">{text.lost}</p>{/if}
      {#if scan.offenders.length}
        <table>
          <thead><tr><th>{text.driver}</th><th>{text.count}</th><th>{text.total}</th><th>{text.max}</th></tr></thead>
          <tbody>
            {#each scan.offenders as o}
              <tr><td>{o.driver}</td><td>{o.count.toLocaleString()}</td><td>{o.totalDurationMs.toFixed(2)}</td><td>{o.maxDurationMs.toFixed(2)}</td></tr>
            {/each}
          </tbody>
        </table>
      {:else}<p>{text.empty}</p>{/if}
    {:else}<p>{text.initial}</p>{/if}
  </div>
</section>

<style>
  .dpc-panel { background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: 16px; }
  .heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
  h2 { margin: 0; font-size: 1rem; }
  p { margin: 10px 0; }
  .hint { color: var(--text-secondary); font-size: 0.85rem; }
  table { width: 100%; border-collapse: collapse; font-variant-numeric: tabular-nums; }
  th, td { text-align: left; padding: 8px; border-bottom: 1px solid var(--border); }
  .error { color: var(--danger, #f38ba8); }
</style>
