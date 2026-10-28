// WEB-002: the transcript in Chromium at 390 x 844, against a real local hub. Every example
// envelope of crates/protocol/schemas/turn-event/examples is appended through the dev-fixture
// route (POST /v1/dev/threads/{id}/events, DEV_FIXTURES=1) into a real Thread DO; the page gets
// it over the subscription WebSocket (ticket minted with the session token) and must render it.
import { readFileSync, readdirSync } from 'node:fs';
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
const names = readdirSync(EXAMPLES)
  .filter((file) => file.endsWith('.json'))
  .map((file) => file.slice(0, -'.json'.length))
  .sort();
const example = (name, patch = {}) => ({
  ...JSON.parse(readFileSync(new URL(`${name}.json`, EXAMPLES), 'utf8')),
  ...patch,
});
const ids = (task, run) => ({ task_id: task, run_id: run });

/** A StateSnapshot / Activity the examples rely on (snapshot before delta, rule 8). */
const extra = (event, task = null, run = null) => ({
  v: 1,
  seq: 0,
  thread_id: 'x',
  task_id: task,
  run_id: run,
  ts: '2026-10-27T22:00:00.000Z',
  event,
});

/**
 * The examples as one valid log (turn-event.md §5): task tk_1 with run_1 (paused on an
 * interrupt) and run_2 (resumed, finished), then task tk_2 whose run_3 errors. `name: null`
 * entries are not examples, only what an example needs before it.
 */
function scenario() {
  const t1 = ids('tk_1', 'run_1');
  const t1b = ids('tk_1', 'run_2');
  const t2 = ids('tk_2', 'run_3');
  const of = (name, patch) => ({ name, envelope: example(name, patch) });
  const other = (event, task, run) => ({ name: null, envelope: extra(event, task, run) });
  return [
    of('UserMessage'),
    of('RunStarted', t1),
    of('TextMessageStart', t1),
    of('TextMessageContent', t1),
    of('TextMessageEnd', t1),
    of('ReasoningMessageStart', t1),
    of('ReasoningMessageContent', t1),
    of('ReasoningMessageEnd', t1),
    of('ToolCallStart', t1),
    of('ToolCallArgs', t1),
    of('Activity', t1),
    other(
      {
        type: 'Activity',
        activity_id: 'term_1',
        activity_type: 'terminal',
        mode: 'snapshot',
        content: { terminal_id: 'term_1', command: 'cargo test' },
      },
      'tk_1',
      'run_1',
    ),
    of('Activity.terminal', t1),
    of('Activity.plan', t1),
    of('Activity.policy', t1),
    of('ToolCallResult', t1),
    of('ToolCallEnd', t1),
    of('StateSnapshot'),
    other({
      type: 'StateSnapshot',
      scope: 'usage',
      snapshot: { context_used: 1000, context_size: 200000, cost: { amount: 0, currency: 'USD' } },
    }),
    of('StateDelta'),
    of('SubagentStarted', t1),
    of('StepStarted', t1),
    of('Checkpoint', t1),
    of('Raw', t1),
    of('Presence'),
    of('Interrupt', t1),
    of('RunFinished.interrupt', t1),
    of('InterruptResolved'),
    of('RunStarted.resume', t1b),
    of('SubagentFinished', t1b),
    of('StepFinished', t1b),
    of('RunFinished', t1b),
    of('UserMessage', {
      task_id: 'tk_2',
      event: { ...example('UserMessage').event, message_id: 'msg_u2', parts: [{ type: 'text', text: 'Second task' }] },
    }),
    of('RunStarted', t2),
    other({ type: 'SubagentStarted', subagent_run_id: 'sub_2', name: 'fixer' }, 'tk_2', 'run_3'),
    of('SubagentError', {
      ...t2,
      event: { ...example('SubagentError').event, subagent_run_id: 'sub_2' },
    }),
    of('RunError', t2),
  ];
}

/** Appends the whole scenario; resolves when the page has applied the last event. */
async function playScenario(page, thread) {
  let last = 0;
  for (const { envelope } of scenario()) {
    const reply = await appendEvent(thread, envelope);
    last = reply.stored[reply.stored.length - 1].seq;
  }
  await expect(page.locator('#thread')).toHaveAttribute('data-last-seq', String(last));
  return String(last);
}

/** A machine, a hosted thread and an open transcript for it. */
async function openThread(page, { presence = 'online' } = {}) {
  const machine = await pairMachine(unique('desk'));
  await setPresence(machine.machine_id, presence);
  const thread = await addHostedThread(machine.machine_id);
  await signedIn(page, await webSession());
  await page.goto(`/#/t/${thread.thread_id}`);
  return { machine, thread: thread.thread_id };
}

