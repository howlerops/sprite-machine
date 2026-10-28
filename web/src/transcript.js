// The transcript model (turn-event.md §2, §4): envelopes in, an ordered list of items out.
// Pure data, no DOM. `apply` drops an envelope whose seq is not newer than the last applied one
// (the subscription may replay from a run boundary, §6), so the same log never shows twice.
//
// Items are keyed, so a later event for the same key updates the item in place:
//   user / text / reasoning  by message_id        tool       by tool_call_id (upsert)
//   activity (diff, plan, terminal, policy)       by activity_id
//   interrupt by interrupt_id   subagent by subagent_run_id   step by step_id + iteration
//   run by run_id   checkpoint by checkpoint_id   presence / raw / unknown: one item per event
// Scoped state (`session`, `usage`, `thread`, `flow`) is kept apart in `states`.

/** Applies an RFC 6902 patch (add, remove, replace, move, copy; `test` is ignored) in place. */
export function applyPatch(document, patch) {
  let root = document;
  const parts = (path) =>
    path === ''
      ? []
      : path
          .split('/')
          .slice(1)
          .map((part) => part.replace(/~1/g, '/').replace(/~0/g, '~'));
  const walk = (keys) => {
    let node = root;
    for (const key of keys) {
      if (node === null || typeof node !== 'object') return undefined;
      node = node[key];
    }
    return node;
  };
  const put = (path, value, replace) => {
    const keys = parts(path);
    if (!keys.length) {
      root = value;
      return;
    }
    const last = keys.pop();
    const parent = walk(keys);
    if (parent === null || typeof parent !== 'object') return;
    if (Array.isArray(parent)) {
      const index = last === '-' ? parent.length : Number(last);
      if (!Number.isInteger(index) || index < 0 || index > parent.length) return;
      if (replace) parent[index] = value;
      else parent.splice(index, 0, value);
    } else {
      parent[last] = value;
    }
  };
  const take = (path) => {
    const keys = parts(path);
    const last = keys.pop();
    const parent = walk(keys);
    if (parent === null || typeof parent !== 'object') return undefined;
    if (Array.isArray(parent)) return parent.splice(Number(last), 1)[0];
    const value = parent[last];
    delete parent[last];
    return value;
  };
  for (const op of patch || []) {
    if (op.op === 'add') put(op.path, structuredClone(op.value), false);
    else if (op.op === 'replace') put(op.path, structuredClone(op.value), true);
    else if (op.op === 'remove') take(op.path);
    else if (op.op === 'move') put(op.path, take(op.from), false);
    else if (op.op === 'copy') put(op.path, structuredClone(walk(parts(op.from))), false);
  }
  return root;
}

export class Transcript {
  constructor() {
    this.reset();
  }

  reset() {
    this.lastSeq = 0;
    this.items = [];
    this.states = {};
    this.index = new Map();
    this.version = 0;
  }

  /** Replaces the scoped state (the hub's `snapshot` message: `{session, usage, thread, flow}`). */
  setStates(states) {
    this.states = structuredClone(states || {});
    this.version += 1;
  }

  /** Applies one stored envelope. `false` when it was already applied (seq not newer). */
  apply(envelope) {
    if (!envelope || typeof envelope.seq !== 'number' || envelope.seq <= this.lastSeq) return false;
    this.lastSeq = envelope.seq;
    this.version += 1;
    const event = envelope.event || {};
    const handler = HANDLERS[event.type];
    if (handler) handler(this, event, envelope);
    else this.push('unknown', null, envelope, { type: event.type || 'unknown' });
    return true;
  }

  /** The item for `key` of `kind`, created at the end of the list when new. */
  upsert(kind, key, envelope, fields = {}) {
    const id = `${kind}:${key}`;
    let item = this.index.get(id);
    if (!item) {
      item = { kind, key, seq: envelope.seq, run_id: envelope.run_id, ts: envelope.ts };
      this.index.set(id, item);
      this.items.push(item);
    }
    Object.assign(item, fields);
    item.last_seq = envelope.seq;
    return item;
  }

  /** An item that is never updated (its key is its seq). */
  push(kind, _key, envelope, fields) {
    return this.upsert(kind, `seq${envelope.seq}`, envelope, fields);
  }

  find(kind, key) {
    return this.index.get(`${kind}:${key}`);
  }
}

const textOf = (parts) =>
  (parts || [])
    .filter((part) => part && part.type === 'text')
    .map((part) => part.text)
    .join('\n');

function message(kind) {
  return {
    start: (t, e, env) =>
      t.upsert(kind, e.message_id, env, { role: e.role || kind, text: '', open: true }),
    content: (t, e, env) => {
      const item = t.upsert(kind, e.message_id, env, {});
      item.text = (item.text || '') + (e.delta || '');
      item.open ??= true;
    },
    end: (t, e, env) => t.upsert(kind, e.message_id, env, { open: false }),
  };
}
const text = message('text');
const reasoning = message('reasoning');

