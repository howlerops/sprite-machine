import type { Harness } from '@sprite-machine/shared';
import type { CreateRunInput, RunStore } from '../runs.js';

export interface Agent {
  start(runId: string, input: CreateRunInput): Promise<void>;
  sendUserMessage(runId: string, text: string): Promise<void>;
  cancel(runId: string): Promise<void>;
}

export interface AgentDeps {
  store: RunStore;
  defaultPermissionMode: 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan';
  dataDir: string;
}

export interface HarnessFactoryDeps extends AgentDeps {}

export type HarnessFactories = Record<Harness, (deps: HarnessFactoryDeps) => Agent>;
