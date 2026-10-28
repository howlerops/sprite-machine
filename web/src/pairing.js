// Pairing (client-api.md §5.7): redeem a runner's QR link, or run the RFC 8628 device-code flow
// with a code this page shows. Both give a session that the page shows for confirmation before
// storing it (app.js). GitHub sign-in (§5.9) is at the end: the same module, because the service
// worker's shell list (sw.js) names the files it caches for offline.

import { request } from '/src/api.js';

/** `POST /v1/auth/links/redeem`: the session behind a one-time link code (no auth). */
export function redeemLink(code) {
  return request('POST', '/v1/auth/links/redeem', { body: { code }, auth: false });
}

/** `POST /v1/auth/device/code`: `{device_code, user_code, verification_uri, interval, expires_in}`. */
export function startDevice() {
  return request('POST', '/v1/auth/device/code', { body: {}, auth: false });
}

function sleep(ms, signal) {
  return new Promise((resolve, reject) => {
    const stop = () => {
      clearTimeout(timer);
      reject(new DOMException('aborted', 'AbortError'));
    };
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', stop);
      resolve();
    }, ms);
    if (signal.aborted) stop();
    else signal.addEventListener('abort', stop, { once: true });
  });
}

/**
 * Polls `POST /v1/auth/device/token` every `interval` seconds until the code is approved. On
 * `slow_down` it waits the interval the hub names (else 5 s more). Resolves to the session; rejects
 * with ApiError (`expired_token`, `access_denied`, ...) or AbortError.
 */
export async function pollDevice(device, signal) {
  let interval = device.interval || 5;
  for (;;) {
    await sleep(interval * 1000, signal);
    try {
      return await request('POST', '/v1/auth/device/token', {
        body: { device_code: device.device_code },
        auth: false,
      });
    } catch (error) {
      if (error.name === 'AbortError') throw error;
      if (error.code === 'slow_down') {
        interval = Number(error.body.interval) || interval + 5;
      } else if (error.code !== 'authorization_pending' && error.status) {
        throw error;
      }
      // authorization_pending, or the network blipped (no status): wait and ask again.
    }
  }
}

// ---- GitHub sign-in ----------------------------------------------------------------------------
// GitHub sign-in (client-api.md §5.9): authorization code + PKCE (S256). The page makes a random
// verifier and state, keeps them only in sessionStorage (this tab, gone when it closes), and sends
// the browser to the hub. The hub returns to `/#/auth/github?code=&state=`; app.js reads the code
// from the fragment, cleans the address at once, and `completeGithubSignIn` trades the code and the
// verifier for the session. The verifier appears nowhere but sessionStorage and that one POST body.

const VERIFIER = 'sm.github.verifier';
const STATE = 'sm.github.state';

/** A refusal with a `code` the app turns into a message: access_denied, state, storage, error. */
export class GithubError extends Error {
  constructor(code) {
    super(code);
    this.code = code;
  }
}

const base64url = (bytes) =>
  btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const randomToken = (size) => base64url(crypto.getRandomValues(new Uint8Array(size)));

/** The web page's own redirect URI; the hub only allows its own origin for the page. */
export const redirectUri = () => `${location.origin}/auth/github/callback`;

/** Stores a fresh verifier and state, then sends the browser to the hub's authorize route. */
export async function startGithubSignIn() {
  const verifier = randomToken(32);
  const state = randomToken(16);
  const digest = new Uint8Array(
    await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier)),
  );
  try {
    sessionStorage.setItem(VERIFIER, verifier);
    sessionStorage.setItem(STATE, state);
  } catch {
    throw new GithubError('storage');
  }
  const query = new URLSearchParams({
    redirect_uri: redirectUri(),
    code_challenge: base64url(digest),
    code_challenge_method: 'S256',
    state,
  });
  location.assign(`/v1/auth/github/authorize?${query}`);
}

/** Reads and removes the pending verifier and state (single use, whatever happens next). */
function takePending() {
  try {
    const pending = { verifier: sessionStorage.getItem(VERIFIER), state: sessionStorage.getItem(STATE) };
    sessionStorage.removeItem(VERIFIER);
    sessionStorage.removeItem(STATE);
    return pending.verifier && pending.state ? pending : null;
  } catch {
    return null;
  }
}

/** `params`: the fragment's query. Resolves to the session `{token, expires_at, user, org, role}`. */
export async function completeGithubSignIn(params) {
  const pending = takePending();
  const code = params.get('code');
  const state = params.get('state');
  // The state must be the one this tab made, or the code is not ours to trade.
  if (!pending || !state || state !== pending.state) throw new GithubError('state');
  if (params.get('error')) throw new GithubError(params.get('error'));
  if (!code) throw new GithubError('error');
  return request('POST', '/v1/auth/github/token', {
    body: { code, code_verifier: pending.verifier, redirect_uri: redirectUri() },
    auth: false,
  });
}