const item = (page, kind, key) => page.locator(`[data-kind="${kind}"][data-key="${key}"]`);
const lastSeq = (page) => page.locator('#thread').getAttribute('data-last-seq');

const sockets = (page) => {
  const urls = [];
  page.on('websocket', (socket) => urls.push(socket.url()));
  return urls;
};

test.describe('transcript', () => {
  test('the scenario uses every example', () => {
    const used = new Set(scenario().map((entry) => entry.name).filter(Boolean));
    expect([...used].sort()).toEqual(names);
  });

  test('every example in turn-event/examples renders', async ({ page }) => {
    const { thread } = await openThread(page);
    await expect(page.locator('#conn')).toHaveAttribute('data-state', 'live');

    await playScenario(page, thread);

    const checks = {
      UserMessage: async () => {
        const user = item(page, 'user', 'msg_u1');
        await expect(user).toContainText('Add a /healthz route');
        await expect(user).toContainText('Jacob');
        await expect(user).toContainText('screen.png');
      },
      TextMessageStart: async () => {
        await expect(item(page, 'text', 'msg_a1')).toHaveCount(1);
      },
      TextMessageContent: async () => {
        await expect(item(page, 'text', 'msg_a1')).toContainText('Adding the route now.');
      },
      TextMessageEnd: async () => {
        await expect(item(page, 'text', 'msg_a1')).toHaveAttribute('data-open', 'false');
        await expect(item(page, 'text', 'msg_a1').locator('.streaming')).toHaveCount(0);
      },
      ReasoningMessageStart: async () => {
        await expect(item(page, 'reasoning', 'rsn_1')).toHaveCount(1);
      },
      ReasoningMessageContent: async () => {
        const reasoning = item(page, 'reasoning', 'rsn_1');
        await expect(reasoning.locator('summary')).toHaveText('Reasoning');
        // Collapsed until opened.
        expect(await reasoning.evaluate((el) => el.open)).toBe(false);
        await expect(reasoning.locator('.body')).toBeHidden();
        await reasoning.locator('summary').click();
        await expect(reasoning.locator('.body')).toHaveText('The router lives in src/main.rs.');
      },
      ReasoningMessageEnd: async () => {
        await expect(item(page, 'reasoning', 'rsn_1').locator('summary')).toHaveText('Reasoning');
      },
      ToolCallStart: async () => {
        const tool = item(page, 'tool', 'tc_1');
        await expect(tool).toHaveCount(1); // one item for the whole call: upsert by id
        await expect(tool.locator('.tool-title')).toHaveText('Edit src/main.rs');
        await expect(tool).toContainText('edit');
        await expect(tool).toContainText('src/main.rs:12');
      },
      ToolCallArgs: async () => {
        const tool = item(page, 'tool', 'tc_1');
        await tool.locator('summary').click();
        await expect(tool.locator('.code-block').first()).toContainText('"old_string": "Router::new()"');
      },
      ToolCallResult: async () => {
        await expect(item(page, 'tool', 'tc_1').locator('.tool-result')).toHaveText('Edited 1 file');
      },
      ToolCallEnd: async () => {
        await expect(item(page, 'tool', 'tc_1')).toHaveAttribute('data-status', 'completed');
      },
      Activity: async () => {
        const diff = item(page, 'activity', 'tc_1:src/main.rs');
        await expect(diff).toContainText('src/main.rs');
        await expect(diff.locator('.del')).toHaveText('- let app = Router::new();');
        await expect(diff.locator('.add')).toHaveText('+ let app = Router::new().route("/healthz", get(ok));');
      },
      'Activity.plan': async () => {
        const entries = item(page, 'activity', 'plan').locator('li');
        await expect(entries).toHaveCount(2);
        await expect(entries.nth(0)).toContainText('Add route');
        await expect(entries.nth(0)).toContainText('in progress');
        await expect(entries.nth(1)).toContainText('Add test');
        await expect(entries.nth(1)).toContainText('pending');
      },
      'Activity.terminal': async () => {
        const terminal = item(page, 'activity', 'term_1');
        await expect(terminal).toContainText('cargo test');
        await expect(terminal.locator('.terminal-output')).toHaveText('test result: ok. 3 passed\n');
      },
      'Activity.policy': async () => {
        await expect(item(page, 'activity', 'tc_2:policy')).toHaveText('Policy allow: preset:read (tc_2)');
      },
      Interrupt: async () => {
        const interrupt = item(page, 'interrupt', 'int_1');
        await expect(interrupt).toContainText('Allow edit to src/main.rs?');
        await expect(interrupt.locator('.options li')).toHaveText(['Allow', 'Deny']);
      },
      InterruptResolved: async () => {
        const interrupt = item(page, 'interrupt', 'int_1');
        await expect(interrupt).toHaveAttribute('data-resolved', 'true');
        await expect(interrupt).toContainText('selected allow by Jacob');
      },
      SubagentStarted: async () => {
        await expect(item(page, 'subagent', 'sub_1')).toContainText('Subagent reviewer');
      },
      SubagentFinished: async () => {
        const sub = item(page, 'subagent', 'sub_1');
        await expect(sub).toHaveAttribute('data-status', 'finished');
        await expect(sub).toContainText('"verdict": "approve"');
      },
      SubagentError: async () => {
        const sub = item(page, 'subagent', 'sub_2');
        await expect(sub).toHaveAttribute('data-status', 'error');
        await expect(sub).toContainText('child cancelled (cancelled)');
      },
      StepStarted: async () => {
        await expect(item(page, 'step', 'implement.verify#2')).toContainText('Step verify #2');
      },
      StepFinished: async () => {
        await expect(item(page, 'step', 'implement.verify#2')).toHaveAttribute('data-status', 'success');
      },
      Checkpoint: async () => {
        const checkpoint = item(page, 'checkpoint', 'cp_7');
        await expect(checkpoint).toContainText('Checkpoint cp_7: 1 files changed');
        await expect(checkpoint).toContainText('refs/sm/checkpoints/th_1/7');
      },
      Presence: async () => {
        await expect(page.locator('[data-kind="presence"]')).toHaveText('Machine mch_laptop is asleep');
      },
      Raw: async () => {
        const raw = page.locator('[data-kind="raw"]');
        await expect(raw.locator('summary')).toHaveText('Raw child TextMessageContent');
        await raw.locator('summary').click();
        await expect(raw).toContainText('Looks good.');
      },
      RunStarted: async () => {
        const run = item(page, 'run', 'run_1');
        await expect(run).toContainText('Run started');
        await expect(item(page, 'run', 'run_3')).toContainText('Run started');
      },
      'RunStarted.resume': async () => {
        await expect(item(page, 'run', 'run_2')).toContainText('Run resumed');
      },
      'RunFinished.interrupt': async () => {
        await expect(item(page, 'run', 'run_1')).toContainText('Run paused: waiting for you');
      },
      RunFinished: async () => {
        await expect(item(page, 'run', 'run_2')).toContainText('Run finished: end_turn');
      },
      RunError: async () => {
        const run = item(page, 'run', 'run_3');
        await expect(run).toHaveAttribute('data-status', 'error');
        await expect(run).toContainText('Run error: expired');
        await expect(run).toContainText('approval expired after 10 min');
      },
      StateSnapshot: async () => {
        await expect(page.locator('#session-state')).toHaveText('Mode Default · 1 command');
      },
      StateDelta: async () => {
        await expect(page.locator('#usage')).toHaveText(
          'Context 48,211 of 200,000 tokens · 0.042 USD',
        );
      },
    };
    // A new example without a check here fails the test, so the page cannot lag behind the schema.
    expect(Object.keys(checks).sort()).toEqual(names);
    for (const name of names) {
      await test.step(name, checks[name]);
    }
    // No horizontal scroll at 390 wide.
    const widths = await page.evaluate(() => [document.documentElement.scrollWidth, innerWidth]);
    expect(widths[0]).toBeLessThanOrEqual(widths[1]);
  });

  test('text, tool calls and terminal output update in place as events stream in', async ({ page }) => {
    const { thread } = await openThread(page);
    const t = ids('tk_s', 'run_s');
    const sent = (name, patch) => appendEvent(thread, example(name, patch));
    await sent('UserMessage', { task_id: 'tk_s' });
    await sent('RunStarted', t);

    await sent('TextMessageStart', t);
    await expect(item(page, 'text', 'msg_a1').locator('.streaming')).toHaveText('writing…');
    await sent('TextMessageContent', t);
    await sent('TextMessageContent', {
      ...t,
      event: { type: 'TextMessageContent', message_id: 'msg_a1', delta: ' More.' },
    });
    await expect(item(page, 'text', 'msg_a1').locator('.body')).toHaveText('Adding the route now. More.');
    await sent('TextMessageEnd', t);
    await expect(item(page, 'text', 'msg_a1').locator('.streaming')).toHaveCount(0);

    await sent('ToolCallStart', t);
    await expect(item(page, 'tool', 'tc_1')).toHaveAttribute('data-status', 'pending');
    await sent('ToolCallEnd', t);
    await expect(item(page, 'tool', 'tc_1')).toHaveAttribute('data-status', 'completed');
    await expect(item(page, 'tool', 'tc_1')).toHaveCount(1);

    const terminal = (mode, output) =>
      appendEvent(thread, {
        ...example('Activity.terminal', t),
        event: {
          type: 'Activity',
          activity_id: 'term_9',
          activity_type: 'terminal',
          mode,
          content: { terminal_id: 'term_9', output },
        },
      });
    await terminal('snapshot', 'one\n');
    await terminal('append', 'two\n');
    await expect(item(page, 'activity', 'term_9').locator('.terminal-output')).toHaveText('one\ntwo\n');
    await expect(item(page, 'activity', 'term_9')).toHaveCount(1);
  });

  test('an interrupt shows as needing you, then resolved; the run shows it is paused', async ({ page }) => {
    const { thread } = await openThread(page);
    const t = ids('tk_i', 'run_i');
    await appendEvent(thread, example('UserMessage', { task_id: 'tk_i' }));
    await appendEvent(thread, example('RunStarted', t));
    await appendEvent(thread, example('Interrupt', t));
    await appendEvent(thread, example('RunFinished.interrupt', t));
    const interrupt = item(page, 'interrupt', 'int_1');
    await expect(interrupt).toHaveAttribute('data-resolved', 'false');
    await expect(interrupt.locator('.options li').first()).toHaveText('Allow');
    await expect(interrupt).toContainText('needs you');
    await expect(item(page, 'run', 'run_i')).toContainText('Run paused: waiting for you');
    await appendEvent(thread, example('InterruptResolved', { task_id: 'tk_i' }));
    await expect(interrupt).toHaveAttribute('data-resolved', 'true');
    await expect(interrupt).toContainText('resolved');
  });

  test('event text is shown as text, never as HTML', async ({ page }) => {
    const { thread } = await openThread(page);
    const t = ids('tk_x', 'run_x');
    const hostile = '<img src=x onerror="window.__pwned=1"><script>window.__pwned=1</script>';
    await appendEvent(thread, example('UserMessage', {
      task_id: 'tk_x',
      event: { ...example('UserMessage').event, message_id: 'msg_x', parts: [{ type: 'text', text: hostile }] },
    }));
    await appendEvent(thread, example('RunStarted', t));
    await appendEvent(thread, example('ToolCallStart', {
      ...t,
      event: { ...example('ToolCallStart').event, tool_call_id: 'tc_x', title: hostile },
    }));
    await expect(item(page, 'user', 'msg_x').locator('.body')).toHaveText(hostile);
    await expect(item(page, 'tool', 'tc_x').locator('.tool-title')).toHaveText(hostile);
    expect(await page.locator('#transcript img, #transcript script').count()).toBe(0);
    expect(await page.evaluate(() => window.__pwned)).toBeUndefined();
  });
});

