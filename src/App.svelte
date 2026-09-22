<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { locale, t } from "svelte-i18n";
  import GpuTest from "./pages/GpuTest.svelte";
  import SettingsPage from "./pages/Settings.svelte";
  import TimerPage from "./pages/Timer.svelte";
  import ConfirmDialog from "./components/ConfirmDialog.svelte";
  import AboutDialog from "./components/AboutDialog.svelte";
  import * as ipc from "./lib/ipc";
  import {
    gpuOperationBusy,
    benchmarkProgress,
    benchmarkState,
    isPortable,
    settings,
    topology,
    updateState,
  } from "./lib/stores";
  import type { BenchmarkProgress, UpdateState } from "./lib/types";
  import { checkForUpdates, installUpdate } from "./lib/updater";

  type Tab = "gpu" | "timer" | "settings";
  let tab = $state<Tab>("gpu");

  // 基準測試執行中 → 鎖定導覽，不能離開 GPU 測試頁
  const benchmarkRunning = $derived(
    $benchmarkState?.status === "Running" ||
      $benchmarkState?.gpuBusy ||
      $gpuOperationBusy,
  );
  // compact progress 視窗模式（後端 windowLayout=CompactProgress）→ 隱藏側欄/橫幅
  const compact = $derived($benchmarkState?.windowLayout === "CompactProgress");

  function switchTab(next: Tab) {
    if (benchmarkRunning && next !== "gpu") return; // 執行中禁止離開
    tab = next;
  }

  function navDisabled(next: Tab) {
    return benchmarkRunning && next !== "gpu";
  }

  // 進入 Running → 自動切到 GPU 測試頁（reload/reopen 也落在警告+進度+取消）
  $effect(() => {
    if ($benchmarkState?.status === "Running") tab = "gpu";
  });

  // 主題：監聽 settings store 變更，同步到 document.documentElement
  $effect(() => {
    const theme = $settings?.theme ?? "Dark";
    document.documentElement.setAttribute("data-theme", theme.toLowerCase());
  });

  // 啟動時找到的更新橫幅：本機 dismiss 旗標，不影響 store 狀態
  let updateBannerDismissed = $state(false);
  let updateConfirmOpen = $state(false);
  let aboutOpen = $state(false);
  let exitBlocked = $state(false);

  onMount(() => {
    let unlisteners: Array<() => void> = [];
    const poll = setInterval(() => {
      if ($benchmarkState?.gpuBusy || $benchmarkState?.status === "Running")
        void ipc.getBenchmarkState().then(benchmarkState.set);
    }, 1000);
    (async () => {
      unlisteners.push(
        await listen("gpu-exit-blocked", () => {
          exitBlocked = true;
        }),
      );
      unlisteners.push(
        await listen("show-about", () => {
          aboutOpen = true;
        }),
      );
      topology.set(await ipc.getTopology());
      const s = await ipc.getSettings();
      settings.set(s);
      locale.set(s.language);

      // 重建基準測試執行期狀態（reload 後不重啟/不停止 session）
      benchmarkState.set(await ipc.getBenchmarkState());

      // 取得版本資訊與可攜版旗標
      const info = await ipc.getUpdateInfo();
      isPortable.set(info.portable);

      // 初始化 updateState 的 currentVersion
      updateState.set({
        status: "Idle",
        latestVersion: null,
        currentVersion: info.version,
        progress: null,
        error: null,
      });

      // 事件監聽（必須在檢查更新前註冊，避免 race）
      unlisteners.push(
        await listen<UpdateState>("update-state", (e) =>
          updateState.set(e.payload),
        ),
      );
      // GPU 基準測試進度事件 → 即時更新 state（後端仍是執行期唯一 owner）
      unlisteners.push(
        await listen<BenchmarkProgress>("gpu-benchmark-progress", (e) => {
          benchmarkProgress.set(e.payload);
          void ipc.getBenchmarkState().then((s) => benchmarkState.set(s));
        }),
      );

      // 啟動時自動檢查更新（匯流至 updater 模組）
      await checkForUpdates();
    })();
    return () => {
      clearInterval(poll);
      unlisteners.forEach((u) => u());
    };
  });

  /** 橫幅「安裝」按鈕：確認後呼叫共享安裝流程 */
  async function bannerInstall() {
    const curState = $updateState;
    if (!curState || curState.status !== "Available") return;
    updateConfirmOpen = true;
  }

  async function confirmBannerInstall() {
    updateConfirmOpen = false;
    await installUpdate();
  }

  // ── 導覽項目定義（icon 為 inline SVG path data） ──
  const navItems: { tab: Tab; label: string; icon: string }[] = [
    {
      tab: "gpu",
      label: "gpuTest",
      icon: "M21 3H3v18h18V3zm-2 16H5V5h14v14zm-4.5-7h-3v3h-2v-3h-3V9h3V6h2v3h3v2z",
    },
    {
      tab: "timer",
      label: "timer",
      icon: "M11.99 2C6.47 2 2 6.48 2 12s4.47 10 9.99 10C17.52 22 22 17.52 22 12S17.52 2 11.99 2zM12 20c-4.42 0-8-3.58-8-8s3.58-8 8-8 8 3.58 8 8-3.58 8-8 8zm.5-13H11v6l5.25 3.15.75-1.23-4.5-2.67V7z",
    },
    {
      tab: "settings",
      label: "settings",
      icon: "M12 15.5A3.5 3.5 0 018.5 12 3.5 3.5 0 0112 8.5a3.5 3.5 0 013.5 3.5 3.5 3.5 0 01-3.5 3.5zm7.43-2.53c.04-.32.07-.64.07-.97 0-.33-.03-.66-.07-.98l2.11-1.65c.19-.15.24-.42.12-.64l-2-3.46a.5.5 0 00-.61-.22l-2.49 1c-.52-.4-1.08-.73-1.69-.98l-.38-2.65A.49.49 0 0014 2h-4c-.25 0-.46.18-.49.42l-.38 2.65c-.61.25-1.17.59-1.69.98l-2.49-1a.5.5 0 00-.61.22l-2 3.46c-.13.22-.07.49.12.64l2.11 1.65c-.04.32-.07.65-.07.98 0 .33.03.66.07.98l-2.11 1.65c-.19.15-.24.42-.12.64l2 3.46a.5.5 0 00.61.22l2.49-1c.52.4 1.08.73 1.69.98l.38 2.65c.03.24.24.42.49.42h4c.25 0 .46-.18.49-.42l.38-2.65c.61-.25 1.17-.59 1.69-.98l2.49 1c.23.09.49 0 .61-.22l2-3.46c.13-.22.07-.49-.12-.64l-2.11-1.65zM12 17c-2.76 0-5-2.24-5-5s2.24-5 5-5 5 2.24 5 5-2.24 5-5 5z",
    },
  ];
