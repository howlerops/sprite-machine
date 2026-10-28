// The service worker (WEB-004): caches the app shell so the page opens offline, shows Web Push
// notifications and answers their Allow / Deny actions (client-api.md §5.8). Classic script.
//
// - API responses (/v1/*) are never cached: only the shell files below.
// - A push carries ids only. The worker shows the payload's generic title, and adds the thread's
//   title when it can fetch it with the session token the page stored in IndexedDB (database
//   `sm-push`, store `kv`, key `session`). The token is only ever sent in an Authorization header.
// - Allow / Deny use the option ids the payload carries and an Idempotency-Key; no unauthenticated
//   route exists, so without a stored session (or on any failure) the click opens the thread.

const CACHE = 'sm-shell-v1';
const SHELL = [
  '/',
  '/styles.css',
  '/manifest.webmanifest',
  '/icons/icon-192.png',
  '/icons/icon-512.png',
  '/src/api.js',
  '/src/app.js',
  '/src/approval.js',
  '/src/composer.js',
  '/src/outbox.js',
  '/src/pairing.js',
  '/src/push.js',
  '/src/render.js',
  '/src/sessions.js',
  '/src/thread.js',
  '/src/transcript.js',
];
const ID = /^[A-Za-z0-9_.-]{1,128}$/;

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL.map((path) => new Request(path, { cache: 'reload' }))))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) => Promise.all(names.filter((name) => name !== CACHE).map((name) => caches.delete(name))))
      .then(() => self.clients.claim()),
  );
});

// Network first, so a new deploy is never hidden by an old copy; the cache answers when offline.
self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  const navigation = request.mode === 'navigate';
  if (!navigation && !SHELL.includes(url.pathname)) return;
  event.respondWith(
    fetch(request)
      .then((response) => {
        if (!navigation && response.ok) {
          const copy = response.clone();
          event.waitUntil(caches.open(CACHE).then((cache) => cache.put(url.pathname, copy)));
        }
        return response;
      })
      .catch(async () => {
        const hit = await caches.match(navigation ? '/' : url.pathname);
        return hit || Response.error();
      }),
  );
});

// ---- the session token, kept by the page for this worker -------------------------------------

function storedToken() {
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
      const get = db.transaction('kv').objectStore('kv').get('session');
      get.onsuccess = () => {
        db.close();
        const value = get.result;
        resolve(value && typeof value.token === 'string' ? value.token : null);
      };
      get.onerror = () => {
        db.close();
        resolve(null);
      };
    };
  });
}

// ---- push ------------------------------------------------------------------------------------

async function threadTitle(threadId) {
  const token = await storedToken();
  if (!token || !ID.test(threadId)) return '';
  try {
    const reply = await fetch(`/v1/threads/${encodeURIComponent(threadId)}`, {
      headers: { authorization: `Bearer ${token}` },
      cache: 'no-store',
    });
    if (!reply.ok) return '';
    const thread = await reply.json();
    return typeof thread.title === 'string' ? thread.title : '';
  } catch {
    return '';
  }
}

async function showPush(event) {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch {
    // Not JSON: a generic notification.
  }
  const threadId = ID.test(String(data.thread_id || '')) ? data.thread_id : '';
  const interruptId = ID.test(String(data.interrupt_id || '')) ? data.interrupt_id : '';
  const offered = Array.isArray(data.actions) ? data.actions : [];
  const actions = offered
    .filter((a) => a && typeof a.action === 'string' && typeof a.title === 'string' && ID.test(String(a.option_id || '')))
    .slice(0, Math.max(0, Number(self.Notification && Notification.maxActions) || 2));
  const options = {
    body: threadId ? await threadTitle(threadId) : '',
    icon: '/icons/icon-192.png',
    tag: String(data.task_id || threadId || 'sprite-machine'),
    data: {
      thread_id: threadId,
      interrupt_id: interruptId,
      options: Object.fromEntries(actions.map((a) => [a.action, a.option_id])),
    },
    actions: actions.map((a) => ({ action: a.action, title: a.title })),
  };
  await self.registration.showNotification(typeof data.title === 'string' && data.title ? data.title : 'Sprite Machine', options);
}

self.addEventListener('push', (event) => {
  event.waitUntil(showPush(event));
});

/** Opens (or reuses a window of) the thread page `/t/<id>`; the hub redirects it to `/#/t/<id>`. */
async function openThread(threadId) {
  const url = threadId ? `/t/${encodeURIComponent(threadId)}` : '/';
  const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  const client = windows[0];
  if (client) {
    try {
      await client.focus();
    } catch {
      // Focus needs a user gesture; navigating still works.
    }
    try {
      await client.navigate(url);
      return;
    } catch {
      // Fall through to a new window.
    }
  }
  await self.clients.openWindow(url);
}

/** Answers the interrupt with the option the action stands for; false when it did not work out. */
async function answer(threadId, interruptId, optionId) {
  const token = await storedToken();
  if (!token || !ID.test(threadId) || !ID.test(interruptId) || !ID.test(optionId)) return false;
  try {
    const reply = await fetch(
      `/v1/threads/${encodeURIComponent(threadId)}/interrupts/${encodeURIComponent(interruptId)}`,
      {
        method: 'POST',
        headers: {
          authorization: `Bearer ${token}`,
          'content-type': 'application/json',
          // One intent per (interrupt, option): a retried click is the same answer.
          'idempotency-key': `sw-${interruptId}-${optionId}`,
        },
        body: JSON.stringify({ outcome: { type: 'selected', option_id: optionId } }),
        cache: 'no-store',
      },
    );
    return reply.ok;
  } catch {
    return false;
  }
}

async function clicked(event) {
  const notification = event.notification;
  const info = notification.data || {};
  notification.close();
  const optionId = event.action && info.options ? info.options[event.action] : '';
  if (optionId && (await answer(info.thread_id, info.interrupt_id, optionId))) return;
  // A body click, or an answer that did not go through: show the thread so the person can look.
  await openThread(info.thread_id);
}

self.addEventListener('notificationclick', (event) => {
  const done = clicked(event);
  try {
    event.waitUntil(done);
  } catch {
    // Only a synthetic event (the browser tests) refuses waitUntil; the work still runs.
  }
});
