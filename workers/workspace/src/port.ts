// The narrow seam between the Workspace DO and the sandbox. The DO only ever talks to a
// SandboxPort: `CloudflareSandbox` (src/sandbox.ts) drives the real Container + @cloudflare/sandbox,
// `FakeSandbox` (src/fake.ts) is an in-memory stand-in for tests and `wrangler dev`.

import type { BackupHandle, InstanceType } from "./types";

export type PortErrorCode = "not_found" | "sandbox_error" | "invalid_state";

export class SandboxPortError extends Error {
  readonly code: PortErrorCode;
  constructor(code: PortErrorCode, message: string) {
    super(message);
    this.name = "SandboxPortError";
    this.code = code;
  }
}

export interface PortExecResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

export interface PortSnapshot {
  id: string;
  size: number;
  name?: string;
}

export interface SandboxPort {
  /** True while the container is up (the container can also exit on its own). */
  readonly running: boolean;
  start(opts: { image: string; instance: InstanceType }): Promise<void>;
  /** Replace whatever is running with a container restored from a snapshot. */
  startFromSnapshot(opts: { id: string; instance: InstanceType }): Promise<void>;
  stop(): Promise<void>;
  snapshot(name?: string): Promise<PortSnapshot>;
  backup(dir: string, name?: string): Promise<BackupHandle>;
  restoreBackup(handle: BackupHandle, dir?: string): Promise<void>;
  exec(
    cmd: string[],
    opts: { env?: Record<string, string>; cwd?: string },
  ): Promise<PortExecResult>;
  /** Forward a request to a TCP port inside the sandbox (previews). */
  fetchPort(port: number, request: Request): Promise<Response>;
}
