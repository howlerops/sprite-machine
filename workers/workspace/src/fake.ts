// In-memory SandboxPort. Selected by DEV_FAKE_SANDBOX=1; the only sandbox usable under
// `wrangler dev --enable-containers=false` and in vitest.

import {
  SandboxPortError,
  type PortExecResult,
  type PortSnapshot,
  type SandboxPort,
} from "./port";
import type { BackupHandle, InstanceType } from "./types";

export type FakeOp =
  | "start"
  | "startFromSnapshot"
  | "stop"
  | "snapshot"
  | "backup"
  | "restoreBackup"
  | "exec"
  | "fetchPort";

export class FakeSandbox implements SandboxPort {
  running = false;
  image: string | null = null;
  instance: InstanceType | null = null;
  readonly snapshots = new Map<string, PortSnapshot>();
  readonly backups = new Map<string, BackupHandle>();
  readonly calls: string[] = [];
  private seq = 0;
  private failures = new Map<FakeOp, string>();

  /** Make the next call of `op` throw a sandbox_error with `message`. */
  failNext(op: FakeOp, message = `injected ${op} failure`): void {
    this.failures.set(op, message);
  }

  private enter(op: FakeOp): void {
    this.calls.push(op);
    const msg = this.failures.get(op);
    if (msg !== undefined) {
      this.failures.delete(op);
      throw new SandboxPortError("sandbox_error", msg);
    }
  }

  async start(opts: { image: string; instance: InstanceType }): Promise<void> {
    this.enter("start");
    this.running = true;
    this.image = opts.image;
    this.instance = opts.instance;
  }

  async startFromSnapshot(opts: { id: string; instance: InstanceType }): Promise<void> {
    this.enter("startFromSnapshot");
    if (!this.snapshots.has(opts.id)) {
      throw new SandboxPortError("not_found", `unknown snapshot ${opts.id}`);
    }
    this.running = true;
    this.instance = opts.instance;
  }

  async stop(): Promise<void> {
    this.enter("stop");
    this.running = false;
  }

  async snapshot(name?: string): Promise<PortSnapshot> {
    this.enter("snapshot");
    const snap: PortSnapshot = { id: `snap-${++this.seq}`, size: 1024, ...(name ? { name } : {}) };
    this.snapshots.set(snap.id, snap);
    return snap;
  }

  async backup(dir: string, name?: string): Promise<BackupHandle> {
    this.enter("backup");
    const handle: BackupHandle = {
      id: `bak-${++this.seq}`,
      dir,
      size: 2048,
      sha256: "0".repeat(64),
      format: "tar+zstd/1",
      ...(name ? { name } : {}),
    };
    this.backups.set(handle.id, handle);
    return handle;
  }

  async restoreBackup(handle: BackupHandle, _dir?: string): Promise<void> {
    this.enter("restoreBackup");
    if (!this.backups.has(handle.id)) {
      throw new SandboxPortError("not_found", `unknown backup ${handle.id}`);
    }
  }

  /** `echo` prints its args, `false` exits 1, `env` prints KEY=VALUE lines; anything else exits 127. */
  async exec(
    cmd: string[],
    opts: { env?: Record<string, string>; cwd?: string },
  ): Promise<PortExecResult> {
    this.enter("exec");
    const [bin, ...args] = cmd;
    switch (bin) {
      case "echo":
        return { exitCode: 0, stdout: `${args.join(" ")}\n`, stderr: "" };
      case "false":
        return { exitCode: 1, stdout: "", stderr: "" };
      case "env":
        return {
          exitCode: 0,
          stdout: Object.entries(opts.env ?? {})
            .map(([k, v]) => `${k}=${v}\n`)
            .join(""),
          stderr: "",
        };
      default:
        return { exitCode: 127, stdout: "", stderr: `${bin}: command not found\n` };
    }
  }

  async fetchPort(port: number, request: Request): Promise<Response> {
    this.enter("fetchPort");
    return new Response(`fake preview port=${port} path=${new URL(request.url).pathname}`);
  }
}
