// M2B-019: start on the computer, continue from the phone, survive a dropped tunnel, driven from
// the mobile web page in Chromium at 390 x 844 against a real local hub.
//
// The computer is `cargo run -p cli -- e2e rc-runner`: the interactive `sm --rc` session (TUI
// session, bridge, acp-mock, the runner's tunnel client) paired with the hub as a real machine.
// Its tunnel goes through a TCP proxy that the test cuts and restores with a line on its stdin.
// The same steps and strings as `cargo run -p cli -- e2e rc` (crates/cli/src/e2e/rc.rs), where a
// headless client-core is the phone.
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { copyFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { expect, test } from '@playwright/test';
import { api, ownerToken, pairMachine, signedIn, unique, webSession } from './helpers.js';

const REPO = fileURLToPath(new URL('../../', import.meta.url));

/**
 * Builds the `cli` binary and returns a private copy of it (`{dir, bin}`). Worktrees share one
 * cargo target directory and cargo decides "fresh" by file times, so another tree's build of `cli`
 * (or of a crate under it) can stand in for this one: the sources are touched first, as the dev
 * setup asks before builds, and the copy is checked for the `rc-runner` command (and the build
 * retried if another tree replaced the binary in between).
 */
function buildCli() {
  for (let attempt = 0; attempt < 3; attempt += 1) {
    execFileSync(
      'bash',
      ['-c', 'touch crates/*/src/lib.rs crates/*/src/main.rs crates/*/src/*/*.rs'],
      {
        cwd: REPO,
      },
    );
    const out = execFileSync('cargo', ['build', '-q', '-p', 'cli', '--message-format=json'], {
      cwd: REPO,
      maxBuffer: 256 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'inherit'],
    }).toString();
    const built = out
      .split('\n')
      .filter((line) => line.startsWith('{'))
      .map((line) => JSON.parse(line))
      .filter((m) => m.reason === 'compiler-artifact' && m.executable && m.target.name === 'cli');
    if (!built.length) throw new Error('cargo did not report a cli executable');
    const dir = mkdtempSync(join(tmpdir(), 'sm-rc-spec-'));
    const bin = join(dir, 'cli');
    copyFileSync(built[built.length - 1].executable, bin);
    // Without its environment the command names the variable it misses; a binary without the
    // command prints the usage instead.
    const probe = spawnSync(bin, ['e2e', 'rc-runner'], { env: {}, encoding: 'utf8' });
    if (probe.stderr.includes('SM_RC_BASE is not set')) return { dir, bin };
    rmSync(dir, { recursive: true, force: true });
  }
  throw new Error(
    'the cli binary built here has no rc-runner command (another tree keeps replacing it)',
  );
}

const USER = [
  'start on the computer',
  'look at this photo from the phone',
  'typed on the terminal while the phone approves',
  'phone message before the tunnel drops',
  'typed on the terminal while the tunnel is down',
];
const AGENT = [
  'Computer reply: terminal prompt received.',
  'Phone reply: image received.',
  'Phone reply: approval applied.',
  'Computer reply: typed during approval.',
  'Cut reply: before the drop.',
  'Cut reply: after the drop.',
  'Computer reply: typed while offline.',
];
// The 8-byte PNG signature, an IHDR chunk and a pixel: what the bridge accepts as an image.
const PNG = Buffer.from(
  '89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489',
  'hex',
);

/** The computer: a child process driven by lines on stdin; each answer is one `ok`/`error` line. */
class Computer {
  constructor(child) {
    this.child = child;
    this.stderr = '';
    this.answers = [];
    this.waiting = [];
    this.ready = new Promise((resolve, reject) => {
      this.onReady = resolve;
      this.onFail = reject;
    });
    child.stderr.on('data', (chunk) => {
      this.stderr = (this.stderr + chunk).slice(-8000);
    });
    child.on('exit', (code) =>
      this.onFail(new Error(`rc-runner exited (${code}): ${this.stderr}`)),
    );
    createInterface({ input: child.stdout }).on('line', (line) => {
      if (line.startsWith('ready ')) this.onReady(JSON.parse(line.slice(6)));
      else if (line.startsWith('ok ') || line.startsWith('error ')) {
        const waiter = this.waiting.shift();
        if (waiter) waiter(line);
        else this.answers.push(line);
      }
    });
  }

