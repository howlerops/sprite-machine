import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import path from 'node:path';
import { WebSocket } from 'ws';
import { nowIso } from '@sprite-machine/shared';
import type {
  CheckpointHandle,
  CloneOpts,
  CreateOpts,
  ExecOpts,
  ExecResult,
  SpriteHandle,
  SpriteStatus,
  SpritesClient,
} from './types.js';

export interface FlySpritesClientOptions {
  apiToken: string;
  apiBaseUrl?: string;
  workerName?: string;
  // Per-sprite agent tokens; the control plane provisions these via writeFile.
  // For idempotent create() to work, we cache the token alongside the sprite name.
  // Caller persists this in DB. If unset on a fresh sprite, we generate one.
  agentTokens?: Map<string, string>;
}

interface RawSprite {
  id: string;
  name: string;
  status: 'cold' | 'warm' | 'running';
  url: string;
  url_settings?: { auth?: 'public' | 'sprite' };
  created_at: string;
  updated_at: string;
}

interface RawCheckpoint {
  id: string;
  create_time: string;
  source_id?: string;
  comment?: string;
  health?: string;
  is_auto?: boolean;
}

const DEFAULT_BASE = 'https://api.sprites.dev';

/**
 * FlySpritesClient — production implementation against api.sprites.dev.
 *
 * Architecture: SINGLE worker sprite (default name "sprite-machine"). Repeated
 * `create()` and `cloneFromCheckpoint()` calls return the same handle.
 * `suspend()` / `resume()` are observational — Sprites manages the
 * cold/warm/running lifecycle natively. Resume can be triggered by exec'ing a
 * no-op command (which auto-wakes a cold sprite).
 *
 * The `agentToken` returned in the handle is generated locally on first
 * create() and provisioned onto the sprite via writeFile by the caller (or
 * via the build-template / provision-worker scripts).
 */
export class FlySpritesClient implements SpritesClient {
  private readonly token: string;
  private readonly baseUrl: string;
  private readonly wsBase: string;
  private readonly workerName: string;
  private readonly agentTokens: Map<string, string>;

  constructor(opts: FlySpritesClientOptions) {
    this.token = opts.apiToken;
    this.baseUrl = (opts.apiBaseUrl ?? DEFAULT_BASE).replace(/\/$/, '');
    this.wsBase = this.baseUrl.replace(/^http/, 'ws');
    this.workerName = opts.workerName ?? 'sprite-machine';
    this.agentTokens = opts.agentTokens ?? new Map();
  }

  async create(opts: CreateOpts = {}): Promise<SpriteHandle> {
    const name = opts.name ?? this.workerName;
    let raw = await this.tryGetRaw(name);
    if (!raw) {
      raw = await this.postSprite({ name, url_settings: { auth: 'public' } });
    }
    return this.toHandle(raw);
  }

  async get(id: string): Promise<SpriteHandle | null> {
    const raw = await this.tryGetRaw(id);
    return raw ? this.toHandle(raw) : null;
  }

  async list(): Promise<SpriteHandle[]> {
    const r = (await this.fetchJson('/v1/sprites')) as { sprites: RawSprite[] };
    return r.sprites.map((s) => this.toHandle(s));
  }

  async destroy(id: string): Promise<void> {
    const res = await this.rawFetch(`/v1/sprites/${encodeURIComponent(id)}`, { method: 'DELETE' });
    if (!res.ok && res.status !== 404) {
      throw new Error(`destroy ${id} returned ${res.status}: ${await res.text()}`);
    }
    this.agentTokens.delete(id);
  }

  async suspend(id: string): Promise<SpriteHandle> {
    // Sprites natively hibernates idle sprites. Best we can do is observe
    // current status. Returning the current handle.
    const raw = await this.tryGetRaw(id);
    if (!raw) throw new Error(`unknown sprite: ${id}`);
    return this.toHandle(raw);
  }

  async resume(id: string): Promise<SpriteHandle> {
    let raw = await this.tryGetRaw(id);
    if (!raw) throw new Error(`unknown sprite: ${id}`);
    // Force a tiny exec even when the API says warm/running. That validates the
    // shell path and wakes stale/cold workers whose status snapshot lags behind
    // reality. It is effectively a no-op inside the sprite.
    await this.exec(id, ['true'], { timeoutMs: 60_000 });
    raw = (await this.tryGetRaw(id)) ?? raw;
    return this.toHandle(raw);
  }

