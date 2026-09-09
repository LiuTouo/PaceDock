<script lang="ts">
  import { locale, t } from 'svelte-i18n';
  import * as ipc from '../lib/ipc';
  import { settings, updateState, isPortable } from '../lib/stores';
  import type { Settings, UpdateStatus } from '../lib/types';
  import { checkForUpdates, installUpdate } from '../lib/updater';
  import ConfirmDialog from '../components/ConfirmDialog.svelte';

  let updateConfirmOpen = $state(false);

  // busy 狀態直接從 updateState store 推導
  let checking = $derived($updateState?.status === 'Checking');
  let installing = $derived(
    $updateState?.status === 'Downloading' || $updateState?.status === 'Installing',
  );

  let saveError = $state<string | null>(null);

  // 序列化 save 佇列 + 最後成功快照：快速連續 save 不會有 stale rollback 覆蓋較新成功。
  let saveChain = Promise.resolve();
  let confirmed: Settings | null = null; // 最近一次成功持久化的完整設定
  let latest: Settings | null = null; // 最近一次樂觀寫入（判斷此 save 是否仍是最新）

  // 載入完成時以持久化狀態作為 rollback 基準（一次性 seed）
  $effect(() => {
    if (confirmed === null && $settings) confirmed = $settings;
  });

  function save(partial: Partial<Settings>) {
    const current = $settings;
    if (!current) return;
    const next = { ...current, ...partial };
    latest = next;
    settings.set(next);
    saveChain = saveChain.then(async () => {
      try {
        await ipc.saveSettings(next);
        confirmed = next;
        if (latest === next) {
          locale.set(next.language);
          saveError = null;
        }
      } catch (e) {
        if (latest === next && confirmed) {
          settings.set(confirmed);
          locale.set(confirmed.language);
          saveError = String(e);
        }
      }
    });
  }

  /// 手動檢查更新
  async function manualCheck() {
    await checkForUpdates();
  }

  /// 執行更新（含確認對話框）
  async function doUpdate() {
    const curState = $updateState;
    if (!curState || curState.status !== 'Available') return;
    updateConfirmOpen = true;
  }

  async function confirmUpdate() {
    updateConfirmOpen = false;
    await installUpdate();
  }

  /// from update-state event 或手動設定
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

  const dataDir = '%APPDATA%\\FrameAnchor';
</script>

{#if $settings}
  {#if saveError}
    <div class="save-error" role="alert">
      <span class="error-msg">
        <svg viewBox="0 0 24 24" width="14" height="14" aria-hidden="true"><path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm1 15h-2v-2h2v2zm0-4h-2V7h2v6z" fill="currentColor"/></svg>
        {$t('settings.saveFailed', { values: { error: saveError } })}
      </span>
    </div>
  {/if}

  <!-- ── 外觀 ── -->
  <section class="panel settings-section">
    <h2 class="section-title">{$t('settings.appearance')}</h2>
    <div class="section">
      <div class="opt row">
        <span>{$t('settings.language')}</span>
        <select
          value={$settings.language}
          onchange={(e) => save({ language: e.currentTarget.value })}
        >
          <option value="zh-TW">繁體中文</option>
          <option value="en">English</option>
        </select>
      </div>

      <div class="opt row">
        <span>{$t('settings.theme')}</span>
        <select
          value={$settings.theme}
          onchange={(e) => save({ theme: e.currentTarget.value as 'Dark' | 'Light' })}
        >
          <option value="Dark">{$t('settings.themeDark')}</option>
          <option value="Light">{$t('settings.themeLight')}</option>
        </select>
      </div>
    </div>
  </section>

  <!-- ── 更新與版本 ── -->
  <section class="panel settings-section">
    <h2 class="section-title">{$t('settings.updateSection')}</h2>
    <div class="section">
      <div class="opt row">
        <span class="hint">
          FrameAnchor · {$t('settings.version')} {$updateState?.currentVersion ?? '0.0.0'}
          {#if $isPortable}
            <span class="tag">{$t('settings.portableBuild')}</span>
          {/if}
        </span>
      </div>

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

        {#if $updateState?.status === 'Available'}
          <button onclick={doUpdate} disabled={installing}>
            {installing ? '…' : $t('settings.updateInstall')}
          </button>
        {:else if $updateState?.status !== 'Downloading' && $updateState?.status !== 'Installing'}
          <button onclick={manualCheck} disabled={checking || installing}>
            {checking ? '…' : $t('settings.updateCheck')}
          </button>
        {/if}
      </div>

      {#if $updateState?.error}
        <div class="opt">
          <span class="error-msg" role="alert">
            <svg viewBox="0 0 24 24" width="14" height="14" aria-hidden="true"><path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm1 15h-2v-2h2v2zm0-4h-2V7h2v6z" fill="currentColor"/></svg>
            {$t('settings.updateErrorDetail', { values: { error: $updateState.error } })}
          </span>
        </div>
      {/if}
    </div>
  </section>

  <!-- ── 資料 ── -->
  <section class="panel settings-section">
    <h2 class="section-title">{$t('settings.data')}</h2>
    <div class="section">
      <div class="opt row">
        <span class="hint mono">{dataDir}</span>
        <button onclick={() => ipc.openDataFolder()}>{$t('settings.dataFolder')}</button>
      </div>
    </div>
  </section>
{/if}

<ConfirmDialog
  bind:open={updateConfirmOpen}
  title={$t('settings.updateConfirmTitle') as string}
  message={$t('settings.updateConfirmBody', {
    values: { version: $updateState?.latestVersion ?? '', current: $updateState?.currentVersion ?? '' },
  }) as string}
  confirmLabel={$t('settings.updateInstall') as string}
  cancelLabel={$t('common.cancel') as string}
  onconfirm={confirmUpdate}
/>

<style>
  .settings-section {
    margin-bottom: var(--space-4);
  }

  .settings-section:last-child {
    margin-bottom: 0;
  }

  .save-error {
    margin-bottom: var(--space-4);
  }

  .section {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .opt {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    cursor: pointer;
    min-height: var(--control-md);
  }

  .opt.row {
    cursor: default;
  }

  .opt.row {
    justify-content: space-between;
  }



  .tag {
    background: var(--surface-2);
    padding: 1px 8px;
    border-radius: var(--radius-full);
    font-size: 11px;
    margin-left: var(--space-2);
  }

  .error-msg {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    color: var(--danger);
    font-size: 12px;
  }

  .mono {
    font-family: 'IBM Plex Sans TC', monospace;
    font-size: 11px;
  }
</style>
