// The composer under the transcript (plan §4.4, client-api §5.3, §5.5, §5.6): write a prompt, attach
// files or a photo, send, and stop the run.
//
// - A prompt goes to the outbox first (outbox.js) and is sent from there, so it survives going
//   offline and a reload. While a task runs the hub queues it: the queued prompts show as chips
//   that can be removed (DELETE .../pending/{task_id}). Prompts still waiting in the outbox show
//   as chips too.
// - A file is uploaded as soon as it is picked: POST /v1/attachments, then PUT the bytes to the
//   signed URL (at most 20 MB); the hub's FilePart goes into the prompt.
// - Stop cancels the task that is running (POST .../cancel).

import { ApiError } from '/src/api.js';
import { h } from '/src/render.js';
import {
  enqueue,
  keyedRequest,
  newId,
  onOutboxChange,
  readOutbox,
  removeEntry,
  startOutbox,
  flushOutbox,
} from '/src/outbox.js';

export const MAX_BYTES = 20 * 1024 * 1024;

/** The `type/subtype` the hub accepts for a picked file. */
export function mimeOf(file) {
  const mime = String(file.type || '').split(';')[0].trim().toLowerCase();
  return /^[a-z0-9.+-]{1,127}\/[a-z0-9.+-]{1,127}$/.test(mime) ? mime : 'application/octet-stream';
}

/** The task that is running, from the envelopes seen so far (null when none). */
export class TaskTracker {
  constructor() {
    this.task = null;
  }

  observe(envelope) {
    const event = (envelope && envelope.event) || {};
    const task = envelope && envelope.task_id;
    if (!task) return;
    if (event.type === 'UserMessage' || event.type === 'RunStarted') {
      this.task = task;
    } else if (this.task === task) {
      const paused = event.type === 'RunFinished' && event.outcome && event.outcome.type === 'interrupt';
      if ((event.type === 'RunFinished' && !paused) || event.type === 'RunError') this.task = null;
    }
  }
}

async function upload(file) {
  if (file.size === 0) throw new Error('This file is empty.');
  if (file.size > MAX_BYTES) throw new Error('This file is over 20 MB.');
  const mime = mimeOf(file);
  const created = await keyedRequest('POST', '/v1/attachments', {
    body: { name: (file.name || 'file').slice(0, 255), mime_type: mime, size: file.size },
  });
  const put = await fetch(created.upload_url, {
    method: 'PUT',
    headers: { 'content-type': mime },
    body: file,
  });
  if (!put.ok) throw new Error(`The upload failed (${put.status}).`);
  return created.part;
}

const previewOf = (text, parts) =>
  text || parts.map((part) => part.name).filter(Boolean).join(', ') || 'Attachment';

