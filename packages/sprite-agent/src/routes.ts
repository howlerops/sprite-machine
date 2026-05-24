import { Hono } from 'hono';
import { streamSSE } from 'hono/streaming';
import {
  CreateRunRequest,
  ResolvePermissionRequest,
  SendMessageRequest,
} from '@sprite-machine/shared';
import type { Agent } from './agent.js';
import type { RunStore } from './runs.js';

interface RouteDeps {
  store: RunStore;
  agent: Agent;
  webhookConfigured: boolean;
  webhookTokenConfigured: boolean;
}

export function buildRoutes(deps: RouteDeps): Hono {
  const app = new Hono();
  const { store, agent } = deps;

  app.get('/healthz', (c) =>
    c.json({
      ok: true,
      role: 'sprite-agent',
      webhookConfigured: deps.webhookConfigured,
      webhookTokenConfigured: deps.webhookTokenConfigured,
    }),
  );

  app.post('/runs', async (c) => {
    const parsed = CreateRunRequest.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    const run = await store.create(parsed.data);
    setImmediate(() => {
      void agent.start(run.summary.id, parsed.data).catch(async (err) => {
        console.error(`[run ${run.summary.id}] agent.start failed:`, err);
        await store.appendEvent(run.summary.id, {
          type: 'error',
          message: (err as Error).message,
          at: new Date().toISOString(),
        });
        await store.setStatus(run.summary.id, 'failed');
      });
    });
    return c.json(run.summary, 201);
  });

  app.get('/runs', (c) => c.json({ runs: store.list() }));

  app.get('/runs/:id', (c) => {
    const run = store.get(c.req.param('id'));
    if (!run) return c.json({ error: 'not found' }, 404);
    return c.json({ ...run.summary, events: run.events });
  });

  app.post('/runs/:id/messages', async (c) => {
    const parsed = SendMessageRequest.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    const id = c.req.param('id');
    const run = store.get(id);
    if (!run) return c.json({ error: 'not found' }, 404);
    try {
      void agent
        .sendUserMessage(id, parsed.data.text)
        .catch((err) => console.error(`[run ${id}] sendUserMessage failed:`, err));
      return c.json({ accepted: true }, 202);
    } catch (err) {
      return c.json({ error: (err as Error).message }, 409);
    }
  });

  app.post('/runs/:id/cancel', async (c) => {
    const id = c.req.param('id');
    if (!store.get(id)) return c.json({ error: 'not found' }, 404);
    await agent.cancel(id);
    return c.json({ cancelled: true });
  });

  app.post('/runs/:id/permissions/:requestId', async (c) => {
    const parsed = ResolvePermissionRequest.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    const id = c.req.param('id');
    const requestId = c.req.param('requestId');
    const ok = store.resolvePendingPermission(id, requestId, {
      allow: parsed.data.decision === 'allow',
      ...(parsed.data.reason !== undefined ? { reason: parsed.data.reason } : {}),
    });
    if (!ok) return c.json({ error: 'unknown permission request' }, 404);
    return c.json({ resolved: true });
  });

  app.get('/runs/:id/events', (c) => {
    const id = c.req.param('id');
    const run = store.get(id);
    if (!run) return c.json({ error: 'not found' }, 404);
    const after = Number(c.req.query('after') ?? 0);
    return streamSSE(c, async (stream) => {
      let cursor = Math.max(0, after);
      // backfill
      while (cursor < run.events.length) {
        const ev = run.events[cursor];
        if (ev) await stream.writeSSE({ id: String(cursor), data: JSON.stringify(ev) });
        cursor += 1;
      }
      // live
      let resolveDone: (() => void) | null = null;
      const done = new Promise<void>((resolve) => {
        resolveDone = resolve;
      });
      const onEvent = (runId: string): void => {
        if (runId !== id) return;
        const fresh = run.events.slice(cursor);
        for (const ev of fresh) {
          stream
            .writeSSE({ id: String(cursor), data: JSON.stringify(ev) })
            .catch(() => resolveDone?.());
          cursor += 1;
        }
        if (run.summary.status === 'completed' || run.summary.status === 'failed') {
          resolveDone?.();
        }
      };
      store.on('event', onEvent);
      stream.onAbort(() => resolveDone?.());
      try {
        await done;
      } finally {
        store.off('event', onEvent);
      }
    });
  });

  return app;
}
