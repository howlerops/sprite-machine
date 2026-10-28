// Allow / Deny on an approval interrupt (client-api.md §5.4). The first answer wins: a 409 means
// someone else (or another device) answered first and the note says who. The buttons come from
// the interrupt's own options; the transcript then shows the resolved state from the
// subscription.

import { ApiError } from '/src/api.js';
import { keyedRequest } from '/src/outbox.js';

let threadId = null;
const notes = new Map(); // interrupt_id -> note text
const busy = new Set(); // answers in flight
const closed = new Set(); // answered, or found already answered

/** Tells the controls which thread is on screen. */
export function setApprovalThread(id) {
  threadId = id;
  notes.clear();
  busy.clear();
  closed.clear();
}

/** "Web Owner" for the actor that resolved an interrupt. */
function whoText(actor) {
  return (actor && (actor.name || actor.id || actor.kind)) || 'someone else';
}

function paint(id) {
  for (const note of document.querySelectorAll('.approval-note')) {
    if (note.dataset.interrupt === id) note.textContent = notes.get(id) || '';
  }
  for (const box of document.querySelectorAll('.approval')) {
    if (box.dataset.interrupt !== id) continue;
    for (const button of box.querySelectorAll('button')) {
      button.disabled = busy.has(id) || closed.has(id);
    }
  }
}

async function answer(id, option) {
  if (busy.has(id) || closed.has(id)) return;
  busy.add(id);
  notes.set(id, 'Sending…');
  paint(id);
  try {
    await keyedRequest(
      'POST',
      `/v1/threads/${encodeURIComponent(threadId)}/interrupts/${encodeURIComponent(id)}`,
      { body: { outcome: { type: 'selected', option_id: option.option_id } } },
    );
    closed.add(id);
    notes.set(id, `Sent: ${option.name}`);
  } catch (error) {
    if (error instanceof ApiError && error.status === 409) {
      closed.add(id);
      notes.set(id, `This was already resolved by ${whoText(error.body.resolved_by)}.`);
    } else if (error instanceof ApiError && error.status === 410) {
      closed.add(id);
      notes.set(id, 'This request expired.');
    } else if (error instanceof ApiError && error.status === 404) {
      closed.add(id);
      notes.set(id, 'This request no longer exists.');
    } else if (error instanceof ApiError) {
      notes.set(id, `Could not send (${error.status}). Try again.`);
    } else {
      notes.set(id, 'No connection. Try again.');
    }
  } finally {
    busy.delete(id);
    paint(id);
  }
}

/** The buttons for an open approval item, or null. Event text goes in with textContent only. */
export function approvalControls(item) {
  if (item.interrupt_kind !== 'approval' || !(item.options || []).length) return null;
  const box = document.createElement('div');
  box.className = 'approval';
  box.dataset.interrupt = item.key;
  for (const option of item.options) {
    const button = document.createElement('button');
    button.type = 'button';
    button.dataset.option = option.option_id;
    if (String(option.kind || '').startsWith('allow')) button.className = 'primary';
    button.textContent = option.name;
    button.disabled = busy.has(item.key) || closed.has(item.key);
    button.addEventListener('click', () => answer(item.key, option));
    box.append(button);
  }
  return box;
}

/**
 * What came of this page's answer ("already resolved by Sam"). It is shown for an open and a
 * resolved item alike: the resolution can arrive on the subscription before the 409 does.
 */
export function approvalNote(item) {
  const note = document.createElement('p');
  note.className = 'approval-note muted';
  note.dataset.interrupt = item.key;
  note.setAttribute('role', 'status');
  note.textContent = notes.get(item.key) || '';
  return note;
}
