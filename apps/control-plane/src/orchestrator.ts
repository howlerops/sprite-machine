import { newId, nowIso, type Harness, type RunEvent, type RunStatus } from '@sprite-machine/shared';
import type { SpritesClient } from '@sprite-machine/sprites-client';
import type { RunRow, SpriteRow, Storage } from './db/types.js';
import type { SseBus } from './sse-bus.js';
import type { PushDispatcher } from './push.js';

export interface OrchestratorOpts {
  storage: Storage;
  sprites: SpritesClient;
  templateCheckpointId: string | null;
  sseBus: SseBus;
  push: PushDispatcher;
}

export interface CreateRunInput {
  prompt: string;
  harness?: Harness;
  workdir?: string;
  model?: string;
  permissionMode?: 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan';
  allowedTools?: string[];
  metadata?: Record<string, unknown>;
}

export class Orchestrator {
  constructor(private readonly opts: OrchestratorOpts) {}

  async createRun(input: CreateRunInput): Promise<RunRow> {
    const tplId = this.opts.templateCheckpointId;
    const sprite = tplId
      ? await this.opts.sprites.cloneFromCheckpoint(tplId, { name: `run-${newId('s').slice(2)}` })
      : await this.opts.sprites.create({ name: `run-${newId('s').slice(2)}` });

    // Upsert: in single-worker model the same sprite is reused across runs.
    const existing = await this.opts.storage.getSprite(sprite.id);
    if (!existing) {
      await this.opts.storage.createSprite({
        id: sprite.id,
        status: sprite.status,
        agentBaseUrl: sprite.agentBaseUrl,
        agentToken: sprite.agentToken,
        fromCheckpointId: sprite.fromCheckpointId,
      });
    } else if (existing.agentBaseUrl !== sprite.agentBaseUrl || existing.status !== sprite.status) {
      await this.opts.storage.updateSprite(sprite.id, {
        status: sprite.status,
        agentBaseUrl: sprite.agentBaseUrl,
      });
    }

    if (!sprite.agentBaseUrl) {
      throw new Error('sprite has no agentBaseUrl after create');
    }

    const harness: Harness = input.harness ?? 'claude';
    const sidecarBody: Record<string, unknown> = { prompt: input.prompt, harness };
    if (input.workdir !== undefined) sidecarBody.workdir = input.workdir;
    if (input.model !== undefined) sidecarBody.model = input.model;
    if (input.permissionMode !== undefined) sidecarBody.permissionMode = input.permissionMode;
    if (input.allowedTools !== undefined) sidecarBody.allowedTools = input.allowedTools;
    if (input.metadata !== undefined) sidecarBody.metadata = input.metadata;

    const res = await this.callSidecar(
      sprite.agentBaseUrl,
      sprite.agentToken,
      'POST',
      '/runs',
      sidecarBody,
    );
    if (res.status !== 201) {
      throw new Error(`sidecar /runs returned ${res.status}: ${await res.text()}`);
    }
    const sidecarRun = (await res.json()) as { id: string; status: string };

    return await this.opts.storage.createRun({
      id: newId('cprun'),
      spriteId: sprite.id,
      sidecarRunId: sidecarRun.id,
      status: sidecarRun.status as RunStatus,
      harness,
      prompt: input.prompt,
      workdir: input.workdir ?? null,
      ...(input.metadata !== undefined ? { metadata: input.metadata } : {}),
    });
  }

  async sendMessage(runId: string, text: string): Promise<void> {
    const { run, sprite } = await this.requireRunAndLiveSprite(runId);
    if (!sprite.agentBaseUrl) throw new Error('sprite has no agentBaseUrl after resume');
    const res = await this.callSidecar(
      sprite.agentBaseUrl,
      sprite.agentToken,
      'POST',
      `/runs/${run.sidecarRunId}/messages`,
      { text },
    );
    if (!res.ok) throw new Error(`sidecar messages returned ${res.status}: ${await res.text()}`);
  }

  async cancelRun(runId: string): Promise<void> {
    const { run, sprite } = await this.requireRunAndLiveSprite(runId);
    if (!sprite.agentBaseUrl) return;
    await this.callSidecar(
      sprite.agentBaseUrl,
      sprite.agentToken,
      'POST',
      `/runs/${run.sidecarRunId}/cancel`,
      null,
    );
  }

  async resolvePermission(
    runId: string,
    requestId: string,
    decision: 'allow' | 'deny',
    reason?: string,
  ): Promise<void> {
    const { run, sprite } = await this.requireRunAndLiveSprite(runId);
    if (!sprite.agentBaseUrl) throw new Error('sprite has no agentBaseUrl');
    const res = await this.callSidecar(
      sprite.agentBaseUrl,
      sprite.agentToken,
      'POST',
      `/runs/${run.sidecarRunId}/permissions/${requestId}`,
      { decision, ...(reason !== undefined ? { reason } : {}) },
    );
    if (!res.ok) {
      throw new Error(`sidecar permissions returned ${res.status}: ${await res.text()}`);
    }
  }

