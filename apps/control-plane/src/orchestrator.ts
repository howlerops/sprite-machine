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
  workdirs?: string[];
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

    const harness: Harness = input.harness ?? 'pi';
    const effectiveWorkdir = input.workdirs?.[0] ?? input.workdir;
    const sidecarBody: Record<string, unknown> = { prompt: input.prompt, harness };
    if (effectiveWorkdir !== undefined) sidecarBody.workdir = effectiveWorkdir;
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
      workdirs: input.workdirs ?? null,
      ...(input.metadata !== undefined ? { metadata: input.metadata } : {}),
    });
  }

  async refreshRunFromSidecar(runId: string): Promise<RunRow> {
    const { run, sprite } = await this.requireRunAndLiveSprite(runId);
    if (!sprite.agentBaseUrl) return run;

    const res = await this.callSidecar(
      sprite.agentBaseUrl,
      sprite.agentToken,
      'GET',
      `/runs/${run.sidecarRunId}`,
      null,
    );
    if (!res.ok) return run;

    const sidecarRun = (await res.json()) as {
      status?: RunStatus;
      sessionId?: string;
      totalCostUsd?: number;
      totalInputTokens?: number;
      totalOutputTokens?: number;
      events?: RunEvent[];
    };

    if (Array.isArray(sidecarRun.events)) {
      const existing = await this.opts.storage.listRunEvents(run.id);
      const existingSignatures = existing.map((row) => eventSignature(row.event));
      let lastExistingMatchIndex = -1;
      let lastSidecarMatchIndex = -1;
      for (let i = 0; i < sidecarRun.events.length; i++) {
        const event = sidecarRun.events[i]!;
        const idx = existingSignatures.indexOf(eventSignature(event), lastExistingMatchIndex + 1);
        if (idx !== -1) {
          lastExistingMatchIndex = idx;
          lastSidecarMatchIndex = i;
        }
      }
      const missingEvents = sidecarRun.events.slice(lastSidecarMatchIndex + 1);
      for (const event of missingEvents) {
        await this.ingestWebhookEvent(sprite.id, run.sidecarRunId, event);
      }
    }

    const latest = await this.opts.storage.getRun(run.id);
    const current = latest ?? run;
    const patch: Parameters<Storage['updateRun']>[1] = {};
    if (sidecarRun.status && sidecarRun.status !== current.status) patch.status = sidecarRun.status;
    if (sidecarRun.sessionId !== undefined && sidecarRun.sessionId !== current.sessionId) {
      patch.sessionId = sidecarRun.sessionId;
    }
    if (
      typeof sidecarRun.totalCostUsd === 'number' &&
      sidecarRun.totalCostUsd !== current.totalCostUsd
    ) {
      patch.totalCostUsd = sidecarRun.totalCostUsd;
    }
    if (
      typeof sidecarRun.totalInputTokens === 'number' &&
      sidecarRun.totalInputTokens !== current.totalInputTokens
    ) {
      patch.totalInputTokens = sidecarRun.totalInputTokens;
    }
    if (
      typeof sidecarRun.totalOutputTokens === 'number' &&
      sidecarRun.totalOutputTokens !== current.totalOutputTokens
    ) {
      patch.totalOutputTokens = sidecarRun.totalOutputTokens;
    }
    if (Object.keys(patch).length === 0) return current;
    return await this.opts.storage.updateRun(run.id, patch);
  }

  async sendMessage(runId: string, text: string): Promise<void> {
    const { run, sprite } = await this.requireRunAndLiveSprite(runId);
    if (!sprite.agentBaseUrl) throw new Error('sprite has no agentBaseUrl after resume');

    // Persist + publish the user's message immediately from the control plane so
    // the chat timeline updates before the sidecar/agent turn starts. The
    // sidecar also emits a user_message; ingestWebhookEvent de-dupes that copy.
    const userEvent: RunEvent = { type: 'user_message', text, at: nowIso() };
    const { seq } = await this.opts.storage.appendRunEvent(run.id, userEvent);
    await this.opts.storage.updateRun(run.id, { lastEventAt: nowIso() });
    this.opts.sseBus.publish({ runId: run.id, seq, event: userEvent });

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
    // Always ask the backend to wake/refresh. A sprite can be marked running in
    // our DB while its shell/sidecar endpoint is stale or no longer reachable.
    // The Fly/Sprites backend treats resume as observational/idempotent for
    // already-running sprites, and it gives us a fresh URL/status snapshot.
    const resumed = await this.opts.sprites.resume(spriteId);
    return await this.opts.storage.updateSprite(spriteId, {
      status: 'running',
      agentBaseUrl: resumed.agentBaseUrl,
      lastActiveAt: nowIso(),
    });
  }

  async destroySprite(spriteId: string): Promise<void> {
    const sprite = await this.opts.storage.getSprite(spriteId);
    if (!sprite) throw new Error(`sprite not found: ${spriteId}`);
    if (sprite.status === 'destroyed') return; // idempotent
    await this.opts.sprites.destroy(spriteId);
    await this.opts.storage.updateSprite(spriteId, { status: 'destroyed' });
  }

  async applyGitHubIntegration(input: {
    token: string;
    username: string;
    gitUserName: string;
    gitUserEmail: string;
  }): Promise<{ applied: string[]; errors: Array<{ spriteId: string; message: string }> }> {
    const sprites = await this.opts.storage.listActiveSprites();
    const result = {
      applied: [] as string[],
      errors: [] as Array<{ spriteId: string; message: string }>,
    };
    for (const sprite of sprites) {
      if (sprite.status === 'destroyed') continue;
      try {
        await this.configureGitHubOnSprite(sprite.id, input);
        result.applied.push(sprite.id);
      } catch (err) {
        result.errors.push({ spriteId: sprite.id, message: (err as Error).message });
      }
    }
    return result;
  }

  async applyAiAuthIntegration(input: {
    codexAuthJson?: string;
    piAuthJson?: string;
    claudeCredentialsJson?: string;
  }): Promise<{ applied: string[]; errors: Array<{ spriteId: string; message: string }> }> {
    const sprites = await this.opts.storage.listActiveSprites();
    const result = {
      applied: [] as string[],
      errors: [] as Array<{ spriteId: string; message: string }>,
    };
    for (const sprite of sprites) {
      if (sprite.status === 'destroyed') continue;
      try {
        await this.configureAiAuthOnSprite(sprite.id, input);
        result.applied.push(sprite.id);
      } catch (err) {
        result.errors.push({ spriteId: sprite.id, message: (err as Error).message });
      }
    }
    return result;
  }

  async configureAiAuthOnSprite(
    spriteId: string,
    input: { codexAuthJson?: string; piAuthJson?: string; claudeCredentialsJson?: string },
  ): Promise<void> {
    await this.resumeSprite(spriteId);
    const mkdir = await this.opts.sprites.exec(
      spriteId,
      [
        'bash',
        '-lc',
        'mkdir -p ~/.codex ~/.pi/agent ~/.claude && chmod 700 ~/.codex ~/.pi ~/.pi/agent ~/.claude',
      ],
      { timeoutMs: 60_000 },
    );
    if (mkdir.exitCode !== 0) throw new Error(mkdir.stderr || mkdir.stdout || 'mkdir failed');

    if (input.codexAuthJson) {
      JSON.parse(input.codexAuthJson);
      await this.opts.sprites.writeFile(
        spriteId,
        '/home/sprite/.codex/auth.json',
        input.codexAuthJson,
      );
    }
    if (input.piAuthJson) {
      JSON.parse(input.piAuthJson);
      await this.opts.sprites.writeFile(
        spriteId,
        '/home/sprite/.pi/agent/auth.json',
        input.piAuthJson,
      );
    }
    if (input.claudeCredentialsJson) {
      JSON.parse(input.claudeCredentialsJson);
      await this.opts.sprites.writeFile(
        spriteId,
        '/home/sprite/.claude/.credentials.json',
        input.claudeCredentialsJson,
      );
      await this.opts.sprites.writeFile(
        spriteId,
        '/home/sprite/.claude/credentials.json',
        input.claudeCredentialsJson,
      );
    }
    const chmod = await this.opts.sprites.exec(
      spriteId,
      [
        'bash',
        '-lc',
        'chmod 600 ~/.codex/auth.json ~/.pi/agent/auth.json ~/.claude/.credentials.json ~/.claude/credentials.json 2>/dev/null || true',
      ],
      { timeoutMs: 60_000 },
    );
    if (chmod.exitCode !== 0) throw new Error(chmod.stderr || chmod.stdout || 'chmod failed');
  }

  async configureGitHubOnSprite(
    spriteId: string,
    input: { token: string; username: string; gitUserName: string; gitUserEmail: string },
  ): Promise<void> {
    await this.resumeSprite(spriteId);
    const hosts = `github.com:\n    oauth_token: ${input.token}\n    git_protocol: https\n    users:\n        ${input.username}:\n            oauth_token: ${input.token}\n    user: ${input.username}\n`;
    await this.opts.sprites.exec(
      spriteId,
      ['bash', '-lc', 'mkdir -p ~/.config/gh && chmod 700 ~/.config/gh'],
      { timeoutMs: 60_000 },
    );
    await this.opts.sprites.writeFile(spriteId, '/home/sprite/.config/gh/hosts.yml', hosts);
    const res = await this.opts.sprites.exec(
      spriteId,
      [
        'bash',
        '-lc',
        `chmod 600 ~/.config/gh/hosts.yml && git config --global user.name ${JSON.stringify(input.gitUserName)} && git config --global user.email ${JSON.stringify(input.gitUserEmail)} && gh auth setup-git`,
      ],
      { timeoutMs: 60_000 },
    );
    if (res.exitCode !== 0) throw new Error(res.stderr || res.stdout || 'gh auth setup-git failed');
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
    if (event.type === 'user_message') {
      const existing = await this.opts.storage.listRunEvents(run.id);
      const alreadyIngested = existing.some(
        (row) => row.event.type === 'user_message' && row.event.text === event.text,
      );
      if (alreadyIngested) return;
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

function eventSignature(event: RunEvent): string {
  switch (event.type) {
    case 'status':
      return `status:${event.status}`;
    case 'user_message':
      return `user_message:${event.text}`;
    case 'assistant_text':
      return `assistant_text:${event.text}`;
    case 'assistant_delta':
      return `assistant_delta:${event.text}`;
    case 'error':
      return `error:${event.message}`;
    case 'turn_complete':
      return `turn_complete:${event.costUsd ?? ''}:${event.inputTokens ?? ''}:${event.outputTokens ?? ''}`;
    case 'session_started':
      return `session_started:${event.sessionId}`;
    case 'tool_use':
      return `tool_use:${event.toolUseId}`;
    case 'tool_result':
      return `tool_result:${event.toolUseId}`;
    case 'permission_requested':
      return `permission_requested:${event.requestId}`;
    case 'permission_resolved':
      return `permission_resolved:${event.requestId}:${event.decision}`;
    case 'completed':
      return 'completed';
    default:
      return JSON.stringify(event);
  }
}
