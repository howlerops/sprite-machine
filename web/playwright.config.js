// Browser tests of the mobile web page against a real hub: `wrangler dev --local` (Workers Static
// Assets serve /, the Worker serves /v1) with DEV_FIXTURES=1 and random per-run secrets.
// Chromium comes from /opt/pw-browsers (never `playwright install`). Run: npm ci && npx playwright test

import { execFileSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync } from 'node:fs';
import { defineConfig } from '@playwright/test';

// The config is loaded again in every worker: pick once, in the first process, and let the
// environment carry the values (and the secrets) to the rest.
const freePort = () =>
  Number(
    execFileSync(process.execPath, [
      '-e',
      'const s=require("net").createServer().listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})',
    ])
      .toString()
      .trim(),
  );
process.env.SM_PORT ||= String(freePort());
process.env.BOOTSTRAP_OWNER_TOKEN ||= randomBytes(24).toString('hex');
process.env.ATTACHMENT_SIGNING_KEY ||= randomBytes(32).toString('hex');
const baseURL = `http://127.0.0.1:${process.env.SM_PORT}`;
process.env.SM_BASE_URL = baseURL;

const chromium = '/opt/pw-browsers/chromium';

export default defineConfig({
  testDir: './tests',
  testMatch: '*.spec.js',
  globalSetup: './tests/helpers-setup.js',
  // One hub, one org: the tests share it and run one after another.
  workers: 1,
  fullyParallel: false,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: [['list']],
  use: {
    baseURL,
    viewport: { width: 390, height: 844 },
    trace: 'retain-on-failure',
    launchOptions: existsSync(chromium) ? { executablePath: chromium } : {},
  },
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
  webServer: {
    command: 'bash tests/helpers-hub.sh',
    url: `${baseURL}/healthz`,
    reuseExistingServer: false,
    // A cold wasm build of the hub is the long part.
    timeout: 20 * 60_000,
    gracefulShutdown: { signal: 'SIGTERM', timeout: 15_000 },
    stdout: 'ignore',
    stderr: 'pipe',
  },
});