function tool(t, e, env, fields) {
  return t.upsert('tool', e.tool_call_id, env, fields);
}

/** Fields of an activity item from its content; `append` extends strings, `patch` edits. */
function activity(t, e, env) {
  const key = e.activity_id;
  const existing = t.find('activity', key);
  let content = existing ? existing.content : {};
  if (e.mode === 'snapshot') {
    content = structuredClone(e.content || {});
  } else if (e.mode === 'append') {
    content = { ...content };
    for (const [name, value] of Object.entries(e.content || {})) {
      content[name] =
        typeof value === 'string' && typeof content[name] === 'string'
          ? content[name] + value
          : value;
    }
  } else if (e.mode === 'patch') {
    content = applyPatch(structuredClone(content), e.patch);
  }
  t.upsert('activity', key, env, { activity_type: e.activity_type, mode: e.mode, content });
}

function patchState(t, scope, fn) {
  t.states = { ...t.states, [scope]: fn(t.states[scope]) };
}

const HANDLERS = {
  UserMessage: (t, e, env) =>
    t.upsert('user', e.message_id, env, {
      text: textOf(e.parts),
      files: (e.parts || []).filter((part) => part.type === 'file'),
      author: e.author || null,
    }),
  TextMessageStart: text.start,
  TextMessageContent: text.content,
  TextMessageEnd: text.end,
  ReasoningMessageStart: reasoning.start,
  ReasoningMessageContent: reasoning.content,
  ReasoningMessageEnd: reasoning.end,
  ToolCallStart: (t, e, env) =>
    tool(t, e, env, {
      title: e.title || e.tool_call_id,
      tool_kind: e.kind || 'other',
      status: e.status || 'pending',
      locations: e.locations || [],
    }),
  ToolCallArgs: (t, e, env) => tool(t, e, env, { raw_input: e.raw_input }),
  ToolCallResult: (t, e, env) => tool(t, e, env, { result: e.content || [] }),
  ToolCallEnd: (t, e, env) => tool(t, e, env, { status: e.status || 'completed', ended: true }),
  Activity: activity,
  Interrupt: (t, e, env) =>
    t.upsert('interrupt', e.interrupt_id, env, {
      title: e.title,
      interrupt_kind: e.kind,
      options: e.options || [],
      expires_at: e.expires_at,
      tool_call_id: e.tool_call_id,
    }),
  InterruptResolved: (t, e, env) =>
    t.upsert('interrupt', e.interrupt_id, env, {
      outcome: e.outcome,
      resolved_by: e.resolved_by || null,
    }),
  SubagentStarted: (t, e, env) =>
    t.upsert('subagent', e.subagent_run_id, env, {
      name: e.name,
      status: 'running',
      child_thread_id: e.child_thread_id,
      agent_id: e.agent_id,
    }),
  SubagentFinished: (t, e, env) =>
    t.upsert('subagent', e.subagent_run_id, env, { status: 'finished', result: e.result }),
  SubagentError: (t, e, env) =>
    t.upsert('subagent', e.subagent_run_id, env, {
      status: 'error',
      message: e.message,
      code: e.code,
    }),
  StepStarted: (t, e, env) =>
    t.upsert('step', `${e.step_id}#${e.iteration ?? 0}`, env, {
      name: e.step_name || e.step_id,
      iteration: e.iteration,
      status: 'running',
    }),
  StepFinished: (t, e, env) =>
    t.upsert('step', `${e.step_id}#${e.iteration ?? 0}`, env, {
      name: e.step_name || e.step_id,
      iteration: e.iteration,
      status: e.outcome || 'success',
    }),
  Checkpoint: (t, e, env) =>
    t.upsert('checkpoint', e.checkpoint_id, env, {
      checkpoint_kind: e.kind,
      git_ref: e.git_ref,
      files_changed: e.files_changed,
    }),
  Presence: (t, e, env) =>
    t.push('presence', null, env, { machine_id: e.machine_id, state: e.state, since: e.since }),
  Raw: (t, e, env) => t.push('raw', null, env, { source: e.source, event: e.event }),
  RunStarted: (t, e, env) =>
    t.upsert('run', env.run_id, env, {
      reason: e.reason,
      status: 'running',
      machine_id: e.machine_id,
      agent_id: e.agent_id,
      parent_run_id: e.parent_run_id,
    }),
  RunFinished: (t, e, env) => {
    const interrupted = e.outcome && e.outcome.type === 'interrupt';
    t.upsert('run', env.run_id, env, {
      status: interrupted ? 'interrupted' : 'finished',
      outcome: e.outcome,
    });
  },
  RunError: (t, e, env) =>
    t.upsert('run', env.run_id, env, {
      status: 'error',
      reason_code: e.reason,
      code: e.code,
      message: e.message,
    }),
  StateSnapshot: (t, e) => patchState(t, e.scope, () => structuredClone(e.snapshot)),
  StateDelta: (t, e) =>
    patchState(t, e.scope, (state) => applyPatch(structuredClone(state ?? {}), e.patch)),
};
