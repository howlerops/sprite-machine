// The thread view (plan §4.4, client-api §5.2, turn-event §6): a transcript kept live by the
// subscription WebSocket.
//
// A browser cannot set Authorization on a WebSocket, so before every open and reopen the page
// mints a ticket (POST .../subscribe-ticket, with the session token in the header) and puts only
// that ticket in the URL: single use, 60 s. The session token is never in a URL.
//
// Resuming: the envelopes applied so far are kept in localStorage per thread. After a reload the
// page replays them into the model, then subscribes with ?after=<last seq>; the hub replays from
// the open run's start and the model drops every seq it already has, so nothing shows twice.

import { request } from '/src/api.js';
import { Transcript } from '/src/transcript.js';
import { h, renderTranscript, sessionText, usageText } from '/src/render.js';
import { normaliseMachine } from '/src/sessions.js';

export const MACHINE_POLL_MS = 5000;
const PING_MS = 25000;
const RETRY_MIN_MS = 1000;
const RETRY_MAX_MS = 15000;
// What is kept for resuming: the newest envelopes, at most about this many bytes.
const CACHE_EVENTS = 2000;
const CACHE_BYTES = 1_500_000;

const cacheKey = (threadId) => `sm.thread.${threadId}`;

function loadCache(threadId) {
  try {
    const parsed = JSON.parse(localStorage.getItem(cacheKey(threadId)) || 'null');
    if (parsed && parsed.v === 1 && Array.isArray(parsed.events)) return parsed;
  } catch {
    // Storage blocked or damaged: start from the whole log.
  }
  return null;
}

function saveCache(threadId, events, states) {
  try {
    let kept = events.slice(-CACHE_EVENTS);
    let body = JSON.stringify({ v: 1, events: kept, states });
    while (body.length > CACHE_BYTES && kept.length > 1) {
      kept = kept.slice(Math.ceil(kept.length / 2));
      body = JSON.stringify({ v: 1, events: kept, states });
    }
    localStorage.setItem(cacheKey(threadId), body);
  } catch {
    // Storage full or blocked: the next load subscribes from 0.
  }
}

/** Forgets every kept transcript (sign out: they belong to the person who left). */
export function clearThreadCaches() {
  try {
    for (const key of Object.keys(localStorage)) {
      if (key.startsWith('sm.thread.')) localStorage.removeItem(key);
    }
  } catch {
    // Storage blocked: nothing was kept.
  }
}

/** The text of the banner for a machine that is not online, or '' when there is none. */
export function waitingText(machine) {
  if (!machine || machine.presence === 'online') return '';
  const how = machine.presence === 'asleep' ? 'asleep' : 'offline';
  return `Waiting for ${machine.name} (${how})`;
}

function socketUrl(threadId, after, ticket) {
  const url = new URL(`/v1/threads/${encodeURIComponent(threadId)}/subscribe`, location.href);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.search = new URLSearchParams({ after: String(after), ticket }).toString();
  return url.toString();
}

