// Test helpers: the hub's HTTP API from Node, and the dev-fixture routes (DEV_FIXTURES=1).

const base = () => process.env.SM_BASE_URL;
export const ownerToken = () => process.env.SM_OWNER_TOKEN;

let counter = 0;
export const unique = (prefix) => `${prefix}-${Date.now().toString(36)}-${counter++}`;

/** `{status, body}` of one API call. */
export async function api(method, path, { token, body } = {}) {
  const headers = {};
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
export function addThread(machineId, title, status = 'idle', threadId) {
  return ok(
    api('POST', '/v1/dev/fixtures/threads', {
      token: ownerToken(),
      body: { machine_id: machineId, title, status, thread_id: threadId },
    }),
    200,
  );
}

/** Opens the list in `page` already signed in as `session`, as if confirmed earlier. */
export async function signedIn(page, session) {
  await page.goto('/');
  await page.evaluate((value) => localStorage.setItem('sm.session', value), JSON.stringify(session));
  await page.goto('/');
}
