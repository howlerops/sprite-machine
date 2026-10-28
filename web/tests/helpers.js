// Test helpers: the hub's HTTP API from Node, and the dev-fixture routes (DEV_FIXTURES=1).

import { createHash, randomBytes } from 'node:crypto';

const base = () => process.env.SM_BASE_URL;
export const ownerToken = () => process.env.SM_OWNER_TOKEN;

let counter = 0;
export const unique = (prefix) => `${prefix}-${Date.now().toString(36)}-${counter++}`;

/** `{status, body}` of one API call. */
export async function api(method, path, { token, body, idempotencyKey } = {}) {
  const headers = {};
  if (idempotencyKey) headers['idempotency-key'] = idempotencyKey;
  if (token) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const reply = await fetch(`${base()}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await reply.text();
  let parsed = null;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = text;
  }
  return { status: reply.status, body: parsed };
}

async function ok(call, status) {
  const reply = await call;
  if (reply.status !== status) {
    throw new Error(`expected ${status}, got ${reply.status}: ${JSON.stringify(reply.body)}`);
  }
  return reply.body;
}

/** A machine paired through the device flow, approved by the owner: `{machine_id, credential}`. */
export async function pairMachine(name = unique('laptop')) {
  // Starting a pairing is rate limited per IP; wait it out rather than fail.
  let start;
  for (let attempt = 0; attempt < 12; attempt += 1) {
    start = await api('POST', '/v1/devices/code', { body: {} });
    if (start.status !== 429) break;
    await new Promise((resolve) => setTimeout(resolve, 5000));
  }
  start = await ok(Promise.resolve(start), 200);
  await ok(
    api('POST', '/v1/devices/approve', {
      token: ownerToken(),
      body: { user_code: start.user_code, name },
    }),
    200,
  );
  const granted = await ok(
    api('POST', '/v1/devices/token', { body: { device_code: start.device_code } }),
    200,
  );
  return { machine_id: granted.machine_id, credential: granted.credential, name };
}

/** A one-time sign-in link (`/pair?code=...`) from a paired machine. */
export async function mintLink(machine) {
  const minted = await ok(
    api('POST', '/v1/devices/signin-links', { token: machine.credential, body: {} }),
    201,
  );
  const url = new URL(minted.url);
  return `${url.pathname}${url.search}`;
}

/** A web session for the owner's user, redeemed from a fresh link (what the page would store). */
let minter = null;
export async function webSession() {
  minter ??= pairMachine(unique('minter'));
  const link = await mintLink(await minter);
  const code = new URL(link, 'http://x').searchParams.get('code');
  return ok(api('POST', '/v1/auth/links/redeem', { body: { code } }), 201);
}

/** Dev fixture: a machine's presence (`online`, `asleep`, `offline`). */
export function setPresence(machineId, state) {
  return ok(
    api('POST', `/v1/dev/fixtures/machines/${machineId}/presence`, {
      token: ownerToken(),
      body: { state },
    }),
    200,
  );
}

/** Dev fixture: a thread on a machine (`status`: idle, running, needs_you, done). */
export function addThread(machineId, title, status = 'idle', threadId, { init = false } = {}) {
  return ok(
    api('POST', '/v1/dev/fixtures/threads', {
      token: ownerToken(),
      body: { machine_id: machineId, title, status, thread_id: threadId, init },
    }),
    200,
  );
}

/** Dev fixture: a thread hosted in the owner's org (a real Thread DO), ready for events. */
export function addHostedThread(machineId, title = 'Transcript fixture') {
  return addThread(machineId, title, 'idle', unique('th'), { init: true });
}

/** Appends one envelope through `POST /v1/dev/threads/{id}/events` (DEV_FIXTURES=1); the hub stamps `seq`. */
export function appendEvent(threadId, envelope) {
  return ok(
    api('POST', `/v1/dev/threads/${threadId}/events`, {
      token: ownerToken(),
      body: { ...envelope, thread_id: threadId, seq: 0 },
    }),
    200,
  );
}

/** A prompt from the owner; while a task runs it is queued (client-api §5.3). */
export function sendPrompt(threadId, text) {
  return ok(
    api('POST', `/v1/threads/${threadId}/prompts`, {
      token: ownerToken(),
      body: { task_id: unique('tk'), parts: [{ type: 'text', text }] },
      idempotencyKey: unique('idk'),
    }),
    202,
  );
}

/** Opens the list in `page` already signed in as `session`, as if confirmed earlier. */
export async function signedIn(page, session) {
  await page.goto('/');
  await page.evaluate((value) => localStorage.setItem('sm.session', value), JSON.stringify(session));
  await page.goto('/');
}

// ---- GitHub sign-in against the mock GitHub (WEB-006) ----------------------------------------

/** A made-up GitHub account for the mock: lowercase login and a numeric id. */
export function githubAccount(prefix = 'gh') {
  return { login: unique(prefix), id: 1_000_000 + Math.floor(Math.random() * 900_000_000) };
}

/** The code the mock accepts for `account` (the browser's authorize step is played by the test). */
export const githubCode = (account) => `ghcode.${account.login}.${account.id}`;

/** The owner invites a GitHub login (`role` viewer, member or admin). */
export function invite(login, role = 'member') {
  return ok(
    api('POST', '/v1/invites', { token: ownerToken(), body: { github_login: login, role } }),
    201,
  );
}

/**
 * The whole PKCE flow without a browser, as the desktop app would run it: invites the account,
 * then authorize -> (mock GitHub) -> callback -> token. Resolves to the session
 * `{token, expires_at, user, org, role}`.
 */
export async function githubSession(account, role = 'member') {
  await invite(account.login, role);
  const verifier = randomBytes(32).toString('base64url');
  const challenge = createHash('sha256').update(verifier).digest('base64url');
  const state = randomBytes(16).toString('base64url');
  const redirectUri = `${base()}/auth/github/callback`;
  const query = new URLSearchParams({
    redirect_uri: redirectUri,
    code_challenge: challenge,
    code_challenge_method: 'S256',
    state,
  });
  const authorize = await fetch(`${base()}/v1/auth/github/authorize?${query}`, { redirect: 'manual' });
  if (authorize.status !== 302) throw new Error(`authorize answered ${authorize.status}`);
  const hubState = new URL(authorize.headers.get('location')).searchParams.get('state');
  const callback = await fetch(
    `${base()}/auth/github/callback?${new URLSearchParams({ code: githubCode(account), state: hubState })}`,
    { redirect: 'manual' },
  );
  if (callback.status !== 302) throw new Error(`callback answered ${callback.status}`);
  const back = new URL(callback.headers.get('location'));
  const returned = new URLSearchParams(back.hash.split('?')[1] || '');
  if (returned.get('state') !== state) throw new Error('the hub did not echo the state');
  return ok(
    api('POST', '/v1/auth/github/token', {
      body: { code: returned.get('code'), code_verifier: verifier, redirect_uri: redirectUri },
    }),
    201,
  );
}
