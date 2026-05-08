import { nowIso, type RunEvent } from '@sprite-machine/shared';
import { JsonlSubprocessAgent, type SpawnOpts, type TranslationCtx } from './jsonl-subprocess.js';
import type { AgentDeps } from './types.js';

/**
 * Codex CLI adapter.
 *
 * Invocation: `codex exec --json --skip-git-repo-check [resume <thread_id>] <prompt>`
 * Output (JSONL):
 *   {"type":"thread.started","thread_id":"<uuid>"}
 *   {"type":"turn.started"}
 *   {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
 *   {"type":"item.completed","item":{"type":"function_call",...}}
 *   {"type":"turn.completed","usage":{"input_tokens":N,"output_tokens":M,...}}
 */
export class CodexCliAgent extends JsonlSubprocessAgent {
  override readonly harnessName = 'codex';

  constructor(deps: AgentDeps) {
    super(deps);
  }

  override buildArgs(
    prompt: string,
    opts: SpawnOpts,
  ): { cmd: string; args: string[]; env?: Record<string, string> } {
    const args = ['exec', '--json', '--skip-git-repo-check'];
    if (opts.model) args.push('-m', opts.model);
    if (opts.resumeSessionId) {
      args.push('resume', opts.resumeSessionId);
    }
    args.push(prompt);
    return { cmd: 'codex', args };
  }

  override translateLine(line: object, _ctx: TranslationCtx): RunEvent[] {
    const at = nowIso();
    const msg = line as CodexLine;
    if (msg.type === 'thread.started' && msg.thread_id) {
      return [{ type: 'session_started', sessionId: msg.thread_id, model: 'codex', at }];
    }
    if (msg.type === 'item.completed' && msg.item) {
      if (msg.item.type === 'agent_message' && typeof msg.item.text === 'string') {
        return [{ type: 'assistant_text', text: msg.item.text, at }];
      }
      if (msg.item.type === 'function_call' && msg.item.id && msg.item.name) {
        return [
          {
            type: 'tool_use',
            toolUseId: msg.item.id,
            name: msg.item.name,
            input: msg.item.arguments ?? {},
            at,
          },
        ];
      }
    }
    if (msg.type === 'turn.completed') {
      const events: RunEvent[] = [];
      events.push({
        type: 'turn_complete',
        ...(typeof msg.usage?.input_tokens === 'number'
          ? { inputTokens: msg.usage.input_tokens }
          : {}),
        ...(typeof msg.usage?.output_tokens === 'number'
          ? { outputTokens: msg.usage.output_tokens }
          : {}),
        at,
      });
      return events;
    }
    return [];
  }
}

interface CodexLine {
  type?: string;
  thread_id?: string;
  item?: { type?: string; text?: string; id?: string; name?: string; arguments?: unknown };
  usage?: { input_tokens?: number; output_tokens?: number; cached_input_tokens?: number };
}
