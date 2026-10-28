// The session list: the org's machines with presence (GET /v1/machines) and threads
// (GET /v1/threads), grouped by machine. It polls every 5 s while the page is visible (a
// subscription needs a socket per machine; polling is two cheap GETs) and on becoming visible.

import { request } from '/src/api.js';

export const POLL_MS = 5000;
const MAX_PAGES = 4;

const PRESENCE = { online: 'Online', asleep: 'Asleep', offline: 'Offline' };
const STATUS = { idle: 'Idle', running: 'Running', needs_you: 'Needs you', done: 'Done' };

const esc = (text) => String(text ?? '').replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

/** A machine row from GET /v1/machines (client-api §5.1). */
export function normaliseMachine(machine) {
  const state = machine.presence && machine.presence.state;
  return {
    id: machine.machine_id,
    name: machine.name || machine.machine_id,
    presence: PRESENCE[state] ? state : 'offline',
  };
}

/** "just now", "3m ago", "2h ago", "5d ago". */
export function ago(iso, now = Date.now()) {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return '';
  const seconds = Math.max(0, Math.round((now - then) / 1000));
  if (seconds < 45) return 'just now';
  if (seconds < 3600) return `${Math.max(1, Math.round(seconds / 60))}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

/** Machines (online first, then by name) with their threads (as listed); orphans last. */
export function group(machines, threads) {
  const known = machines.slice();
  const order = { online: 0, asleep: 1, offline: 2 };
  known.sort((a, b) => order[a.presence] - order[b.presence] || a.name.localeCompare(b.name));
  const byMachine = new Map(known.map((m) => [m.id, { machine: m, threads: [] }]));
  const orphans = [];
  for (const thread of threads) {
    const slot = byMachine.get(thread.machine_id);
    (slot ? slot.threads : orphans).push(thread);
  }
  const groups = [...byMachine.values()];
  if (orphans.length) {
    groups.push({ machine: { id: '', name: 'Other machines' }, threads: orphans, other: true });
  }
  return groups;
}

function threadHtml(thread, now) {
  const status = STATUS[thread.status] ? thread.status : 'done';
  const href = `#/t/${encodeURIComponent(thread.thread_id)}`;
  return (
    `<li><a class="thread" href="${esc(href)}">` +
    `<span class="thread-title">${esc(thread.title || 'Untitled')}</span>` +
    `<span class="chip status-${status}">${STATUS[status]}</span>` +
    `<time class="thread-time" datetime="${esc(thread.updated_at)}">${esc(ago(thread.updated_at, now))}</time>` +
    '</a></li>'
  );
}

function groupHtml({ machine, threads, other }, now) {
  const presence = other
    ? ''
    : `<span class="chip presence-${machine.presence}" data-presence="${machine.presence}">${PRESENCE[machine.presence]}</span>`;
  const body = threads.length
    ? `<ul class="threads">${threads.map((t) => threadHtml(t, now)).join('')}</ul>`
    : '<p class="empty">No sessions</p>';
  return (
    `<section class="machine" aria-label="${esc(machine.name)}" data-machine="${esc(machine.id)}">` +
    `<h2><span class="machine-name">${esc(machine.name)}</span>${presence}</h2>${body}</section>`
  );
}

/** The list's HTML for one fetch result. */
export function listHtml(machines, threads, now = Date.now()) {
  const groups = group(machines, threads);
  if (!groups.length) {
    return '<p class="empty">No machines yet. Run <code>sm rc</code> on a computer to add one.</p>';
  }
  return groups.map((g) => groupHtml(g, now)).join('');
}

async function fetchThreads() {
  const threads = [];
  let cursor = null;
  for (let page = 0; page < MAX_PAGES; page += 1) {
    const query = cursor ? `?cursor=${encodeURIComponent(cursor)}` : '';
    const body = await request('GET', `/v1/threads${query}`);
    threads.push(...(body.threads || []));
    cursor = body.next_cursor;
    if (!cursor) break;
  }
  return threads;
}

/**
 * Renders the list into `host` and keeps it fresh. Returns a stop function. `host.dataset.state`
 * is loading | ready | error.
 */
export function mountSessions(host) {
  let stopped = false;
  let last = '';
  let timer = null;
  host.dataset.state = 'loading';
  host.innerHTML = '<p class="empty">Loading…</p>';

  async function refresh() {
    try {
      const [machines, threads] = await Promise.all([
        request('GET', '/v1/machines').then((b) => (b.machines || []).map(normaliseMachine)),
        fetchThreads(),
      ]);
      if (stopped) return;
      const html = listHtml(machines, threads);
      if (html !== last) {
        host.innerHTML = html;
        last = html;
      }
      host.dataset.state = 'ready';
    } catch (error) {
      // A 401 already sent the app to sign-in. Anything else keeps the last list and says so.
      if (stopped || error.status === 401) return;
      host.dataset.state = 'error';
      if (!last) host.innerHTML = '<p class="empty" role="alert">Cannot reach the hub. Retrying…</p>';
    }
  }

  function schedule() {
    clearInterval(timer);
    timer = null;
    if (document.visibilityState === 'visible') timer = setInterval(refresh, POLL_MS);
  }
  function onVisibility() {
    schedule();
    if (document.visibilityState === 'visible') refresh();
  }

  document.addEventListener('visibilitychange', onVisibility);
  schedule();
  refresh();
  return () => {
    stopped = true;
    clearInterval(timer);
    document.removeEventListener('visibilitychange', onVisibility);
  };
}