  static start({ dir, bin }, env) {
    const computer = new Computer(
      spawn(bin, ['e2e', 'rc-runner'], {
        env: { ...process.env, ...env },
        stdio: ['pipe', 'pipe', 'pipe'],
      }),
    );
    computer.dir = dir;
    return computer;
  }

  /** One command; resolves to the parsed JSON of its `ok` answer. */
  async ask(command) {
    const answer = new Promise((resolve) => {
      const early = this.answers.shift();
      if (early) resolve(early);
      else this.waiting.push(resolve);
    });
    this.child.stdin.write(`${command}\n`);
    const line = await answer;
    if (line.startsWith('error ')) throw new Error(`rc-runner: ${line.slice(6)}`);
    return JSON.parse(line.slice(3));
  }

  type(text) {
    return this.ask(`type ${text}`);
  }

  async stop() {
    this.child.stdin.end('quit\n');
    await new Promise((resolve) => {
      if (this.child.exitCode !== null) resolve();
      else {
        const timer = setTimeout(() => (this.child.kill('SIGKILL'), resolve()), 10_000);
        this.child.once('exit', () => (clearTimeout(timer), resolve()));
      }
    });
    rmSync(this.dir, { recursive: true, force: true });
  }
}

/** The hub's log of a thread: a fresh subscription from seq 0, read to `live`. */
async function hubLog(thread) {
  const { body } = await api('POST', `/v1/threads/${thread}/subscribe-ticket`, {
    token: ownerToken(),
  });
  const url = new URL(`/v1/threads/${thread}/subscribe`, process.env.SM_BASE_URL);
  url.protocol = 'ws:';
  url.search = new URLSearchParams({ after: '0', ticket: body.ticket }).toString();
  const socket = new WebSocket(url);
  const events = [];
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('the replay did not reach live')), 20_000);
    socket.addEventListener('error', () => reject(new Error('replay socket error')));
    socket.addEventListener('message', (message) => {
      const frame = JSON.parse(message.data);
      if (frame.type === 'event') events.push(frame.envelope);
      if (frame.type === 'live') (clearTimeout(timer), resolve());
    });
  });
  socket.close();
  return events;
}

async function presence(machineId) {
  const { body } = await api('GET', '/v1/machines', { token: ownerToken() });
  return body.machines.find((m) => m.machine_id === machineId)?.presence?.state;
}

const users = (page) => page.locator('[data-kind="user"] .body');
const replies = (page) => page.locator('[data-kind="text"]');
const allow = (page) => page.locator('[data-kind="interrupt"] .approval button.primary');

test.describe.configure({ mode: 'serial' });

