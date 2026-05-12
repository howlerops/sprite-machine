import { defineConfig, devices } from '@playwright/test';

/**
 * Playwright config for the HowlerForge web app smoke tests.
 *
 * Targets the deployed app at https://sprite-machine-cp.fly.dev.
 * Override via BASE_URL env var for local dev:
 *   BASE_URL=http://localhost:19006 pnpm e2e
 *
 * Install the Chromium binary once with:
 *   pnpm dlx playwright install chromium
 */
export default defineConfig({
  testDir: './e2e',
  /* Run tests sequentially — the suite is small and auth state is shared. */
  fullyParallel: false,
  /* Keep CI clean: don't leave an accidental .only in the tree. */
  forbidOnly: !!process.env['CI'],
  /* Single retry on CI to absorb transient network blips; zero locally. */
  retries: process.env['CI'] ? 1 : 0,
  /* One worker is enough for this sequential, state-sharing suite. */
  workers: 1,
  reporter: [
    ['list'],
    /* HTML report is written to playwright-report/ — open with:
       pnpm dlx playwright show-report apps/mobile/playwright-report */
    ['html', { outputFolder: 'playwright-report', open: 'never' }],
  ],
  use: {
    baseURL: process.env['BASE_URL'] ?? 'https://sprite-machine-cp.fly.dev',
    /* Capture a trace on first retry so failures are easy to diagnose. */
    trace: 'on-first-retry',
    /* Screenshots on failure. */
    screenshot: 'only-on-failure',
    /* Give the deployed app a generous navigation timeout. */
    navigationTimeout: 20_000,
    actionTimeout: 10_000,
  },
  projects: [
    {
      name: 'chromium',
      use: {
        ...devices['Desktop Chrome'],
        /* Headless by default; use `pnpm e2e:headed` for visual debugging. */
        headless: true,
      },
    },
  ],
  /* Test artefacts (traces, screenshots) land here. */
  outputDir: 'test-results',
});
