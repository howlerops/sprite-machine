// Workspace logic: argument validation, the stopped/running state machine, and mapping of
// failures to typed results. Pure over a SandboxPort and a tiny key-value store, so it runs the
// same against FakeSandbox and the real sandbox.

import { SandboxPortError, type SandboxPort } from "./port";
import {
  INSTANCE_TYPES,
  type BackupHandle,
  type BackupResponse,
  type ErrorCode,
  type ExecResponse,
  type InstanceType,
  type PreviewResponse,
  type Result,
  type RestoreBackupResponse,
  type RestoreResponse,
  type SnapshotResponse,
  type StartResponse,
  type StatusResponse,
  type StopResponse,
} from "./types";

export interface KvStore {
  get<T>(key: string): Promise<T | undefined>;
  put<T>(key: string, value: T): Promise<void>;
}

interface Persisted {
  image: string | null;
  instance: InstanceType | null;
  startedAt: number | null;
  lastSnapshotId: string | null;
}

const KEY = "workspace";
const DEFAULT_INSTANCE: InstanceType = "standard-2";
const EMPTY: Persisted = { image: null, instance: null, startedAt: null, lastSnapshotId: null };

class ArgError extends Error {
  constructor(
    readonly code: ErrorCode,
    message: string,
  ) {
    super(message);
  }
}

const bad = (m: string): never => {
  throw new ArgError("invalid_argument", m);
};
const state = (m: string): never => {
  throw new ArgError("invalid_state", m);
};

function obj(v: unknown, what: string): Record<string, unknown> {
  if (typeof v !== "object" || v === null || Array.isArray(v)) return bad(`${what} must be an object`);
  return v as Record<string, unknown>;
}

function str(v: unknown, what: string, max: number): string {
  if (typeof v !== "string" || v.length === 0) return bad(`${what} must be a non-empty string`);
  if (v.length > max) return bad(`${what} is longer than ${max} characters`);
  if (v.includes("\0")) return bad(`${what} must not contain NUL`);
  return v;
}

function optStr(v: unknown, what: string, max: number): string | undefined {
  return v === undefined ? undefined : str(v, what, max);
}

function absDir(v: unknown, what: string): string {
  const s = str(v, what, 4096);
  if (!s.startsWith("/")) return bad(`${what} must be an absolute path`);
  if (s.split("/").includes("..")) return bad(`${what} must not contain ..`);
  return s;
}

function instance(v: unknown): InstanceType {
  if (v === undefined) return DEFAULT_INSTANCE;
  if (typeof v === "string" && (INSTANCE_TYPES as readonly string[]).includes(v)) {
    return v as InstanceType;
  }
  return bad(`instance must be one of ${INSTANCE_TYPES.join(", ")}`);
}

function env(v: unknown): Record<string, string> | undefined {
  if (v === undefined) return undefined;
  const o = obj(v, "env");
  const out: Record<string, string> = {};
  for (const [k, val] of Object.entries(o)) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(k)) return bad(`env key ${JSON.stringify(k)} is invalid`);
    if (typeof val !== "string" || val.includes("\0")) return bad(`env value for ${k} must be a string`);
    out[k] = val;
  }
  return out;
}

function handle(v: unknown): BackupHandle {
  const o = obj(v, "handle");
  const size = o["size"];
  if (typeof size !== "number" || !Number.isFinite(size) || size < 0) return bad("handle.size must be a number");
  const name = optStr(o["name"], "handle.name", 256);
  return {
    id: str(o["id"], "handle.id", 128),
    dir: absDir(o["dir"], "handle.dir"),
    size,
    sha256: str(o["sha256"], "handle.sha256", 128),
    format: str(o["format"], "handle.format", 64),
    ...(name ? { name } : {}),
  };
}

export class WorkspaceCore {
  constructor(
    private readonly port: SandboxPort,
    private readonly store: KvStore,
    private readonly now: () => number = Date.now,
  ) {}

  private async load(): Promise<Persisted> {
    return (await this.store.get<Persisted>(KEY)) ?? { ...EMPTY };
  }

  private save(p: Persisted): Promise<void> {
    return this.store.put(KEY, p);
  }

  private async guard<T>(fn: () => Promise<T>): Promise<Result<T>> {
    try {
      return { ok: true, value: await fn() };
    } catch (e) {
      if (e instanceof ArgError) return { ok: false, error: { code: e.code, message: e.message } };
      if (e instanceof SandboxPortError) {
        const code: ErrorCode = e.code === "not_found" ? "not_found" : e.code === "invalid_state" ? "invalid_state" : "sandbox_error";
        return { ok: false, error: { code, message: e.message } };
      }
      return { ok: false, error: { code: "internal", message: e instanceof Error ? e.message : String(e) } };
    }
  }

