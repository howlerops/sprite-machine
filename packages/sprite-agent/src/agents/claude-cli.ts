import { type RunEvent } from '@sprite-machine/shared';
import { translateClaudeMessage } from './claude-sdk.js';
import { JsonlSubprocessAgent, type SpawnOpts, type TranslationCtx } from './jsonl-subprocess.js';
import type { AgentDeps } from './types.js';

/**
 * Claude CLI adapter — uses your Claude Code Pro/Max OAuth login.
 *
 * Invocation: `claude --print --output-format stream-json --verbose [--resume <id>] <prompt>`
 *
 * Why this over the Agent SDK: the SDK is API-key-only. The `claude` CLI uses
 * the same OAuth login (`claude /login`) you use locally, picks up your
 * installed plugins (claude-code-plugins, claude-code-workflows, omc, etc.),
 * and matches the codex/pi subprocess pattern.
 *
 * Output is JSONL with the same shape the Agent SDK emits:
 *   {"type":"system","subtype":"init",...}
 *   {"type":"assistant","message":{"content":[...]}}
 *   {"type":"user","message":{"content":[{"type":"tool_result",...}]}}
 *   {"type":"result","subtype":"success","total_cost_usd":N,"usage":{...}}
 *
 * The translator (`translateClaudeMessage`) is reused as-is from claude-sdk.ts.
 *
 * Auth: the sprite's `claude` must be logged in. SSH in once and run `claude /login`.
 * Credentials persist at `~/.claude/.credentials.json` (Linux) and survive Sprites
 * checkpoints.
 */
export class ClaudeCliAgent extends JsonlSubprocessAgent {
  override readonly harnessName = 'claude';

  constructor(deps: AgentDeps) {
    super(deps);
  }

  override buildArgs(
    prompt: string,
    opts: SpawnOpts,
  ): { cmd: string; args: string[]; env?: Record<string, string> } {
    const args = ['--print', '--output-format', 'stream-json', '--verbose'];
    if (opts.model) args.push('--model', opts.model);
    if (opts.resumeSessionId) args.push('--resume', opts.resumeSessionId);
    if (opts.permissionMode) args.push(`--permission-mode=${opts.permissionMode}`);
    if (opts.allowedTools && opts.allowedTools.length > 0) {
      // Use `=` syntax — `--allowed-tools` may be variadic and consume the prompt arg
      args.push(`--allowed-tools=${opts.allowedTools.join(',')}`);
    }
    args.push(prompt);
    return { cmd: 'claude', args };
  }

  override translateLine(line: object, _ctx: TranslationCtx): RunEvent[] {
    return translateClaudeMessage(line);
  }
}
