// Transcript items (transcript.js) to DOM. Event text is untrusted: every string goes in with
// textContent, never as HTML.

import { approvalControls, approvalNote } from '/src/approval.js';

/** `h('div', {class: 'x', 'data-k': 'v'}, child | 'text', ...)`. */
export function h(tag, attrs = {}, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (value !== undefined && value !== null && value !== false) element.setAttribute(name, value);
  }
  for (const child of children.flat()) {
    if (child === undefined || child === null || child === false) continue;
    element.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return element;
}

const chip = (label, tone = 'off') => h('span', { class: `chip tone-${tone}` }, label);

const number = (value) => (typeof value === 'number' ? value.toLocaleString('en-US') : String(value));

const TOOL_TONE = { pending: 'info', in_progress: 'info', completed: 'ok', failed: 'alert' };
const PLAN_TONE = { pending: 'off', in_progress: 'info', completed: 'ok' };

function json(value) {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function author(actor) {
  return actor ? actor.name || actor.id || actor.kind : '';
}

// ---- items ------------------------------------------------------------------------------------

function user(item) {
  return h(
    'article',
    { class: 'item user' },
    h('div', { class: 'item-head' }, h('strong', {}, author(item.author) || 'You')),
    item.text && h('p', { class: 'body' }, item.text),
    item.files.length > 0 &&
      h(
        'ul',
        { class: 'files' },
        item.files.map((file) =>
          h('li', {}, `${file.name || 'file'}${file.size ? ` (${number(file.size)} bytes)` : ''}`),
        ),
      ),
  );
}

function assistantText(item) {
  return h(
    'article',
    { class: 'item text', 'data-open': item.open ? 'true' : 'false' },
    h('p', { class: 'body' }, item.text),
    item.open && h('span', { class: 'streaming' }, 'writing…'),
  );
}

function reasoningItem(item) {
  return h(
    'details',
    { class: 'item reasoning' },
    h('summary', {}, item.open ? 'Reasoning…' : 'Reasoning'),
    h('p', { class: 'body' }, item.text),
  );
}

function toolItem(item) {
  const where = item.locations.map((l) => (l.line ? `${l.path}:${l.line}` : l.path)).join(', ');
  const result = (item.result || [])
    .map((part) => (part.type === 'text' ? part.text : part.name || part.url || part.type))
    .join('\n');
  return h(
    'article',
    { class: 'item tool', 'data-status': item.status },
    h(
      'div',
      { class: 'item-head' },
      h('strong', { class: 'tool-title' }, item.title),
      chip(item.tool_kind, 'off'),
      chip(item.status, TOOL_TONE[item.status] || 'off'),
    ),
    where && h('p', { class: 'muted' }, where),
    (item.raw_input !== undefined || result) &&
      h(
        'details',
        { class: 'tool-details' },
        h('summary', {}, 'Input and result'),
        item.raw_input !== undefined && h('pre', { class: 'code-block' }, json(item.raw_input)),
        result && h('pre', { class: 'code-block tool-result' }, result),
      ),
  );
}

/** Removed lines, then added lines, after trimming the lines the two texts share at both ends. */
function diffLines(before, after) {
  const a = before ? before.split('\n') : [];
  const b = after.split('\n');
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) start += 1;
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA -= 1;
    endB -= 1;
  }
  const lines = [];
  // A trailing newline leaves one empty last element; it is not a line.
  const keep = (list, from, to) => list.slice(from, to).filter((line, i, all) => line !== '' || i < all.length - 1);
  for (const line of keep(a, start, endA)) lines.push(['del', `- ${line}`]);
  for (const line of keep(b, start, endB)) lines.push(['add', `+ ${line}`]);
  return lines;
}

function diffActivity(item) {
  const c = item.content;
  return h(
    'article',
    { class: 'item activity diff' },
    h('div', { class: 'item-head' }, h('strong', {}, 'Edit'), h('code', {}, c.path)),
    h(
      'pre',
      { class: 'code-block diff-lines' },
      diffLines(c.old_text, c.new_text || '').map(([kind, line]) =>
        h('span', { class: `line ${kind}` }, `${line}\n`),
      ),
    ),
  );
}

