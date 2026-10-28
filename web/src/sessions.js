// The session list: the org's machines with presence (GET /v1/machines) and threads
// (GET /v1/threads), grouped by machine. It polls every 5 s while the page is visible (a
// subscription needs a socket per machine; polling is two cheap GETs) and on becoming visible.

import { ApiError, request } from '/src/api.js';

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

// ---- Members screen ----------------------------------------------------------------------------
// The members screen (client-api.md §5.9): everyone sees the list; an admin also invites by
// GitHub login, revokes open invites, changes roles and removes members. The hub enforces every
// rule (never above your own role, never yourself); the page only hides what would be refused.
// All text goes in with textContent.


const ROLES = ['viewer', 'member', 'admin', 'owner'];
const rank = (role) => ROLES.indexOf(role);

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === 'text') node.textContent = value;
    else if (key === 'class') node.className = value;
    else if (key in node) node[key] = value;
    else node.setAttribute(key, value);
  }
  node.append(...children);
  return node;
}

function failure(error) {
  if (!(error instanceof ApiError)) return 'Could not reach the hub. Try again.';
  if (error.status === 403) return 'Your role does not allow that.';
  if (error.status === 409) return 'That GitHub account is already a member or has an open invite.';
  if (error.status === 404) return 'That member or invite is gone. The list was refreshed.';
  if (error.status === 400) return 'That is not a valid GitHub login.';
  return 'The hub refused that. Try again.';
}

/** Mounts the screen in `container`; returns the teardown. `session` is the stored session. */
export function mountMembers(container, session) {
  const myRank = rank(session.role);
  const admin = myRank >= rank('admin');
  const myId = session.user && session.user.id;
  let alive = true;

  const message = el('p', { class: 'notice', role: 'alert', hidden: true });
  const list = el('ul', { class: 'people', id: 'members-list', 'aria-label': 'Members' });
  const invitesHeading = el('h2', { text: 'Open invites', hidden: true });
  const invites = el('ul', { class: 'people', id: 'invites-list', 'aria-label': 'Open invites', hidden: true });
  container.replaceChildren(message, list);

  const say = (text) => {
    message.textContent = text || '';
    message.hidden = !text;
  };
  const run = async (call) => {
    say('');
    try {
      await call();
    } catch (error) {
      say(failure(error));
    }
    await refresh();
  };

  function memberRow(member) {
    const name = member.display_name || member.user_id;
    const who = el('div', { class: 'person' }, el('strong', { text: name }));
    if (member.github_login) who.append(el('span', { class: 'login', text: `@${member.github_login}` }));
    const row = el('li', { 'data-user': member.user_id }, who);
    const manageable = admin && member.user_id !== myId && rank(member.role) <= myRank;
    if (!manageable) {
      row.append(el('span', { class: 'role', text: member.role }));
      return row;
    }
    const select = el('select', { 'aria-label': `Role of ${name}` });
    for (const role of ROLES.filter((r) => rank(r) <= myRank)) {
      select.append(el('option', { value: role, text: role, selected: role === member.role }));
    }
    select.addEventListener('change', () =>
      run(() =>
        request('PATCH', `/v1/members/${encodeURIComponent(member.user_id)}`, {
          body: { role: select.value },
        }),
      ),
    );
    const remove = el('button', { type: 'button', text: 'Remove', 'aria-label': `Remove ${name}` });
    remove.addEventListener('click', () => {
      if (remove.dataset.armed !== '1') {
        remove.dataset.armed = '1';
        remove.textContent = 'Confirm remove';
        return;
      }
      run(() => request('DELETE', `/v1/members/${encodeURIComponent(member.user_id)}`));
    });
    row.append(select, remove);
    return row;
  }

  function inviteRow(invite) {
    const row = el(
      'li',
      { 'data-invite': invite.id },
      el(
        'div',
        { class: 'person' },
        el('strong', { text: `@${invite.github_login}` }),
        el('span', { class: 'login', text: `invited as ${invite.role}` }),
      ),
    );
    const revoke = el('button', {
      type: 'button',
      text: 'Revoke',
      'aria-label': `Revoke invite for ${invite.github_login}`,
    });
    revoke.addEventListener('click', () =>
      run(() => request('DELETE', `/v1/invites/${encodeURIComponent(invite.id)}`)),
    );
    row.append(revoke);
    return row;
  }

  async function refresh() {
    try {
      const [members, open] = await Promise.all([
        request('GET', '/v1/members'),
        admin ? request('GET', '/v1/invites') : Promise.resolve(null),
      ]);
      if (!alive) return;
      list.replaceChildren(...(members.members || []).map(memberRow));
      if (open) {
        const items = open.invites || [];
        invites.replaceChildren(...items.map(inviteRow));
        invitesHeading.hidden = items.length === 0;
        invites.hidden = items.length === 0;
      }
    } catch (error) {
      if (alive && !(error instanceof ApiError && error.status === 401)) {
        say('Could not load the members. Try again.');
      }
    }
  }

  if (admin) {
    const login = el('input', {
      type: 'text',
      id: 'invite-login',
      autocomplete: 'off',
      autocapitalize: 'none',
      spellcheck: false,
      required: true,
    });
    const role = el('select', { id: 'invite-role', 'aria-label': 'Role for the invite' });
    for (const name of ROLES.filter((r) => rank(r) <= myRank && r !== 'owner')) {
      role.append(el('option', { value: name, text: name, selected: name === 'member' }));
    }
    const form = el(
      'form',
      { class: 'invite-form', id: 'invite-form' },
      el('h2', { text: 'Invite by GitHub login' }),
      el('label', { htmlFor: 'invite-login', text: 'GitHub login' }),
      login,
      role,
      el('button', { type: 'submit', class: 'primary', text: 'Send invite' }),
    );
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      const value = login.value.trim().replace(/^@/, '');
      if (!value) return;
      run(async () => {
        await request('POST', '/v1/invites', { body: { github_login: value, role: role.value } });
        login.value = '';
      });
    });
    container.append(form, invitesHeading, invites);
  } else {
    container.append(el('p', { class: 'hint', text: 'Only admins can invite or change members.' }));
  }

  refresh();
  return () => {
    alive = false;
  };
}
