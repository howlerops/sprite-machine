// The mobile web page: routing and the screens (plan §4.4).
//
//   signed out:  sign-in            /            "Sign in with a device code"
//                QR link            /pair?code=  redeem, then confirm
//                device code        /pair/device the page's own code, polled, then confirm
//   signed in:   session list       /            machines, presence, threads (sessions.js)
//                thread             /#/t/<id>    the transcript, live over the subscription (thread.js)
//
// The session token is stored only after the person confirms the user and org it names.

import {
  ApiError,
  clearSession,
  currentSession,
  revoke,
  saveSession,
  setUnauthorizedHandler,
  signOut,
} from '/src/api.js';
import { pollDevice, redeemLink, startDevice } from '/src/pairing.js';
import { disablePush, forgetSession, mountPushButton, registerServiceWorker, rememberSession } from '/src/push.js';
import { mountSessions } from '/src/sessions.js';
import { clearThreadCaches, mountThread } from '/src/thread.js';

const root = document.getElementById('app');
const esc = (text) => String(text ?? '').replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

let teardown = () => {};
let notice = '';
// A code in /pair?code= is single use: it is read once, then the address is cleaned.
let linkCode = null;
// The thread on screen: a hash change fires both hashchange and popstate, one mount is enough.
let shownThread = null;

function screen(html, stop = () => {}) {
  teardown();
  shownThread = null;
  teardown = stop;
  root.innerHTML = html;
  const heading = root.querySelector('h1');
  if (heading) heading.focus({ preventScroll: true });
}

function go(path) {
  history.replaceState(null, '', path);
  route();
}

// ---- signed out -----------------------------------------------------------------------------

function showSignIn(message = notice) {
  notice = '';
  screen(`
    <main class="card">
      <h1 tabindex="-1">Sign in</h1>
      ${message ? `<p class="notice" role="alert">${esc(message)}</p>` : ''}
      <p>Scan the QR code that <code>sm rc</code> prints on your computer, or pair this phone with a device code.</p>
      <button type="button" class="primary" id="device">Sign in with a device code</button>
    </main>`);
  root.querySelector('#device').addEventListener('click', () => go('/pair/device'));
}

async function showRedeem(code) {
  screen('<main class="card"><h1 tabindex="-1">Signing in</h1><p>Checking your sign-in link…</p></main>');
  try {
    showConfirm(await redeemLink(code));
  } catch (error) {
    const expired = error instanceof ApiError && error.status === 410;
    notice = expired
      ? 'This sign-in link is used or expired. Ask your computer for a new one (sm rc).'
      : 'Could not use this sign-in link. Check your connection and try again.';
    go('/');
  }
}

function showDevice() {
  const abort = new AbortController();
  screen(
    `<main class="card">
      <h1 tabindex="-1">Pair this phone</h1>
      <p id="device-status">Getting a code…</p>
      <p class="code" id="user-code" aria-label="Device code"></p>
      <p id="device-help"></p>
      <button type="button" id="cancel">Cancel</button>
    </main>`,
    () => abort.abort(),
  );
  root.querySelector('#cancel').addEventListener('click', () => go('/'));
  const status = root.querySelector('#device-status');
  (async () => {
    try {
      const device = await startDevice();
      if (abort.signal.aborted) return;
      root.querySelector('#user-code').textContent = device.user_code;
      status.textContent = 'Waiting for approval…';
      root.querySelector('#device-help').textContent =
        'Approve this code in a Sprite Machine app where you are signed in. It expires in ' +
        `${Math.round((device.expires_in || 600) / 60)} minutes.`;
      showConfirm(await pollDevice(device, abort.signal));
    } catch (error) {
      if (error.name === 'AbortError') return;
      status.textContent =
        error.code === 'access_denied'
          ? 'The request was denied.'
          : error.code === 'expired_token'
            ? 'The code expired.'
            : error.status === 429
              ? 'Too many tries. Wait a minute, then try again.'
              : 'Could not pair. Try again.';
      status.setAttribute('role', 'alert');
      const retry = document.createElement('button');
      retry.type = 'button';
      retry.className = 'primary';
      retry.textContent = 'Try again';
      retry.addEventListener('click', showDevice);
      root.querySelector('#cancel').before(retry);
    }
  })();
}