function planActivity(item) {
  return h(
    'article',
    { class: 'item activity plan' },
    h('div', { class: 'item-head' }, h('strong', {}, 'Plan')),
    h(
      'ol',
      { class: 'plan-entries' },
      (item.content.entries || []).map((entry) =>
        h(
          'li',
          { 'data-status': entry.status },
          h('span', {}, entry.content),
          chip(entry.status.replace('_', ' '), PLAN_TONE[entry.status] || 'off'),
        ),
      ),
    ),
  );
}

function terminalActivity(item) {
  const c = item.content;
  const exit = c.exit
    ? c.exit.signal
      ? `signal ${c.exit.signal}`
      : `exit ${c.exit.exit_code ?? '?'}`
    : '';
  return h(
    'article',
    { class: 'item activity terminal' },
    h(
      'div',
      { class: 'item-head' },
      h('strong', {}, 'Terminal'),
      c.command && h('code', {}, c.command),
      exit && chip(exit, c.exit.exit_code === 0 ? 'ok' : 'alert'),
    ),
    h('pre', { class: 'code-block terminal-output' }, c.output || ''),
  );
}

function policyActivity(item) {
  const c = item.content;
  return h(
    'p',
    { class: 'item activity policy muted' },
    `Policy ${c.decision}: ${c.rule} (${c.tool_call_id})`,
  );
}

function activityItem(item) {
  switch (item.activity_type) {
    case 'diff':
      return diffActivity(item);
    case 'plan':
      return planActivity(item);
    case 'terminal':
      return terminalActivity(item);
    case 'policy':
      return policyActivity(item);
    default:
      return h(
        'article',
        { class: 'item activity' },
        h('strong', {}, `Activity ${item.activity_type}`),
        h('pre', { class: 'code-block' }, json(item.content)),
      );
  }
}

function outcomeText(outcome) {
  if (!outcome) return '';
  if (outcome.type === 'selected') return `selected ${outcome.option_id}`;
  return outcome.type;
}

function interruptItem(item) {
  const resolved = item.outcome;
  return h(
    'article',
    { class: 'item interrupt', 'data-resolved': resolved ? 'true' : 'false' },
    h(
      'div',
      { class: 'item-head' },
      h('strong', {}, item.title),
      chip(resolved ? 'resolved' : 'needs you', resolved ? 'ok' : 'warn'),
    ),
    h(
      'ul',
      { class: 'options' },
      (item.options || []).map((option) => h('li', {}, chip(option.name, 'off'))),
    ),
    resolved &&
      h(
        'p',
        { class: 'muted' },
        `${outcomeText(resolved)}${item.resolved_by ? ` by ${author(item.resolved_by)}` : ''}`,
      ),
    !resolved && approvalControls(item),
    approvalNote(item),
    !resolved && item.expires_at && h('p', { class: 'muted' }, `Expires ${item.expires_at}`),
  );
}

function subagentItem(item) {
  const tone = { running: 'info', finished: 'ok', error: 'alert' }[item.status] || 'off';
  return h(
    'article',
    { class: 'item subagent', 'data-status': item.status },
    h(
      'div',
      { class: 'item-head' },
      h('strong', {}, `Subagent ${item.name || item.key}`),
      chip(item.status, tone),
    ),
    item.result !== undefined && h('pre', { class: 'code-block' }, json(item.result)),
    item.message && h('p', { class: 'muted' }, `${item.message}${item.code ? ` (${item.code})` : ''}`),
  );
}

function stepItem(item) {
  const tone = { running: 'info', success: 'ok' }[item.status] || 'alert';
  return h(
    'p',
    { class: 'item step', 'data-status': item.status },
    h('strong', {}, `Step ${item.name}`),
    item.iteration !== undefined && ` #${item.iteration} `,
    chip(item.status, tone),
  );
}

function checkpointItem(item) {
  return h(
    'p',
    { class: 'item checkpoint muted' },
    `Checkpoint ${item.key}`,
    item.files_changed !== undefined && `: ${number(item.files_changed)} files changed`,
    item.git_ref && h('code', {}, item.git_ref),
  );
}

function presenceItem(item) {
  return h(
    'p',
    { class: 'item presence muted' },
    `Machine ${item.machine_id} is ${item.state}`,
  );
}

