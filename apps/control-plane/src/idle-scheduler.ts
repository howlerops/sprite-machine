import { nowIso } from '@sprite-machine/shared';
import type { SpritesClient } from '@sprite-machine/sprites-client';
import type { Storage } from './db/types.js';

export interface IdleSchedulerOpts {
  storage: Storage;
  sprites: SpritesClient;
  idleSuspendAfterMs: number;
  idleCheckIntervalMs: number;
  now?: () => number;
}

export interface IdleTickResult {
  evaluated: number;
  suspended: string[];
  errors: Array<{ spriteId: string; message: string }>;
}

/**
 * Periodically suspends sprites that have been idle long enough. Runs in the
 * background (setInterval) and exposes `tick()` for deterministic tests.
 *
 * A sprite is eligible for suspension when:
 *   - status === 'running'
 *   - lastActiveAt (or updatedAt as fallback) is older than idleSuspendAfterMs
 *   - no associated run is in 'starting' or 'running' status
 */
export class IdleScheduler {
  private timer: NodeJS.Timeout | null = null;
  private readonly now: () => number;

  constructor(private readonly opts: IdleSchedulerOpts) {
    this.now = opts.now ?? Date.now;
  }

  start(): void {
    if (this.timer) return;
    this.timer = setInterval(() => {
      void this.tick().catch((err) => console.warn('[idle] tick failed:', err));
    }, this.opts.idleCheckIntervalMs);
    this.timer.unref();
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  async tick(): Promise<IdleTickResult> {
    const sprites = await this.opts.storage.listActiveSprites();
    const result: IdleTickResult = { evaluated: 0, suspended: [], errors: [] };
    const now = this.now();
    for (const sprite of sprites) {
      if (sprite.status !== 'running') continue;
      result.evaluated += 1;
      const lastActive = sprite.lastActiveAt
        ? new Date(sprite.lastActiveAt).getTime()
        : new Date(sprite.updatedAt).getTime();
      if (now - lastActive < this.opts.idleSuspendAfterMs) continue;
      const runs = await this.opts.storage.listRunsForSprite(sprite.id);
      const busy = runs.some((r) => r.status === 'starting' || r.status === 'running');
      if (busy) continue;
      try {
        await this.opts.sprites.suspend(sprite.id);
        await this.opts.storage.updateSprite(sprite.id, {
          status: 'suspended',
          agentBaseUrl: null,
          lastActiveAt: nowIso(),
        });
        result.suspended.push(sprite.id);
      } catch (err) {
        result.errors.push({ spriteId: sprite.id, message: (err as Error).message });
      }
    }
    return result;
  }
}
