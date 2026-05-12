/**
 * Smoke spec for the HowlerForge web app.
 *
 * Covers:
 *   1. Login via Fly personal-access token
 *   2. Chats dashboard — empty state OR at least one chat row
 *   3. Run detail — title or status pill visible within 5 s
 *   4. (Optional) Chat — send a message and verify it lands in the timeline
 *
 * Prerequisites:
 *   export FLY_TOKEN=$(flyctl auth token)
 *   pnpm e2e
 *
 * Install browsers once with:
 *   pnpm dlx playwright install chromium
 */

import { test, expect } from '@playwright/test';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Grab the token or skip cleanly. */
function requireToken(): string {
  const token = process.env['FLY_TOKEN'];
  if (!token) {
    // Using test.skip() inside a helper throws; return empty and let the
    // caller call test.skip(). The guard is checked at the top of each test.
    return '';
  }
  return token;
}

// ---------------------------------------------------------------------------
// Auth + dashboard
// ---------------------------------------------------------------------------

test('login → runs dashboard', async ({ page }) => {
  const token = requireToken();
  test.skip(!token, 'FLY_TOKEN not set — skipping');

  // Collect console errors so we can fail at the end if any surfaced.
  const consoleErrors: string[] = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      consoleErrors.push(msg.text());
    }
  });

  // 1. Navigate to the login page.
  await page.goto('/app/login');

  // The login page should render the token input.
  // The InputField renders as a plain <input> in react-native-web.
  const tokenInput = page.locator('input[placeholder="fo1_xxx..."]');
  await expect(tokenInput).toBeVisible({ timeout: 10_000 });

  // 2. Paste the token and submit.
  await tokenInput.fill(token);

  const continueBtn = page.getByRole('button', { name: /continue/i });
  await expect(continueBtn).toBeEnabled();
  await continueBtn.click();

  // 3. Expect navigation away from /login — expo-router replaces to '/'.
  //    The (tabs) group maps '/' → '/app/(tabs)/index' on the deployed app.
  await expect(page).toHaveURL(/\/app(?:\/\(tabs\))?(?:\/index)?\/?(?:\?.*)?$/, {
    timeout: 15_000,
  });

  // 4. The app shell/runs area should render. Desktop can show the welcome
  //    panel plus an empty sidebar; mobile can show the empty-state directly.
  await expect(page.locator('body')).toContainText(/Welcome|No chats yet|Running/i, {
    timeout: 10_000,
  });

  // Fail the test if the browser emitted any console errors during the flow.
  expect(consoleErrors, `Console errors detected:\n${consoleErrors.join('\n')}`).toHaveLength(0);
});

// ---------------------------------------------------------------------------
// Run detail
// ---------------------------------------------------------------------------

test('run detail renders title or status pill', async ({ page }) => {
  const token = requireToken();
  test.skip(!token, 'FLY_TOKEN not set — skipping');

  const consoleErrors: string[] = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error') consoleErrors.push(msg.text());
  });

  // Log in first.
  await page.goto('/app/login');
  const tokenInput = page.locator('input[placeholder="fo1_xxx..."]');
  await expect(tokenInput).toBeVisible({ timeout: 10_000 });
  await tokenInput.fill(token);
  await page.getByRole('button', { name: /continue/i }).click();
  await expect(page).toHaveURL(/\/app(?:\/\(tabs\))?(?:\/index)?\/?(?:\?.*)?$/, {
    timeout: 15_000,
  });

  // Check whether any real sidebar run rows exist. Avoid matching filter chips
  // such as "Running" in the filter bar.
  await expect(page.locator('body')).toContainText(/Welcome|No chats yet|Running/i, {
    timeout: 10_000,
  });
  const runCard = page.locator('[class*="border-b"][class*="sidebar-border"]').filter({
    has: page.locator('text=/running|starting|completed|failed|cancelled|awaiting/i'),
  });

  const hasRuns = await runCard
    .first()
    .isVisible()
    .catch(() => false);
  if (!hasRuns) {
    test.skip(true, 'No chats present; skipping chat-detail check');
    return;
  }

  // Click the first real run row.
  await runCard.first().click();
  await expect(page).toHaveURL(/\/app\/run\/[A-Za-z0-9_-]+\/?(?:\?.*)?$/, { timeout: 10_000 });

  // The run detail header always shows a status pill.
  const statusPill = page
    .locator('[class*="rounded-full"]')
    .filter({
      has: page.locator('text=/running|starting|completed|failed|cancelled|awaiting|idle/i'),
    })
    .first();

  await expect(statusPill).toBeVisible({ timeout: 5_000 });

  expect(consoleErrors, `Console errors detected:\n${consoleErrors.join('\n')}`).toHaveLength(0);
});

// ---------------------------------------------------------------------------
// Chat (optional — only runs when E2E_CHAT=1)
// ---------------------------------------------------------------------------

test('chat message lands in timeline', async ({ page }) => {
  const token = requireToken();
  test.skip(!token, 'FLY_TOKEN not set — skipping');
  test.skip(process.env['E2E_CHAT'] !== '1', 'E2E_CHAT not set — skipping chat test');

  const consoleErrors: string[] = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error') consoleErrors.push(msg.text());
  });

  // Log in.
  await page.goto('/app/login');
  const tokenInput = page.locator('input[placeholder="fo1_xxx..."]');
  await expect(tokenInput).toBeVisible({ timeout: 10_000 });
  await tokenInput.fill(token);
  await page.getByRole('button', { name: /continue/i }).click();
  await expect(page).toHaveURL(/\/app(?:\/\(tabs\))?(?:\/index)?\/?(?:\?.*)?$/, {
    timeout: 15_000,
  });

  // Navigate to the first run that is in 'awaiting_input' state — only those
  // runs accept messages.
  const awaitingCard = page
    .locator('[class*="rounded"]')
    .filter({ has: page.locator('text=/awaiting/i') })
    .first();

  const hasAwaiting = await awaitingCard.isVisible({ timeout: 5_000 }).catch(() => false);
  test.skip(!hasAwaiting, 'No awaiting_input run present — skipping chat test');

  await awaitingCard.click();

  // Wait for the run detail to load.
  await expect(
    page.locator('[class*="rounded-full"]').filter({
      has: page.locator('text=/awaiting/i'),
    }),
  ).toBeVisible({ timeout: 5_000 });

  // Compose and send a message.
  const probe = `e2e-probe-${Date.now()}`;
  const composer = page.locator('input[placeholder="Send a message…"]');
  await expect(composer).toBeVisible({ timeout: 5_000 });
  await composer.fill(probe);
  await page.getByRole('button', { name: /^send$/i }).click();

  // The message should appear in the timeline (as a user bubble).
  await expect(page.locator(`text=${probe}`)).toBeVisible({ timeout: 10_000 });

  expect(consoleErrors, `Console errors detected:\n${consoleErrors.join('\n')}`).toHaveLength(0);
});