</script>

<div class="shell" class:compact>
  <!-- 側欄導覽（compact progress 模式隱藏） -->
  {#if !compact}
    <nav class="sidebar" aria-label={$t("nav.settings")}>
      <div class="brand">
        <svg class="brand-icon" viewBox="0 0 24 24" aria-hidden="true">
          <path
            d="M12 2L2 7l10 5 10-5-10-5zM2 17l10 5 10-5M2 12l10 5 10-5"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
        </svg>
        <span class="brand-text"
          >Pace<span class="brand-accent">Dock</span></span
        >
      </div>

      <div class="nav-items">
        {#each navItems as item (item.tab)}
          <button
            class="nav-btn ghost"
            class:active={tab === item.tab}
            disabled={navDisabled(item.tab)}
            onclick={() => switchTab(item.tab)}
            aria-current={tab === item.tab ? "page" : undefined}
          >
            <svg viewBox="0 0 24 24" class="nav-icon" aria-hidden="true">
              <path d={item.icon} fill="currentColor" />
            </svg>
            <span>{$t(`nav.${item.label}`)}</span>
          </button>
        {/each}
      </div>

      <div class="sidebar-footer">
        <span class="hint version-label"
          >v{$updateState?.currentVersion ?? "0.0.0"}</span
        >
      </div>
    </nav>
  {/if}

  <!-- 主內容區 -->
  <main class="content">
    {#if exitBlocked}<div role="alert" class="panel">
        <p>{$t("quick.exitBlocked")}</p>
        <button onclick={() => (exitBlocked = false)}
          >{$t("quick.dismiss")}</button
        >
      </div>{/if}
    {#if $updateState?.status === "Available" && !updateBannerDismissed && !compact}
      <div class="update-banner" role="status">
        <svg class="banner-icon" viewBox="0 0 24 24" aria-hidden="true">
          <path
            d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm-2 15l-5-5 1.41-1.41L10 14.17l7.59-7.59L19 8l-9 9z"
            fill="currentColor"
          />
        </svg>
        <span>
          {$t("update.bannerText", {
            values: {
              version: $updateState.latestVersion ?? "",
              current: $updateState.currentVersion,
            },
          })}
        </span>
        <div class="banner-actions">
          <button class="small primary" onclick={bannerInstall}>
            {$t("update.bannerInstall")}
          </button>
          <button class="small" onclick={() => (updateBannerDismissed = true)}>
            {$t("update.bannerDismiss")}
          </button>
        </div>
      </div>
    {/if}

    <div class="page">
      {#if tab === "gpu"}
        <GpuTest />
      {:else if tab === "timer"}
        <TimerPage />
      {:else}
        <SettingsPage />
      {/if}
    </div>
  </main>
</div>

<ConfirmDialog
  bind:open={updateConfirmOpen}
  title={$t("settings.updateConfirmTitle") as string}
  message={$t("settings.updateConfirmBody", {
    values: {
      version: $updateState?.latestVersion ?? "",
      current: $updateState?.currentVersion ?? "",
    },
  }) as string}
  confirmLabel={$t("settings.updateInstall") as string}
  cancelLabel={$t("common.cancel") as string}
  onconfirm={confirmBannerInstall}
/>

<AboutDialog bind:open={aboutOpen} />

<style>
  .shell {
    display: flex;
    height: 100%;
  }

  /* ── 側欄 ── */
  .sidebar {
    width: 216px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    background: var(--surface-1);
    border-right: 1px solid var(--border-subtle);
    padding: var(--space-4) var(--space-3);
    gap: var(--space-2);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-1) var(--space-2) var(--space-5);
  }

  .brand-icon {
    width: 26px;
    height: 26px;
    color: var(--accent);
    flex-shrink: 0;
  }

  .brand-text {
    font-weight: var(--font-weight-semibold);
    font-size: 16px;
    letter-spacing: -0.01em;
  }

  .brand-accent {
    color: var(--accent);
  }

  /* ── 導覽按鈕 ── */
  .nav-items {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    flex: 1;
  }

  .nav-btn {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    width: 100%;
    text-align: left;
    justify-content: flex-start;
    padding: 7px var(--space-3);
    min-height: 40px;
    font-size: 13.5px;
  }

  .nav-btn.active::before {
    content: "";
    position: absolute;
    left: 0;
    top: 9px;
    bottom: 9px;
    width: 3px;
    border-radius: var(--radius-full);
    background: var(--accent);
  }

  .nav-icon {
    width: 18px;
    height: 18px;
    flex-shrink: 0;
    color: currentColor;
    transition: color var(--transition-fast);
  }

  .nav-btn.active .nav-icon {
    color: var(--accent);
  }

  /* ── 側欄底部 ── */
  .sidebar-footer {
    padding: var(--space-3) var(--space-2) 0;
    border-top: 1px solid var(--border-subtle);
    margin-top: var(--space-2);
  }

  .version-label {
    font-size: 11px;
  }

  /* ── 主內容區 ── */
  .content {
    flex: 1;
    display: flex;
    flex-direction: column;
    overflow: hidden;
    min-width: 0;
  }

  .page {
    flex: 1;
    overflow-y: auto;
    padding: var(--space-6);
  }

  /* ── 更新橫幅 ── */
  .update-banner {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-5);
    background: var(--accent-muted);
    border-bottom: 1px solid color-mix(in srgb, var(--accent) 32%, transparent);
    font-size: 13px;
    flex-shrink: 0;
  }

  .banner-icon {
    width: 18px;
    height: 18px;
    color: var(--accent);
    flex-shrink: 0;
  }

  .update-banner span {
    flex: 1 1 240px;
    overflow-wrap: anywhere;
  }

  .banner-actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    max-width: 100%;
  }

  @media (max-width: 999px) {
    .page {
      padding: var(--space-4);
    }
  }

  /* compact 由進度資訊區捲動，取消按鈕保留在底部。 */
  .shell.compact .page {
    padding: var(--space-2);
    overflow: hidden;
    min-height: 0;
  }
</style>
