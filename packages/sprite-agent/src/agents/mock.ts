import { nowIso } from '@sprite-machine/shared';
import type { CreateRunInput } from '../runs.js';
import type { Agent, AgentDeps } from './types.js';

export class MockAgent implements Agent {
  constructor(private readonly deps: AgentDeps) {}

  async start(runId: string, input: CreateRunInput): Promise<void> {
    await this.simulate(runId, input.prompt, false);
  }

  async sendUserMessage(runId: string, text: string): Promise<void> {
    const run = this.deps.store.get(runId);
    if (!run) throw new Error('run not found');
    await this.deps.store.appendEvent(runId, { type: 'user_message', text, at: nowIso() });
    await this.simulate(runId, text, true);
  }

  async cancel(runId: string): Promise<void> {
    await this.deps.store.setStatus(runId, 'cancelled');
  }

  private async simulate(runId: string, prompt: string, resume: boolean): Promise<void> {
    const { store } = this.deps;
    await store.setStatus(runId, 'starting');
    const sessionId = store.get(runId)?.summary.sessionId ?? `mock-${runId}`;
    if (!resume) {
      await store.appendEvent(runId, {
        type: 'session_started',
        sessionId,
        model: 'mock-model',
        at: nowIso(),
      });
    }
    await store.setStatus(runId, 'running');
    await delay(20);
    await store.appendEvent(runId, {
      type: 'assistant_text',
      text: `[mock] received: ${prompt}`,
      at: nowIso(),
    });
    await delay(20);
    await store.appendEvent(runId, {
      type: 'turn_complete',
      costUsd: 0,
      inputTokens: prompt.length,
      outputTokens: 16,
      at: nowIso(),
    });
    await store.setStatus(runId, 'idle');
  }
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
