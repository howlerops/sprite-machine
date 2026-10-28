// WEB-004: the installable PWA and Web Push in Chromium at 390 x 844, against a real local hub.
// A push goes the whole way except the browser's own push service, which the sandbox lacks: the
// page's subscribe() is stubbed to hand out a subscription whose endpoint is a fake push service
// run by this test; the hub encrypts and signs a push to it as it would to a real one, the test
// decrypts it with the subscription's private key and gives the plaintext to the service worker
// (CDP ServiceWorker.deliverPushMessage), as the browser would.
//
// Playwright lists the service worker's own requests only with this switch (set before the
// browser starts), so the Allow / Deny calls can be inspected.
process.env.PW_EXPERIMENTAL_SERVICE_WORKER_NETWORK_EVENTS = '1';

import { createServer } from 'node:http';
import { createDecipheriv, createECDH, hkdfSync, randomBytes } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';
import {
  addHostedThread,
  api,
  appendEvent,
  pairMachine,
  sendPrompt,
  setPresence,
  signedIn,
  unique,
  webSession,
} from './helpers.js';

const EXAMPLES = new URL('../../crates/protocol/schemas/turn-event/examples/', import.meta.url);
const example = (name, patch = {}) => ({
  ...JSON.parse(readFileSync(new URL(`${name}.json`, EXAMPLES), 'utf8')),
  ...patch,
});
const SECRET_PROMPT = 'do the secret thing in secret-plans.txt';

// ---- the fake push service and the subscription's keys ----------------------------------------

