// Pairing (client-api.md §5.7): redeem a runner's QR link, or run the RFC 8628 device-code flow
// with a code this page shows. Both give a session that the page shows for confirmation before
// storing it (app.js).

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
