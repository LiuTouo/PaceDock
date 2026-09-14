<script lang="ts">
  import { onDestroy } from 'svelte';
  import { locale, t } from 'svelte-i18n';
  import { getSystemHealth } from '../lib/ipc';
  import type { HealthCheck } from '../lib/types';

  let checks = $state<HealthCheck[]>([]);
  let error = $state('');
  let alive = true;
  const en = $derived($locale === 'en');
  const text = $derived(en ? {
    title: 'System health (read-only)',
    hint: 'Advisory checks only. PaceDock never changes these settings; it only reports them.',
    unknown: 'Could not read this setting.',
    check: {
      powerPlan: { name: 'Power plan', Ok: 'Using the high-performance (or Ultimate) plan.', Info: 'Balanced plan is active. A high-performance or Ultimate plan lowers latency.', Warn: '', Unknown: '' },
      pcieAspm: { name: 'PCIe link power (ASPM)', Ok: 'PCIe link state power management is off.', Info: '', Warn: 'PCIe link state power management is on and can add latency. Consider disabling it in the active plan’s advanced settings.', Unknown: '' },
      gameDvr: { name: 'Game DVR capture', Ok: 'Background game capture is off.', Info: '', Warn: 'Game DVR / background recording may consume GPU resources and hurt frame pacing.', Unknown: '' },
      hags: { name: 'Hardware-accelerated GPU scheduling', Ok: 'Hardware-accelerated GPU scheduling is off (classic scheduling).', Info: 'Hardware-accelerated GPU scheduling (HAGS) is on; reported for information only.', Unknown: '' },
      usbSelectiveSuspend: { name: 'USB selective suspend', Ok: 'USB selective suspend is disabled on AC and battery.', Info: '', Warn: 'USB selective suspend is enabled and can add input latency. Disable it from the power-tweaks card on the status tab.', Unknown: '' },
    } as Record<string, Record<string, string>>,
  } : {
    title: '系統環境健檢（唯讀）',
    hint: '純建議性檢查。PaceDock 不會變更這些設定，僅回報現況。',
    unknown: '無法讀取此設定。',
    check: {
      powerPlan: { name: '電源計畫', Ok: '目前使用高效能（或終極效能）方案。', Info: '目前為平衡方案；高效能／終極效能方案可降低延遲。', Warn: '', Unknown: '' },
      pcieAspm: { name: 'PCIe 鏈路電源管理（ASPM）', Ok: 'PCIe 鏈路電源管理已停用。', Info: '', Warn: 'PCIe 鏈路電源管理開啟中，可能增加延遲；建議在電源計畫進階設定中停用。', Unknown: '' },
      gameDvr: { name: 'Game DVR 背景錄製', Ok: '遊戲背景錄製已停用。', Info: '', Warn: 'Game DVR／背景錄製可能佔用 GPU 資源、影響幀格穩定。', Unknown: '' },
      hags: { name: '硬體加速 GPU 排程（HAGS）', Ok: '硬體加速 GPU 排程未啟用（傳統排程）。', Info: '硬體加速 GPU 排程（HAGS）已啟用；此為中性資訊。', Unknown: '' },
      usbSelectiveSuspend: { name: 'USB 選擇性暫停', Ok: 'AC 與電池皆已停用 USB 選擇性暫停。', Info: '', Warn: 'USB 選擇性暫停開啟中，可能造成輸入延遲；可於狀態分頁的電源微調卡片停用。', Unknown: '' },
    } as Record<string, Record<string, string>>,
  });
  $effect(() => {
    getSystemHealth()
      .then(c => { if (alive) checks = c; })
      .catch(e => { if (alive) error = String(e); });
  });
  onDestroy(() => { alive = false; });
</script>

<section class="health-panel">
  <h2>{text.title}</h2>
  <p class="hint">{text.hint}</p>
  {#if error}<p class="error" role="alert">{$t(`errors.${error}`, { default: error })}</p>
  {:else if !checks.length}<p>{en ? 'Loading…' : '載入中…'}</p>
  {:else}
    <ul>
      {#each checks as c}
        <li>
          <span class={`badge ${c.status.toLowerCase()}`}>{c.status}</span>
          <div>
            <strong>{text.check[c.id]?.name ?? c.id}</strong>
            <p class="hint">{text.check[c.id]?.[c.status] || c.detail || text.unknown}</p>
          </div>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .health-panel { background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: 16px; }
  h2 { margin: 0; font-size: 1rem; }
  .hint { color: var(--text-secondary); font-size: 0.85rem; margin: 6px 0; }
  ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 10px; }
  li { display: flex; align-items: flex-start; gap: 10px; }
  li p { margin: 2px 0 0; }
  .badge { flex: none; font-size: 0.72rem; font-weight: 600; padding: 2px 8px; border-radius: 999px; border: 1px solid var(--border); margin-top: 2px; }
  .badge.ok { color: var(--success, #4caf50); }
  .badge.warn { color: var(--warning, #f6a11a); }
  .badge.info { color: var(--text-secondary); }
  .badge.unknown { color: var(--text-secondary); opacity: 0.7; }
  .error { color: var(--danger, #f38ba8); }
</style>
