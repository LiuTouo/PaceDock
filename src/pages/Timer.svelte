<script lang="ts">
  import { t } from 'svelte-i18n';
  import * as ipc from '../lib/ipc';
  import { settings } from '../lib/stores';
  import type { GameWindow, TimerExemptEntry, TimerStatus } from '../lib/types';

  // 計時器三值（Clockres 式）：僅按下手動偵測時查詢，不自動輪詢
  let status = $state<TimerStatus | null>(null);
  let detecting = $state(false);
  let saveError = $state<string | null>(null);
  let saving = $state(false);

  // 全域登錄值狀態（null = 讀取中/未設定）
  let globalEnabled = $state<boolean | null>(null);
  let globalBusy = $state(false);
  let rebootPending = $state(false);

  // 遊戲節流豁免
  let games = $state<GameWindow[]>([]);
  let exemptGame = $state('');
  let refreshingGames = $state(false);
  let exempts = $state<TimerExemptEntry[]>([]);

  async function detect() {
    if (detecting) return;
    detecting = true;
    try {
      status = await ipc.getTimerStatus();
      saveError = null;
    } catch (e) {
      saveError = String(e);
    } finally {
      detecting = false;
    }
  }

  async function refreshGames() {
    refreshingGames = true;
    try {
      games = await ipc.listGameWindows();
      if (!games.some((g) => String(g.pid) === exemptGame)) exemptGame = games[0] ? String(games[0].pid) : '';
      saveError = null;
    } catch (e) {
      saveError = String(e); // 枚舉失敗必須可見（例如後端未重建、command 不存在）
    } finally {
      refreshingGames = false;
    }
  }

  // 掛載時單次讀取（不輪詢）：全域登錄值狀態、遊戲清單、目前豁免清單
  $effect(() => {
    void refreshGames();
    void ipc.getTimerGlobalEnabled().then((v) => (globalEnabled = v)).catch(() => {});
    void ipc.listTimerExempts().then((v) => (exempts = v)).catch(() => {});
  });

  async function toggle(checked: boolean) {
    const current = $settings;
    if (!current || saving) return;
    saving = true;
    const next = { ...current, highPrecisionTimer: checked };
    settings.set(next); // 樂觀更新
    try {
      await ipc.saveSettings(next);
      saveError = null;
    } catch (e) {
      settings.set(current); // 後端拒絕 → 還原 checkbox
      saveError = String(e);
    } finally {
      saving = false;
    }
  }

  async function toggleGlobal(checked: boolean) {
    if (globalBusy) return;
    globalBusy = true;
    try {
      await ipc.setTimerGlobalEnabled(checked);
      globalEnabled = await ipc.getTimerGlobalEnabled();
      rebootPending = checked && globalEnabled === true;
      saveError = null;
    } catch (e) {
      saveError = String(e);
    } finally {
      globalBusy = false;
    }
  }

  // 後端 set_timer_exempt 已改 config；前端 store 不同步的話，
  // 下一次任意 saveSettings 會用 store 舊值蓋掉豁免名單
  function syncStorePrograms(next: string[]) {
    const cur = $settings;
    if (cur) settings.set({ ...cur, timerExemptPrograms: next });
  }

  async function addExempt() {
    const game = games.find((g) => String(g.pid) === exemptGame);
    if (!game || !game.exeName) return;
    try {
      await ipc.setTimerExempt(game.exeName, true);
      exempts = await ipc.listTimerExempts();
      syncStorePrograms([...new Set([...($settings?.timerExemptPrograms ?? []), game.exeName])]);
      saveError = null;
    } catch (e) {
      saveError = String(e);
    }
  }

  async function removeExempt(entry: TimerExemptEntry) {
    try {
      await ipc.setTimerExempt(entry.exeName, false);
      exempts = await ipc.listTimerExempts();
      syncStorePrograms(($settings?.timerExemptPrograms ?? []).filter((p) => p !== entry.exeName));
      saveError = null;
    } catch (e) {
      saveError = String(e);
    }
  }
</script>

