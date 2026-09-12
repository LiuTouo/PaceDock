import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests/ui',
  timeout: 90_000,
  workers: 2,
  use: {
    baseURL: 'http://127.0.0.1:1421',
    // Windows uses its installed Edge; other platforms use Playwright Chromium.
    channel: process.platform === 'win32' ? 'msedge' : undefined,
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
  webServer: {
    command: 'npm run dev -- --host 127.0.0.1 --port 1421',
    url: 'http://127.0.0.1:1421',
    reuseExistingServer: false,
  },
});