/** A push service on a loopback port; each POST the hub makes is queued with its body. */
async function startPushService() {
  const requests = [];
  const waiting = [];
  const server = createServer((req, res) => {
    const chunks = [];
    req.on('data', (chunk) => chunks.push(chunk));
    req.on('end', () => {
      const got = { url: req.url, headers: req.headers, body: Buffer.concat(chunks) };
      const waiter = waiting.shift();
      if (waiter) waiter(got);
      else requests.push(got);
      res.writeHead(201).end();
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    endpoint: `http://127.0.0.1:${server.address().port}/push/${unique('sub')}`,
    next: () =>
      requests.length
        ? Promise.resolve(requests.shift())
        : new Promise((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error('no push reached the fake push service')), 30_000);
            waiting.push((got) => {
              clearTimeout(timer);
              resolve(got);
            });
          }),
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

/** The user agent's half of a push subscription. */
function browserKeys() {
  const ecdh = createECDH('prime256v1');
  ecdh.generateKeys();
  return { ecdh, auth: randomBytes(16) };
}

/** RFC 8291 (aes128gcm): the plaintext of a push body for these keys. */
function decryptPush(body, { ecdh, auth }) {
  const salt = body.subarray(0, 16);
  const idLen = body[20];
  const serverKey = body.subarray(21, 21 + idLen);
  const sealed = body.subarray(21 + idLen);
  const uaKey = ecdh.getPublicKey();
  const secret = ecdh.computeSecret(serverKey);
  const keyInfo = Buffer.concat([Buffer.from('WebPush: info\0'), uaKey, serverKey]);
  const ikm = Buffer.from(hkdfSync('sha256', secret, auth, keyInfo, 32));
  const cek = Buffer.from(hkdfSync('sha256', ikm, salt, Buffer.from('Content-Encoding: aes128gcm\0'), 16));
  const nonce = Buffer.from(hkdfSync('sha256', ikm, salt, Buffer.from('Content-Encoding: nonce\0'), 12));
  const decipher = createDecipheriv('aes-128-gcm', cek, nonce);
  decipher.setAuthTag(sealed.subarray(sealed.length - 16));
  const plain = Buffer.concat([decipher.update(sealed.subarray(0, sealed.length - 16)), decipher.final()]);
  let end = plain.length;
  while (end > 0 && plain[end - 1] === 0) end -= 1;
  return plain.subarray(0, end - 1).toString('utf8'); // drop the 0x02 delimiter
}

// ---- a signed-in page with a subscription ------------------------------------------------------

/** Opens the list signed in, with PushManager.subscribe stubbed to the fake service's endpoint. */
async function pushPage(context, page, service, keys) {
  await context.grantPermissions(['notifications'], { origin: process.env.SM_BASE_URL });
  await page.addInitScript(
    ({ endpoint, p256dh, auth }) => {
      const stub = {
        endpoint,
        toJSON: () => ({ endpoint, keys: { p256dh, auth } }),
        unsubscribe: async () => {
          window.__subscription = null;
          return true;
        },
      };
      window.__subscribeCalls = [];
      PushManager.prototype.getSubscription = async () => window.__subscription || null;
      PushManager.prototype.subscribe = async function subscribe(options) {
        window.__subscribeCalls.push({
          userVisibleOnly: options.userVisibleOnly,
          key: Array.from(new Uint8Array(options.applicationServerKey)),
        });
        window.__subscription = stub;
        return stub;
      };
    },
    {
      endpoint: service.endpoint,
      p256dh: keys.ecdh.getPublicKey().toString('base64url'),
      auth: keys.auth.toString('base64url'),
    },
  );
  const session = await webSession();
  await signedIn(page, session);
  return session;
}

/** The registered service worker (sw.js) and a CDP session to deliver pushes to it. */
async function workerOf(context, page) {
  await page.evaluate(() => navigator.serviceWorker.ready);
  const worker = context.serviceWorkers().find((w) => w.url().endsWith('/sw.js'));
  expect(worker, 'the service worker is running').toBeTruthy();
  const cdp = await context.newCDPSession(page);
  const registered = new Promise((resolve) => {
    cdp.on('ServiceWorker.workerRegistrationUpdated', ({ registrations }) => {
      const found = registrations.find((r) => r.scopeURL === `${process.env.SM_BASE_URL}/`);
      if (found) resolve(found.registrationId);
    });
  });
  await cdp.send('ServiceWorker.enable');
  const registrationId = await registered;
  return {
    worker,
    deliver: (data) =>
      cdp.send('ServiceWorker.deliverPushMessage', {
        origin: process.env.SM_BASE_URL,
        registrationId,
        data,
      }),
    notifications: () =>
      worker.evaluate(async () =>
        (await self.registration.getNotifications()).map((n) => ({
          title: n.title,
          body: n.body,
          tag: n.tag,
          data: n.data,
          actions: n.actions.map((a) => ({ action: a.action, title: a.title })),
        })),
      ),
    // A click the browser would send: a notificationclick event for the shown notification.
    click: (tag, action) =>
      worker.evaluate(
        async ({ tag, action }) => {
          const [notification] = await self.registration.getNotifications({ tag });
          self.dispatchEvent(new NotificationEvent('notificationclick', { notification, action }));
        },
        { tag, action },
      ),
  };
}

/** A hosted thread with a running task; `raise(id)` opens an approval and returns the push body. */
async function threadWithApproval() {
  const machine = await pairMachine(unique('desk'));
  await setPresence(machine.machine_id, 'online');
  const thread = (await addHostedThread(machine.machine_id)).thread_id;
  const started = await sendPrompt(thread, SECRET_PROMPT);
  const task = started.task_id;
  await appendEvent(thread, example('RunStarted', { task_id: task, run_id: 'run_a' }));
  return {
    thread,
    task,
    raise: (id) =>
      appendEvent(thread, {
        ...example('Interrupt', { task_id: task, run_id: 'run_a' }),
        event: {
          ...example('Interrupt').event,
          interrupt_id: id,
          expires_at: new Date(Date.now() + 600_000).toISOString(),
        },
      }),
  };
}

const interruptItem = (page, id) => page.locator(`[data-kind="interrupt"][data-key="${id}"]`);

// ---- tests ------------------------------------------------------------------------------------

test.describe('installable PWA', () => {
  test('the manifest is linked, valid and its icons are PNGs', async ({ page }) => {
    await page.goto('/');
    await expect(page.locator('link[rel="manifest"]')).toHaveAttribute('href', '/manifest.webmanifest');
    const manifest = await page.evaluate(() => fetch('/manifest.webmanifest').then((r) => r.json()));
    expect(manifest).toMatchObject({
      name: 'Sprite Machine',
      start_url: '/',
      scope: '/',
      display: 'standalone',
    });
    expect(manifest.theme_color).toMatch(/^#[0-9a-f]{6}$/i);
    const sizes = manifest.icons.map((icon) => icon.sizes);
    expect(sizes).toEqual(expect.arrayContaining(['192x192', '512x512']));
    expect(manifest.icons.some((icon) => icon.purpose === 'maskable')).toBe(true);
    for (const icon of manifest.icons) {
      const reply = await page.request.get(icon.src);
      expect(reply.status(), icon.src).toBe(200);
      expect(reply.headers()['content-type']).toBe('image/png');
      expect((await reply.body()).subarray(0, 8).toString('hex')).toBe('89504e470d0a1a0a');
    }
  });

  test('a service worker is registered at / and caches the app shell, never the API', async ({ context, page }) => {
    const session = await webSession();
    await signedIn(page, session);
    const state = await page.evaluate(async () => {
      const registration = await navigator.serviceWorker.ready;
      return { scope: registration.scope, script: registration.active.scriptURL, state: registration.active.state };
    });
    expect(state.scope).toBe(`${process.env.SM_BASE_URL}/`);
    expect(state.script).toBe(`${process.env.SM_BASE_URL}/sw.js`);
    expect(state.state).toBe('activated');
    expect(context.serviceWorkers().some((w) => w.url().endsWith('/sw.js'))).toBe(true);

    // The page is controlled after a reload; an API call goes through it.
    await page.reload();
    await expect.poll(() => page.evaluate(() => !!navigator.serviceWorker.controller)).toBe(true);
    await page.evaluate((token) => fetch('/v1/machines', { headers: { authorization: `Bearer ${token}` } }), session.token);
    const cached = await page.evaluate(async () => {
      const urls = [];
      for (const name of await caches.keys()) {
        for (const request of await (await caches.open(name)).keys()) urls.push(new URL(request.url).pathname);
      }
      return urls;
    });
    expect(cached).toEqual(expect.arrayContaining(['/', '/styles.css', '/src/app.js', '/src/push.js', '/manifest.webmanifest']));
    expect(cached.filter((path) => path.startsWith('/v1/'))).toEqual([]);
  });

  test('the shell opens offline from the cache', async ({ context, page }) => {
    await signedIn(page, await webSession());
    await page.evaluate(() => navigator.serviceWorker.ready);
    await page.reload();
    await expect.poll(() => page.evaluate(() => !!navigator.serviceWorker.controller)).toBe(true);
    await context.setOffline(true);
    try {
      await page.reload();
      await expect(page.locator('#signout')).toBeVisible();
    } finally {
      await context.setOffline(false);
    }
  });
});

test.describe('Web Push', () => {
  let service;
  let keys;
  let session;
  test.beforeEach(async () => {
    service = await startPushService();
    keys = browserKeys();
  });
  test.afterEach(async () => {
    // The user's subscriptions get every later event of this org: remove this one.
    if (session) await api('DELETE', '/v1/push/subscriptions', { token: session.token, body: { endpoint: service.endpoint } });
    await service.close();
  });

  /** Enables notifications on the page and raises an approval; resolves to what the worker got. */
  async function arrange(context, page, interruptId, { watch = false } = {}) {
    session = await pushPage(context, page, service, keys);
    const sw = await workerOf(context, page);
    // Permission is granted, so the page subscribes by itself on load.
    await expect(page.locator('#notify')).toHaveText('Notifications on');
    const arranged = await threadWithApproval();
    if (watch) {
      await page.goto(`/#/t/${arranged.thread}`);
      await expect(page.locator('#conn')).toHaveAttribute('data-state', 'live');
    }
    await arranged.raise(interruptId);
    const request = await service.next();
    const payload = JSON.parse(decryptPush(request.body, keys));
    return { ...arranged, ...sw, request, payload };
  }

  test('with permission the page subscribes with the hub key and registers it; a push reaches the service worker', async ({
    context,
    page,
  }) => {
    const got = await arrange(context, page, 'int_push');

    // subscribe() got the hub's VAPID key, and the hub registered the subscription (it pushed to it).
    const served = await api('GET', '/v1/push/vapid-public-key', { token: session.token });
    const calls = await page.evaluate(() => window.__subscribeCalls);
    expect(calls).toHaveLength(1);
    expect(calls[0].userVisibleOnly).toBe(true);
    expect(Buffer.from(calls[0].key).toString('base64url')).toBe(served.body.key || served.body.public_key);
    expect(got.request.headers['content-encoding']).toBe('aes128gcm');
    expect(got.request.headers.authorization).toMatch(/^vapid /);

    // Ids only: nothing the person wrote or the agent did is in the push.
    expect(got.payload).toMatchObject({
      kind: 'approval_needed',
      thread_id: got.thread,
      task_id: got.task,
      interrupt_id: 'int_push',
      title: 'Approval needed',
    });
    expect(got.payload.actions).toEqual([
      { action: 'allow', title: 'Allow', option_id: 'allow' },
      { action: 'deny', title: 'Deny', option_id: 'reject' },
    ]);
    expect(JSON.stringify(got.payload)).not.toContain('secret');

    // The worker receives the push and shows it, titled from the generic title and the thread.
    await got.deliver(JSON.stringify(got.payload));
    await expect.poll(async () => (await got.notifications()).length).toBe(1);
    const [shown] = await got.notifications();
    expect(shown.title).toBe('Approval needed');
    expect(shown.body).not.toContain('secret');
    expect(shown.tag).toBe(got.task);
    expect(shown.actions).toEqual([
      { action: 'allow', title: 'Allow' },
      { action: 'deny', title: 'Deny' },
    ]);
  });

  test('a notification click opens /t/<id>', async ({ context, page }) => {
    const got = await arrange(context, page, 'int_open');
    await got.deliver(JSON.stringify(got.payload));
    await expect.poll(async () => (await got.notifications()).length).toBe(1);
    await expect(page.locator('#bar-user')).toBeVisible();

    await got.click(got.task, '');
    await expect(page).toHaveURL(new RegExp(`/#/t/${got.thread}$`));
    await expect(page.locator('#conn')).toHaveAttribute('data-state', 'live');
    await expect.poll(async () => (await got.notifications()).length).toBe(0);
  });

  test('Allow answers the interrupt with its own option id and an Idempotency-Key', async ({ context, page }) => {
    const calls = [];
    context.on('request', (request) => {
      if (request.serviceWorker() && request.method() === 'POST' && request.url().includes('/interrupts/')) {
        calls.push({ url: request.url(), headers: request.headers(), body: request.postDataJSON() });
      }
    });
    const got = await arrange(context, page, 'int_allow', { watch: true });
    await got.deliver(JSON.stringify(got.payload));
    await expect.poll(async () => (await got.notifications()).length).toBe(1);

    await got.click(got.task, 'allow');
    await expect(interruptItem(page, 'int_allow')).toHaveAttribute('data-resolved', 'true');
    await expect(interruptItem(page, 'int_allow')).toContainText('selected allow');
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(`${process.env.SM_BASE_URL}/v1/threads/${got.thread}/interrupts/int_allow`);
    expect(calls[0].body).toEqual({ outcome: { type: 'selected', option_id: 'allow' } });
    expect(calls[0].headers['idempotency-key']).toBeTruthy();
    // The token travels in the header only.
    expect(calls[0].headers.authorization).toBe(`Bearer ${session.token}`);
    expect(calls[0].url).not.toContain(session.token);
    // A successful answer leaves the page where it was.
    expect(new URL(page.url()).hash).toBe(`#/t/${got.thread}`);
  });

  test('Deny answers with the reject option', async ({ context, page }) => {
    const got = await arrange(context, page, 'int_deny', { watch: true });
    await got.deliver(JSON.stringify(got.payload));
    await expect.poll(async () => (await got.notifications()).length).toBe(1);

    await got.click(got.task, 'deny');
    await expect(interruptItem(page, 'int_deny')).toHaveAttribute('data-resolved', 'true');
    await expect(interruptItem(page, 'int_deny')).toContainText('selected reject');
  });

  test('an answer that cannot be made opens the thread instead', async ({ context, page }) => {
    const got = await arrange(context, page, 'int_late');
    await got.deliver(JSON.stringify(got.payload));
    await expect.poll(async () => (await got.notifications()).length).toBe(1);

    // Someone else answers first: the worker's Allow gets a 409 and shows the thread.
    const first = await api('POST', `/v1/threads/${got.thread}/interrupts/int_late`, {
      token: session.token,
      idempotencyKey: unique('idk'),
      body: { outcome: { type: 'selected', option_id: 'reject' } },
    });
    expect(first.status).toBe(200);
    await got.click(got.task, 'allow');
    await expect(page).toHaveURL(new RegExp(`/#/t/${got.thread}$`));
  });

  test('without a stored session the worker still shows a generic notification', async ({ context, page }) => {
    const got = await arrange(context, page, 'int_anon');
    await page.evaluate(
      () =>
        new Promise((resolve) => {
          const open = indexedDB.open('sm-push', 1);
          open.onsuccess = () => {
            const tx = open.result.transaction('kv', 'readwrite');
            tx.objectStore('kv').delete('session');
            tx.oncomplete = () => resolve();
          };
        }),
    );
    await got.deliver(JSON.stringify(got.payload));
    await expect.poll(async () => (await got.notifications()).length).toBe(1);
    const [shown] = await got.notifications();
    expect(shown.title).toBe('Approval needed');
    expect(shown.body).toBe('');
  });
});
