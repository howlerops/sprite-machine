import { Hono } from 'hono';
import { SpriteWebhookEvent } from '@sprite-machine/shared';
import type { Orchestrator } from '../orchestrator.js';

export interface WebhooksRoutesDeps {
  orchestrator: Orchestrator;
  webhookToken: string;
}

export function buildWebhooksRoutes(deps: WebhooksRoutesDeps): Hono {
  const app = new Hono();
  const { orchestrator, webhookToken } = deps;

  app.post('/webhooks/sprite', async (c) => {
    const auth = c.req.header('authorization') ?? '';
    if (auth !== `Bearer ${webhookToken}`) {
      return c.json({ error: 'unauthorized' }, 401);
    }
    const parsed = SpriteWebhookEvent.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    try {
      const ingested = await orchestrator.ingestWebhookEvent(
        parsed.data.spriteId,
        parsed.data.runId,
        parsed.data.event,
      );
      if (!ingested) return c.json({ accepted: false, retry: true }, 409);
      return c.json({ accepted: true }, 202);
    } catch (err) {
      console.error('[webhooks/sprite] ingest failed:', err);
      return c.json({ error: (err as Error).message }, 500);
    }
  });

  return app;
}
