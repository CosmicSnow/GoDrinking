import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests/browser',
  testMatch: '**/*.pw.ts',
  timeout: 30_000,
  retries: 0,
  workers: 1,
  reporter: 'list',
  outputDir: '../../e2e-artifacts/browser-tests',
  use: {
    browserName: 'chromium',
    baseURL: 'http://127.0.0.1:1437',
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
    launchOptions: { args: ['--enable-unsafe-swiftshader'] },
  },
  webServer: {
    command: 'npm run dev -- --host 127.0.0.1 --port 1437',
    url: 'http://127.0.0.1:1437',
    reuseExistingServer: false,
  },
});
