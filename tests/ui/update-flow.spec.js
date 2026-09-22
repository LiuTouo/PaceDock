// 更新流程 UI 回歸：檢查 → 有新版 → 確認 → 下載進度即時顯示。
// 防止「按繼續後卡在 0%」的退化（downloadAndInstall 未接 onEvent / 無逾時）。
import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { mockTauri } from './mock-tauri.js';

const zh = JSON.parse(readFileSync(new URL('../../src/i18n/zh-TW.json', import.meta.url), 'utf8'));

test.afterEach(async ({ page }) => {
  expect(await page.evaluate(() => window.__uiMock?.unexpected ?? [])).toEqual([]);
});

test('安裝版：確認更新後顯示即時下載進度，流程完成不卡死', async ({ page }) => {
  await page.addInitScript(mockTauri, { language: 'zh-TW', portable: false });
  await page.goto('/');

  // 啟動自動檢查已找到新版（0.4.0 > 0.3.0）→ 設定頁顯示「有新版本」與安裝鈕
  await page.getByRole('button', { name: zh.nav.settings }).click();
  await expect(page.getByText(zh.settings.updateAvailable, { exact: false })).toBeVisible();

  await page.getByRole('button', { name: zh.settings.updateInstall }).click();
  await page
    .getByRole('alertdialog')
    .getByRole('button', { name: zh.settings.updateInstall })
    .click();

  // mock 回報 500/1000 → 狀態列顯示「正在下載… · 50%」（不是停在 0%）
  await expect(page.getByText(/50%/)).toBeVisible();

  // downloadAndInstall 有接進度 Channel、有帶 timeout
  const downloadArgs = await page.evaluate(() => window.__uiMock.downloadArgs);
  expect(downloadArgs?.onEvent).toBeTruthy();
  expect(downloadArgs?.timeout).toBeGreaterThan(0);

  // 下載完成 → Installing → relaunch；busy 鎖釋放（end_update 被叫，不卡死）
  await page.evaluate(() => window.__uiMock.finishDownload());
  await expect
    .poll(() => page.evaluate(() => window.__uiMock.calls))
    .toContain('plugin:process|restart');
  await expect
    .poll(() => page.evaluate(() => window.__uiMock.calls))
    .toContain('end_update');
});

test('可攜版：檢查有新版，確認後收到後端下載進度事件', async ({ page }) => {
  await page.addInitScript(mockTauri, { language: 'zh-TW', portable: true });
  await page.goto('/');

  const banner = page.locator('.update-banner');
  await expect(banner).toBeVisible();
  await banner.getByRole('button', { name: zh.update.bannerInstall }).click();
  await page
    .getByRole('alertdialog')
    .getByRole('button', { name: zh.settings.updateInstall })
    .click();

  await expect
    .poll(() => page.evaluate(() => window.__uiMock.calls))
    .toContain('perform_portable_update');
});
