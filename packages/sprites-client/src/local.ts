import { spawn, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync } from 'node:fs';
import { cp, mkdir, readFile, readdir, rm, writeFile, stat } from 'node:fs/promises';
import { createServer } from 'node:net';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { newId, nowIso } from '@sprite-machine/shared';
import type {
  CheckpointHandle,
  CloneOpts,
  CreateOpts,
  ExecOpts,
  ExecResult,
  SpriteHandle,
  SpritesClient,
} from './types.js';

interface PersistedHandle extends SpriteHandle {
  port: number | null;
}

interface PersistedCheckpoint extends CheckpointHandle {
  rootPath: string;
}

export interface LocalSpritesClientOptions {
  rootDir: string;
  workspaceRoot: string;
  spawnSidecar?: boolean;
  sidecarMode?: 'auto' | 'sdk' | 'mock';
}

const PROCESSES = new Map<string, ChildProcess>();

export class LocalSpritesClient implements SpritesClient {
  constructor(private readonly opts: LocalSpritesClientOptions) {}

  async create(opts: CreateOpts = {}): Promise<SpriteHandle> {
    const id = newId('lsp');
    const root = this.spritePath(id);
    await mkdir(path.join(root, 'agent-data'), { recursive: true });
    await mkdir(path.join(root, 'home'), { recursive: true });
    await mkdir(path.join(root, 'workdir'), { recursive: true });
    if (opts.fromCheckpointId) {
      const ckpt = await this.loadCheckpoint(opts.fromCheckpointId);
      if (!ckpt) throw new Error(`unknown checkpoint: ${opts.fromCheckpointId}`);
      await rm(root, { recursive: true, force: true });
      await cp(ckpt.rootPath, root, { recursive: true });
    }
    const handle: PersistedHandle = {
      id,
      status: 'creating',
      agentBaseUrl: null,
      agentToken: randomBytes(24).toString('hex'),
      fromCheckpointId: opts.fromCheckpointId ?? null,
      createdAt: nowIso(),
      updatedAt: nowIso(),
      meta: { name: opts.name, ...(opts.meta ?? {}) },
      port: null,
    };
    await this.saveHandle(handle);
    if (this.opts.spawnSidecar !== false) {
      return await this.startSidecar(handle);
    }
    return this.toPublic(handle);
  }

  async get(id: string): Promise<SpriteHandle | null> {
    const handle = await this.loadHandle(id);
    return handle ? this.toPublic(handle) : null;
  }

  async list(): Promise<SpriteHandle[]> {
    const dir = path.join(this.opts.rootDir, 'sprites');
    if (!existsSync(dir)) return [];
    const ids = await readdir(dir);
    const handles: SpriteHandle[] = [];
    for (const id of ids) {
      const h = await this.loadHandle(id);
      if (h) handles.push(this.toPublic(h));
    }
    return handles;
  }

  async destroy(id: string): Promise<void> {
    const handle = await this.loadHandle(id);
    if (!handle) return;
    await this.killSidecar(id);
    handle.status = 'destroyed';
    await this.saveHandle(handle);
    await rm(this.spritePath(id), { recursive: true, force: true });
  }

  async suspend(id: string): Promise<SpriteHandle> {
    const handle = await this.requireHandle(id);
    await this.killSidecar(id);
    handle.status = 'suspended';
    handle.agentBaseUrl = null;
    handle.port = null;
    handle.updatedAt = nowIso();
    await this.saveHandle(handle);
    return this.toPublic(handle);
  }

  async resume(id: string): Promise<SpriteHandle> {
    const handle = await this.requireHandle(id);
    if (handle.status === 'destroyed') throw new Error('cannot resume destroyed sprite');
    if (handle.status === 'running') return this.toPublic(handle);
    return await this.startSidecar(handle);
  }

