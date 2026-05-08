import { Hono } from 'hono';
import { z } from 'zod';
import type { Storage } from '../db/types.js';

const RegisterPushRequest = z.object({
  token: z.string().min(1),
  label: z.string().optional(),
});

export interface PushRoutesDeps {
  storage: Storage;
}

export function buildPushRoutes(deps: PushRoutesDeps): Hono {
  const app = new Hono();
  const { storage } = deps;

  app.post('/push/tokens', async (c) => {
    const parsed = RegisterPushRequest.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    const row = await storage.registerPushToken(parsed.data.token, parsed.data.label);
    return c.json(row, 201);
  });

  app.get('/push/tokens', async (c) => {
    const tokens = await storage.listPushTokens();
    return c.json({ tokens });
  });

  app.delete('/push/tokens/:token', async (c) => {
    await storage.removePushToken(c.req.param('token'));
    return c.json({ deleted: true });
  });

  return app;
}