{#if $settings}
  {#if saveError}
    <div class="panel error" role="alert">{$t('settings.saveFailed', { values: { error: saveError } })}</div>
  {/if}

  <!-- ── 常駐請求 ── -->
  <section class="panel">
    <h2>{$t('settings.highPrecisionTimer')}</h2>
    <label class="row">
      <span>{$t('settings.highPrecisionTimerEnable')}</span>
      <input
        type="checkbox"
        checked={$settings.highPrecisionTimer}
        disabled={saving}
        onchange={(e) => toggle(e.currentTarget.checked)}
      />
    </label>
    <p class="hint">{$t('settings.highPrecisionTimerHint')}</p>
  </section>

  <!-- ── 全域模式（登錄值）── -->
  <section class="panel">
    <h2>{$t('timer.globalTitle')}</h2>
    <p class="hint">{$t('timer.globalHint')}</p>
    <label class="row">
      <span>{$t('timer.globalEnable')}</span>
      <input
        type="checkbox"
        checked={globalEnabled === true}
        disabled={globalBusy}
        onchange={(e) => toggleGlobal(e.currentTarget.checked)}
      />
    </label>
    <p class="hint">
      {globalEnabled === true
        ? $t('timer.globalStatusOn')
        : $t('timer.globalStatusOff')}
    </p>
    {#if rebootPending}
      <p class="hint">{$t('timer.globalReboot')}</p>
    {/if}
  </section>

  <!-- ── 遊戲節流豁免（持久化名單）── -->
  <section class="panel">
    <h2>{$t('timer.exemptTitle')}</h2>
    <p class="hint">{$t('timer.exemptHint')}</p>
    <div class="row">
      <select bind:value={exemptGame}>
        <option value="" disabled hidden>{$t('timer.exemptPick')}</option>
        {#each games as game}<option value={String(game.pid)}>{game.title} — {game.exeName}</option>{/each}
      </select>
      <button class="primary" disabled={!exemptGame} onclick={addExempt}>{$t('timer.exemptAdd')}</button>
      <button disabled={refreshingGames} onclick={() => void refreshGames()}>{$t('measure.refresh')}</button>
    </div>
    {#if !exempts.length}
      <p class="hint">{$t('timer.exemptNone')}</p>
    {:else}
      <ul class="exempt-list">
        {#each exempts as entry (entry.exeName)}
          <li>
            <span>
              {entry.exeName}
              <small>{entry.pids.length ? `PID ${entry.pids.join(', ')}` : $t('timer.exemptNotRunning')}</small>
            </span>
            <button class="danger" onclick={() => removeExempt(entry)}>{$t('timer.exemptRemove')}</button>
          </li>
        {/each}
      </ul>
      <p class="hint">{$t('timer.exemptNote')}</p>
    {/if}
  </section>

  <!-- ── 計時器狀態（手動偵測）── -->
  <section class="panel">
    <h2>{$t('timer.statusTitle')}</h2>
    <p class="hint">{$t('timer.detectHint')}</p>
    <div class="reading">
      <span class="resolution" class:fast={$settings.highPrecisionTimer && (status?.currentResolutionMs ?? 16) <= 1}>
        {#if status?.currentResolutionMs}{status.currentResolutionMs.toFixed(2)}<small> ms</small>{:else}—{/if}
      </span>
      <span class="dot" class:on={$settings.highPrecisionTimer} aria-hidden="true"></span>
    </div>
    <div class="row">
      <button class="primary" disabled={detecting} onclick={detect}>{$t('timer.detect')}</button>
    </div>
    {#if status?.minIntervalMs && status?.maxIntervalMs}
      <p class="hint">
        {$t('timer.range', {
          values: {
            min: status.minIntervalMs.toFixed(2),
            max: status.maxIntervalMs.toFixed(3),
          },
        })}
      </p>
    {/if}
    <p class="hint">
      {$settings.highPrecisionTimer
        ? $t('timer.stateOn')
        : $t('timer.stateOff')}
    </p>
    <p class="hint">{$t('timer.globalNote')}</p>
  </section>
{/if}

<style>
  section {
    margin-bottom: var(--space-4);
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    min-height: var(--control-md);
    cursor: default;
  }
  .row select {
    flex: 1 1 220px;
    min-width: 0;
    cursor: pointer;
  }
  .exempt-list {
    list-style: none;
    margin: var(--space-2) 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .exempt-list li {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .exempt-list small {
    opacity: 0.6;
  }
  .hint {
    margin: var(--space-2) 0 0;
    font-size: 12px;
    opacity: 0.75;
    line-height: 1.5;
  }
  .error {
    color: var(--danger);
    margin-bottom: var(--space-4);
  }
  .reading {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    margin-top: var(--space-2);
  }
  .resolution {
    font-size: 40px;
    font-weight: var(--font-weight-semibold);
    font-variant-numeric: tabular-nums;
    line-height: 1;
    margin: 0;
  }
  .resolution small {
    font-size: 16px;
    opacity: 0.7;
  }
  .dot {
    width: 10px;
    height: 10px;
    border-radius: var(--radius-full);
    background: var(--text-muted);
    opacity: 0.4;
  }
  .dot.on {
    background: var(--accent);
    opacity: 1;
  }
</style>
