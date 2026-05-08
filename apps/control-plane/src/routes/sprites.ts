import { Hono } from 'hono';
import type { Orchestrator } from '../orchestrator.js';
import type { Storage } from '../db/types.js';
import type { IdleScheduler } from '../idle-scheduler.js';

export interface SpritesRoutesDeps {
  orchestrator: Orchestrator;
  storage: Storage;
  idleScheduler: IdleScheduler;
}

export function buildSpritesRoutes(deps: SpritesRoutesDeps): Hono {
  const app = new Hono();
  const { orchestrator, storage, idleScheduler } = deps;

  app.get('/sprites', async (c) => {
    const sprites = await storage.listActiveSprites();
    return c.json({ sprites });
  });

  app.get('/sprites/:id', async (c) => {
    const sprite = await storage.getSprite(c.req.param('id'));
    if (!sprite) return c.json({ error: 'not found' }, 404);
    return c.json(sprite);
  });

  app.post('/sprites/:id/suspend', async (c) => {
    try {
      const sprite = await orchestrator.suspendSprite(c.req.param('id'));
      return c.json(sprite);
    } catch (err) {
      return c.json({ error: (err as Error).message }, 409);
    }
  });

  app.post('/sprites/:id/resume', async (c) => {
    try {
      const sprite = await orchestrator.resumeSprite(c.req.param('id'));
      return c.json(sprite);
    } catch (err) {
      return c.json({ error: (err as Error).message }, 409);
    }
  });

  app.post('/admin/idle/tick', async (c) => {
    const result = await idleScheduler.tick();
    return c.json(result);
  });

  app.get('/stats', async (c) => {
    const since = c.req.query('since');
    const stats = await storage.getStats(since);
    return c.json(stats);
  });

  return app;
}
