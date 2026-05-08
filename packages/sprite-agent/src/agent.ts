/**
 * Multi-harness agent dispatcher. The control plane stamps each Run with
 * `harness` ('claude' | 'codex' | 'pi' | 'mock'). The router looks up the
 * run's harness and dispatches to the right per-harness Agent implementation.
 */
import type { Harness } from '@sprite-machine/shared';
import type { CreateRunInput, RunStore } from './runs.js';
import { ClaudeCliAgent } from './agents/claude-cli.js';
import { ClaudeSdkAgent } from './agents/claude-sdk.js';
import { CodexCliAgent } from './agents/codex-cli.js';
import { MockAgent } from './agents/mock.js';
import { PiCliAgent } from './agents/pi-cli.js';
import type { Agent, AgentDeps } from './agents/types.js';

export type { Agent, AgentDeps } from './agents/types.js';

export interface AgentRouterDeps extends AgentDeps {
  /**
   * Selects which Claude implementation to use:
   *   'cli'  (default) — subprocesses `claude --print --output-format stream-json --verbose`,
   *                      uses your Claude Code OAuth login (Pro/Max), picks up installed plugins.
   *   'sdk'  — uses `@anthropic-ai/claude-agent-sdk`, requires ANTHROPIC_API_KEY.
   *   'mock' — MockAgent (set by sidecar when neither auth is available).
   */
  claudeImpl?: 'cli' | 'sdk' | 'mock';
}

export class AgentRouter implements Agent {
  private readonly impls: Record<Harness, Agent>;
  private readonly store: RunStore;

  constructor(deps: AgentRouterDeps) {
    this.store = deps.store;
    const baseDeps: AgentDeps = {
      store: deps.store,
      defaultPermissionMode: deps.defaultPermissionMode,
      dataDir: deps.dataDir,
    };
    const claudeImpl = deps.claudeImpl ?? 'cli';
    const claude: Agent =
      claudeImpl === 'mock'
        ? new MockAgent(baseDeps)
        : claudeImpl === 'sdk'
          ? new ClaudeSdkAgent(baseDeps)
          : new ClaudeCliAgent(baseDeps);
    this.impls = {
      mock: new MockAgent(baseDeps),
      claude,
      codex: new CodexCliAgent(baseDeps),
      pi: new PiCliAgent(baseDeps),
    };
  }

  async start(runId: string, input: CreateRunInput): Promise<void> {
    const harness = input.harness ?? 'claude';
    return this.impls[harness].start(runId, input);
  }

  async sendUserMessage(runId: string, text: string): Promise<void> {
    return this.impls[this.harnessFor(runId)].sendUserMessage(runId, text);
  }

  async cancel(runId: string): Promise<void> {
    return this.impls[this.harnessFor(runId)].cancel(runId);
  }

  private harnessFor(runId: string): Harness {
    return this.store.get(runId)?.summary.harness ?? 'claude';
  }
}
