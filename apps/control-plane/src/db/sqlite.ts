import { mkdirSync } from 'node:fs';
import path from 'node:path';
import Database from 'better-sqlite3';
import { nowIso, type RunEvent } from '@sprite-machine/shared';
import { SCHEMA_SQL } from './schema.js';
import type {
  NewRun,
  NewSprite,
  PushTokenRow,
  RunEventRow,
  RunPatch,
  RunRow,
  SpritePatch,
  SpriteRow,
  Stats,
  Storage,
} from './types.js';

export class SqliteStorage implements Storage {
  private readonly db: Database.Database;

  constructor(dbPath: string) {
    mkdirSync(path.dirname(dbPath), { recursive: true });
    this.db = new Database(dbPath);
    this.db.pragma('journal_mode = WAL');
    this.db.pragma('foreign_keys = ON');
    this.db.exec(SCHEMA_SQL);
    // Idempotent migration for existing dev/prod DBs that pre-date the harness column
    try {
      this.db.exec(`alter table runs add column harness text not null default 'claude'`);
    } catch {
      // column already exists
    }
  }

  close(): void {
    this.db.close();
  }

  async createSprite(s: NewSprite): Promise<SpriteRow> {
    const now = nowIso();
    this.db
      .prepare(
        `insert into sprites (id, status, agent_base_url, agent_token, from_checkpoint_id, created_at, updated_at, metadata)
         values (?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .run(
        s.id,
        s.status,
        s.agentBaseUrl,
        s.agentToken,
        s.fromCheckpointId,
        now,
        now,
        JSON.stringify(s.metadata ?? {}),
      );
    const row = await this.getSprite(s.id);
    if (!row) throw new Error('sprite vanished after insert');
    return row;
  }

  async getSprite(id: string): Promise<SpriteRow | null> {
    const r = this.db.prepare(`select * from sprites where id = ?`).get(id) as
      | Record<string, unknown>
      | undefined;
    return r ? toSpriteRow(r) : null;
  }

  async updateSprite(id: string, patch: SpritePatch): Promise<SpriteRow> {
    const sets: string[] = ['updated_at = ?'];
    const vals: unknown[] = [nowIso()];
    if (patch.status !== undefined) {
      sets.push('status = ?');
      vals.push(patch.status);
    }
    if (patch.agentBaseUrl !== undefined) {
      sets.push('agent_base_url = ?');
      vals.push(patch.agentBaseUrl);
    }
    if (patch.lastActiveAt !== undefined) {
      sets.push('last_active_at = ?');
      vals.push(patch.lastActiveAt);
    }
    vals.push(id);
    this.db.prepare(`update sprites set ${sets.join(', ')} where id = ?`).run(...vals);
    const row = await this.getSprite(id);
    if (!row) throw new Error(`sprite ${id} not found`);
    return row;
  }

  async listActiveSprites(): Promise<SpriteRow[]> {
    const rows = this.db
      .prepare(`select * from sprites where status in ('creating','running','suspended')`)
      .all() as Record<string, unknown>[];
    return rows.map(toSpriteRow);
  }

  async createRun(r: NewRun): Promise<RunRow> {
    const now = nowIso();
    this.db
      .prepare(
        `insert into runs (id, sprite_id, sidecar_run_id, status, harness, prompt, workdir, created_at, updated_at, metadata)
         values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .run(
        r.id,
        r.spriteId,
        r.sidecarRunId,
        r.status,
        r.harness,
        r.prompt,
        r.workdir,
        now,
        now,
        JSON.stringify(r.metadata ?? {}),
      );
    const row = await this.getRun(r.id);
    if (!row) throw new Error('run vanished after insert');
    return row;
  }

  async getRun(id: string): Promise<RunRow | null> {
    const r = this.db.prepare(`select * from runs where id = ?`).get(id) as
      | Record<string, unknown>
      | undefined;
    return r ? toRunRow(r) : null;
  }

  async getRunBySidecar(spriteId: string, sidecarRunId: string): Promise<RunRow | null> {
    const r = this.db
      .prepare(`select * from runs where sprite_id = ? and sidecar_run_id = ?`)
      .get(spriteId, sidecarRunId) as Record<string, unknown> | undefined;
    return r ? toRunRow(r) : null;
  }

  async listRuns(): Promise<RunRow[]> {
    const rows = this.db
      .prepare(`select * from runs order by updated_at desc limit 200`)
      .all() as Record<string, unknown>[];
    return rows.map(toRunRow);
  }

  async listRunsForSprite(spriteId: string): Promise<RunRow[]> {
    const rows = this.db
      .prepare(`select * from runs where sprite_id = ? order by updated_at desc`)
      .all(spriteId) as Record<string, unknown>[];
    return rows.map(toRunRow);
  }

  async updateRun(id: string, patch: RunPatch): Promise<RunRow> {
    const sets: string[] = ['updated_at = ?'];
    const vals: unknown[] = [nowIso()];
    if (patch.status !== undefined) {
      sets.push('status = ?');
      vals.push(patch.status);
    }
    if (patch.sessionId !== undefined) {
      sets.push('session_id = ?');
      vals.push(patch.sessionId);
    }
    if (patch.totalCostUsd !== undefined) {
      sets.push('total_cost_usd = ?');
      vals.push(patch.totalCostUsd);
    }
    if (patch.totalInputTokens !== undefined) {
      sets.push('total_input_tokens = ?');
      vals.push(patch.totalInputTokens);
    }
    if (patch.totalOutputTokens !== undefined) {
      sets.push('total_output_tokens = ?');
      vals.push(patch.totalOutputTokens);
    }
    if (patch.lastEventAt !== undefined) {
      sets.push('last_event_at = ?');
      vals.push(patch.lastEventAt);
    }
    vals.push(id);
    this.db.prepare(`update runs set ${sets.join(', ')} where id = ?`).run(...vals);
    const row = await this.getRun(id);
    if (!row) throw new Error(`run ${id} not found`);
    return row;
  }

  async appendRunEvent(runId: string, event: RunEvent): Promise<{ seq: number }> {
    const now = nowIso();
    return this.db.transaction(() => {
      const row = this.db
        .prepare(`select coalesce(max(seq), -1) as max_seq from run_events where run_id = ?`)
        .get(runId) as { max_seq: number } | undefined;
      const seq = (row?.max_seq ?? -1) + 1;
      this.db
        .prepare(`insert into run_events (run_id, seq, event, created_at) values (?, ?, ?, ?)`)
        .run(runId, seq, JSON.stringify(event), now);
      return { seq };
    })();
  }

  async listRunEvents(runId: string, afterSeq = -1): Promise<RunEventRow[]> {
    const rows = this.db
      .prepare(
        `select run_id, seq, event, created_at from run_events where run_id = ? and seq > ? order by seq asc`,
      )
      .all(runId, afterSeq) as Array<{
      run_id: string;
      seq: number;
      event: string;
      created_at: string;
    }>;
    return rows.map((r) => ({
      runId: r.run_id,
      seq: r.seq,
      event: JSON.parse(r.event) as RunEvent,
      createdAt: r.created_at,
    }));
  }

  async registerPushToken(token: string, label?: string): Promise<PushTokenRow> {
    const now = nowIso();
    this.db
      .prepare(
        `insert into push_tokens (expo_token, label, created_at, last_used_at)
         values (?, ?, ?, ?)
         on conflict(expo_token) do update set label = excluded.label, last_used_at = excluded.last_used_at`,
      )
      .run(token, label ?? null, now, now);
    const r = this.db.prepare(`select * from push_tokens where expo_token = ?`).get(token) as
      | Record<string, unknown>
      | undefined;
    if (!r) throw new Error('push token vanished after insert');
    return toPushTokenRow(r);
  }

  async listPushTokens(): Promise<PushTokenRow[]> {
    const rows = this.db.prepare(`select * from push_tokens`).all() as Record<string, unknown>[];
    return rows.map(toPushTokenRow);
  }

  async removePushToken(token: string): Promise<void> {
    this.db.prepare(`delete from push_tokens where expo_token = ?`).run(token);
  }

  async getStats(sinceIso?: string): Promise<Stats> {
    const since = sinceIso ?? new Date(Date.now() - 24 * 60 * 60 * 1000).toISOString();
    const sumRow = this.db
      .prepare(
        `select count(*) as total_runs,
                coalesce(sum(total_cost_usd), 0) as total_cost,
                coalesce(sum(total_input_tokens), 0) as total_in,
                coalesce(sum(total_output_tokens), 0) as total_out
         from runs`,
      )
      .get() as {
      total_runs: number;
      total_cost: number;
      total_in: number;
      total_out: number;
    };
    const recentRow = this.db
      .prepare(`select count(*) as c from runs where created_at >= ?`)
      .get(since) as { c: number };
    const statusRows = this.db
      .prepare(`select status, count(*) as c from runs group by status`)
      .all() as Array<{ status: string; c: number }>;
    const spriteCounts = this.db
      .prepare(`select status, count(*) as c from sprites group by status`)
      .all() as Array<{ status: string; c: number }>;

    const byStatus: Record<string, number> = {};
    for (const r of statusRows) byStatus[r.status] = r.c;

    let totalSprites = 0;
    let activeSprites = 0;
    let suspendedSprites = 0;
    for (const r of spriteCounts) {
      totalSprites += r.c;
      if (r.status === 'running' || r.status === 'creating') activeSprites += r.c;
      if (r.status === 'suspended') suspendedSprites += r.c;
    }

    return {
      totalRuns: sumRow.total_runs,
      totalSprites,
      activeSprites,
      suspendedSprites,
      totalCostUsd: sumRow.total_cost,
      totalInputTokens: sumRow.total_in,
      totalOutputTokens: sumRow.total_out,
      runsByStatus: byStatus,
      runsLast24h: recentRow.c,
    };
  }
}

function toSpriteRow(r: Record<string, unknown>): SpriteRow {
  return {
    id: r.id as string,
    status: r.status as SpriteRow['status'],
    agentBaseUrl: (r.agent_base_url as string | null) ?? null,
    agentToken: r.agent_token as string,
    fromCheckpointId: (r.from_checkpoint_id as string | null) ?? null,
    createdAt: r.created_at as string,
    updatedAt: r.updated_at as string,
    lastActiveAt: (r.last_active_at as string | null) ?? null,
    metadata: JSON.parse(((r.metadata as string) ?? '{}') || '{}') as Record<string, unknown>,
  };
}

function toRunRow(r: Record<string, unknown>): RunRow {
  return {
    id: r.id as string,
    spriteId: r.sprite_id as string,
    sidecarRunId: r.sidecar_run_id as string,
    status: r.status as RunRow['status'],
    harness: ((r.harness as string | null) ?? 'claude') as RunRow['harness'],
    prompt: r.prompt as string,
    workdir: (r.workdir as string | null) ?? null,
    sessionId: (r.session_id as string | null) ?? null,
    createdAt: r.created_at as string,
    updatedAt: r.updated_at as string,
    lastEventAt: (r.last_event_at as string | null) ?? null,
    totalCostUsd: r.total_cost_usd as number,
    totalInputTokens: r.total_input_tokens as number,
    totalOutputTokens: r.total_output_tokens as number,
    metadata: JSON.parse(((r.metadata as string) ?? '{}') || '{}') as Record<string, unknown>,
  };
}

function toPushTokenRow(r: Record<string, unknown>): PushTokenRow {
  return {
    expoToken: r.expo_token as string,
    label: (r.label as string | null) ?? null,
    createdAt: r.created_at as string,
    lastUsedAt: (r.last_used_at as string | null) ?? null,
  };
}