/** Builds the view in `host` and starts the subscription. Returns a stop function. */
export function mountThread(host, threadId) {
  const model = new Transcript();
  const log = []; // applied envelopes, for the resume cache
  let stopped = false;
  let socket = null;
  let retryTimer = null;
  let pingTimer = null;
  let retryMs = RETRY_MIN_MS;
  let machine = null;
  let renderQueued = false;
  let saveTimer = null;

  const status = h('p', { id: 'conn', class: 'conn muted', role: 'status', 'data-state': 'connecting' }, 'Connecting…');
  const waiting = h('section', { id: 'waiting', class: 'waiting', hidden: true, 'aria-live': 'polite' });
  const usage = h('p', { id: 'usage', class: 'state-line', hidden: true });
  const session = h('p', { id: 'session-state', class: 'state-line', hidden: true });
  const list = h('div', { id: 'transcript', class: 'transcript', role: 'log', 'aria-label': 'Transcript' });
  const empty = h('p', { id: 'transcript-empty', class: 'empty' }, 'No messages yet.');
  host.replaceChildren(status, waiting, usage, session, list, empty);

  function setConnection(state, text) {
    status.dataset.state = state;
    status.textContent = text;
  }

  function renderStates() {
    const merged = model.states;
    const usageLine = usageText(merged.usage);
    usage.textContent = usageLine;
    usage.hidden = !usageLine;
    const sessionLine = sessionText(merged.session);
    session.textContent = sessionLine;
    session.hidden = !sessionLine;
    renderWaiting(merged.thread);
  }

  function renderWaiting(thread) {
    const banner = waitingText(machine);
    const queued = (thread && thread.pending_tasks) || [];
    waiting.hidden = !banner;
    if (!banner) {
      waiting.replaceChildren();
      return;
    }
    waiting.replaceChildren(
      h('strong', { id: 'waiting-text' }, banner),
      queued.length
        ? h(
            'ul',
            { id: 'queued', class: 'queued', 'aria-label': 'Queued' },
            queued.map((task) => h('li', { 'data-task': task.task_id }, task.preview || task.task_id)),
          )
        : h('p', { class: 'muted' }, 'Nothing is queued.'),
    );
  }

  function render() {
    renderQueued = false;
    if (stopped) return;
    renderTranscript(list, model.items);
    empty.hidden = model.items.length > 0;
    renderStates();
    host.dataset.lastSeq = String(model.lastSeq);
  }
  function scheduleRender() {
    if (renderQueued) return;
    renderQueued = true;
    requestAnimationFrame(render);
  }
  function scheduleSave() {
    clearTimeout(saveTimer);
    saveTimer = setTimeout(flush, 500);
  }
  function flush() {
    clearTimeout(saveTimer);
    if (log.length > 2 * CACHE_EVENTS) log.splice(0, log.length - CACHE_EVENTS);
    saveCache(threadId, log, model.states);
  }

  function applyEnvelope(envelope) {
    if (model.apply(envelope)) {
      log.push(envelope);
      scheduleSave();
    }
    scheduleRender();
  }

  function onMessage(raw) {
    let message;
    try {
      message = JSON.parse(raw);
    } catch {
      return;
    }
    switch (message.type) {
      case 'hello':
        break;
      case 'snapshot':
        // The state as of the replay start; the events replayed after it bring it up to date.
        // When we already applied past that point our own state is newer: keep it.
        if (model.lastSeq <= message.at_seq) model.setStates(message.states);
        scheduleRender();
        break;
      case 'event':
        applyEnvelope(message.envelope);
        break;
      case 'live':
        retryMs = RETRY_MIN_MS;
        setConnection('live', 'Live');
        break;
      case 'error':
        // The hub closes next; the reconnect resumes from the last applied seq.
        setConnection('error', 'Replay failed. Reconnecting…');
        break;
      default:
    }
  }

  async function open() {
    if (stopped) return;
    setConnection('connecting', log.length ? 'Reconnecting…' : 'Connecting…');
    let ticket;
    try {
      ({ ticket } = await request('POST', `/v1/threads/${encodeURIComponent(threadId)}/subscribe-ticket`));
    } catch (error) {
      if (stopped || error.status === 401) return; // 401 already sent the app to sign-in
      if (error.status === 404) {
        setConnection('error', 'This session was not found.');
        return;
      }
      retry();
      return;
    }
    if (stopped) return;
    const ws = new WebSocket(socketUrl(threadId, model.lastSeq, ticket));
    socket = ws;
    ws.addEventListener('open', () => {
      clearInterval(pingTimer);
      pingTimer = setInterval(() => {
        if (ws.readyState === WebSocket.OPEN) ws.send('{"type":"ping"}');
      }, PING_MS);
    });
    ws.addEventListener('message', (event) => {
      if (typeof event.data === 'string' && event.data !== '{"type":"pong"}') onMessage(event.data);
    });
    ws.addEventListener('close', () => {
      clearInterval(pingTimer);
      if (socket === ws) socket = null;
      if (!stopped) {
        setConnection('connecting', 'Disconnected. Reconnecting…');
        retry();
      }
    });
  }

  function retry() {
    clearTimeout(retryTimer);
    retryTimer = setTimeout(open, retryMs);
    retryMs = Math.min(RETRY_MAX_MS, retryMs * 2);
  }

  // The machine behind the thread, and whether it is online.
  let machineId = null;
  async function refreshMachine() {
    try {
      if (!machineId) {
        const info = await request('GET', `/v1/threads/${encodeURIComponent(threadId)}`).catch(
          () => null,
        );
        machineId = (info && info.machine_id) || null;
        if (!machineId) {
          const listed = await request('GET', '/v1/threads');
          const row = (listed.threads || []).find((t) => t.thread_id === threadId);
          machineId = (row && row.machine_id) || null;
        }
      }
      const body = await request('GET', '/v1/machines');
      if (stopped) return;
      const found = (body.machines || []).map(normaliseMachine).find((m) => m.id === machineId);
      machine = found || null;
      host.dataset.machine = machine ? machine.presence : '';
      scheduleRender();
    } catch {
      // Keep the last known state; the next poll tries again.
    }
  }
  const machineTimer = setInterval(() => {
    if (document.visibilityState === 'visible') refreshMachine();
  }, MACHINE_POLL_MS);
  const onVisible = () => {
    if (document.visibilityState === 'visible') refreshMachine();
  };
  document.addEventListener('visibilitychange', onVisible);
  // A reload or a closed tab must not lose the last events: save as the page goes away.
  window.addEventListener('pagehide', flush);

  // Resume from what an earlier load kept.
  const cached = loadCache(threadId);
  if (cached) {
    for (const envelope of cached.events) {
      if (model.apply(envelope)) log.push(envelope);
    }
    model.setStates(cached.states);
    render();
  }

  refreshMachine();
  open();

  return () => {
    stopped = true;
    clearTimeout(retryTimer);
    clearTimeout(saveTimer);
    clearInterval(pingTimer);
    clearInterval(machineTimer);
    document.removeEventListener('visibilitychange', onVisible);
    window.removeEventListener('pagehide', flush);
    if (socket) socket.close();
    flush();
  };
}