  async checkpoint(id: string, label?: string): Promise<CheckpointHandle> {
    const handle = await this.requireHandle(id);
    const ckptId = newId('ckpt');
    const ckptRoot = path.join(this.opts.rootDir, 'checkpoints', ckptId);
    await mkdir(path.dirname(ckptRoot), { recursive: true });
    await cp(this.spritePath(id), ckptRoot, {
      recursive: true,
      filter: (src) => !src.endsWith('.pid') && !src.endsWith('handle.json'),
    });
    const ckpt: PersistedCheckpoint = {
      id: ckptId,
      spriteId: handle.id,
      label: label ?? null,
      createdAt: nowIso(),
      rootPath: ckptRoot,
    };
    await writeFile(
      path.join(this.opts.rootDir, 'checkpoints', `${ckptId}.json`),
      JSON.stringify(ckpt, null, 2),
    );
    return { id: ckpt.id, spriteId: ckpt.spriteId, label: ckpt.label, createdAt: ckpt.createdAt };
  }

  async cloneFromCheckpoint(checkpointId: string, opts: CloneOpts = {}): Promise<SpriteHandle> {
    return this.create({
      ...(opts.name !== undefined ? { name: opts.name } : {}),
      ...(opts.meta !== undefined ? { meta: opts.meta } : {}),
      fromCheckpointId: checkpointId,
    });
  }

  async exec(id: string, cmd: string[], opts: ExecOpts = {}): Promise<ExecResult> {
    const handle = await this.requireHandle(id);
    const cwd = opts.cwd
      ? path.isAbsolute(opts.cwd)
        ? opts.cwd
        : path.join(this.spritePath(handle.id), 'workdir', opts.cwd)
      : path.join(this.spritePath(handle.id), 'workdir');
    return await new Promise((resolve, reject) => {
      const child = spawn(cmd[0]!, cmd.slice(1), {
        cwd,
        env: {
          ...process.env,
          HOME: path.join(this.spritePath(handle.id), 'home'),
          ...(opts.env ?? {}),
        },
        stdio: ['pipe', 'pipe', 'pipe'],
      });
      let stdout = '';
      let stderr = '';
      child.stdout.on('data', (d) => (stdout += d.toString()));
      child.stderr.on('data', (d) => (stderr += d.toString()));
      let timer: NodeJS.Timeout | null = null;
      if (opts.timeoutMs) {
        timer = setTimeout(() => child.kill('SIGKILL'), opts.timeoutMs);
      }
      child.on('error', (err) => {
        if (timer) clearTimeout(timer);
        reject(err);
      });
      child.on('exit', (code) => {
        if (timer) clearTimeout(timer);
        resolve({ exitCode: code ?? -1, stdout, stderr });
      });
      if (opts.stdin !== undefined) child.stdin.end(opts.stdin);
      else child.stdin.end();
    });
  }

  async writeFile(id: string, remotePath: string, content: string | Buffer): Promise<void> {
    const target = this.resolveRemotePath(id, remotePath);
    await mkdir(path.dirname(target), { recursive: true });
    await writeFile(target, content);
  }

  async readFile(id: string, remotePath: string): Promise<Buffer> {
    return await readFile(this.resolveRemotePath(id, remotePath));
  }

  async uploadDir(id: string, localDir: string, remoteDir: string): Promise<void> {
    const target = this.resolveRemotePath(id, remoteDir);
    await mkdir(target, { recursive: true });
    await cp(localDir, target, { recursive: true });
  }

  private resolveRemotePath(id: string, remotePath: string): string {
    const root = this.spritePath(id);
    if (remotePath.startsWith('~')) {
      return path.join(root, 'home', remotePath.slice(1).replace(/^\/+/, ''));
    }
    if (path.isAbsolute(remotePath)) {
      return path.join(root, remotePath.replace(/^\/+/, ''));
    }
    return path.join(root, 'workdir', remotePath);
  }

  private spritePath(id: string): string {
    return path.join(this.opts.rootDir, 'sprites', id);
  }

