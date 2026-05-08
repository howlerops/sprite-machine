import path from 'node:path';
import { LocalSpritesClient, type LocalSpritesClientOptions } from './local.js';
import { FlySpritesClient, type FlySpritesClientOptions } from './fly.js';
import type { SpritesClient } from './types.js';

export type SpritesBackend = 'local' | 'fly';

export interface CreateClientOptions {
  backend?: SpritesBackend;
  local?: Partial<LocalSpritesClientOptions>;
  fly?: Partial<FlySpritesClientOptions>;
  workspaceRoot: string;
}

export function createSpritesClient(opts: CreateClientOptions): SpritesClient {
  const backend = opts.backend ?? (process.env.SPRITES_BACKEND as SpritesBackend) ?? 'local';
  if (backend === 'fly') {
    const apiToken = opts.fly?.apiToken ?? process.env.SPRITES_API_TOKEN;
    if (!apiToken) {
      throw new Error('SPRITES_API_TOKEN required for fly backend');
    }
    return new FlySpritesClient({
      apiToken,
      ...(opts.fly?.apiBaseUrl !== undefined ? { apiBaseUrl: opts.fly.apiBaseUrl } : {}),
      ...(opts.fly?.workerName !== undefined ? { workerName: opts.fly.workerName } : {}),
      ...(opts.fly?.agentTokens !== undefined ? { agentTokens: opts.fly.agentTokens } : {}),
    });
  }
  return new LocalSpritesClient({
    rootDir:
      opts.local?.rootDir ??
      process.env.SPRITE_MACHINE_DATA ??
      path.join(opts.workspaceRoot, '.sprite-machine-data'),
    workspaceRoot: opts.local?.workspaceRoot ?? opts.workspaceRoot,
    ...(opts.local?.spawnSidecar !== undefined ? { spawnSidecar: opts.local.spawnSidecar } : {}),
    ...(opts.local?.sidecarMode !== undefined ? { sidecarMode: opts.local.sidecarMode } : {}),
  });
}

export type {
  SpriteHandle,
  CheckpointHandle,
  CreateOpts,
  CloneOpts,
  ExecOpts,
  ExecResult,
  SpritesClient,
  SpriteStatus,
} from './types.js';
export { LocalSpritesClient } from './local.js';
export { FlySpritesClient } from './fly.js';
