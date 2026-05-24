import { newId, nowIso, type RunEvent } from '@sprite-machine/shared';
import type { CreateRunInput } from '../runs.js';
import type { Agent, AgentDeps } from './types.js';

export class ClaudeSdkAgent implements Agent {
  constructor(private readonly deps: AgentDeps) {}

  async start(runId: string, input: CreateRunInput): Promise<void> {
    await this.runOnce(runId, input.prompt, {
      ...(input.workdir !== undefined ? { workdir: input.workdir } : {}),
      ...(input.model !== undefined ? { model: input.model } : {}),
      permissionMode: input.permissionMode ?? this.deps.defaultPermissionMode,
      ...(input.allowedTools !== undefined ? { allowedTools: input.allowedTools } : {}),
    });
  }

  async sendUserMessage(runId: string, text: string): Promise<void> {
    const run = this.deps.store.get(runId);
    if (!run) throw new Error('run not found');
    if (run.summary.status === 'running' || run.summary.status === 'starting') {
      throw new Error('run is busy; cancel first or wait for current turn to finish');
    }
    await this.deps.store.appendEvent(runId, { type: 'user_message', text, at: nowIso() });
    await this.runOnce(runId, text, {
      ...(run.summary.workdir !== undefined ? { workdir: run.summary.workdir } : {}),
      permissionMode: this.deps.defaultPermissionMode,
      ...(run.summary.sessionId !== undefined ? { resume: run.summary.sessionId } : {}),
    });
  }

  async cancel(runId: string): Promise<void> {
    const run = this.deps.store.get(runId);
    if (!run?.abortController) return;
    run.abortController.abort();
    await this.deps.store.setStatus(runId, 'cancelled');
  }

  private async runOnce(
    runId: string,
    prompt: string,
    opts: {
      workdir?: string;
      model?: string;
      permissionMode: 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan';
      allowedTools?: string[];
      resume?: string;
    },
  ): Promise<void> {
    const { store } = this.deps;
    const ac = new AbortController();
    store.setAbortController(runId, ac);
    await store.setStatus(runId, 'starting');

    let sdkQuery: typeof import('@anthropic-ai/claude-agent-sdk').query;
    try {
      const sdk = await import('@anthropic-ai/claude-agent-sdk');
      sdkQuery = sdk.query;
    } catch (err) {
      await store.appendEvent(runId, {
        type: 'error',
        message: `Failed to load Claude Agent SDK: ${(err as Error).message}`,
        at: nowIso(),
      });
      await store.setStatus(runId, 'failed');
      store.setAbortController(runId, null);
      return;
    }

    const canUseTool = async (
      toolName: string,
      input: Record<string, unknown>,
    ): Promise<
      | { behavior: 'allow'; updatedInput: Record<string, unknown> }
      | { behavior: 'deny'; message: string }
    > => {
      const requestId = newId('perm');
      const decision = await new Promise<{ allow: boolean; reason?: string }>((resolve) => {
        store.registerPendingPermission(runId, { requestId, toolName, input, resolve });
        void store.appendEvent(runId, {
          type: 'permission_requested',
          requestId,
          toolName,
          input,
          at: nowIso(),
        });
      });
      await store.appendEvent(runId, {
        type: 'permission_resolved',
        requestId,
        decision: decision.allow ? 'allow' : 'deny',
        ...(decision.reason !== undefined ? { reason: decision.reason } : {}),
        at: nowIso(),
      });
      return decision.allow
        ? { behavior: 'allow', updatedInput: input }
        : { behavior: 'deny', message: decision.reason ?? 'denied by user' };
    };

    try {
      const queryOpts: Record<string, unknown> = {
        abortController: ac,
        permissionMode: opts.permissionMode,
        canUseTool,
      };
      queryOpts.cwd = opts.workdir ?? this.deps.defaultWorkdir;
      if (opts.model) queryOpts.model = opts.model;
      if (opts.allowedTools) queryOpts.allowedTools = opts.allowedTools;
      if (opts.resume) queryOpts.resume = opts.resume;

      const stream = sdkQuery({ prompt, options: queryOpts });
      await store.setStatus(runId, 'running');

      for await (const msg of stream) {
        for (const ev of translateClaudeMessage(msg)) {
          await store.appendEvent(runId, ev);
        }
        if (ac.signal.aborted) break;
      }

      if (!ac.signal.aborted) {
        await store.setStatus(runId, 'idle');
      }
    } catch (err) {
      await store.appendEvent(runId, {
        type: 'error',
        message: (err as Error).message,
        at: nowIso(),
      });
      await store.setStatus(runId, 'failed');
    } finally {
      store.setAbortController(runId, null);
    }
  }
}

interface ContentBlock {
  type: string;
  text?: string;
  thinking?: string;
  id?: string;
  name?: string;
  input?: unknown;
  tool_use_id?: string;
  content?: unknown;
  is_error?: boolean;
}

interface SdkLikeMessage {
  type: string;
  subtype?: string;
  session_id?: string;
  model?: string;
  message?: { content?: ContentBlock[] };
  total_cost_usd?: number;
  usage?: { input_tokens?: number; output_tokens?: number };
  result?: string;
}

export function translateClaudeMessage(raw: unknown): RunEvent[] {
  const events: RunEvent[] = [];
  const at = nowIso();
  if (!raw || typeof raw !== 'object') return events;
  const msg = raw as SdkLikeMessage;

  if (msg.type === 'system' && msg.subtype === 'init' && msg.session_id) {
    events.push({
      type: 'session_started',
      sessionId: msg.session_id,
      ...(msg.model !== undefined ? { model: msg.model } : {}),
      at,
    });
    return events;
  }

  if (msg.type === 'assistant' && msg.message?.content) {
    for (const block of msg.message.content) {
      if (block.type === 'text' && typeof block.text === 'string') {
        events.push({ type: 'assistant_text', text: block.text, at });
      } else if (block.type === 'tool_use' && block.id && block.name) {
        events.push({
          type: 'tool_use',
          toolUseId: block.id,
          name: block.name,
          input: block.input ?? {},
          at,
        });
      }
    }
    return events;
  }

  if (msg.type === 'user' && msg.message?.content) {
    for (const block of msg.message.content) {
      if (block.type === 'tool_result' && typeof block.tool_use_id === 'string') {
        events.push({
          type: 'tool_result',
          toolUseId: block.tool_use_id,
          output: block.content ?? null,
          ...(block.is_error !== undefined ? { isError: block.is_error } : {}),
          at,
        });
      }
    }
    return events;
  }

  if (msg.type === 'result') {
    events.push({
      type: 'turn_complete',
      ...(typeof msg.total_cost_usd === 'number' ? { costUsd: msg.total_cost_usd } : {}),
      ...(typeof msg.usage?.input_tokens === 'number'
        ? { inputTokens: msg.usage.input_tokens }
        : {}),
      ...(typeof msg.usage?.output_tokens === 'number'
        ? { outputTokens: msg.usage.output_tokens }
        : {}),
      at,
    });
    if (msg.subtype && msg.subtype !== 'success') {
      events.push({
        type: 'error',
        message: `result subtype: ${msg.subtype}${msg.result ? ` — ${msg.result}` : ''}`,
        at,
      });
    }
    return events;
  }

  return events;
}
