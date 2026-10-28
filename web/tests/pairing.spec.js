// WEB-001: pairing and the session list, in Chromium at 390 x 844 against a real local hub.
import { expect, test } from '@playwright/test';
import {
  addThread,
  api,
  mintLink,
  ownerToken,
  pairMachine,
  setPresence,
  signedIn,
  unique,
  webSession,
} from './helpers.js';

const stored = (page) =>
  page.evaluate(() => JSON.parse(localStorage.getItem('sm.session') || 'null'));

test.describe('hub serves the page', () => {
  test('/, /pair and /pair/device are the page; /v1 and /t/<id> stay with the hub', async ({
    page,
    request,
  }) => {
    for (const path of ['/', '/pair', '/pair/device']) {
      const reply = await request.get(path, { headers: { 'sec-fetch-mode': 'navigate' } });
      expect(reply.status(), path).toBe(200);
      expect(await reply.text(), path).toContain('<title>Sprite Machine</title>');
    }
    expect((await request.get('/src/app.js')).headers()['content-type']).toContain('javascript');
    // A browser navigation to an API path is not answered with the page.
    const nope = await request.get('/v1/nope', { headers: { 'sec-fetch-mode': 'navigate' } });
    expect(nope.status()).toBe(404);
    expect((await nope.json()).error).toBe('not_found');
    const unknown = await request.get('/no-such-route', {
      headers: { 'sec-fetch-mode': 'navigate' },
    });
    expect(unknown.status()).toBe(404);
    expect((await unknown.json()).error).toBe('not_found');
    // Not served as assets: the tests and the package files.
    expect(await (await request.get('/package.json')).text()).not.toContain('@playwright/test');
    // The deep link /t/<id> is the hub's 302 to /#/t/<id>.
    await page.goto('/t/th_deep');
    await expect(page).toHaveURL(/\/#\/t\/th_deep$/);
  });
});

test.describe('sign in', () => {
  test('a runner QR link redeems, shows user and org, and stores the token only on confirm', async ({
    page,
  }) => {
    const link = await mintLink(await pairMachine());
    const code = new URL(link, 'http://x').searchParams.get('code');
    await page.goto(link);

    await expect(page.getByRole('heading', { name: 'Confirm sign-in' })).toBeVisible();
    await expect(page.locator('#who-user')).toHaveText('Web Owner');
    await expect(page.locator('#who-org')).toHaveText('Web Test Org');
    // Not stored yet, and the one-time code is no longer in the address bar.
    expect(await stored(page)).toBeNull();
    expect(page.url()).not.toContain(code);

    await page.getByRole('button', { name: 'Confirm and sign in' }).click();
    await expect(page.getByRole('heading', { name: 'Sessions' })).toBeVisible();
    expect(new URL(page.url()).pathname).toBe('/');
    const session = await stored(page);
    expect(session.token).toMatch(/^smt_/);
    expect((await api('GET', '/v1/threads', { token: session.token })).status).toBe(200);

    // The code was single use (signed out first, or the page would just show the list).
    await page.evaluate(() => localStorage.clear());
    await page.goto(link);
    await expect(page.getByRole('alert')).toContainText('used or expired');
    await expect(page.getByRole('button', { name: 'Sign in with a device code' })).toBeVisible();
  });

  test('declining the confirmation stores nothing and revokes the session', async ({ page }) => {
    const link = await mintLink(await pairMachine());
    await page.goto(link);
    const [revoke] = await Promise.all([
      page.waitForRequest((req) => req.method() === 'DELETE' && req.url().includes('/sessions/')),
      page.getByRole('button', { name: 'Cancel' }).click(),
    ]);
    const token = revoke.headers().authorization.replace('Bearer ', '');
    await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
    expect(await stored(page)).toBeNull();
    await expect
      .poll(async () => (await api('GET', '/v1/threads', { token })).status)
      .toBe(401);
  });

  test('a device code shown by the page, approved through the API, signs in after confirm', async ({
    page,
  }) => {
    await page.goto('/');
    await page.getByRole('button', { name: 'Sign in with a device code' }).click();
    await expect(page).toHaveURL(/\/pair\/device$/);
    const shown = page.locator('#user-code');
    await expect(shown).toHaveText(/\S+/);
    const userCode = (await shown.innerText()).trim();

    const approved = await api('POST', '/v1/auth/device/approve', {
      token: ownerToken(),
      body: { user_code: userCode },
    });
    expect(approved.status, JSON.stringify(approved.body)).toBe(200);

    // The page polls every `interval` seconds (5), so the confirmation takes a few seconds.
    await expect(page.getByRole('heading', { name: 'Confirm sign-in' })).toBeVisible({
      timeout: 30_000,
    });
    await expect(page.locator('#who-user')).toHaveText('Web Owner');
    await expect(page.locator('#who-org')).toHaveText('Web Test Org');
    expect(await stored(page)).toBeNull();
    await page.getByRole('button', { name: 'Confirm and sign in' }).click();
    await expect(page.getByRole('heading', { name: 'Sessions' })).toBeVisible();
    expect((await stored(page)).token).toMatch(/^smt_/);
  });

  test('polling honours the interval and slow_down', async ({ page }) => {
    const polls = [];
    await page.route('**/v1/auth/device/code', (route) =>
      route.fulfill({
        status: 200,
        json: {
          device_code: 'dc_mock',
          user_code: 'MOCK-CODE',
          verification_uri: 'http://x/pair/device',
          interval: 1,
          expires_in: 600,
        },
      }),
    );
    await page.route('**/v1/auth/device/token', (route) => {
      polls.push(Date.now());
      if (polls.length === 1) {
        return route.fulfill({ status: 400, json: { error: 'slow_down', interval: 3 } });
      }
      return route.fulfill({ status: 400, json: { error: 'authorization_pending' } });
    });
    await page.goto('/pair/device');
    await expect(page.locator('#user-code')).toHaveText('MOCK-CODE');
    await expect.poll(() => polls.length, { timeout: 15_000 }).toBeGreaterThanOrEqual(3);
    const first = polls[1] - polls[0];
    expect(polls[0]).toBeGreaterThan(0);
    // The first poll waited the interval (1 s); after slow_down the wait is the new 3 s.
    expect(first).toBeGreaterThanOrEqual(2900);
  });

  test('a 401 from any call returns to the sign-in screen', async ({ page }) => {
    await signedIn(page, await webSession());
    await expect(page.getByRole('heading', { name: 'Sessions' })).toBeVisible();
    const token = (await stored(page)).token;
    expect((await api('DELETE', '/v1/auth/sessions/current', { token })).status).toBe(200);
    // The next poll (5 s) gets a 401.
    await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
    await expect(page.getByRole('alert')).toContainText('session ended');
    expect(await stored(page)).toBeNull();
  });

  test('sign out clears the token and revokes the session', async ({ page }) => {
    await signedIn(page, await webSession());
    const token = (await stored(page)).token;
    await page.getByRole('button', { name: 'Sign out' }).click();
    await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
    expect(await stored(page)).toBeNull();
    await expect
      .poll(async () => (await api('GET', '/v1/threads', { token })).status)
      .toBe(401);
    await page.reload();
    await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
  });
});

test.describe('session list', () => {
  test('machines with presence and their threads, tapping a thread opens #/t/<id>', async ({
    page,
  }) => {
    const on = await pairMachine(unique('desk'));
    const sleepy = await pairMachine(unique('mac'));
    const off = await pairMachine(unique('box'));
    await setPresence(on.machine_id, 'online');
    await setPresence(sleepy.machine_id, 'asleep');
    const refactor = await addThread(on.machine_id, 'Refactor the parser', 'running');
    await addThread(on.machine_id, 'Fix flaky test', 'needs_you');
    await addThread(sleepy.machine_id, 'Write release notes', 'done');

    await signedIn(page, await webSession());
    const section = (m) => page.locator('section.machine', { has: page.getByText(m.name) });

    await expect(section(on).locator('.chip.presence-online')).toHaveText('Online');
    await expect(section(sleepy).locator('.chip.presence-asleep')).toHaveText('Asleep');
    await expect(section(off).locator('.chip.presence-offline')).toHaveText('Offline');
    await expect(section(on).getByText('Refactor the parser')).toBeVisible();
    await expect(section(on).locator('.chip.status-running')).toHaveText('Running');
    await expect(section(on).locator('.chip.status-needs_you')).toHaveText('Needs you');
    await expect(section(sleepy).getByText('Write release notes')).toBeVisible();
    await expect(section(sleepy).getByText('Refactor the parser')).toHaveCount(0);
    await expect(section(off).getByText('No sessions')).toBeVisible();
    await expect(section(on).locator('time').first()).toHaveText(/just now|\dm ago/);

    // Layout at 390 wide: nothing scrolls sideways.
    const widths = await page.evaluate(() => [document.documentElement.scrollWidth, innerWidth]);
    expect(widths[0]).toBeLessThanOrEqual(widths[1]);

    await section(on).getByRole('link', { name: /Refactor the parser/ }).click();
    await expect(page).toHaveURL(new RegExp(`#/t/${refactor.thread_id}$`));
    await expect(page.getByRole('heading', { name: 'Session' })).toBeVisible();
    await page.getByRole('link', { name: 'Back' }).click();
    await expect(page.getByRole('heading', { name: 'Sessions' })).toBeVisible();
  });

  test('a presence change and a new thread show up without a reload', async ({ page }) => {
    const machine = await pairMachine(unique('live'));
    await setPresence(machine.machine_id, 'online');
    await signedIn(page, await webSession());
    const section = page.locator('section.machine', { has: page.getByText(machine.name) });
    await expect(section.locator('.chip')).toHaveText('Online');

    await page.evaluate(() => {
      window.__sameDocument = true;
    });
    await setPresence(machine.machine_id, 'asleep');
    await addThread(machine.machine_id, 'Appears live', 'running');
    // Polled every 5 s.
    await expect(section.locator('.chip.presence-asleep')).toHaveText('Asleep');
    await expect(section.getByText('Appears live')).toBeVisible();

    await setPresence(machine.machine_id, 'offline');
    await expect(section.locator('.chip.presence-offline')).toHaveText('Offline');
    expect(await page.evaluate(() => window.__sameDocument)).toBe(true);
  });
});
