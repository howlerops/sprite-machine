import type { Harness, RunEvent, RunStatus } from '@sprite-machine/shared';

export interface SpriteRow {
  id: string;
  status: 'creating' | 'running' | 'suspended' | 'destroyed';
  agentBaseUrl: string | null;
  agentToken: string;
  fromCheckpointId: string | null;
  createdAt: string;
  updatedAt: string;
  lastActiveAt: string | null;
  metadata: Record<string, unknown>;
}

export interface RunRow {
  id: string;
  spriteId: string;
  sidecarRunId: string;
  status: RunStatus;
  harness: Harness;
  prompt: string;
  workdir: string | null;
  sessionId: string | null;
  createdAt: string;
  updatedAt: string;
  lastEventAt: string | null;
  totalCostUsd: number;
  totalInputTokens: number;
  totalOutputTokens: number;
  metadata: Record<string, unknown>;
}

export interface RunEventRow {
  runId: string;
  seq: number;
  event: RunEvent;
  createdAt: string;
}

export interface PushTokenRow {
  expoToken: string;
  label: string | null;
  createdAt: string;
  lastUsedAt: string | null;
}

export interface NewSprite {
  id: string;
  status: SpriteRow['status'];
  agentBaseUrl: string | null;
  agentToken: string;
  fromCheckpointId: string | null;
  metadata?: Record<string, unknown>;
}

export interface NewRun {
  id: string;
  spriteId: string;
  sidecarRunId: string;
  status: RunStatus;
  harness: Harness;
  prompt: string;
  workdir: string | null;
  metadata?: Record<string, unknown>;
}

export interface SpritePatch {
  status?: SpriteRow['status'];
  agentBaseUrl?: string | null;
  lastActiveAt?: string;
}

export interface RunPatch {
  status?: RunStatus;
  sessionId?: string | null;
  totalCostUsd?: number;
  totalInputTokens?: number;
  totalOutputTokens?: number;
  lastEventAt?: string;
}

export interface Storage {
  createSprite(s: NewSprite): Promise<SpriteRow>;
  getSprite(id: string): Promise<SpriteRow | null>;
  updateSprite(id: string, patch: SpritePatch): Promise<SpriteRow>;
  listActiveSprites(): Promise<SpriteRow[]>;

  createRun(r: NewRun): Promise<RunRow>;
  getRun(id: string): Promise<RunRow | null>;
  getRunBySidecar(spriteId: string, sidecarRunId: string): Promise<RunRow | null>;
  listRuns(): Promise<RunRow[]>;
  listRunsForSprite(spriteId: string): Promise<RunRow[]>;
  updateRun(id: string, patch: RunPatch): Promise<RunRow>;

  appendRunEvent(runId: string, event: RunEvent): Promise<{ seq: number }>;
  listRunEvents(runId: string, afterSeq?: number): Promise<RunEventRow[]>;

  registerPushToken(token: string, label?: string): Promise<PushTokenRow>;
  listPushTokens(): Promise<PushTokenRow[]>;
  removePushToken(token: string): Promise<void>;

  getStats(sinceIso?: string): Promise<Stats>;
}

export interface Stats {
  totalRuns: number;
  totalSprites: number;
  activeSprites: number;
  suspendedSprites: number;
  totalCostUsd: number;
  totalInputTokens: number;
  totalOutputTokens: number;
  runsByStatus: Record<string, number>;
  runsLast24h: number;
}
