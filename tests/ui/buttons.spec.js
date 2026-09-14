import { test, expect } from '@playwright/test';
import { mockTauri } from './mock-tauri.js';

// Measure computed colors (including color-mix and ancestor alpha), not token strings.
async function inspect(button) {
  return button.evaluate(el => {
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 1;
    const ctx = canvas.getContext('2d', { willReadFrequently: true });
    const rgba = css => {
      ctx.clearRect(0, 0, 1, 1);
      ctx.fillStyle = css;
      ctx.fillRect(0, 0, 1, 1);
      const data = [...ctx.getImageData(0, 0, 1, 1).data];
      return [...data.slice(0, 3), data[3] / 255];
    };
    const over = (fg, bg) => [...fg.slice(0, 3).map((v, i) => v * fg[3] + bg[i] * (1 - fg[3])), 1];
    const luminance = rgb => rgb.slice(0, 3).map(v => v / 255)
      .map(v => v <= .04045 ? v / 12.92 : ((v + .055) / 1.055) ** 2.4)
      .reduce((sum, v, i) => sum + v * [.2126, .7152, .0722][i], 0);
    const contrast = (a, b) => (Math.max(luminance(a), luminance(b)) + .05) / (Math.min(luminance(a), luminance(b)) + .05);
    const parents = [];
    for (let p = el.parentElement; p; p = p.parentElement) parents.unshift(p);
    let backdrop = [255, 255, 255, 1];
    for (const p of parents) backdrop = over(rgba(getComputedStyle(p).backgroundColor), backdrop);
    const s = getComputedStyle(el);
    const bg = over(rgba(s.backgroundColor), backdrop);
    const fg = over(rgba(s.color), bg);
    const opacity = [el, ...parents].reduce((v, p) => v * Number(getComputedStyle(p).opacity), 1);
    const rect = el.getBoundingClientRect();
    const range = document.createRange();
    range.selectNodeContents(el);
    const textRect = range.getBoundingClientRect();
    return {
      label: el.textContent.trim(), ratio: contrast(fg, bg), opacity,
      outline: s.outlineStyle, outlineWidth: s.outlineWidth,
      focusRatio: contrast(rgba(s.outlineColor), backdrop),
      textFits: textRect.left >= rect.left && textRect.right <= rect.right + 1 && textRect.bottom <= rect.bottom + 1,
      widthFits: rect.left >= 0 && rect.right <= innerWidth + 1,
      containerFits: parents.every(p => {
        const box = p.getBoundingClientRect();
        return getComputedStyle(p).display === 'inline' || box.width === 0 ||
          (rect.left >= box.left - 1 && rect.right <= box.right + 1);
      }),
    };
  });
}

async function readable(button) {
  const result = await inspect(button);
  expect(result.ratio, JSON.stringify(result)).toBeGreaterThanOrEqual(4.5);
  expect(result.opacity, result.label).toBe(1);
  expect(result.textFits, result.label).toBe(true);
  expect(result.widthFits, result.label).toBe(true);
  expect(result.containerFits, result.label).toBe(true);
}

async function audit(page, scope = page) {
  await page.evaluate(() => { window.__uiAudit = true; });
  const buttons = scope.locator('button:visible');
  for (let i = 0; i < await buttons.count(); i++) {
    const button = buttons.nth(i);
    await button.scrollIntoViewIfNeeded();
    await page.mouse.move(0, 0);
    await readable(button);
    if (await button.isDisabled()) continue;
    await button.hover();
    await readable(button);
    await page.mouse.down();
    await readable(button);
    // Release away from the button: an audit must not activate native actions.
    await page.mouse.move(0, 0);
    await page.mouse.up();
    await button.focus();
    await page.keyboard.press('Tab');
    await button.focus();
    const focused = await inspect(button);
    expect(focused.outline, focused.label).toBe('solid');
    expect(focused.outlineWidth, focused.label).toBe('2px');
    expect(focused.focusRatio, focused.label).toBeGreaterThanOrEqual(3);
    // Exercise disabled CSS for every actual component/variant, including primary/danger.
    await button.evaluate(el => { el.disabled = true; });
    await readable(button);
    await button.evaluate(el => { el.disabled = false; });
  }
  await page.evaluate(() => { window.__uiAudit = false; });
}

async function boot(page, options) {
  await page.addInitScript(mockTauri, options);
  await page.addInitScript(() => {
    // A mouse release outside a dialog button can click its overlay ancestor.
    document.addEventListener('click', event => {
      if (window.__uiAudit) { event.preventDefault(); event.stopImmediatePropagation(); }
    }, true);
  });
  await page.goto('/');
  await expect(page).toHaveTitle('PaceDock');
  if (!options.compact) await expect(page.locator('.brand-text')).toHaveText('PaceDock');
  await expect(page.locator('html')).toHaveAttribute('data-theme', options.theme.toLowerCase());
  await expect(page.locator('.gpu-page')).toBeVisible();
  await page.evaluate(() => document.fonts.ready);
}

