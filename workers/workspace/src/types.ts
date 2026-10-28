// Wire types for the Workspace DO (plan §4.3). Every method takes one JSON-serialisable request
// object and returns a `Result`. The same shapes travel over JS RPC and over `POST /rpc/<method>`.

export const INSTANCE_TYPES = [
  "lite",
  "basic",
  "standard-1",
  "standard-2",
  "standard-3",
  "standard-4",
] as const;
export type InstanceType = (typeof INSTANCE_TYPES)[number];

export const RPC_METHODS = [
  "start",
  "stop",
  "status",
  "snapshot",
  "restore",
  "backup",
  "restoreBackup",
  "exec",
  "preview",
] as const;
export type RpcMethod = (typeof RPC_METHODS)[number];

export type ErrorCode =
  | "invalid_argument" // the request failed validation
  | "invalid_state" // e.g. exec while stopped, start while running
  | "not_found" // unknown snapshot / backup / method
  | "sandbox_error" // the SDK / container failed
  | "internal";

export interface RpcError {
  code: ErrorCode;
  message: string;
}

export type Result<T> = { ok: true; value: T } | { ok: false; error: RpcError };

export type WorkspaceState = "stopped" | "running";

/** Opaque, serialisable record of a DirectoryBackup (the SDK's DirectoryBackupRecord). */
export interface BackupHandle {
  id: string;
  dir: string;
  size: number;
  sha256: string;
  format: string;
  name?: string;
}

export interface StartRequest {
  /** Pinned workspace image reference. */
  image: string;
  instance?: InstanceType;
}
export interface StartResponse {
  state: "running";
  image: string;
  instance: InstanceType;
}

export type StopRequest = Record<string, never>;
export interface StopResponse {
  state: "stopped";
  /** False when the workspace was already stopped. */
  wasRunning: boolean;
}

export type StatusRequest = Record<string, never>;
export interface StatusResponse {
  state: WorkspaceState;
  image: string | null;
  instance: InstanceType | null;
  startedAt: number | null;
  lastSnapshotId: string | null;
}

export interface SnapshotRequest {
  name?: string;
}
export interface SnapshotResponse {
  id: string;
  size: number;
  name?: string;
}

export interface RestoreRequest {
  /** Snapshot id from `snapshot()`. Restarts the container from it. */
  id: string;
}
export interface RestoreResponse {
  state: "running";
  restoredFrom: string;
}

export interface BackupRequest {
  /** Absolute directory inside the sandbox, e.g. /workspace. */
  dir: string;
  name?: string;
}
export interface BackupResponse {
  handle: BackupHandle;
}

export interface RestoreBackupRequest {
  handle: BackupHandle;
  /** Restore into this directory instead of handle.dir. */
  dir?: string;
}
export interface RestoreBackupResponse {
  restored: true;
  dir: string;
}

export interface ExecRequest {
  cmd: string[];
  env?: Record<string, string>;
  cwd?: string;
}
export interface ExecResponse {
  exitCode: number;
  stdout: string;
  stderr: string;
}

export interface PreviewRequest {
  port: number;
}
export interface PreviewResponse {
  port: number;
  /** Path on this DO's fetch handler that proxies to the sandbox port. The hub signs/rewrites it. */
  path: string;
}

/** Request/response map, used to type the RPC surface and the HTTP dispatcher. */
export interface RpcMap {
  start: [StartRequest, StartResponse];
  stop: [StopRequest, StopResponse];
  status: [StatusRequest, StatusResponse];
  snapshot: [SnapshotRequest, SnapshotResponse];
  restore: [RestoreRequest, RestoreResponse];
  backup: [BackupRequest, BackupResponse];
  restoreBackup: [RestoreBackupRequest, RestoreBackupResponse];
  exec: [ExecRequest, ExecResponse];
  preview: [PreviewRequest, PreviewResponse];
}