  async checkpoint(id: string, label?: string): Promise<CheckpointHandle> {
    const body = JSON.stringify(label !== undefined ? { comment: label } : {});
    const res = await this.rawFetch(`/v1/sprites/${encodeURIComponent(id)}/checkpoint`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body,
    });
    if (!res.ok) {
      throw new Error(`checkpoint ${id} returned ${res.status}: ${await res.text()}`);
    }
    // Parse NDJSON: the `complete` event's data is "Checkpoint <id> created successfully".
    const text = await res.text();
    let newId: string | null = null;
    let lastErr: string | null = null;
    let createdAt: string | null = null;
    for (const line of text.split('\n')) {
      if (!line.trim()) continue;
      try {
        const ev = JSON.parse(line) as { type?: string; data?: unknown; time?: string };
        if (ev.type === 'error') lastErr = String(ev.data ?? 'unknown');
        if (ev.type === 'complete' && typeof ev.data === 'string') {
          const match = /Checkpoint\s+(\S+)\s+created/.exec(ev.data);
          if (match?.[1]) newId = match[1];
          if (typeof ev.time === 'string') createdAt = ev.time;
        }
        if (ev.type === 'info' && typeof ev.data === 'string' && !newId) {
          const m = /^\s*ID:\s+(\S+)/.exec(ev.data);
          if (m?.[1]) newId = m[1];
        }
      } catch {
        // ignore partial lines
      }
    }
    if (lastErr) throw new Error(`checkpoint ${id}: ${lastErr}`);
    if (!newId) {
      throw new Error(
        `checkpoint ${id}: could not parse new checkpoint id from response. body=${text.slice(0, 500)}`,
      );
    }
    return {
      id: newId,
      spriteId: id,
      label: label ?? null,
      createdAt: createdAt ?? new Date().toISOString(),
    };
  }

  async cloneFromCheckpoint(_checkpointId: string, _opts: CloneOpts = {}): Promise<SpriteHandle> {
    // Single-worker model: ALWAYS return the worker. opts.name is intentionally
    // ignored — the orchestrator passes a per-run name from its run-id naming
    // scheme, but in this model every run shares the same worker sprite. Per-run
    // isolation lives in the sidecar's session_id, not a new VM. To roll back
    // the worker to a checkpoint, call restoreCheckpoint() instead.
    return this.create({ name: this.workerName });
  }

  async exec(id: string, cmd: string[], opts: ExecOpts = {}): Promise<ExecResult> {
    if (cmd.length === 0) throw new Error('exec: empty command');
    const params = new URLSearchParams();
    for (const arg of cmd) params.append('cmd', arg);
    params.set('tty', 'false');
    params.set('stdin', opts.stdin !== undefined ? 'true' : 'false');
    if (opts.env) {
      for (const [k, v] of Object.entries(opts.env)) {
        params.append('env', `${k}=${v}`);
      }
    }
    if (opts.cwd) {
      // Sprites exec runs from $HOME; we wrap with a `cd` shim.
      // Replace the strategy: prefix command with `cd <cwd> && exec`.
    }
    const url = `${this.wsBase}/v1/sprites/${encodeURIComponent(id)}/exec?${params.toString()}`;
    const stdinBuf =
      opts.stdin !== undefined
        ? Buffer.isBuffer(opts.stdin)
          ? opts.stdin
          : Buffer.from(opts.stdin)
        : null;

    return await new Promise<ExecResult>((resolve, reject) => {
      const ws = new WebSocket(url, {
        headers: { Authorization: `Bearer ${this.token}` },
      });
      let stdout = '';
      let stderr = '';
      let exitCode: number | null = null;
      let timer: NodeJS.Timeout | null = null;
      let settled = false;
      const settle = (fn: () => void): void => {
        if (settled) return;
        settled = true;
        if (timer) clearTimeout(timer);
        fn();
      };

      ws.on('open', () => {
        if (stdinBuf) {
          // Stream stdin in chunks to stay well under typical 1 MiB frame caps.
          const chunkSize = 256 * 1024;
          for (let off = 0; off < stdinBuf.length; off += chunkSize) {
            const slice = stdinBuf.subarray(off, off + chunkSize);
            ws.send(Buffer.concat([Buffer.from([0]), slice]));
          }
          ws.send(Buffer.from([4])); // stdin_eof
        }
        if (opts.timeoutMs) {
          timer = setTimeout(() => {
            ws.terminate();
            settle(() => reject(new Error(`exec timeout after ${opts.timeoutMs}ms`)));
          }, opts.timeoutMs);
        }
      });

      ws.on('message', (data, isBinary) => {
        if (!isBinary) return; // JSON debug/info ignored
        const buf = data as Buffer;
        if (buf.length === 0) return;
        const sid = buf[0];
        const payload = buf.subarray(1);
        if (sid === 1) stdout += payload.toString('utf8');
        else if (sid === 2) stderr += payload.toString('utf8');
        else if (sid === 3) exitCode = payload[0] ?? -1;
      });

      ws.on('close', () => {
        settle(() => resolve({ exitCode: exitCode ?? -1, stdout, stderr }));
      });

      ws.on('error', (err) => {
        settle(() => reject(err));
      });
    });
  }

  async writeFile(id: string, remotePath: string, content: string | Buffer): Promise<void> {
    const buf = Buffer.isBuffer(content) ? content : Buffer.from(content);
    const dir = path.posix.dirname(remotePath);
    const r = await this.exec(
      id,
      ['bash', '-c', `mkdir -p ${shellQ(dir)} && cat > ${shellQ(remotePath)}`],
      { stdin: buf },
    );
    if (r.exitCode !== 0) {
      throw new Error(`writeFile ${remotePath} exit=${r.exitCode}: ${r.stderr}`);
    }
  }

  async readFile(id: string, remotePath: string): Promise<Buffer> {
    const r = await this.exec(id, ['cat', remotePath]);
    if (r.exitCode !== 0) {
      throw new Error(`readFile ${remotePath} exit=${r.exitCode}: ${r.stderr}`);
    }
    return Buffer.from(r.stdout, 'utf8');
  }

  async uploadDir(id: string, localDir: string, remoteDir: string): Promise<void> {
    const tarBuf = await tarLocalDir(localDir);
    await this.exec(id, ['mkdir', '-p', remoteDir]);
    const r = await this.exec(id, ['tar', '-xf', '-', '-C', remoteDir], { stdin: tarBuf });
    if (r.exitCode !== 0) {
      throw new Error(`uploadDir ${localDir} → ${remoteDir} exit=${r.exitCode}: ${r.stderr}`);
    }
  }

  /** Restore the worker (or any sprite) to a prior checkpoint, in-place. */
  async restoreCheckpoint(id: string, checkpointId: string): Promise<void> {
    const res = await this.rawFetch(
      `/v1/sprites/${encodeURIComponent(id)}/checkpoints/${encodeURIComponent(checkpointId)}/restore`,
      { method: 'POST' },
    );
    if (!res.ok) {
      throw new Error(`restore ${id}@${checkpointId} returned ${res.status}: ${await res.text()}`);
    }
    await res.text(); // drain NDJSON
  }

  async putService(
    id: string,
    serviceName: string,
    spec: { cmd: string; args?: string[]; http_port?: number; needs?: string[] },
  ): Promise<void> {
    const res = await this.rawFetch(
      `/v1/sprites/${encodeURIComponent(id)}/services/${encodeURIComponent(serviceName)}`,
      {
        method: 'PUT',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(spec),
      },
    );
    if (!res.ok) {
      throw new Error(`putService ${serviceName} returned ${res.status}: ${await res.text()}`);
    }
  }

  async startService(id: string, serviceName: string): Promise<void> {
    const res = await this.rawFetch(
      `/v1/sprites/${encodeURIComponent(id)}/services/${encodeURIComponent(serviceName)}/start`,
      { method: 'POST' },
    );
    if (!res.ok && res.status !== 200 && res.status !== 202) {
      throw new Error(`startService ${serviceName} returned ${res.status}: ${await res.text()}`);
    }
  }

  async stopService(id: string, serviceName: string): Promise<void> {
    const res = await this.rawFetch(
      `/v1/sprites/${encodeURIComponent(id)}/services/${encodeURIComponent(serviceName)}/stop`,
      { method: 'POST' },
    );
    if (!res.ok && res.status !== 404) {
      throw new Error(`stopService ${serviceName} returned ${res.status}: ${await res.text()}`);
    }
  }

  async listServices(id: string): Promise<unknown[]> {
    return (await this.fetchJson(`/v1/sprites/${encodeURIComponent(id)}/services`)) as unknown[];
  }

  async listCheckpoints(id: string): Promise<CheckpointHandle[]> {
    const list = (await this.fetchJson(
      `/v1/sprites/${encodeURIComponent(id)}/checkpoints`,
    )) as RawCheckpoint[];
    return list
      .filter((c) => c.id !== 'Current' && !c.is_auto)
      .map((c) => ({
        id: c.id,
        spriteId: id,
        label: c.comment ?? null,
        createdAt: c.create_time,
      }));
  }

  // --- internals ---

  private async tryGetRaw(name: string): Promise<RawSprite | null> {
    const res = await this.rawFetch(`/v1/sprites/${encodeURIComponent(name)}`);
    if (res.status === 404) return null;
    if (!res.ok) throw new Error(`get ${name} returned ${res.status}: ${await res.text()}`);
    return (await res.json()) as RawSprite;
  }

  private async postSprite(body: {
    name: string;
    url_settings?: { auth?: 'public' | 'sprite' };
  }): Promise<RawSprite> {
    const res = await this.rawFetch('/v1/sprites', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      throw new Error(`create sprite returned ${res.status}: ${await res.text()}`);
    }
    return (await res.json()) as RawSprite;
  }

  private async fetchJson(path: string): Promise<unknown> {
    const res = await this.rawFetch(path);
    if (!res.ok) throw new Error(`GET ${path} returned ${res.status}: ${await res.text()}`);
    return await res.json();
  }

  private async rawFetch(path: string, init: RequestInit = {}): Promise<Response> {
    const headers = new Headers(init.headers);
    headers.set('authorization', `Bearer ${this.token}`);
    return await fetch(`${this.baseUrl}${path}`, { ...init, headers });
  }

  private toHandle(raw: RawSprite): SpriteHandle {
    let token = this.agentTokens.get(raw.name);
    if (!token) {
      token = randomBytes(24).toString('hex');
      this.agentTokens.set(raw.name, token);
    }
    return {
      id: raw.name,
      status: mapStatus(raw.status),
      agentBaseUrl: raw.url,
      agentToken: token,
      fromCheckpointId: null,
      createdAt: raw.created_at,
      updatedAt: raw.updated_at,
      meta: { spriteUuid: raw.id, rawStatus: raw.status, urlAuth: raw.url_settings?.auth },
    };
  }
}

function mapStatus(s: RawSprite['status']): SpriteStatus {
  switch (s) {
    case 'cold':
      return 'suspended';
    case 'warm':
    case 'running':
      return 'running';
    default:
      return 'creating';
  }
}

function shellQ(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

async function tarLocalDir(dir: string): Promise<Buffer> {
  return await new Promise((resolve, reject) => {
    const child = spawn('tar', ['-cf', '-', '-C', dir, '.'], {
      stdio: ['ignore', 'pipe', 'pipe'],
      // COPYFILE_DISABLE: macOS tar would otherwise emit ._* AppleDouble files.
      env: { ...process.env, COPYFILE_DISABLE: '1' },
    });
    const chunks: Buffer[] = [];
    let stderr = '';
    child.stdout.on('data', (c: Buffer) => chunks.push(c));
    child.stderr.on('data', (c: Buffer) => (stderr += c.toString()));
    child.on('error', reject);
    child.on('exit', (code) => {
      if (code !== 0) reject(new Error(`tar exit ${code}: ${stderr}`));
      else resolve(Buffer.concat(chunks));
    });
  });
}

void nowIso; // keep import to retain shared dependency for type stability
