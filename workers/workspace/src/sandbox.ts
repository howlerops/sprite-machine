// SandboxPort over the real Cloudflare Container (`ctx.container`) and @cloudflare/sandbox 1.0.
// 1.0 has no getSandbox()/Sandbox class: the Durable Object owns the Container and the package
// adds `DirectoryBackup` (R2 backups), `Files` and `S3Mount`. Snapshots and exec are native
// Container methods. Not exercised locally (needs real containers/FUSE): verified on the deployed
// preview environment by later leader-run items.

import { DirectoryBackup, SandboxBackupError, SandboxFileError } from "@cloudflare/sandbox";
import type { DirectoryBackupGatewayBinding, DirectoryBackupRecord } from "@cloudflare/sandbox";
import {
  SandboxPortError,
  type PortExecResult,
  type PortSnapshot,
  type SandboxPort,
} from "./port";
import type { BackupHandle, InstanceType } from "./types";

export interface CloudflareSandboxDeps {
  container: Container;
  gateway: DirectoryBackupGatewayBinding;
  /** R2 binding name (wrangler.jsonc r2_buckets) and key prefix for DirectoryBackup. */
  backupBinding: string;
  backupPrefix: string;
}

function toPortError(err: unknown): SandboxPortError {
  if (err instanceof SandboxPortError) return err;
  if (SandboxBackupError.is(err)) {
    return new SandboxPortError(
      err.code === "BACKUP_NOT_FOUND" ? "not_found" : "sandbox_error",
      `${err.code}: ${err.detail}`,
    );
  }
  if (SandboxFileError.is(err)) {
    return new SandboxPortError("sandbox_error", `${err.code} ${err.operation} ${err.path}`);
  }
  return new SandboxPortError("sandbox_error", err instanceof Error ? err.message : String(err));
}

export class CloudflareSandbox implements SandboxPort {
  readonly #c: Container;
  readonly #backup: DirectoryBackup;
  readonly #dec = new TextDecoder();

  constructor(deps: CloudflareSandboxDeps) {
    this.#c = deps.container;
    this.#backup = new DirectoryBackup(deps.container, deps.gateway, {
      binding: deps.backupBinding,
      prefix: deps.backupPrefix,
    });
  }

  get running(): boolean {
    return this.#c.running;
  }

  async start(opts: { image: string; instance: InstanceType }): Promise<void> {
    try {
      // Credentials never go in; egress is off until the outbound handler (later item) allows hosts.
      this.#c.start({ image: opts.image, instance: opts.instance, enableInternet: false });
    } catch (e) {
      throw toPortError(e);
    }
  }

  async startFromSnapshot(opts: { id: string; instance: InstanceType }): Promise<void> {
    try {
      if (this.#c.running) await this.#c.destroy();
      this.#c.start({
        containerSnapshot: { id: opts.id },
        instance: opts.instance,
        enableInternet: false,
      });
    } catch (e) {
      throw toPortError(e);
    }
  }

  async stop(): Promise<void> {
    try {
      if (this.#c.running) await this.#c.destroy();
    } catch (e) {
      throw toPortError(e);
    }
  }

  async snapshot(name?: string): Promise<PortSnapshot> {
    try {
      const s = await this.#c.snapshotContainer(name ? { name } : undefined);
      return { id: s.id, size: s.size, ...(s.name ? { name: s.name } : {}) };
    } catch (e) {
      throw toPortError(e);
    }
  }

  async backup(dir: string, name?: string): Promise<BackupHandle> {
    try {
      const r = await this.#backup.backup({ dir, ...(name ? { name } : {}), gitignore: true });
      return { ...r };
    } catch (e) {
      throw toPortError(e);
    }
  }

  async restoreBackup(handle: BackupHandle, dir?: string): Promise<void> {
    try {
      // The record is plain data that round-tripped through JSON; the SDK re-checks sha256 on restore.
      const record = handle as unknown as DirectoryBackupRecord;
      await this.#backup.restore(record, dir ? { dir } : undefined);
    } catch (e) {
      throw toPortError(e);
    }
  }

  async exec(
    cmd: string[],
    opts: { env?: Record<string, string>; cwd?: string },
  ): Promise<PortExecResult> {
    try {
      const p = await this.#c.exec(cmd, {
        ...(opts.env ? { env: opts.env } : {}),
        ...(opts.cwd ? { cwd: opts.cwd } : {}),
      });
      const out = await p.output();
      return {
        exitCode: out.exitCode,
        stdout: this.#dec.decode(out.stdout),
        stderr: this.#dec.decode(out.stderr),
      };
    } catch (e) {
      throw toPortError(e);
    }
  }

  async fetchPort(port: number, request: Request): Promise<Response> {
    try {
      return await this.#c.getTcpPort(port).fetch(request);
    } catch (e) {
      throw toPortError(e);
    }
  }
}
