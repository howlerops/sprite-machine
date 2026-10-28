// The hub client: the session token in localStorage and one fetch wrapper (client-api.md §2, §4).
// A 401 from an authenticated call clears the session and tells the app, which shows sign-in.

const KEY = 'sm.session';

// Used when storage is unavailable.
let memory = null;

/** The stored session `{token, expires_at, user, org, role}`, or null. */
function stored() {
  try {
    const session = JSON.parse(localStorage.getItem(KEY) || 'null');
    return session && typeof session.token === 'string' ? session : null;
  } catch {
    return null;
  }
}

export const currentSession = () => stored() || memory;

export function saveSession(session) {
  memory = session;
  try {
    localStorage.setItem(KEY, JSON.stringify(session));
  } catch {
    // Storage blocked: the session lives until the page closes.
  }
}

export function clearSession() {
  memory = null;
  try {
    localStorage.removeItem(KEY);
  } catch {
    // Nothing stored that we could remove.
  }
}

export class ApiError extends Error {
  constructor(status, body) {
    super((body && body.message) || `HTTP ${status}`);
    this.status = status;
    this.body = body || {};
    this.code = this.body.error || 'error';
  }
}

let onUnauthorized = () => {};
export function setUnauthorizedHandler(handler) {
  onUnauthorized = handler;
}

/**
 * `request('GET', '/v1/machines')` with the stored token; `{token}` overrides it and
 * `{auth: false}` sends none (sign-in routes). Resolves to the JSON body, rejects with ApiError.
 */
export async function request(method, path, { body, token, auth = true } = {}) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  const bearer = token || (auth ? (currentSession() || {}).token : null);
  if (bearer) headers.authorization = `Bearer ${bearer}`;
  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: 'no-store',
  });
  let parsed = null;
  try {
    parsed = await response.json();
  } catch {
    // An empty or non-JSON body.
  }
  if (!response.ok) {
    if (response.status === 401 && auth && !token) {
      clearSession();
      onUnauthorized();
    }
    throw new ApiError(response.status, parsed);
  }
  return parsed;
}

/** Revokes a session on the hub (best effort). */
export async function revoke(token) {
  try {
    await request('DELETE', '/v1/auth/sessions/current', { token });
  } catch {
    // Already gone or the hub is unreachable.
  }
}

/** Sign out: forget the session here and revoke it on the hub. */
export async function signOut() {
  const session = currentSession();
  clearSession();
  if (session) await revoke(session.token);
}
