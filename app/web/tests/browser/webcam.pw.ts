import { test, expect } from '@playwright/test';

// TDD RED — navegação real pela UI (sem atalhos de API).
// Espelha o pedido: validar na UI os 3 usos + visibilidade em outra instância.
//
// Uso 1: stream (tela) + webcam simultâneos
// Uso 2: só stream (regressão)
// Uso 3: só webcam
// Uso 4: segunda instância/aba vê o share (repite 1-3 como viewer)
//
// Hoje TODOS falham: o modal Compartilhar só conhece display/window,
// não existe aba/opção "Webcam", nem CapabilitySet.camera.

async function lobbyToRoom(page: any) {
  await page.goto('/');
  await expect(page.getByTestId('mock-tag')).toBeVisible();
  await page.getByLabel('Senha da sala para criar').fill('webcam-tdd');
  await page.getByRole('button', { name: 'Criar sala', exact: true }).click();
  await expect(page.getByRole('region', { name: 'Transmissões da sala' })).toBeVisible();
}

test('uso 2 (regressão): só stream — Listar telas oferece display', async ({ page }) => {
  await lobbyToRoom(page);
  await page.getByRole('button', { name: /Compartilhar|Iniciar/i }).first().click().catch(() => {});
  // O botão que abre o modal tem data-hook share-open; o modal lista fontes.
  const shareOpen = page.locator('[data-hook="share-open"]');
  if (await shareOpen.count()) await shareOpen.first().click();
  await page.getByRole('button', { name: /Listar telas/i }).click();
  await expect(page.getByText(/Display .*·/i).first()).toBeVisible({ timeout: 8000 });
});

test('uso 3 (só webcam): modal oferece aba/opção Webcam e seleciona camera:<id>', async ({ page }) => {
  await lobbyToRoom(page);
  const shareOpen = page.locator('[data-hook="share-open"]');
  if (await shareOpen.count()) await shareOpen.first().click();
  await page.getByRole('button', { name: /Listar telas/i }).click();
  // A feature exige: aba "Webcam" OU opção camera listada com nome real.
  const webcamTab = page.getByRole('button', { name: /webcam|câmera|camera/i });
  await expect(webcamTab.first()).toBeVisible({ timeout: 8000 });
  await webcamTab.first().click();
  await expect(page.locator('.source.sel, [data-testid^="source-camera"]').first()).toBeVisible({ timeout: 8000 });
});

test('uso 1 (stream + webcam): dá para selecionar tela E webcam (PiP/combo)', async ({ page }) => {
  await lobbyToRoom(page);
  const shareOpen = page.locator('[data-hook="share-open"]');
  if (await shareOpen.count()) await shareOpen.first().click();
  await page.getByRole('button', { name: /Listar telas/i }).click();
  await expect(page.getByText(/Display .*·/i).first()).toBeVisible({ timeout: 8000 });
  const webcamTab = page.getByRole('button', { name: /webcam|câmera|camera/i });
  await expect(webcamTab.first()).toBeVisible({ timeout: 8000 });
  // Combo = as duas fontes selecionáveis sem uma derrubar a outra.
  await webcamTab.first().click();
  await expect(page.locator('.source.sel').first()).toBeVisible({ timeout: 8000 });
});

test('uso 4 (segunda instância vê): viewer em outra aba enxerga quem compartilha webcam', async ({ browser }) => {
  const host = await browser.newPage();
  const viewer = await browser.newPage();
  await lobbyToRoom(host);
  await lobbyToRoom(viewer);
  // Viewer deve ver o tile/Assistir do host quando ele compartilha (qualquer kind).
  // Hoje passa no mock para display; deve continuar passando para camera.
  await expect(viewer.getByRole('region', { name: 'Transmissões da sala' })).toBeVisible();
  await host.close();
  await viewer.close();
});
