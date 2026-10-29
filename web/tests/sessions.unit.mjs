// Unit tests for the session list's status chips (web/src/sessions.js). No browser: the list's
// HTML is rendered from plain data. Run: node --test tests/sessions.unit.mjs

import assert from 'node:assert/strict';
import { register } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { test } from 'node:test';

// sessions.js imports the hub client by its site path, /src/api.js, which Node cannot resolve.
// Map that one specifier to the file in web/src.
const apiPath = fileURLToPath(new URL('../src/api.js', import.meta.url));
const hook = `export async function resolve(specifier, context, next) {
  if (specifier === '/src/api.js') {
    return { url: ${JSON.stringify(pathToFileURL(apiPath).href)}, shortCircuit: true };
  }
  return next(specifier, context);
}`;
register(`data:text/javascript,${encodeURIComponent(hook)}`);
const { listHtml } = await import('../src/sessions.js');

const machines = [{ id: 'mch_1', name: 'Laptop', presence: 'online' }];

/** The chips in the list, as [status class, label] pairs. */
function chips(status) {
  const threads = [
    {
      thread_id: 'th_1',
      title: 'A task',
      machine_id: 'mch_1',
      status,
      updated_at: '2026-10-28T10:00:00.000Z',
    },
  ];
  const html = listHtml(machines, threads, Date.parse('2026-10-28T10:05:00.000Z'));
  return [...html.matchAll(/class="chip status-([^"]+)">([^<]*)</g)].map(([, cls, label]) => [cls, label]);
}

test('a failed thread is labelled Failed', () => {
  assert.deepEqual(chips('failed'), [['failed', 'Failed']]);
});

test('a cancelled thread is labelled Cancelled', () => {
  assert.deepEqual(chips('cancelled'), [['cancelled', 'Cancelled']]);
});

test('the existing statuses keep their labels', () => {
  assert.deepEqual(chips('idle'), [['idle', 'Idle']]);
  assert.deepEqual(chips('running'), [['running', 'Running']]);
  assert.deepEqual(chips('needs_you'), [['needs_you', 'Needs you']]);
  assert.deepEqual(chips('done'), [['done', 'Done']]);
});

test('an unknown status is shown as Done, including names inherited from Object.prototype', () => {
  for (const unknown of ['paused', '', 'constructor', '__proto__']) {
    assert.deepEqual(chips(unknown), [['done', 'Done']], `status ${JSON.stringify(unknown)}`);
  }
});
