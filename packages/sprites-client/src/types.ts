export type SpriteStatus = 'creating' | 'running' | 'suspended' | 'destroyed';

export interface SpriteHandle {
  id: string;
  status: SpriteStatus;
  agentBaseUrl: string | null;
  agentToken: string;
  fromCheckpointId: string | null;
  createdAt: string;
  updatedAt: string;
  meta: Record<string, unknown>;
}

export interface CheckpointHandle {
  id: string;
  spriteId: string;
  label: string | null;
  createdAt: string;
}

export interface CreateOpts {
  name?: string;
  fromCheckpointId?: string;
  meta?: Record<string, unknown>;
}

export interface CloneOpts {
  name?: string;
  meta?: Record<string, unknown>;
}

export interface ExecOpts {
  cwd?: string;
  env?: Record<string, string>;
  timeoutMs?: number;
  stdin?: string | Buffer;
}

export interface ExecResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

export interface SpritesClient {
  create(opts?: CreateOpts): Promise<SpriteHandle>;
  get(id: string): Promise<SpriteHandle | null>;
  list(): Promise<SpriteHandle[]>;
  destroy(id: string): Promise<void>;
  suspend(id: string): Promise<SpriteHandle>;
  resume(id: string): Promise<SpriteHandle>;
  checkpoint(id: string, label?: string): Promise<CheckpointHandle>;
  cloneFromCheckpoint(checkpointId: string, opts?: CloneOpts): Promise<SpriteHandle>;
  exec(id: string, cmd: string[], opts?: ExecOpts): Promise<ExecResult>;
  writeFile(id: string, remotePath: string, content: string | Buffer): Promise<void>;
  readFile(id: string, remotePath: string): Promise<Buffer>;
  uploadDir(id: string, localDir: string, remoteDir: string): Promise<void>;
}