  private requireRunning(): void {
    if (!this.port.running) state("workspace is stopped; call start first");
  }

  start(req: unknown): Promise<Result<StartResponse>> {
    return this.guard(async () => {
      const o = obj(req, "request");
      const image = str(o["image"], "image", 512);
      const inst = instance(o["instance"]);
      if (this.port.running) state("workspace is already running");
      await this.port.start({ image, instance: inst });
      const p = await this.load();
      await this.save({ ...p, image, instance: inst, startedAt: this.now() });
      return { state: "running", image, instance: inst } as const;
    });
  }

  stop(_req?: unknown): Promise<Result<StopResponse>> {
    return this.guard(async () => {
      const wasRunning = this.port.running;
      if (wasRunning) await this.port.stop();
      const p = await this.load();
      await this.save({ ...p, startedAt: null });
      return { state: "stopped", wasRunning } as const;
    });
  }

  status(_req?: unknown): Promise<Result<StatusResponse>> {
    return this.guard(async () => {
      const p = await this.load();
      const running = this.port.running;
      return {
        state: running ? "running" : "stopped",
        image: p.image,
        instance: p.instance,
        startedAt: running ? p.startedAt : null,
        lastSnapshotId: p.lastSnapshotId,
      } as const;
    });
  }

  snapshot(req: unknown): Promise<Result<SnapshotResponse>> {
    return this.guard(async () => {
      const o = obj(req ?? {}, "request");
      const name = optStr(o["name"], "name", 256);
      this.requireRunning();
      const s = await this.port.snapshot(name);
      const p = await this.load();
      await this.save({ ...p, lastSnapshotId: s.id });
      return s;
    });
  }

  restore(req: unknown): Promise<Result<RestoreResponse>> {
    return this.guard(async () => {
      const id = str(obj(req, "request")["id"], "id", 128);
      const p = await this.load();
      await this.port.startFromSnapshot({ id, instance: p.instance ?? DEFAULT_INSTANCE });
      await this.save({ ...p, instance: p.instance ?? DEFAULT_INSTANCE, startedAt: this.now() });
      return { state: "running", restoredFrom: id } as const;
    });
  }

  backup(req: unknown): Promise<Result<BackupResponse>> {
    return this.guard(async () => {
      const o = obj(req, "request");
      const dir = absDir(o["dir"], "dir");
      const name = optStr(o["name"], "name", 256);
      this.requireRunning();
      return { handle: await this.port.backup(dir, name) };
    });
  }

  restoreBackup(req: unknown): Promise<Result<RestoreBackupResponse>> {
    return this.guard(async () => {
      const o = obj(req, "request");
      const h = handle(o["handle"]);
      const dir = o["dir"] === undefined ? undefined : absDir(o["dir"], "dir");
      this.requireRunning();
      await this.port.restoreBackup(h, dir);
      return { restored: true, dir: dir ?? h.dir } as const;
    });
  }

  exec(req: unknown): Promise<Result<ExecResponse>> {
    return this.guard(async () => {
      const o = obj(req, "request");
      const cmd = o["cmd"];
      if (!Array.isArray(cmd) || cmd.length === 0 || cmd.length > 256) return bad("cmd must be a non-empty array of at most 256 strings");
      const argv = cmd.map((c, i) => str(c, `cmd[${i}]`, 65536));
      const e = env(o["env"]);
      const cwd = o["cwd"] === undefined ? undefined : absDir(o["cwd"], "cwd");
      this.requireRunning();
      return this.port.exec(argv, { ...(e ? { env: e } : {}), ...(cwd ? { cwd } : {}) });
    });
  }

  preview(req: unknown): Promise<Result<PreviewResponse>> {
    return this.guard(async () => {
      const port = obj(req, "request")["port"];
      if (typeof port !== "number" || !Number.isInteger(port) || port < 1 || port > 65535) {
        return bad("port must be an integer in 1..65535");
      }
      this.requireRunning();
      return { port, path: `/preview/${port}/` };
    });
  }

  /** Proxy for `/preview/<port>/...` requests (the DO's fetch handler). */
  async proxyPreview(port: number, request: Request): Promise<Response> {
    if (!this.port.running) return new Response("workspace is stopped", { status: 409 });
    try {
      return await this.port.fetchPort(port, request);
    } catch (e) {
      return new Response(e instanceof Error ? e.message : "preview failed", { status: 502 });
    }
  }
}
