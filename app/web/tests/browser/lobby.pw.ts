import { test, expect } from '@playwright/test';

test('browser mock UI can create, leave and enter a room without crashing', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto('/');
  await expect(page.getByTestId('mock-tag')).toBeVisible();
  await page.getByLabel('Senha da sala para criar').fill('browser-test');
  await page.getByRole('button', { name: 'Criar sala', exact: true }).click();
  await expect(page.getByRole('region', { name: 'Transmissões da sala' })).toBeVisible();
  await page.getByRole('button', { name: 'Sair da sala' }).click();
  await expect(page.getByRole('button', { name: 'Criar sala', exact: true })).toBeVisible();
  await page.getByLabel('Código da sala').fill('ABC123');
  await page.getByLabel('Senha da sala para entrar').fill('browser-test');
  await page.locator('#joinBtn').click();
  await expect(page.getByRole('button', { name: 'Sair da sala' })).toBeVisible();
  expect(errors).toEqual([]);
});
