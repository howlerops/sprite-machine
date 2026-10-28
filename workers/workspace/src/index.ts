// Workspace Durable Object (plan §4.3): the one TypeScript exception. One DO per cloud workspace.
// Exposed two ways: JS RPC methods (Rust hub binds the namespace) and `POST /rpc/<method>` with a
// JSON body (same request/response types, see types.ts).

import { DurableObject } from "cloudflare:workers";
import type { DirectoryBackupGatewayBinding } from "@cloudflare/sandbox";
import { WorkspaceCore } from "./core";
import { FakeSandbox } from "./fake";
import type { SandboxPort } from "./port";
import { CloudflareSandbox } from "./sandbox";
import {
  RPC_METHODS,
  type BackupRequest,
  type BackupResponse,
  type ErrorCode,
  type ExecRequest,
  type ExecResponse,
  type PreviewRequest,
  type PreviewResponse,
  type Result,
  type RestoreBackupRequest,
  type RestoreBackupResponse,
  type RestoreRequest,
  type RestoreResponse,
  type RpcMethod,
  type SnapshotRequest,
  type SnapshotResponse,
  type StartRequest,
  type StartResponse,
  type StatusRequest,
  type StatusResponse,
  type StopRequest,
  type StopResponse,
} from "./types";

// The package's R2 gateway must be exported from the Worker entry so `ctx.exports` can find it.
export { DirectoryBackupGateway } from "@cloudflare/sandbox";
export * from "./types";

export interface Env {
  WORKSPACE: DurableObjectNamespace<WorkspaceDO>;
  BACKUPS: R2Bucket;
  BACKUP_PREFIX?: string;
  /** Dev var: "1" selects FakeSandbox instead of the real container. */
  DEV_FAKE_SANDBOX?: string;
}

const STATUS: Record<ErrorCode, number> = {
  invalid_argument: 400,
  not_found: 404,
  invalid_state: 409,
  sandbox_error: 502,
  internal: 500,
};

function json(body: unknown, status: number): Response {
  return Response.json(body, { status });
}

function fail(code: ErrorCode, message: string): Response {
  return json({ ok: false, error: { code, message } }, STATUS[code]);
}

export class WorkspaceDO extends DurableObject<Env> {
  /** Exposed for tests (runInDurableObject) to inspect the FakeSandbox. */
  readonly port: SandboxPort;
  readonly core: WorkspaceCore;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.port = this.makePort();
    this.core = new WorkspaceCore(this.port, {
      get: <T>(k: string) => ctx.storage.get<T>(k),
      put: <T>(k: string, v: T) => ctx.storage.put(k, v),
    });
  }

  private makePort(): SandboxPort {
    if (this.env.DEV_FAKE_SANDBOX === "1") return new FakeSandbox();
    const container = this.ctx.container;
    if (!container) throw new Error("no container configured for WorkspaceDO (wrangler `containers`)");
    // ctx.exports.DirectoryBackupGateway is the loopback service of the class re-exported above;
    // its generated type is not in the global Exports interface, so narrow it here.
    const gateway = (this.ctx.exports as unknown as { DirectoryBackupGateway: DirectoryBackupGatewayBinding })
      .DirectoryBackupGateway;
    return new CloudflareSandbox({
      container,
      gateway,
      backupBinding: "BACKUPS",
      backupPrefix: this.env.BACKUP_PREFIX ?? "backups/",
    });
  }

  // ---- JS RPC surface ----------------------------------------------------------------------
  // Inputs are validated at runtime (they come from another Worker), hence the core takes unknown.

  start(req: StartRequest): Promise<Result<StartResponse>> {
    return this.core.start(req);
  }
  stop(req?: StopRequest): Promise<Result<StopResponse>> {
    return this.core.stop(req);
  }
  status(req?: StatusRequest): Promise<Result<StatusResponse>> {
    return this.core.status(req);
  }
  snapshot(req?: SnapshotRequest): Promise<Result<SnapshotResponse>> {
    return this.core.snapshot(req);
  }
  restore(req: RestoreRequest): Promise<Result<RestoreResponse>> {
    return this.core.restore(req);
  }
  backup(req: BackupRequest): Promise<Result<BackupResponse>> {
    return this.core.backup(req);
  }
  restoreBackup(req: RestoreBackupRequest): Promise<Result<RestoreBackupResponse>> {
    return this.core.restoreBackup(req);
  }
  exec(req: ExecRequest): Promise<Result<ExecResponse>> {
    return this.core.exec(req);
  }
  preview(req: PreviewRequest): Promise<Result<PreviewResponse>> {
    return this.core.preview(req);
  }

  // ---- HTTP surface ------------------------------------------------------------------------

  override async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    const rpc = /^\/rpc\/([A-Za-z]+)$/.exec(url.pathname);
    if (rpc) {
      const method = rpc[1] as string;
      if (!(RPC_METHODS as readonly string[]).includes(method)) {
        return fail("not_found", `unknown method ${method}`);
      }
      if (request.method !== "POST") return fail("invalid_argument", "use POST");
      const text = await request.text();
      let body: unknown = {};
      if (text.trim() !== "") {
        try {
          body = JSON.parse(text);
        } catch {
          return fail("invalid_argument", "body is not valid JSON");
        }
      }
      const result = await this.dispatch(method as RpcMethod, body);
      return json(result, result.ok ? 200 : STATUS[result.error.code]);
    }

    const prev = /^\/preview\/(\d+)(\/.*)?$/.exec(url.pathname);
    if (prev) {
      const port = Number(prev[1]);
      if (port < 1 || port > 65535) return fail("invalid_argument", "bad port");
      const target = new URL(request.url);
      target.pathname = prev[2] ?? "/";
      return this.core.proxyPreview(port, new Request(target, request));
    }

    return fail("not_found", "no such route");
  }

  private dispatch(method: RpcMethod, body: unknown): Promise<Result<unknown>> {
    switch (method) {
      case "start":
        return this.core.start(body);
      case "stop":
        return this.core.stop(body);
      case "status":
        return this.core.status(body);
      case "snapshot":
        return this.core.snapshot(body);
      case "restore":
        return this.core.restore(body);
      case "backup":
        return this.core.backup(body);
      case "restoreBackup":
        return this.core.restoreBackup(body);
      case "exec":
        return this.core.exec(body);
      case "preview":
        return this.core.preview(body);
    }
  }
}

// Worker entry, for `wrangler dev` and smoke tests: POST /w/<workspaceId>/rpc/<method> reaches
// that workspace's DO. The Rust hub binds the DO namespace directly and does not use this.
export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const m = /^\/w\/([A-Za-z0-9_.-]{1,128})(\/.*)$/.exec(url.pathname);
    if (!m) return fail("not_found", "use /w/<workspaceId>/rpc/<method>");
    const stub = env.WORKSPACE.getByName(m[1] as string);
    url.pathname = m[2] as string;
    return stub.fetch(new Request(url, request));
  },
} satisfies ExportedHandler<Env>;