for (const theme of ['Dark', 'Light']) {
  for (const language of ['zh-TW', 'en']) {
    test(`${theme} ${language}: all pages and button interaction states`, async ({ page }, testInfo) => {
      const errors = [];
      page.on('pageerror', error => errors.push(error.message));
      await page.setViewportSize({ width: 1280, height: 720 });
      await boot(page, { theme, language });
      await expect(page.locator('.start-row .primary').first()).toBeEnabled();
      await audit(page);

      // Normal and destructive confirmation dialogs use the same variant contract.
      await page.locator('.start-row .primary').first().click();
      await expect(page.locator('.dialog button').first()).toBeFocused();
      await audit(page, page.locator('.dialog'));
      await page.keyboard.press('Escape');

      await page.locator('.tabs button').nth(1).click();
      await page.locator('.gpu-page > .panel .field select').last().selectOption('session-1');
      await expect(page.locator('.gpu-page button.danger')).toBeVisible();
      await audit(page);
      await page.locator('.gpu-page button.danger').click();
      await expect(page.locator('.dialog button.primary')).toHaveClass(/danger/);
      await audit(page, page.locator('.dialog'));
      await page.keyboard.press('Escape');

      await page.locator('.tabs button').nth(2).click();
      await page.locator('.form-grid select').nth(1).selectOption('capture-1');
      await page.locator('.form-grid select').nth(2).selectOption('capture-2');
      await audit(page);
      await page.locator('.gpu-page button.danger').click();
      await expect(page.locator('.dialog button.primary')).toHaveClass(/danger/);
      await audit(page, page.locator('.dialog'));
      await page.keyboard.press('Escape');

      await page.locator('.nav-btn').nth(1).click();
      await expect(page.locator('.exempt-list')).toBeVisible();
      await audit(page);
      await page.screenshot({ path: testInfo.outputPath('timer.png'), fullPage: true });
      await page.evaluate(() => { window.__uiMock.blocked.push('get_timer_status'); });
      const detectButton = page.getByRole('button', { name: language === 'zh-TW' ? '手動偵測' : 'Detect now', exact: true });
      await detectButton.click();
      await expect(detectButton).toBeDisabled();
      await readable(detectButton);
      await page.evaluate(() => window.__uiMock.release.get_timer_status());

      await page.locator('.nav-btn').nth(2).click();
      await audit(page);
      await page.locator('.settings-section button.primary').click();
      await audit(page, page.locator('.dialog'));
      await page.keyboard.press('Escape');

      // Larger viewport and doubled text exercise layout without changing native window constraints.
      await page.setViewportSize({ width: 1920, height: 1080 });
      await audit(page);
      await page.setViewportSize({ width: 1280, height: 720 });
      await page.addStyleTag({ content: 'body { font-size: 27px; } button, .nav-btn, button.small { font-size: 27px !important; }' });
      await audit(page);
      await page.locator('.settings-section button.primary').click();
      await audit(page, page.locator('.dialog'));
      await page.screenshot({ path: testInfo.outputPath('dialog-large-text.png') });
      await page.keyboard.press('Escape');
      await page.locator('.nav-btn').nth(1).click();
      await audit(page);
      await page.locator('.nav-btn').first().click();
      await audit(page);
      await page.locator('.tabs button').nth(2).click();
      await audit(page);
      await page.evaluate(() => window.__uiMock.emit('show-about', null));
      await expect(page.getByRole('dialog')).toBeVisible();
      await expect(page.getByRole('dialog').locator('.brand-text')).toHaveText('PaceDock');
      await audit(page, page.getByRole('dialog'));
      expect(errors).toEqual([]);
    });

    test(`${theme} ${language}: compact cancellation remains visible`, async ({ page }, testInfo) => {
      await page.setViewportSize({ width: 480, height: 300 });
      await boot(page, { theme, language, compact: true });
      const cancel = page.locator('.progress-panel > button');
      await audit(page);
      await page.screenshot({ path: testInfo.outputPath('compact.png') });
      await page.addStyleTag({ content: '.compact p { font-size: 24px !important; } .compact h2 { font-size: 34px !important; } button.small { font-size: 24px !important; }' });
      await expect(cancel).toBeInViewport({ ratio: 1 });
      await audit(page);
      await cancel.focus();
      await page.keyboard.press('Enter');
      await expect(cancel).toBeDisabled();
      await readable(cancel);
      await expect(cancel).toBeInViewport({ ratio: 1 });
      await page.screenshot({ path: testInfo.outputPath('compact-large-text.png') });
    });
  }
}

test('keyboard activation, locked navigation, and modal focus', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await boot(page, { theme: 'Dark', language: 'en' });
  await page.locator('.nav-btn').nth(1).focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('.exempt-list')).toBeVisible();
  await page.locator('.nav-btn').first().focus();
  await page.keyboard.press('Space');
  await expect(page.locator('.tabs')).toBeVisible();
  const start = page.locator('.start-row .primary').first();
  await expect(start).toBeEnabled();
  await start.click();
  const cancel = page.locator('.dialog button').first();
  const confirm = page.locator('.dialog button').last();
  await expect(cancel).toBeFocused();
  await page.keyboard.press('Shift+Tab');
  await expect(confirm).toBeFocused();
  await page.keyboard.press('Tab');
  await expect(cancel).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(start).toBeFocused();
  await page.evaluate(() => { window.__uiMock.blocked.push('sample_gpu_interrupts'); });
  await page.locator('.interrupt-panel button').click();
  await expect(page.locator('.interrupt-panel button')).toBeDisabled();
  await expect(page.locator('.nav-btn').nth(1)).toBeDisabled();
  await expect(page.locator('.nav-btn').nth(2)).toBeDisabled();
  await readable(page.locator('.interrupt-panel button'));
  await readable(page.locator('.nav-btn').nth(1));
  await page.locator('.nav-btn').nth(1).evaluate(el => el.click());
  await expect(page.locator('.tabs')).toBeVisible();
});
