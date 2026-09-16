import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

test('renderer draws correct YUV/RGBA pixels, resizes and recovers context', async ({ page }) => {
  // Same fixture used for real WebKit/GPU checks, loading production TS via Vite.
  const html = readFileSync(resolve(__dirname, '../../../../scripts/fixtures/player-gpu-check.html'), 'utf8')
    .replace("from './playerRenderer.js'", "from '/src/playerRenderer.ts'");
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/renderer-check', route => route.fulfill({ contentType: 'text/html', body: html }));
  await page.goto('/renderer-check');
  await expect(page.locator('#result')).toContainText('"passed":', { timeout: 15_000 });
  const result = JSON.parse(await page.locator('#result').innerText());
  expect(result, JSON.stringify(result)).toMatchObject({ passed: true });
  expect(result.results.some((r: { name: string }) => r.name.startsWith('restored context'))).toBe(true);
  expect(errors).toEqual([]);
});