/** `{el, observe, setPending, stop}`: the composer form and what the thread view feeds it. */
export function mountComposer(threadId) {
  const tracker = new TaskTracker();
  let pending = []; // thread.pending_tasks from the subscription
  let attachments = []; // {id, name, status: 'uploading'|'ready'|'error', part?, error?}
  let note = '';
  let stopping = false;

  const text = h('textarea', {
    id: 'composer-text',
    rows: '2',
    placeholder: 'Message',
    'aria-label': 'Message',
    enterkeyhint: 'enter',
  });
  const attachInput = h('input', { id: 'attach-input', type: 'file', multiple: true, hidden: true });
  const cameraInput = h('input', {
    id: 'camera-input',
    type: 'file',
    accept: 'image/*',
    capture: 'environment',
    hidden: true,
  });
  const chips = h('ul', { id: 'pending-chips', class: 'pending-chips', 'aria-label': 'Queued messages' });
  const files = h('ul', { id: 'attachments', class: 'pending-chips', 'aria-label': 'Attachments' });
  const status = h('p', { id: 'composer-note', class: 'muted', role: 'status' });
  const send = h('button', { type: 'submit', id: 'send', class: 'primary' }, 'Send');
  const stop = h('button', { type: 'button', id: 'stop', hidden: true }, 'Stop');
  const el = h(
    'form',
    { id: 'composer', class: 'composer', autocomplete: 'off' },
    chips,
    files,
    text,
    h(
      'div',
      { class: 'composer-bar' },
      h('label', { class: 'button', for: 'attach-input' }, 'Attach'),
      h('label', { class: 'button', for: 'camera-input' }, 'Camera'),
      stop,
      send,
    ),
    status,
    attachInput,
    cameraInput,
  );

  function chip(attrs, label, removeLabel, onRemove, extra) {
    const remove = h('button', { type: 'button', class: 'chip-remove', 'aria-label': removeLabel }, 'Remove');
    remove.addEventListener('click', onRemove);
    return h('li', attrs, h('span', { class: 'chip-text' }, label), extra, remove);
  }

  function render() {
    const waiting = readOutbox().filter((entry) => entry.thread_id === threadId);
    chips.replaceChildren(
      ...pending.map((task) => {
        const label = task.preview || task.task_id;
        return chip({ 'data-task': task.task_id, class: 'pending-chip' }, label, `Remove queued message: ${label}`, () =>
          removePending(task.task_id),
        );
      }),
      ...waiting.map((entry) =>
        chip(
          { 'data-outbox': entry.key, 'data-state': entry.error ? 'failed' : 'waiting', class: 'pending-chip' },
          entry.preview,
          `Discard waiting message: ${entry.preview}`,
          () => removeEntry(entry.key),
          h('small', { class: 'muted' }, entry.error ? ` Not sent: ${entry.error}` : ' Waiting to send'),
        ),
      ),
    );
    files.replaceChildren(
      ...attachments.map((file) =>
        chip(
          { 'data-attachment': file.id, 'data-state': file.status, class: 'pending-chip' },
          file.name,
          `Remove attachment: ${file.name}`,
          () => {
            attachments = attachments.filter((other) => other.id !== file.id);
            render();
          },
          h(
            'small',
            { class: 'muted' },
            file.status === 'uploading' ? ' Uploading…' : file.status === 'error' ? ` ${file.error}` : ' Ready',
          ),
        ),
      ),
    );
    chips.hidden = chips.children.length === 0;
    files.hidden = files.children.length === 0;
    status.textContent = note;
    stop.hidden = !tracker.task;
    stop.disabled = stopping;
    const uploading = attachments.some((file) => file.status === 'uploading');
    send.disabled = uploading;
  }

  async function removePending(taskId) {
    note = '';
    try {
      await keyedRequest(
        'DELETE',
        `/v1/threads/${encodeURIComponent(threadId)}/pending/${encodeURIComponent(taskId)}`,
      );
    } catch (error) {
      if (!(error instanceof ApiError && error.status === 404)) {
        note = `Could not remove it${error instanceof ApiError ? ` (${error.status} ${error.code})` : ''}. Try again.`;
      }
    }
    render();
  }

  async function attach(list) {
    for (const file of list) {
      const entry = { id: newId('att'), name: file.name || 'file', status: 'uploading' };
      attachments.push(entry);
      render();
      try {
        entry.part = await upload(file);
        entry.status = 'ready';
      } catch (error) {
        entry.status = 'error';
        entry.error = error instanceof ApiError || error instanceof Error
          ? error.name === 'TypeError' ? 'No connection. Remove it and attach again.' : error.message
          : 'The upload failed.';
      }
      render();
    }
  }
  for (const input of [attachInput, cameraInput]) {
    input.addEventListener('change', () => {
      const picked = Array.from(input.files || []);
      input.value = '';
      attach(picked);
    });
  }

  el.addEventListener('submit', (event) => {
    event.preventDefault();
    const typed = text.value.trim();
    const ready = attachments.filter((file) => file.status === 'ready');
    if (attachments.some((file) => file.status === 'uploading')) return;
    const parts = [];
    if (typed) parts.push({ type: 'text', text: typed });
    for (const file of ready) parts.push(file.part);
    if (!parts.length) {
      note = 'Write a message or attach a file.';
      render();
      return;
    }
    enqueue(
      threadId,
      { task_id: newId('tk'), message_id: newId('msg'), parts },
      previewOf(typed, ready.map((file) => file.part)),
    );
    text.value = '';
    attachments = attachments.filter((file) => file.status === 'error');
    note = '';
    render();
    flushOutbox();
  });

  stop.addEventListener('click', async () => {
    const task = tracker.task;
    if (!task || stopping) return;
    stopping = true;
    note = '';
    render();
    try {
      await keyedRequest('POST', `/v1/threads/${encodeURIComponent(threadId)}/cancel`, { body: { task_id: task } });
    } catch {
      note = 'Could not stop the run. Try again.';
    }
    stopping = false;
    render();
  });

  const unsubscribe = onOutboxChange(render);
  const stopOutbox = startOutbox();
  render();

  return {
    el,
    observe(envelope) {
      const before = tracker.task;
      tracker.observe(envelope);
      if (tracker.task !== before) render();
    },
    setPending(list) {
      pending = Array.isArray(list) ? list : [];
      render();
    },
    stop() {
      unsubscribe();
      stopOutbox();
    },
  };
}