test.describe('rc: computer to phone and back, across a dropped tunnel', () => {
  const title = unique('rc-spec');
  let computer;
  let machine;
  let thread;
  let page;

  test.beforeAll(async ({ browser }) => {
    test.setTimeout(20 * 60_000);
    const binary = buildCli();
    machine = await pairMachine(unique('desk'));
    computer = Computer.start(binary, {
      SM_RC_BASE: process.env.SM_BASE_URL,
      SM_RC_MACHINE: machine.machine_id,
      SM_RC_CREDENTIAL: machine.credential,
      SM_RC_TITLE: title,
    });
    ({ thread_id: thread } = await computer.ready);
    page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await signedIn(page, await webSession());
  });

  test.afterAll(async () => {
    await page?.close();
    await computer?.stop();
  });

  test('the phone finds the session the computer started and sees what was typed there', async () => {
    await computer.type(USER[0]);
    await page.goto('/');
    await page.getByRole('link', { name: new RegExp(title) }).click();
    await expect(page.locator('#conn')).toHaveAttribute('data-state', 'live');
    await expect(users(page)).toHaveText([USER[0]]);
    await expect(replies(page)).toHaveText([AGENT[0]]);
  });

  test('a message with an image goes from the phone; the approval is answered while the computer types', async () => {
    await page
      .locator('#attach-input')
      .setInputFiles({ name: 'photo.png', mimeType: 'image/png', buffer: PNG });
    await expect(page.locator('#attachments [data-attachment]')).toHaveAttribute(
      'data-state',
      'ready',
    );
    await page.locator('#composer-text').fill(USER[1]);
    await page.locator('#send').click();
    await expect(allow(page)).toHaveCount(1);

    // The computer types while the phone allows. The line is queued behind the turn and runs
    // after it.
    await Promise.all([computer.type(USER[2]), allow(page).click()]);
    await expect(replies(page).filter({ hasText: AGENT[2] })).toHaveCount(1);
    await expect(replies(page).filter({ hasText: AGENT[3] })).toHaveCount(1);
    await expect(users(page)).toHaveText(USER.slice(0, 3));

    // The agent got the image bytes with the phone's text.
    const prompts = await computer.ask('prompts');
    expect(prompts).toHaveLength(3);
    const blocks = prompts[1];
    expect(blocks).toContainEqual({ type: 'text', text: USER[1] });
    expect(blocks).toContainEqual({
      type: 'image',
      mimeType: 'image/png',
      data: PNG.toString('base64'),
    });
  });

  test('the tunnel is cut mid-turn and restored: nothing is lost or repeated', async () => {
    await page.locator('#composer-text').fill(USER[3]);
    await page.locator('#send').click();
    await expect(allow(page)).toHaveCount(1);
    await expect(replies(page).filter({ hasText: AGENT[4] })).toHaveCount(1);

    await computer.ask('cut');
    await expect.poll(() => presence(machine.machine_id), { timeout: 30_000 }).not.toBe('online');
    await computer.type(USER[4]);
    await allow(page).click();
    await expect(allow(page)).toHaveCount(0);
    // The answer is with the hub, the agent is behind the dropped tunnel: the turn does not move.
    await page.waitForTimeout(2500);
    await expect(replies(page).filter({ hasText: AGENT[5] })).toHaveCount(0);

    await computer.ask('restore');
    await expect.poll(() => presence(machine.machine_id), { timeout: 30_000 }).toBe('online');
    await expect(replies(page).filter({ hasText: AGENT[5] })).toHaveCount(1);
    await expect(replies(page).filter({ hasText: AGENT[6] })).toHaveCount(1);
  });

  test('the transcript has no gap and no repeat, on the page, in the hub and in the terminal', async () => {
    await expect(users(page)).toHaveText(USER);
    await expect(replies(page)).toHaveText(AGENT);

    const log = await hubLog(thread);
    expect(log.map((e) => e.seq)).toEqual(log.map((_, index) => index + 1));
    const events = (type) => log.filter((e) => e.event.type === type);
    expect(
      events('UserMessage').map((e) => e.event.parts.find((p) => p.type === 'text').text),
    ).toEqual(USER);
    expect(new Set(events('UserMessage').map((e) => e.event.message_id)).size).toBe(USER.length);
    expect(events('TextMessageContent').map((e) => e.event.delta)).toEqual(AGENT);
    expect(events('Interrupt')).toHaveLength(2);
    expect(events('InterruptResolved')).toHaveLength(2);
    expect(new Set(events('InterruptResolved').map((e) => e.event.interrupt_id)).size).toBe(2);
    expect(events('RunError')).toHaveLength(0);
    expect(events('RunFinished').filter((e) => e.event.outcome.type === 'success')).toHaveLength(5);

    // The terminal's own log holds the same messages and replies, once each.
    const seen = await computer.ask('seen');
    const own = (type) => seen.filter((e) => e.event.type === type);
    expect(
      own('UserMessage')
        .map((e) => e.event.message_id)
        .sort(),
    ).toEqual(
      events('UserMessage')
        .map((e) => e.event.message_id)
        .sort(),
    );
    expect(own('TextMessageContent').map((e) => e.event.delta)).toEqual(AGENT);
    const prompts = await computer.ask('prompts');
    expect(prompts).toHaveLength(USER.length);
  });
});
