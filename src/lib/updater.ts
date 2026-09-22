// ── 前端更新控制器：集中管理更新檢查與安裝，含並行鎖 ──
//
// 動機：
//   - Tauri updater plugin 回傳的 Update 物件不可序列化，不能放 Svelte store
//   - App.svelte（啟動檢查）與 Settings.svelte（手動檢查/安裝）共享同一個 pending 物件
//   - busy 鎖確保跨元件不會重複觸發檢查或安裝
//
// 生命週期：
//   1. App.svelte onMount 呼叫 checkForUpdates() 進行啟動檢查
//   2. 若有新版，updateState store 設為 Available → App.svelte 橫幅出現
//   3. 使用者從橫幅或 Settings 點安裝 → installUpdate() → confirm → downloadAndInstall
//   4. 安裝後呼叫 relaunch() 重新啟動程序；NSIS passive 模式通常由安裝程式關閉本程序

import { check } from "@tauri-apps/plugin-updater";
import type { DownloadEvent } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { get } from "svelte/store";
import { updateState, isPortable, gpuOperationBusy } from "./stores";
import * as ipc from "./ipc";

type PendingUpdate = Awaited<ReturnType<typeof check>>;

let pendingUpdate: PendingUpdate = null;
let busy = false;

// 檢查/下載逾時：GitHub 在部分網路會被限速或半途停滯（觀測值約 90KB/s），
// 沒有逾時會讓狀態機卡在 Checking/Downloading 永不返回。
const CHECK_TIMEOUT_MS = 30_000;
// 7MB 安裝包在極慢網路（>=8KB/s）約 15 分鐘內可完成；超過視為停滯直接報錯。
const DOWNLOAD_TIMEOUT_MS = 15 * 60_000;

/** 從 store 讀目前版本（若 store 為 null 則退回空字串） */
function currentVersion(): string {
  return get(updateState)?.currentVersion ?? "";
}

/** 檢查更新：安裝版用 updater plugin，可攜版用後端命令。重複呼叫會被並行鎖忽略。 */
export async function checkForUpdates(): Promise<void> {
  if (busy) return;
  busy = true;
  const cv = currentVersion();
  try {
    if (get(isPortable)) {
      updateState.set({
        status: "Checking",
        latestVersion: null,
        currentVersion: cv,
        progress: null,
        error: null,
      });
      await ipc.checkPortableUpdate();
    } else {
      updateState.set({
        status: "Checking",
        latestVersion: null,
        currentVersion: cv,
        progress: null,
        error: null,
      });
      pendingUpdate = await check({ timeout: CHECK_TIMEOUT_MS });
      if (pendingUpdate) {
        updateState.set({
          status: "Available",
          latestVersion: pendingUpdate.version,
          currentVersion: pendingUpdate.currentVersion,
          progress: null,
          error: null,
        });
      } else {
        updateState.set({
          status: "UpToDate",
          latestVersion: null,
          currentVersion: cv,
          progress: null,
          error: null,
        });
      }
    }
  } catch (e) {
    updateState.set({
      status: "Error",
      latestVersion: null,
      currentVersion: cv,
      progress: null,
      error: String(e),
    });
  } finally {
    busy = false;
  }
}

/**
 * 安裝更新。
 * 若 pendingUpdate 為 null（例如啟動時只存了 store 狀態），先重新執行 check()。
 * 回傳 false 表示無法繼續（非 Available、busy 鎖、或重新 check 後無更新）。
 * busy 鎖在所有非退出路徑上保證釋放；僅 relaunch/exit 路徑不釋放。
 */
export async function installUpdate(): Promise<boolean> {
  if (busy) return false;

  const portable = get(isPortable);
  const curState = get(updateState);
  if (!curState || curState.status !== "Available") return false;

  busy = true;
  gpuOperationBusy.set(true);
  let reserved = false;
  const cv = curState.currentVersion;

  try {
    if (!portable) {
      await ipc.beginUpdate();
      reserved = true;
      // 安裝版：確保持有 Update 物件
      if (!pendingUpdate) {
        try {
          pendingUpdate = await check({ timeout: CHECK_TIMEOUT_MS });
        } catch (e) {
          updateState.set({
            status: "Error",
            latestVersion: null,
            currentVersion: cv,
            progress: null,
            error: String(e),
          });
          return false;
        }
        if (!pendingUpdate) {
          updateState.set({
            status: "UpToDate",
            latestVersion: null,
            currentVersion: cv,
            progress: null,
            error: null,
          });
          return false;
        }
      }

      updateState.set({
        status: "Downloading",
        latestVersion: pendingUpdate.version,
        currentVersion: cv,
        progress: 0,
        error: null,
      });
      // 下載進度：updater plugin 以 Channel 回報 Started/Progress/Finished。
      // 沒有這段，慢速網路下會停在 0% 看起來像當機（GitHub 限速約 90KB/s）。
      let received = 0;
      let total = 0;
      await pendingUpdate.downloadAndInstall(
        (e: DownloadEvent) => {
          if (e.event === "Started" && e.data.contentLength) {
            total = e.data.contentLength;
          } else if (e.event === "Progress") {
            received += e.data.chunkLength;
            if (total > 0) {
              const pct = Math.min(99, Math.floor((received / total) * 100));
              const s = get(updateState);
              if (s && s.status === "Downloading" && s.progress !== pct) {
                updateState.set({ ...s, progress: pct });
              }
            }
          }
        },
        { timeout: DOWNLOAD_TIMEOUT_MS },
      );
      // NSIS passive 模式：安裝程式通常會關閉本程序再替換檔案。
      // 若到達此處，呼叫 relaunch() 確保程序重啟。
      updateState.set({
        status: "Installing",
        latestVersion: pendingUpdate.version,
        currentVersion: cv,
        progress: null,
        error: null,
      });
      await relaunch();
      // relaunch 可能在某些平台上返回而不退出；此處為死碼保險
      return true;
    }

    // 可攜版
    updateState.set({
      status: "Downloading",
      latestVersion: null,
      currentVersion: cv,
      progress: 0,
      error: null,
    });
    await ipc.performPortableUpdate();
    // perform_portable_update 觸發輔助腳本後呼叫 app.exit(0)
    return true;
  } catch (e) {
    updateState.set({
      status: "Error",
      latestVersion: null,
      currentVersion: cv,
      progress: null,
      error: String(e),
    });
    return false;
  } finally {
    if (reserved) await ipc.endUpdate();
    gpuOperationBusy.set(false);
    busy = false;
  }
}