/** Shows who the session is for and stores it only on "Confirm". */
function showConfirm(session) {
  const user = (session.user && (session.user.display_name || session.user.id)) || 'Unknown user';
  const org = (session.org && (session.org.name || session.org.id)) || 'Unknown organization';
  screen(`
    <main class="card">
      <h1 tabindex="-1">Confirm sign-in</h1>
      <p>This phone will be signed in as:</p>
      <dl class="who">
        <dt>User</dt><dd id="who-user">${esc(user)}</dd>
        <dt>Organization</dt><dd id="who-org">${esc(org)}</dd>
        ${session.role ? `<dt>Role</dt><dd>${esc(session.role)}</dd>` : ''}
      </dl>
      <div class="actions">
        <button type="button" class="primary" id="confirm">Confirm and sign in</button>
        <button type="button" id="decline">Cancel</button>
      </div>
    </main>`);
  root.querySelector('#confirm').addEventListener('click', () => {
    saveSession(session);
    go('/');
  });
  root.querySelector('#decline').addEventListener('click', () => {
    // Nobody confirmed: the unstored session is revoked, not left alive on the hub.
    revoke(session.token);
    go('/');
  });
}

// ---- signed in ------------------------------------------------------------------------------

function showSessions(session) {
  const who = (session.user && session.user.display_name) || 'You';
  const org = (session.org && session.org.name) || '';
  screen(`<header class="bar">
      <div><h1 tabindex="-1">Sessions</h1>
        <p class="who-line"><span id="bar-user">${esc(who)}</span>${org ? ` · <span id="bar-org">${esc(org)}</span>` : ''}</p></div>
      <button type="button" id="signout">Sign out</button>
    </header>
    <main id="list" aria-label="Machines and sessions"></main>`);
  mountPushButton(root.querySelector('.bar'));
  teardown = mountSessions(root.querySelector('#list'));
  root.querySelector('#signout').addEventListener('click', async () => {
    await disablePush();
    await signOut();
    clearThreadCaches();
    go('/');
  });
}

function showThread(id) {
  if (shownThread === id) return;
  screen(`
    <header class="bar">
      <div><h1 tabindex="-1">Session</h1><p class="who-line">${esc(id)}</p></div>
      <a class="button" href="#/" id="back">Back</a>
    </header>
    <main id="thread" aria-label="Transcript"></main>`);
  teardown = mountThread(root.querySelector('#thread'), id);
  shownThread = id;
}

// ---- routing --------------------------------------------------------------------------------

function route() {
  const session = currentSession();
  const path = location.pathname.replace(/\/+$/, '') || '/';
  const params = new URLSearchParams(location.search);

  if (path === '/pair' && params.has('code')) {
    linkCode = params.get('code');
    history.replaceState(null, '', '/pair');
  }
  if (linkCode) {
    const code = linkCode;
    linkCode = null;
    showRedeem(code);
    return;
  }
  if (session) {
    rememberSession(session);
    if (path !== '/') {
      history.replaceState(null, '', `/${location.hash}`);
    }
    const thread = location.hash.match(/^#\/t\/([^/?]+)/);
    if (thread) showThread(decodeURIComponent(thread[1]));
    else showSessions(session);
    return;
  }
  if (path === '/pair/device') showDevice();
  else showSignIn();
}

setUnauthorizedHandler(() => {
  clearSession();
  forgetSession();
  clearThreadCaches();
  notice = 'Your session ended. Sign in again.';
  go('/');
});
window.addEventListener('hashchange', route);
window.addEventListener('popstate', route);
registerServiceWorker();
route();
