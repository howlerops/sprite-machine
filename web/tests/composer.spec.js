// WEB-003: the composer, attachments, Allow/Deny and Stop in Chromium at 390 x 844, against a
// real local hub. Prompts go through the outbox with idempotency keys; Playwright toggles offline.
import { readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';
import {
  addHostedThread,
  api,
  appendEvent,
  ownerToken,
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

const item = (page, kind, key) => page.locator(`[data-kind="${kind}"][data-key="${key}"]`);
const users = (page) => page.locator('[data-kind="user"]');
const owner = (method, path, body) =>
  api(method, path, { token: ownerToken(), body, idempotencyKey: unique('idk') });

/** A hosted thread on an online machine, opened in `page`. */
async function openThread(page) {
  const machine = await pairMachine(unique('desk'));
  await setPresence(machine.machine_id, 'online');
  const thread = (await addHostedThread(machine.machine_id)).thread_id;
  await signedIn(page, await webSession());
  await page.goto(`/#/t/${thread}`);
  await expect(page.locator('#conn')).toHaveAttribute('data-state', 'live');
  return thread;
}

/** The same, with a task running: `{thread, task}`. */
async function openRunning(page) {
  const thread = await openThread(page);
  const started = await sendPrompt(thread, 'first prompt');
  await expect(users(page)).toHaveCount(1);
  return { thread, task: started.task_id };
}

const interruptEvent = (task, id) => ({
  ...example('Interrupt', { task_id: task, run_id: 'run_a' }),
  event: {
    ...example('Interrupt').event,
    interrupt_id: id,
    expires_at: new Date(Date.now() + 600_000).toISOString(),
  },
});

/** A running task with an open approval `id`. */
async function openApproval(page, id = 'int_a') {
  const { thread, task } = await openRunning(page);
  await appendEvent(thread, example('RunStarted', { task_id: task, run_id: 'run_a' }));
  await appendEvent(thread, interruptEvent(task, id));
  await expect(item(page, 'interrupt', id)).toHaveAttribute('data-resolved', 'false');
  return { thread, task };
}

const pending = async (thread) => (await owner('GET', `/v1/threads/${thread}/pending`)).body.pending;

test.describe('composer', () => {
  test('a prompt is sent; while a task runs it shows as a removable pending-task chip', async ({ page }) => {
    const { thread } = await openRunning(page);
    await page.locator('#composer-text').fill('second prompt');
    await page.locator('#send').click();

    const chip = page.locator('#pending-chips [data-task]');
    await expect(chip).toHaveCount(1);
    await expect(chip).toContainText('second prompt');
    expect((await pending(thread)).map((task) => task.preview)).toEqual(['second prompt']);
    await expect(page.locator('#composer-text')).toHaveValue('');
    // It is queued, not started: only the first prompt is in the transcript.
    await expect(users(page)).toHaveCount(1);

    const taskId = await chip.getAttribute('data-task');
    const [removal] = await Promise.all([
      page.waitForRequest((request) => request.url().includes('/pending/')),
      chip.getByRole('button', { name: /Remove queued message: second prompt/ }).click(),
    ]);
    // Removing a queued task is DELETE .../pending/{task_id} (client-api §5.3), not a cancel.
    expect(removal.method()).toBe('DELETE');
    expect(new URL(removal.url()).pathname).toBe(`/v1/threads/${thread}/pending/${taskId}`);
    await expect(page.locator('#pending-chips [data-task]')).toHaveCount(0);
    expect(await pending(thread)).toEqual([]);
  });

  test('an idle thread starts the prompt, which appears in the transcript', async ({ page }) => {
    await openThread(page);
    await page.locator('#composer-text').fill('hello <b>there</b>');
    await page.locator('#send').click();
    await expect(users(page)).toHaveCount(1);
    await expect(users(page).locator('.body')).toHaveText('hello <b>there</b>');
    await expect(page.locator('#pending-chips [data-task]')).toHaveCount(0);
  });

  test('an empty message is not sent', async ({ page }) => {
    await openThread(page);
    await page.locator('#send').click();
    await expect(page.locator('#composer-note')).toHaveText('Write a message or attach a file.');
    await expect(users(page)).toHaveCount(0);
  });
});

test.describe('attachments', () => {
  test('a picked file is uploaded to the signed URL and sent as a file part', async ({ page }) => {
    await openThread(page);
    const bytes = Buffer.from('89504e470d0a1a0a00112233445566778899', 'hex');
    const created = page.waitForResponse(
      (reply) => reply.url().endsWith('/v1/attachments') && reply.request().method() === 'POST',
    );
    await page.locator('#attach-input').setInputFiles({ name: 'shot.png', mimeType: 'image/png', buffer: bytes });
    const part = (await (await created).json()).part;
    await expect(page.locator('#attachments [data-attachment]')).toHaveAttribute('data-state', 'ready');

    await page.locator('#composer-text').fill('see this');
    await page.locator('#send').click();
    await expect(users(page)).toHaveCount(1);
    await expect(users(page)).toContainText('see this');
    await expect(users(page).locator('.files')).toContainText('shot.png');
    await expect(page.locator('#attachments [data-attachment]')).toHaveCount(0);

    // The bytes are in the store behind the part's URL, with the type they were sent with.
    expect(part).toMatchObject({ type: 'file', name: 'shot.png', mime_type: 'image/png', size: bytes.length });
    const stored = await fetch(part.url);
    expect(stored.status).toBe(200);
    expect(stored.headers.get('content-type')).toContain('image/png');
    expect(Buffer.from(await stored.arrayBuffer()).equals(bytes)).toBe(true);
  });

  test('the camera input takes a photo and sends it with no text', async ({ page }) => {
    await openThread(page);
    const camera = page.locator('#camera-input');
    await expect(camera).toHaveAttribute('capture', 'environment');
    await expect(camera).toHaveAttribute('accept', 'image/*');
    await camera.setInputFiles({ name: 'photo.jpg', mimeType: 'image/jpeg', buffer: Buffer.from('ffd8ffe000104a464946', 'hex') });
    await expect(page.locator('#attachments [data-attachment]')).toHaveAttribute('data-state', 'ready');
    await page.locator('#send').click();
    await expect(users(page).locator('.files')).toContainText('photo.jpg');
  });

  test('a file over 20 MB is refused before any upload; an attachment can be removed', async ({ page }) => {
    await openThread(page);
    const requests = [];
    page.on('request', (request) => {
      if (request.url().includes('/v1/attachments')) requests.push(request.url());
    });
    await page.locator('#attach-input').setInputFiles({
      name: 'big.bin',
      mimeType: 'application/octet-stream',
      buffer: Buffer.alloc(20 * 1024 * 1024 + 1),
    });
    const chip = page.locator('#attachments [data-attachment]');
    await expect(chip).toHaveAttribute('data-state', 'error');
    await expect(chip).toContainText('over 20 MB');
    expect(requests).toEqual([]);
    await chip.getByRole('button', { name: /Remove attachment: big.bin/ }).click();
    await expect(chip).toHaveCount(0);
  });
});

test.describe('Allow, Deny and Stop', () => {
  test('Allow answers the approval and the transcript shows it resolved', async ({ page }) => {
    await openApproval(page);
    const interrupt = item(page, 'interrupt', 'int_a');
    await expect(interrupt.locator('.approval button')).toHaveText(['Allow', 'Deny']);
    await interrupt.getByRole('button', { name: 'Allow' }).click();
    await expect(interrupt).toHaveAttribute('data-resolved', 'true');
    await expect(interrupt).toContainText('selected allow');
    await expect(interrupt.locator('.approval')).toHaveCount(0);
  });

  test('Deny answers with the reject option', async ({ page }) => {
    await openApproval(page, 'int_d');
    const interrupt = item(page, 'interrupt', 'int_d');
    await interrupt.getByRole('button', { name: 'Deny' }).click();
    await expect(interrupt).toHaveAttribute('data-resolved', 'true');
    await expect(interrupt).toContainText('selected reject');
  });

  test('a 409 says it was already resolved, and by whom', async ({ page }) => {
    const { thread } = await openApproval(page, 'int_c');
    let winner = null;
    // Someone else answers just before this page's answer reaches the hub.
    await page.route('**/interrupts/int_c', async (route) => {
      const first = await owner('POST', `/v1/threads/${thread}/interrupts/int_c`, {
        outcome: { type: 'selected', option_id: 'allow' },
      });
      expect(first.status).toBe(200);
      winner = first.body.resolved_by;
      await route.continue();
    });
    const interrupt = item(page, 'interrupt', 'int_c');
    await interrupt.getByRole('button', { name: 'Deny' }).click();
    await expect.poll(() => winner).not.toBeNull();
    const who = winner.name || winner.id;
    await expect(interrupt.locator('.approval-note')).toHaveText(`This was already resolved by ${who}.`);
    // The first answer stands.
    await expect(interrupt).toContainText('selected allow');
  });

  test('Stop cancels the run', async ({ page }) => {
    const { thread, task } = await openRunning(page);
    const stop = page.locator('#stop');
    await expect(stop).toBeVisible();
    await stop.click();
    await expect(page.locator('[data-kind="run"][data-status="error"]')).toContainText('cancelled');
    await expect(stop).toBeHidden();
    const again = await owner('POST', `/v1/threads/${thread}/cancel`, { task_id: task });
    expect(again.body.status).toBe('already_finished');
  });

  test('Stop is not offered when nothing runs', async ({ page }) => {
    await openThread(page);
    await expect(page.locator('#stop')).toBeHidden();
  });
});

test.describe('outbox', () => {
  test('a prompt written offline waits, survives a reload, and is sent once when back', async ({ page, context }) => {
    const thread = await openThread(page);
    await context.setOffline(true);
    await page.locator('#composer-text').fill('typed offline');
    await page.locator('#send').click();

    const waiting = page.locator('#pending-chips [data-outbox]');
    await expect(waiting).toHaveCount(1);
    await expect(waiting).toContainText('typed offline');
    await expect(waiting).toContainText('Waiting to send');

    // The queue is in storage; it holds the prompt but not the session token.
    const stored = await page.evaluate(() => ({
      outbox: localStorage.getItem('sm.outbox'),
      token: JSON.parse(localStorage.getItem('sm.session')).token,
    }));
    expect(JSON.parse(stored.outbox)).toHaveLength(1);
    expect(stored.outbox).toContain('typed offline');
    expect(stored.outbox).not.toContain(stored.token);

    // Back online but the hub still not reachable for prompts: it stays queued across a reload.
    await page.route('**/prompts', (route) => route.abort('connectionfailed'));
    await context.setOffline(false);
    await page.reload();
    await expect(page.locator('#pending-chips [data-outbox]')).toContainText('typed offline');
    expect(await pending(thread)).toEqual([]);
    await expect(users(page)).toHaveCount(0);

    await page.unroute('**/prompts');
    await expect(users(page)).toHaveCount(1);
    await expect(users(page).locator('.body')).toHaveText('typed offline');
    await expect(page.locator('#pending-chips [data-outbox]')).toHaveCount(0);
    expect(await page.evaluate(() => localStorage.getItem('sm.outbox'))).toBe('[]');
  });

  test('a send whose answer was lost is retried with the same key and happens once', async ({ page }) => {
    const thread = await openThread(page);
    const keys = [];
    let lose = true;
    await page.route('**/prompts', async (route) => {
      keys.push(route.request().headers()['idempotency-key']);
      if (lose) {
        lose = false;
        await route.fetch(); // the hub got it ...
        await route.abort('connectionfailed'); // ... but the page never heard back
      } else {
        await route.continue();
      }
    });
    await page.locator('#composer-text').fill('only once');
    await page.locator('#send').click();

    await expect(page.locator('#pending-chips [data-outbox]')).toHaveCount(0, { timeout: 30_000 });
    await expect(users(page)).toHaveCount(1);
    expect(keys.length).toBeGreaterThanOrEqual(2);
    expect(keys[0]).toMatch(/^idk_[0-9a-f]{32}$/);
    expect(new Set(keys).size).toBe(1);
    expect(await pending(thread)).toEqual([]);
    // Nothing was started twice.
    await page.waitForTimeout(500);
    await expect(users(page)).toHaveCount(1);
  });

  test('every POST carries an Idempotency-Key', async ({ page }) => {
    await openRunning(page);
    const missing = [];
    page.on('request', (request) => {
      const url = new URL(request.url());
      const keyed = !['GET', 'HEAD'].includes(request.method()) && url.pathname !== '/v1/machines';
      const isTicket = url.pathname.endsWith('/subscribe-ticket') || url.pathname.endsWith('/upload');
      if (keyed && !isTicket && !request.headers()['idempotency-key']) missing.push(request.url());
    });
    await page.locator('#composer-text').fill('queued one');
    await page.locator('#send').click();
    const chip = page.locator('#pending-chips [data-task]');
    await expect(chip).toHaveCount(1);
    await chip.getByRole('button', { name: /Remove queued message/ }).click();
    await expect(chip).toHaveCount(0);
    await page.locator('#stop').click();
    await expect(page.locator('#stop')).toBeHidden();
    expect(missing).toEqual([]);
  });
});
