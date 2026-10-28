// The outbound queue and the keyed requests (client-api.md §3, tunnel.md idempotency keys).
//
// Every POST and DELETE carries an `Idempotency-Key`, made once per intent. A prompt is written to
// the outbox (localStorage) before it is sent and removed only after the hub accepted it, so it
// survives going offline and a reload; the same key on every retry means a send whose answer was
// lost is replayed by the hub, never done twice. The outbox holds only the prompt body (text and
// file parts); the session token stays in the request header.

import { ApiError, currentSession, request } from '/src/api.js';

const KEY = 'sm.outbox';
const RETRY_MS = 5000;

// Used when storage is unavailable.
let memory = [];
const listeners = new Set();

const hex = () =>
  Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, '0')).join('');

/** A fresh id: `idk_`, `tk_` or `msg_` and 128 random bits. */
export const newId = (prefix) => `${prefix}_${hex()}`;

/** The queued prompts, oldest first: `{key, thread_id, body, preview, queued_at, error?}`. */
export function readOutbox() {
  try {
    const list = JSON.parse(localStorage.getItem(KEY) || 'null');
    if (Array.isArray(list)) return list;
  } catch {
    // Storage blocked or damaged: use what this page holds.
  }
  return memory;
}

function write(list) {
  memory = list;
  try {
    localStorage.setItem(KEY, JSON.stringify(list));
  } catch {
    // Storage full or blocked: the queue lives until the page closes.
  }
  for (const listener of listeners) listener();
}

export function onOutboxChange(listener) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Queues a prompt for a thread. `body` is `{task_id, message_id, parts}`. */
export function enqueue(threadId, body, preview) {
  const entry = {
    key: newId('idk'),
    thread_id: threadId,
    body,
    preview,
    queued_at: new Date().toISOString(),
  };
  write([...readOutbox(), entry]);
  return entry;
}

export function removeEntry(key) {
  write(readOutbox().filter((entry) => entry.key !== key));
}

function failEntry(key, message) {
  write(readOutbox().map((entry) => (entry.key === key ? { ...entry, error: message } : entry)));
}

/** Forgets every queued prompt (sign out: they belong to the person who left). */
export function clearOutbox() {
  write([]);
}

/**
 * One call with an `Idempotency-Key`. Resolves to the JSON body (null when empty); rejects with
 * ApiError for a non-2xx answer and with the fetch error (a TypeError) when there is no network.
 */
export async function keyedRequest(method, path, { body, key = newId('idk') } = {}) {
  const headers = { 'idempotency-key': key };
  if (body !== undefined) headers['content-type'] = 'application/json';
  const session = currentSession();
  if (session) headers.authorization = `Bearer ${session.token}`;
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
    // Let the shared wrapper notice the dead session and send the app to sign-in.
    if (response.status === 401) request('GET', '/v1/machines').catch(() => {});
    throw new ApiError(response.status, parsed);
  }
  return parsed;
}

let sending = false;

/** Sends the queued prompts in order. Stops at the first network or server failure. */
export async function flushOutbox() {
  if (sending) return;
  sending = true;
  try {
    for (const entry of readOutbox()) {
      if (entry.error) continue;
      try {
        await keyedRequest('POST', `/v1/threads/${encodeURIComponent(entry.thread_id)}/prompts`, {
          body: entry.body,
          key: entry.key,
        });
        removeEntry(entry.key);
      } catch (error) {
        const refused =
          error instanceof ApiError && error.status >= 400 && error.status < 500 && error.status !== 429;
        if (!refused || error.status === 401) return;
        // The hub will never take this one: keep it visible so the person can discard it.
        failEntry(entry.key, error.message);
      }
    }
  } finally {
    sending = false;
  }
}

/** Sends now, again when the browser comes back online, and every few seconds while any wait. */
export function startOutbox() {
  const attempt = () => {
    if (readOutbox().some((entry) => !entry.error)) flushOutbox();
  };
  window.addEventListener('online', attempt);
  window.addEventListener('storage', attempt);
  const timer = setInterval(attempt, RETRY_MS);
  attempt();
  return () => {
    window.removeEventListener('online', attempt);
    window.removeEventListener('storage', attempt);
    clearInterval(timer);
  };
}