function rawItem(item) {
  const inner = item.event && item.event.event ? item.event.event.type : 'event';
  return h(
    'details',
    { class: 'item raw' },
    h('summary', {}, `Raw ${item.source || ''} ${inner}`.trim()),
    h('pre', { class: 'code-block' }, json(item.event)),
  );
}

function runItem(item) {
  const label = item.reason === 'resume' ? 'Run resumed' : 'Run started';
  let state;
  if (item.status === 'finished') {
    const o = item.outcome || {};
    state = chip(`Run finished${o.stop_reason ? `: ${o.stop_reason}` : ''}`, 'ok');
  } else if (item.status === 'interrupted') {
    state = chip('Run paused: waiting for you', 'warn');
  } else if (item.status === 'error') {
    state = chip(`Run error${item.code ? `: ${item.code}` : ''}`, 'alert');
  } else {
    state = chip('Running', 'info');
  }
  return h(
    'div',
    { class: 'item run', 'data-status': item.status },
    h('span', { class: 'run-label' }, label),
    state,
    item.status === 'error' && item.message && h('p', { class: 'run-message' }, item.message),
  );
}

const RENDERERS = {
  user,
  text: assistantText,
  reasoning: reasoningItem,
  tool: toolItem,
  activity: activityItem,
  interrupt: interruptItem,
  subagent: subagentItem,
  step: stepItem,
  checkpoint: checkpointItem,
  presence: presenceItem,
  raw: rawItem,
  run: runItem,
  unknown: (item) => h('p', { class: 'item unknown muted' }, `Unknown event ${item.type}`),
};

/** The element for one transcript item. */
export function renderItem(item) {
  const element = RENDERERS[item.kind](item);
  element.dataset.kind = item.kind;
  element.dataset.key = item.key;
  element.dataset.seq = String(item.last_seq);
  return element;
}

/**
 * Brings `list` in line with `items`: an element is rebuilt only when its item changed, and a
 * collapsed or opened `<details>` keeps the state the person gave it.
 */
export function renderTranscript(list, items) {
  const existing = new Map();
  for (const child of list.children) existing.set(`${child.dataset.kind}:${child.dataset.key}`, child);
  let previous = null;
  for (const item of items) {
    const id = `${item.kind}:${item.key}`;
    let element = existing.get(id);
    if (!element || element.dataset.seq !== String(item.last_seq)) {
      const next = renderItem(item);
      if (element) {
        if (element.tagName === 'DETAILS') next.open = element.open;
        for (const [i, details] of element.querySelectorAll('details').entries()) {
          const inner = next.querySelectorAll('details')[i];
          if (inner) inner.open = details.open;
        }
        element.replaceWith(next);
      }
      element = next;
    }
    if (previous ? element.previousElementSibling !== previous : list.firstElementChild !== element) {
      list.insertBefore(element, previous ? previous.nextSibling : list.firstChild);
    }
    previous = element;
  }
}

// ---- scoped state ---------------------------------------------------------------------------

/** "Context 48,211 of 200,000 tokens · 0.042 USD" for the `usage` state. */
export function usageText(usage) {
  if (!usage) return '';
  const parts = [];
  if (typeof usage.context_used === 'number') {
    parts.push(
      typeof usage.context_size === 'number'
        ? `Context ${number(usage.context_used)} of ${number(usage.context_size)} tokens`
        : `Context ${number(usage.context_used)} tokens`,
    );
  }
  if (usage.cost && typeof usage.cost.amount === 'number') {
    parts.push(`${usage.cost.amount} ${usage.cost.currency || ''}`.trim());
  }
  return parts.join(' · ');
}

/** "Mode Default · 1 command" for the `session` state. */
export function sessionText(session) {
  if (!session) return '';
  const parts = [];
  if (session.current_mode_id) {
    const mode = (session.modes || []).find((m) => m.id === session.current_mode_id);
    parts.push(`Mode ${mode ? mode.name : session.current_mode_id}`);
  }
  const commands = (session.available_commands || []).length;
  if (commands) parts.push(`${commands} command${commands === 1 ? '' : 's'}`);
  return parts.join(' · ');
}