  private async saveHandle(handle: PersistedHandle): Promise<void> {
    handle.updatedAt = nowIso();
    await mkdir(this.spritePath(handle.id), { recursive: true });
    await writeFile(
      path.join(this.spritePath(handle.id), 'handle.json'),
      JSON.stringify(handle, null, 2),
    );
  }

  private async loadHandle(id: string): Promise<PersistedHandle | null> {
    const file = path.join(this.spritePath(id), 'handle.json');
    if (!existsSync(file)) return null;
    return JSON.parse(await readFile(file, 'utf8')) as PersistedHandle;
  }

  private async requireHandle(id: string): Promise<PersistedHandle> {
    const h = await this.loadHandle(id);
    if (!h) throw new Error(`unknown sprite: ${id}`);
    return h;
  }

  private async loadCheckpoint(checkpointId: string): Promise<PersistedCheckpoint | null> {
    const file = path.join(this.opts.rootDir, 'checkpoints', `${checkpointId}.json`);
    if (!existsSync(file)) return null;
    return JSON.parse(await readFile(file, 'utf8')) as PersistedCheckpoint;
  }

  private async startSidecar(handle: PersistedHandle): Promise<SpriteHandle> {
    const port = await pickFreePort();
    const tsxBin = path.join(this.opts.workspaceRoot, 'node_modules', '.bin', 'tsx');
    const entry = path.join(this.opts.workspaceRoot, 'packages', 'sprite-agent', 'src', 'index.ts');
    const child = spawn(tsxBin, [entry], {
      env: {
        ...process.env,
        PORT: String(port),
        HOST: '127.0.0.1',
        SPRITE_ID: handle.id,
        SPRITE_AGENT_TOKEN: handle.agentToken,
        SPRITE_AGENT_DATA_DIR: path.join(this.spritePath(handle.id), 'agent-data'),
        SPRITE_AGENT_MODE: this.opts.sidecarMode ?? 'auto',
        SPRITE_AGENT_WORKDIR: path.join(this.spritePath(handle.id), 'workdir'),
        HOME: path.join(this.spritePath(handle.id), 'home'),
      },
      stdio: ['ignore', 'pipe', 'pipe'],
      detached: false,
    });
    PROCESSES.set(handle.id, child);
    child.stdout.on('data', (d) => process.stdout.write(`[sprite ${handle.id}] ${d}`));
    child.stderr.on('data', (d) => process.stderr.write(`[sprite ${handle.id}] ${d}`));
    child.on('exit', () => {
      PROCESSES.delete(handle.id);
    });

    handle.port = port;
    handle.agentBaseUrl = `http://127.0.0.1:${port}`;
    handle.status = 'running';
    await this.saveHandle(handle);

    // wait for /healthz
    for (let i = 0; i < 50; i++) {
      try {
        const res = await fetch(`${handle.agentBaseUrl}/healthz`);
        if (res.ok) break;
      } catch {
        // not yet
      }
      await delay(100);
    }
    return this.toPublic(handle);
  }

  private async killSidecar(id: string): Promise<void> {
    const child = PROCESSES.get(id);
    if (child) {
      child.kill('SIGTERM');
      await new Promise<void>((resolve) => {
        if (child.exitCode !== null) return resolve();
        const timer = setTimeout(() => {
          child.kill('SIGKILL');
          resolve();
        }, 1500);
        child.on('exit', () => {
          clearTimeout(timer);
          resolve();
        });
      });
      PROCESSES.delete(id);
    }
    // ensure no orphaned process from a prior process tree (best-effort)
    const root = this.spritePath(id);
    if (existsSync(root)) {
      try {
        await stat(root);
      } catch {
        // ignore
      }
    }
  }

  private toPublic(h: PersistedHandle): SpriteHandle {
    const { port: _port, ...pub } = h;
    return pub;
  }
}

async function pickFreePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const srv = createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      srv.close(() => {
        if (addr && typeof addr === 'object') resolve(addr.port);
        else reject(new Error('failed to acquire free port'));
      });
    });
  });
}
