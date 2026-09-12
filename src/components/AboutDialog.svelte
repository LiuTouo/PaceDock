<script lang="ts">
  import { t } from 'svelte-i18n';
  import { updateState, isPortable } from '../lib/stores';
  import type { UpdateStatus } from '../lib/types';
  import { checkForUpdates, installUpdate } from '../lib/updater';
  import ConfirmDialog from './ConfirmDialog.svelte';

  let { open = $bindable(false) }: { open?: boolean } = $props();

  let confirmOpen = $state(false);
  let checking = $derived($updateState?.status === 'Checking');
  let installing = $derived(
    $updateState?.status === 'Downloading' || $updateState?.status === 'Installing',
  );

  // 開啟對話框時自動檢查一次（狀態未知/Idle 才觸發；手動檢查按鈕常駐）
  $effect(() => {
    if (open && $updateState?.status === 'Idle') void checkForUpdates();
  });

  function onkeydown(event: KeyboardEvent) {
    if (!open) return;
    if (event.key === 'Escape') {
      event.stopPropagation();
      open = false;
    }
  }

  function statusLabel(s: UpdateStatus | null): string {
    if (!s) return '';
    switch (s) {
      case 'Checking': return $t('settings.updateChecking') as string;
      case 'UpToDate': return $t('settings.updateUpToDate') as string;
      case 'Available': return $t('settings.updateAvailable') as string;
      case 'Downloading': return $t('settings.updateDownloading') as string;
      case 'Installing': return $t('settings.updateInstalling') as string;
      case 'Error': return $t('settings.updateError') as string;
      default: return '';
    }
  }
</script>

<svelte:window {onkeydown} />

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div class="overlay" role="presentation" onclick={() => (open = false)}>
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <div
      class="dialog"
      role="dialog"
      aria-modal="true"
      tabindex="-1"
      aria-label={$t('about.title') as string}
      onclick={(event) => event.stopPropagation()}
    >
      <div class="brand">
        <svg class="brand-icon" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M12 2L2 7l10 5 10-5-10-5zM2 17l10 5 10-5M2 12l10 5 10-5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>
        </svg>
        <span class="brand-text">Frame<span class="brand-accent">Anchor</span></span>
      </div>

      <div class="opt">
        <span class="hint">
          {$t('settings.version')} {$updateState?.currentVersion ?? '0.0.0'}
          {#if $isPortable}
            <span class="tag">{$t('settings.portableBuild')}</span>
          {/if}
        </span>
      </div>

      <!-- ── 更新欄位：狀態 + 手動檢查 + 安裝 ── -->
      <div class="opt row">
        <span class="hint">
          {#if $updateState && $updateState.status !== 'Idle'}
            {statusLabel($updateState.status)}
            {#if $updateState.latestVersion && $updateState.status === 'Available'}
              · {$updateState.latestVersion}
            {/if}
            {#if $updateState.progress !== null && $updateState.status === 'Downloading'}
              · {$updateState.progress}%
            {/if}
          {/if}
        </span>
        <span class="actions">
          {#if $updateState?.status === 'Available'}
            <button class="primary" onclick={() => (confirmOpen = true)} disabled={installing}>
              {$t('settings.updateInstall')}
            </button>
          {/if}
          {#if $updateState?.status !== 'Downloading' && $updateState?.status !== 'Installing'}
            <button onclick={() => void checkForUpdates()} disabled={checking || installing} aria-busy={checking}>
              {checking ? $t('settings.updateChecking') : $t('settings.updateCheck')}
            </button>
          {/if}
        </span>
      </div>

      {#if $updateState?.error}
        <p class="error" role="alert">
          {$t('settings.updateErrorDetail', { values: { error: $updateState.error } })}
        </p>
      {/if}

      <div class="close-row">
        <button onclick={() => (open = false)}>{$t('about.close')}</button>
      </div>
    </div>
  </div>
{/if}

<ConfirmDialog
  bind:open={confirmOpen}
  title={$t('settings.updateConfirmTitle') as string}
  message={$t('settings.updateConfirmBody', {
    values: { version: $updateState?.latestVersion ?? '', current: $updateState?.currentVersion ?? '' },
  }) as string}
  confirmLabel={$t('settings.updateInstall') as string}
  cancelLabel={$t('common.cancel') as string}
  onconfirm={() => void installUpdate()}
/>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: var(--space-5);
    background: var(--overlay);
    backdrop-filter: blur(4px);
  }

  .dialog {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    width: min(420px, 100%);
    max-height: 100%;
    overflow-y: auto;
    padding: var(--space-6);
    background: var(--surface-1);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-xl);
    box-shadow: var(--shadow-lg);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }

  .brand-icon {
    width: 28px;
    height: 28px;
    color: var(--accent);
  }

  .brand-text {
    font-weight: var(--font-weight-semibold);
    font-size: 17px;
  }

  .brand-accent {
    color: var(--accent);
  }

  .opt {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    min-height: var(--control-md);
  }

  .opt.row {
    justify-content: space-between;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    max-width: 100%;
  }

  .hint {
    color: var(--text-secondary);
    font-size: 12.5px;
  }

  .tag {
    background: var(--surface-2);
    padding: 1px 8px;
    border-radius: var(--radius-full);
    font-size: 11px;
    margin-left: var(--space-2);
  }

  .error {
    margin: 0;
    color: var(--danger);
    font-size: 12px;
    line-height: 1.5;
    overflow-wrap: anywhere;
  }

  .close-row {
    display: flex;
    justify-content: flex-end;
    margin-top: var(--space-2);
  }
</style>
