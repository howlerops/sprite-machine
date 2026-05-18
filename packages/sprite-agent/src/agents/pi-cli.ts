import { existsSync, readdirSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import { nowIso, type RunEvent } from '@sprite-machine/shared';
import { JsonlSubprocessAgent, type SpawnOpts, type TranslationCtx } from './jsonl-subprocess.js';
import type { AgentDeps } from './types.js';

/**
 * Pi (oh-my-pi) CLI adapter.
 *
 * Invocation: `pi -p --mode json [--resume <id-prefix>] [--model <m>] <prompt>`
 * Output (JSONL):
 *   {"type":"session","id":"<uuid>","cwd":"...","timestamp":"..."}
 *   {"type":"agent_start"}
 *   {"type":"turn_start"}
 *   {"type":"message_start","message":{"role":"user|assistant",...}}
 *   {"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"..."}}
 *   {"type":"message_end","message":{"role":"assistant","content":[{type:"text",text:"..."}],"usage":{input,output,...,cost:{...}}}}
 *   {"type":"turn_end","message":{...,usage:{...}}}
 *   {"type":"agent_end","messages":[...]}
 */
export class PiCliAgent extends JsonlSubprocessAgent {
  override readonly harnessName = 'pi';

  constructor(deps: AgentDeps) {
    super(deps);
  }

  override buildArgs(
    prompt: string,
    opts: SpawnOpts,
  ): { cmd: string; args: string[]; env?: Record<string, string> } {
    const args = ['-p', '--mode', 'json'];
    if (opts.model) args.push('--model', opts.model);
    if (opts.resumeSessionId) {
      // pi `--resume` is INTERACTIVE (opens a picker) — wrong for our use case.
      // Use `--session <full-path>` instead, found by globbing for the file
      // whose name ends with `_<sessionId>.jsonl`.
      const sessionPath = findPiSessionFile(opts.resumeSessionId);
      if (sessionPath) args.push('--session', sessionPath);
      // else: silently fall back to a fresh session (the prior turn's context
      // is unreachable). This shouldn't happen if pi finished writing the file.
    }
    args.push(prompt);
    // Default to `pi` (the binary installed by `@mariozechner/pi-coding-agent`,
    // which is what the user has locally). Override via PI_CLI_BIN if running
    // a different fork (e.g. `omp` from `@oh-my-pi/pi-coding-agent`).
    const cmd = process.env.PI_CLI_BIN ?? 'pi';
    return { cmd, args };
  }

  override translateLine(line: object, _ctx: TranslationCtx): RunEvent[] {
    const at = nowIso();
    const msg = line as PiLine;
    if (msg.type === 'session' && msg.id) {
      return [{ type: 'session_started', sessionId: msg.id, model: 'pi', at }];
    }
    if (msg.type === 'message_update' && msg.assistantMessageEvent?.type === 'text_delta') {
      const delta = msg.assistantMessageEvent.delta;
      return typeof delta === 'string' && delta.length > 0
        ? [{ type: 'assistant_delta', text: delta, at }]
        : [];
    }
    if (msg.type === 'message_end' && msg.message?.role === 'assistant') {
      const events: RunEvent[] = [];
      if (msg.message.stopReason === 'error' && msg.message.errorMessage) {
        events.push({ type: 'error', message: friendlyPiError(msg.message.errorMessage), at });
      }
      const content = msg.message.content ?? [];
      const text = content
        .filter((b) => b.type === 'text' && typeof b.text === 'string')
        .map((b) => b.text!)
        .join('');
      if (text) events.push({ type: 'assistant_text', text, at });
      // tool calls — pi emits these as content blocks of type=tool_use (when present)
      for (const block of content) {
        if (block.type === 'tool_use' && block.id && block.name) {
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
    if (msg.type === 'turn_end' && msg.message?.usage) {
      const u = msg.message.usage;
      return [
        {
          type: 'turn_complete',
          ...(typeof u.input === 'number' ? { inputTokens: u.input } : {}),
          ...(typeof u.output === 'number' ? { outputTokens: u.output } : {}),
          ...(typeof u.cost?.total === 'number' ? { costUsd: u.cost.total } : {}),
          at,
        },
      ];
    }
    return [];
  }
}

function friendlyPiError(message: string): string {
  if (/No API key for provider: openai-codex/i.test(message)) {
    return 'Pi is not authenticated with its model provider. Reconnect Codex/OpenAI auth for the worker sprite, then try again.';
  }
  return message;
}

function findPiSessionFile(sessionId: string): string | null {
  const root = path.join(homedir(), '.pi', 'agent', 'sessions');
  if (!existsSync(root)) return null;
  try {
    for (const dir of readdirSync(root)) {
      const subdir = path.join(root, dir);
      try {
        if (!statSync(subdir).isDirectory()) continue;
        for (const f of readdirSync(subdir)) {
          if (f.endsWith(`_${sessionId}.jsonl`)) {
            return path.join(subdir, f);
          }
        }
      } catch {
        // skip unreadable
      }
    }
  } catch {
    // skip
  }
  return null;
}

interface PiContentBlock {
  type?: string;
  text?: string;
  id?: string;
  name?: string;
  input?: unknown;
}

interface PiUsage {
  input?: number;
  output?: number;
  cacheRead?: number;
  cacheWrite?: number;
  totalTokens?: number;
  cost?: { input?: number; output?: number; total?: number };
}

interface PiLine {
  type?: string;
  id?: string;
  cwd?: string;
  message?: {
    role?: string;
    content?: PiContentBlock[];
    usage?: PiUsage;
    stopReason?: string;
    errorMessage?: string;
  };
  assistantMessageEvent?: { type?: string; delta?: string; contentIndex?: number };
}
