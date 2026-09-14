<script lang="ts">
  // 電源微調卡片(狀態分頁):USB 選擇性暫停 / PCIe ASPM 檢視 + 套用/還原。
  // 後端套用/還原走 GPU 操作排他鎖,操作期間以 gpuOperationBusy 導航上鎖。
  import { onDestroy } from 'svelte';
  import { locale, t } from 'svelte-i18n';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import { applyPowerTweak, getPowerTweaks, restorePowerTweak } from '../lib/ipc';
  import { gpuOperationBusy } from '../lib/stores';
  import type { PowerTweakKind, PowerTweaksStatus } from '../lib/types';

  let { locked = false }: { locked?: boolean } = $props();

  let status = $state<PowerTweaksStatus | null>(null);
  let busy = $state<PowerTweakKind | null>(null);
  let error = $state('');
  let confirmKind = $state<PowerTweakKind | null>(null);
  let confirmOp = $state<'apply' | 'restore'>('apply');
  let confirmOpen = $state(false);
  let alive = true;
  const en = $derived($locale === 'en');
  const text = $derived(en ? {
    aspmTitle: 'PCIe Link Power Management (ASPM)',
    usbTitle: 'USB Selective Suspend',
    apply: 'Disable',
    restore: 'Restore',
    off: 'Off (no link power management).',
    on: 'On and can add latency; disabling removes PCIe link power states that cause micro-stutter.',
    usbOff: 'Disabled. Input devices are not suspended.',
    usbOn: 'Enabled. USB selective suspend can add input latency on mice and keyboards.',
    unknown: 'Could not read this setting.',
    confirmTitle: 'Disable power setting',
    confirmMsg: 'This applies immediately, no restart needed. The previous value is saved and can be restored here.',
    restorableHint: 'Applied before via PaceDock; you can restore the original value here.',
    abHint: 'Tip: measure a real game before and after (Measurement tab) to verify the effect.',
    confirm: 'Confirm',
    cancel: 'Cancel',
  } : {
    aspmTitle: 'PCIe 鏈路電源管理(ASPM)',
    usbTitle: 'USB 選擇性暫停',
    apply: '停用',
    restore: '還原',
    off: '已停用(鏈路不進省電狀態)。',
    on: '開啟中,可能增加延遲;停用可移除 PCIe 鏈路省電狀態造成的微卡頓。',
    usbOff: '已停用,輸入裝置不會被暫停。',
    usbOn: '開啟中;USB 選擇性暫停可能造成滑鼠/鍵盤輸入延遲。',
    unknown: '無法讀取此設定。',
    confirmTitle: '停用電源設定',
    confirmMsg: '立即生效,無需重新啟動。套用前的原值會被記錄,可在此還原。',
    restorableHint: '此前已由 PaceDock 套用,可在此還原原值。',
    abHint: '提示:可於「量測」分頁在套用前後各測一次實際遊戲量測,對照驗證效果。',
    confirm: '確認',
    cancel: '取消',
  });

  async function refresh() {
    try {
      const result = await getPowerTweaks();
      if (alive) { status = result; error = ''; }
    } catch (e) { if (alive) error = String(e); }
  }

  async function run(kind: PowerTweakKind, op: 'apply' | 'restore') {
    if (locked || busy) return;
    busy = kind; error = ''; gpuOperationBusy.set(true);
    try {
      if (op === 'apply') await applyPowerTweak(kind);
      else await restorePowerTweak(kind);
      await refresh();
    } catch (e) { if (alive) error = String(e); }
    finally { busy = null; gpuOperationBusy.set(false); }
  }

  function confirmRun() {
    const kind = confirmKind;
    const op = confirmOp;
    confirmKind = null;
    if (kind) void run(kind, op);
  }

  $effect(() => { void refresh(); });
  onDestroy(() => { alive = false; });
</script>

{#if status}
  <section class="panel card" class:ok={status.pcieAspm === 0} class:warn={status.pcieAspm != null && status.pcieAspm !== 0}>
    <h2>{text.aspmTitle}</h2>
    <p class="card-status">{status.pcieAspm == null ? text.unknown : status.pcieAspm === 0 ? text.off : text.on}</p>
    {#if status.aspmRestorable}<p class="hint">{text.restorableHint}</p>{/if}
    {#if status.pcieAspm != null && status.pcieAspm !== 0}
      <button disabled={locked || busy !== null} aria-busy={busy === 'Aspm'} onclick={() => { confirmKind = 'Aspm'; confirmOp = 'apply'; confirmOpen = true; }}>{text.apply}</button>
    {:else if status.aspmRestorable}
      <button disabled={locked || busy !== null} aria-busy={busy === 'Aspm'} onclick={() => { confirmKind = 'Aspm'; confirmOp = 'restore'; confirmOpen = true; }}>{text.restore}</button>
    {/if}
  </section>
  <section class="panel card" class:ok={status.usbAc === 0 && status.usbDc === 0} class:warn={(status.usbAc != null && status.usbAc !== 0) || (status.usbDc != null && status.usbDc !== 0)}>
    <h2>{text.usbTitle}</h2>
    <p class="card-status">
      {#if status.usbAc == null && status.usbDc == null}{text.unknown}
      {:else if status.usbAc === 0 && status.usbDc === 0}{text.usbOff}
      {:else}{text.usbOn}
      {/if}
    </p>
    {#if status.usbRestorable}<p class="hint">{text.restorableHint}</p>{/if}
    {#if (status.usbAc != null && status.usbAc !== 0) || (status.usbDc != null && status.usbDc !== 0)}
      <button disabled={locked || busy !== null} aria-busy={busy === 'Usb'} onclick={() => { confirmKind = 'Usb'; confirmOp = 'apply'; confirmOpen = true; }}>{text.apply}</button>
    {:else if status.usbRestorable}
      <button disabled={locked || busy !== null} aria-busy={busy === 'Usb'} onclick={() => { confirmKind = 'Usb'; confirmOp = 'restore'; confirmOpen = true; }}>{text.restore}</button>
    {/if}
  </section>
  <p class="hint ab-hint">{text.abHint}</p>
{/if}
{#if error}<p class="error" role="alert">{$t(`errors.${error}`, { default: error })}</p>{/if}

<ConfirmDialog
  bind:open={confirmOpen}
  title={text.confirmTitle}
  message={text.confirmMsg}
  confirmLabel={text.confirm}
  cancelLabel={text.cancel}
  busy={busy !== null}
  onconfirm={confirmRun}
/>

<style>
  .card { display: flex; flex-direction: column; gap: 6px; }
  h2 { margin: 0; font-size: 1rem; }
  .card-status { margin: 0; }
  .hint { color: var(--text-secondary); font-size: 0.85rem; margin: 0; }
  .ab-hint { margin: 4px 0 0; }
  .error { color: var(--danger, #f38ba8); }
  button { align-self: flex-start; }
</style>