test.describe('subscription', () => {
  test('a ticket is minted for each open; the session token is never in a URL', async ({ page }) => {
    const urls = sockets(page);
    const tickets = [];
    page.on('request', (request) => {
      if (request.url().endsWith('/subscribe-ticket')) tickets.push(request.headers().authorization);
    });
    const { thread } = await openThread(page);
    await expect(page.locator('#conn')).toHaveAttribute('data-state', 'live');
    const token = (await page.evaluate(() => JSON.parse(localStorage.getItem('sm.session')))).token;

    expect(tickets).toHaveLength(1);
    expect(tickets[0]).toBe(`Bearer ${token}`);
    expect(urls).toHaveLength(1);
    const first = new URL(urls[0]);
    expect(first.pathname).toBe(`/v1/threads/${thread}/subscribe`);
    expect(first.searchParams.get('after')).toBe('0');
    expect(first.searchParams.get('ticket')).toMatch(/^sst_/);
    expect(urls[0]).not.toContain(token);
    expect(page.url()).not.toContain(token);

    // The ticket is single use: reusing the same one on a new socket is refused.
    const reuse = await api('GET', `/v1/threads/${thread}/subscribe?ticket=${first.searchParams.get('ticket')}`);
    expect(reuse.status).toBe(401);
  });

  test('after a reload it subscribes with ?after=seq and shows nothing twice', async ({ page }) => {
    const urls = sockets(page);
    const { thread } = await openThread(page);
    const seq = await playScenario(page, thread);
    expect(Number(seq)).toBeGreaterThan(30);
    const count = await page.locator('#transcript > *').count();

    await page.reload();
    await expect(page.locator('#conn')).toHaveAttribute('data-state', 'live');
    // Shown from what was kept, before and after the replay, with no repeats.
    await expect(page.locator('#transcript > *')).toHaveCount(count);
    expect(await lastSeq(page)).toBe(seq);
    await expect(item(page, 'tool', 'tc_1')).toHaveCount(1);
    await expect(item(page, 'text', 'msg_a1')).toHaveCount(1);
    expect(urls).toHaveLength(2);
    expect(new URL(urls[1]).searchParams.get('after')).toBe(seq);
    // A fresh ticket for the second socket.
    expect(new URL(urls[1]).searchParams.get('ticket')).not.toBe(new URL(urls[0]).searchParams.get('ticket'));
    await expect(page.locator('#usage')).toHaveText('Context 48,211 of 200,000 tokens · 0.042 USD');

    // Events after the reload arrive once.
    await appendEvent(thread, example('UserMessage', {
      task_id: 'tk_3',
      event: { ...example('UserMessage').event, message_id: 'msg_u3', parts: [{ type: 'text', text: 'after the reload' }] },
    }));
    await expect(item(page, 'user', 'msg_u3')).toHaveCount(1);
    await expect(page.locator('#transcript > *')).toHaveCount(count + 1);
  });

  test('a dropped socket reopens with a new ticket and the last seq, without repeats', async ({ page }) => {
    const live = [];
    const tickets = [];
    await page.routeWebSocket(/\/subscribe\?/, (ws) => {
      const server = ws.connectToServer();
      live.push({ ws, url: ws.url() });
      server.onMessage((message) => ws.send(message));
      ws.onMessage((message) => server.send(message));
    });
    page.on('request', (request) => {
      if (request.url().endsWith('/subscribe-ticket')) tickets.push(request.url());
    });
    const { thread } = await openThread(page);
    await appendEvent(thread, example('UserMessage', { task_id: 'tk_r' }));
    await appendEvent(thread, example('RunStarted', ids('tk_r', 'run_r')));
    await appendEvent(thread, example('TextMessageStart', ids('tk_r', 'run_r')));
    await expect(item(page, 'text', 'msg_a1')).toHaveCount(1);
    const seq = await lastSeq(page);

    await live[0].ws.close({ code: 1006 });
    await expect.poll(() => live.length, { timeout: 15_000 }).toBe(2);
    expect(tickets).toHaveLength(2);
    const second = new URL(live[1].url);
    expect(second.searchParams.get('after')).toBe(seq);
    expect(second.searchParams.get('ticket')).not.toBe(new URL(live[0].url).searchParams.get('ticket'));
    await expect(page.locator('#conn')).toHaveAttribute('data-state', 'live');
    // Replay from the open run's start repeats nothing.
    await expect(item(page, 'user', 'msg_u1')).toHaveCount(1);
    await expect(item(page, 'run', 'run_r')).toHaveCount(1);
    await expect(item(page, 'text', 'msg_a1')).toHaveCount(1);
    expect(await lastSeq(page)).toBe(seq);
    // And it is live again.
    await appendEvent(thread, example('TextMessageContent', ids('tk_r', 'run_r')));
    await expect(item(page, 'text', 'msg_a1')).toContainText('Adding the route now.');
  });

  test('while the machine is offline it says it is waiting for it, with the queued items', async ({ page }) => {
    const { machine, thread } = await openThread(page, { presence: 'online' });
    await appendEvent(thread, example('UserMessage', { task_id: 'tk_q' }));
    await appendEvent(thread, example('RunStarted', ids('tk_q', 'run_q')));
    await sendPrompt(thread, 'queued: write the changelog');
    await sendPrompt(thread, 'queued: bump the version');

    await expect(page.locator('#waiting')).toBeHidden();
    await setPresence(machine.machine_id, 'offline');
    await expect(page.locator('#waiting-text')).toHaveText(`Waiting for ${machine.name} (offline)`);
    await expect(page.locator('#queued li')).toHaveText([
      'queued: write the changelog',
      'queued: bump the version',
    ]);
    // Back online: the banner goes away.
    await setPresence(machine.machine_id, 'online');
    await expect(page.locator('#waiting')).toBeHidden();
    const widths = await page.evaluate(() => [document.documentElement.scrollWidth, innerWidth]);
    expect(widths[0]).toBeLessThanOrEqual(widths[1]);
  });
});
