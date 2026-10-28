// WEB-006: "Sign in with GitHub" in Chromium against a real local hub and a mock GitHub
// (helpers-ghmock.mjs, started by helpers-hub.sh). The mock GitHub's authorize page is played
// by the test: it answers the browser's redirect with the code of the chosen account.
import { createHash } from 'node:crypto';
import { expect, test } from '@playwright/test';
import { api, githubAccount, githubCode, invite } from './helpers.js';

const VERIFIER_KEY = 'sm.github.verifier';
const storedSession = (page) =>
  page.evaluate(() => JSON.parse(localStorage.getItem('sm.session') || 'null'));
const pending = (page) =>
  page.evaluate((key) => sessionStorage.getItem(key), VERIFIER_KEY);

/** Records every Storage.setItem (which storage, key, value) and every request of the page. */
async function watch(page) {
  const writes = [];
  const requests = [];
  await page.exposeFunction('__smWrite', (kind, key, value) => writes.push({ kind, key, value }));
  await page.addInitScript(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      window.__smWrite(this === window.sessionStorage ? 'session' : 'local', key, String(value));
      return original.call(this, key, value);
    };
  });
  // A redirect chain (authorize -> GitHub -> callback -> page) is reported by its last request:
  // walk back so every hop is recorded.
  page.on('request', (request) => {
    requests.push({ method: request.method(), url: request.url(), body: request.postData() || '' });
    const from = request.redirectedFrom();
    if (from) requests.push({ method: from.method(), url: from.url(), body: '' });
  });
  return { writes, requests };
}

/** Chooses who signs in at the mock GitHub: its authorize page reads this cookie (ports are ignored). */
async function signInAs(page, account) {
  await page.context().addCookies([
    { name: 'gh_account', value: `${account.login}.${account.id}`, domain: '127.0.0.1', path: '/' },
  ]);
}

test.describe('sign in with GitHub', () => {
  test('ends with a working session; the verifier stays in sessionStorage and the code leaves the address', async ({
    page,
  }) => {
    const account = githubAccount('web-gh');
    await invite(account.login, 'member');
    const { writes, requests } = await watch(page);
    await signInAs(page, account);

    await page.goto('/');
    await page.getByRole('button', { name: 'Sign in with GitHub' }).click();
    await expect(page.getByRole('heading', { name: 'Sessions' })).toBeVisible();

    // A working session for the invited account, with the invite's role.
    const session = await storedSession(page);
    expect(session.role).toBe('member');
    expect((await api('GET', '/v1/members', { token: session.token })).status).toBe(200);

    // The verifier was written to sessionStorage only, was the S256 pre-image of the challenge the
    // hub got, and is gone now.
    const verifierWrites = writes.filter((w) => w.key === VERIFIER_KEY);
    expect(verifierWrites).toHaveLength(1);
    expect(verifierWrites[0].kind).toBe('session');
    const verifier = verifierWrites[0].value;
    expect(verifier).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(writes.filter((w) => w.kind === 'local').some((w) => w.value.includes(verifier))).toBe(false);
    const authorize = requests.find((r) => r.url.includes('/v1/auth/github/authorize'));
    const challenge = new URL(authorize.url).searchParams.get('code_challenge');
    expect(challenge).toBe(createHash('sha256').update(verifier).digest('base64url'));
    expect(new URL(authorize.url).searchParams.get('code_challenge_method')).toBe('S256');
    expect(await pending(page)).toBeNull();
    expect(await page.evaluate(() => sessionStorage.getItem('sm.github.state'))).toBeNull();

    // On the wire the verifier is only in the body of the one token POST: never in a URL.
    expect(requests.filter((r) => r.url.includes(verifier))).toHaveLength(0);
    const tokenCalls = requests.filter((r) => r.url.endsWith('/v1/auth/github/token'));
    expect(tokenCalls).toHaveLength(1);
    expect(tokenCalls[0].method).toBe('POST');
    expect(JSON.parse(tokenCalls[0].body).code_verifier).toBe(verifier);
    expect(requests.filter((r) => r !== tokenCalls[0] && r.body.includes(verifier))).toHaveLength(0);

    // The code was read from the fragment and removed from the history entry.
    expect(page.url()).not.toMatch(/code=|state=|auth\/github/);
    await page.goBack();
    expect(page.url()).not.toMatch(/code=|auth\/github/);
  });

  test('an account with no invite is turned away: no session, nothing left in storage', async ({
    page,
  }) => {
    const account = githubAccount('web-nogh');
    await watch(page);
    await signInAs(page, account);

    await page.goto('/');
    await page.getByRole('button', { name: 'Sign in with GitHub' }).click();
    await expect(page.getByRole('alert')).toContainText('no access');
    await expect(page.getByRole('button', { name: 'Sign in with GitHub' })).toBeVisible();
    expect(await storedSession(page)).toBeNull();
    expect(await pending(page)).toBeNull();
    expect(page.url()).not.toMatch(/code=|auth\/github/);
  });

  test('a return with the wrong state, or a second return, trades nothing', async ({ page }) => {
    const { requests } = await watch(page);
    // No account chosen at the mock GitHub: its authorize page refuses, and the tab stays there
    // with the pending verifier still in the hub origin's sessionStorage.
    await page.goto('/');
    await page.getByRole('button', { name: 'Sign in with GitHub' }).click();
    await expect(page.getByText('bad authorize request')).toBeVisible();
    await page.goto('/');
    expect(await pending(page)).not.toBeNull();
    const ownState = new URL(requests.find((r) => r.url.includes('/v1/auth/github/authorize')).url)
      .searchParams.get('state');

    // Someone else's state: refused, and the pending verifier is burned. Each return is a full
    // page load, as it is when the hub redirects the browser back.
    await page.goto('about:blank');
    await page.goto('/#/auth/github?code=forged-code&state=not-the-state');
    await expect(page.getByRole('alert')).toContainText('did not complete');
    expect(await pending(page)).toBeNull();

    // Even the right state cannot be used afterwards.
    await page.goto('about:blank');
    await page.goto(`/#/auth/github?code=${githubCode(githubAccount())}&state=${ownState}`);
    await expect(page.getByRole('alert')).toContainText('did not complete');
    expect(await storedSession(page)).toBeNull();
    expect(requests.filter((r) => r.url.endsWith('/v1/auth/github/token'))).toHaveLength(0);
    expect(page.url()).not.toMatch(/code=|auth\/github/);
  });
});