  async suspendSprite(spriteId: string): Promise<SpriteRow> {
    const sprite = await this.opts.storage.getSprite(spriteId);
    if (!sprite) throw new Error(`sprite not found: ${spriteId}`);
    if (sprite.status === 'destroyed') throw new Error(`sprite ${spriteId} is destroyed`);
    if (sprite.status === 'suspended') return sprite;
    const runs = await this.opts.storage.listRunsForSprite(spriteId);
    const busy = runs.some((r) => r.status === 'starting' || r.status === 'running');
    if (busy) throw new Error(`sprite ${spriteId} has in-flight runs`);
    await this.opts.sprites.suspend(spriteId);
    return await this.opts.storage.updateSprite(spriteId, {
      status: 'suspended',
      agentBaseUrl: null,
      lastActiveAt: nowIso(),
    });
  }

  async resumeSprite(spriteId: string): Promise<SpriteRow> {
    const sprite = await this.opts.storage.getSprite(spriteId);
    if (!sprite) throw new Error(`sprite not found: ${spriteId}`);
    if (sprite.status === 'destroyed') throw new Error(`sprite ${spriteId} is destroyed`);
    if (sprite.status === 'running' && sprite.agentBaseUrl) return sprite;
    const resumed = await this.opts.sprites.resume(spriteId);
    return await this.opts.storage.updateSprite(spriteId, {
      status: 'running',
      agentBaseUrl: resumed.agentBaseUrl,
      lastActiveAt: nowIso(),
    });
  }

  async ingestWebhookEvent(spriteId: string, sidecarRunId: string, event: RunEvent): Promise<void> {
    const sprite = await this.opts.storage.getSprite(spriteId);
    if (!sprite) {
      console.warn(`[orchestrator] webhook for unknown sprite ${spriteId}`);
      return;
    }
    const run = await this.opts.storage.getRunBySidecar(spriteId, sidecarRunId);
    if (!run) {
      console.warn(`[orchestrator] webhook for unknown run ${spriteId}/${sidecarRunId}`);
      return;
    }
    const { seq } = await this.opts.storage.appendRunEvent(run.id, event);
    const patch: Parameters<Storage['updateRun']>[1] = { lastEventAt: nowIso() };
    if (event.type === 'status') patch.status = event.status;
    if (event.type === 'session_started') patch.sessionId = event.sessionId;
    if (event.type === 'turn_complete') {
      if (typeof event.costUsd === 'number') patch.totalCostUsd = run.totalCostUsd + event.costUsd;
      if (typeof event.inputTokens === 'number')
        patch.totalInputTokens = run.totalInputTokens + event.inputTokens;
      if (typeof event.outputTokens === 'number')
        patch.totalOutputTokens = run.totalOutputTokens + event.outputTokens;
    }
    await this.opts.storage.updateRun(run.id, patch);
    await this.opts.storage.updateSprite(spriteId, { lastActiveAt: nowIso() });
    this.opts.sseBus.publish({ runId: run.id, seq, event });
    void this.opts.push.notifyRunEvent(run.id, run.prompt, event);
  }

  private async requireRunAndLiveSprite(
    runId: string,
  ): Promise<{ run: RunRow; sprite: SpriteRow }> {
    const run = await this.opts.storage.getRun(runId);
    if (!run) throw new Error(`run not found: ${runId}`);
    let sprite = await this.opts.storage.getSprite(run.spriteId);
    if (!sprite) throw new Error(`sprite not found: ${run.spriteId}`);
    if (sprite.status === 'destroyed') {
      throw new Error(`sprite ${sprite.id} is destroyed`);
    }
    if (sprite.status === 'suspended' || !sprite.agentBaseUrl) {
      const resumed = await this.opts.sprites.resume(sprite.id);
      sprite = await this.opts.storage.updateSprite(sprite.id, {
        status: 'running',
        agentBaseUrl: resumed.agentBaseUrl,
        lastActiveAt: nowIso(),
      });
    }
    return { run, sprite };
  }

  private async callSidecar(
    base: string,
    token: string,
    method: 'GET' | 'POST',
    path: string,
    body: unknown,
  ): Promise<Response> {
    const headers: Record<string, string> = { authorization: `Bearer ${token}` };
    if (body !== null) headers['content-type'] = 'application/json';
    return await fetch(`${base}${path}`, {
      method,
      headers,
      body: body !== null ? JSON.stringify(body) : null,
    });
  }
}
