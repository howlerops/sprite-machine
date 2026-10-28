// PWA and Web Push on the page (client-api.md §5.8): registers the service worker, keeps the
// session token where the worker can read it (IndexedDB `sm-push`, never in a URL), and subscribes
// this browser to push (VAPID) with the hub. The notification itself is sw.js.

import { request } from '/src/api.js';

const supported = () =>
  'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window;

/** Registers /sw.js (the app shell cache and push); resolves to the registration or null. */
export async function registerServiceWorker() {
  if (!('serviceWorker' in navigator)) return null;
  try {
    return await navigator.serviceWorker.register('/sw.js', { scope: '/' });
  } catch {
    return null;
  }
}

function withStore(mode, work) {
  return new Promise((resolve) => {
    let open;
    try {
      open = indexedDB.open('sm-push', 1);
    } catch {
      resolve(null);
      return;
    }
    open.onupgradeneeded = () => open.result.createObjectStore('kv');
    open.onerror = () => resolve(null);
    open.onsuccess = () => {
      const db = open.result;
      const tx = db.transaction('kv', mode);
      work(tx.objectStore('kv'));
      tx.oncomplete = () => {
        db.close();
        resolve(true);
      };
      tx.onerror = tx.onabort = () => {
        db.close();
        resolve(null);
      };
    };
  });
}

/** Hands the signed-in session's token to the service worker (for Allow / Deny and titles). */
export const rememberSession = (session) =>
  withStore('readwrite', (store) => store.put({ token: session.token }, 'session'));

/** The worker must not keep a token the page has dropped. */
export const forgetSession = () => withStore('readwrite', (store) => store.delete('session'));

const keyBytes = (text) => {
  const raw = atob(text.replace(/-/g, '+').replace(/_/g, '/'));
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
};

/** Subscribes this browser and registers the subscription with the hub (idempotent). */
async function subscribe() {
  const registration = await navigator.serviceWorker.ready;
  let subscription = await registration.pushManager.getSubscription();
  if (!subscription) {
    // The spec names the field `key`; the hub answers `public_key` (reported as a discrepancy).
    const served = await request('GET', '/v1/push/vapid-public-key');
    const key = served.key || served.public_key;
    subscription = await registration.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: keyBytes(key),
    });
  }
  const { endpoint, keys } = subscription.toJSON();
  await request('POST', '/v1/push/subscriptions', { body: { endpoint, keys } });
}

/** Sign-out: drop the hub's subscription for this browser and the token the worker holds. */
export async function disablePush() {
  try {
    const registration = await navigator.serviceWorker.getRegistration('/');
    const subscription = registration && (await registration.pushManager.getSubscription());
    if (subscription) {
      await request('DELETE', '/v1/push/subscriptions', { body: { endpoint: subscription.endpoint } }).catch(
        () => {},
      );
      await subscription.unsubscribe();
    }
  } catch {
    // Nothing subscribed, or push is unsupported here.
  }
  await forgetSession();
}

/**
 * Adds the "Enable notifications" button to `bar` (needs a tap: the permission prompt wants a
 * gesture). Once allowed, the subscription is renewed silently on every visit.
 */
export function mountPushButton(bar) {
  if (!supported() || Notification.permission === 'denied') return;
  const button = document.createElement('button');
  button.type = 'button';
  button.id = 'notify';
  const on = () => {
    button.textContent = 'Notifications on';
    button.disabled = true;
  };
  button.textContent = 'Enable notifications';
  button.addEventListener('click', async () => {
    button.disabled = true;
    try {
      if ((await Notification.requestPermission()) !== 'granted') {
        button.remove();
        return;
      }
      await subscribe();
      on();
    } catch {
      button.textContent = 'Could not enable notifications. Try again.';
      button.disabled = false;
    }
  });
  bar.append(button);
  if (Notification.permission === 'granted') {
    subscribe().then(on, () => {});
  }
}
