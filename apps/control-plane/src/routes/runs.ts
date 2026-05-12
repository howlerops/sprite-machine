import { Hono } from 'hono';
import { streamSSE } from 'hono/streaming';
import {
  CreateRunRequest,
  ResolvePermissionRequest,
  SendMessageRequest,
} from '@sprite-machine/shared';
import type { Orchestrator } from '../orchestrator.js';
import type { Storage } from '../db/types.js';
import type { SseBus } from '../sse-bus.js';

export interface RunsRoutesDeps {
  orchestrator: Orchestrator;
  storage: Storage;
  sseBus: SseBus;
}

export function buildRunsRoutes(deps: RunsRoutesDeps): Hono {
  const app = new Hono();
  const { orchestrator, storage, sseBus } = deps;

  app.post('/runs', async (c) => {
    const parsed = CreateRunRequest.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    try {
      const data = parsed.data;
      const run = await orchestrator.createRun({
        prompt: data.prompt,
        ...(data.harness !== undefined ? { harness: data.harness } : {}),
        ...(data.workdir !== undefined ? { workdir: data.workdir } : {}),
        ...(data.workdirs !== undefined ? { workdirs: data.workdirs } : {}),
        ...(data.model !== undefined ? { model: data.model } : {}),
        ...(data.permissionMode !== undefined ? { permissionMode: data.permissionMode } : {}),
        ...(data.allowedTools !== undefined ? { allowedTools: data.allowedTools } : {}),
        ...(data.metadata !== undefined ? { metadata: data.metadata } : {}),
      });
      return c.json(run, 201);
    } catch (err) {
      return c.json({ error: (err as Error).message }, 500);
    }
  });

  app.get('/runs', async (c) => {
    const runs = await storage.listRuns();
    return c.json({ runs });
  });

  app.get('/runs/:id', async (c) => {
    const existing = await storage.getRun(c.req.param('id'));
    if (!existing) return c.json({ error: 'not found' }, 404);
    // Opening an old run should wake its sprite and reconcile stale control-plane
    // status with the sidecar's durable run store. This recovers runs that were
    // marked running/starting before the sidecar or sprite was suspended.
    const run = await orchestrator.refreshRunFromSidecar(existing.id).catch((err) => {
      console.warn(`[runs] failed to refresh ${existing.id}:`, err);
      return existing;
    });
    const events = await storage.listRunEvents(run.id);
    return c.json({ ...run, events });
  });

  app.post('/runs/:id/messages', async (c) => {
    const parsed = SendMessageRequest.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    try {
      await orchestrator.sendMessage(c.req.param('id'), parsed.data.text);
      return c.json({ accepted: true }, 202);
    } catch (err) {
      return c.json({ error: (err as Error).message }, 500);
    }
  });

  app.delete('/runs/:id', async (c) => {
    const id = c.req.param('id');
    const run = await storage.getRun(id);
    if (!run) return c.json({ error: 'not found' }, 404);
    try {
      if (run.status === 'starting' || run.status === 'running') {
        await orchestrator.cancelRun(id).catch(() => undefined);
      }
      await storage.deleteRun(id);
      return c.json({ deleted: true });
    } catch (err) {
      return c.json({ error: (err as Error).message }, 500);
    }
  });

  app.post('/runs/:id/cancel', async (c) => {
    try {
      await orchestrator.cancelRun(c.req.param('id'));
      return c.json({ cancelled: true });
    } catch (err) {
      return c.json({ error: (err as Error).message }, 500);
    }
  });

  app.post('/runs/:id/permissions/:requestId', async (c) => {
    const parsed = ResolvePermissionRequest.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    try {
      await orchestrator.resolvePermission(
        c.req.param('id'),
        c.req.param('requestId'),
        parsed.data.decision,
        parsed.data.reason,
      );
      return c.json({ resolved: true });
    } catch (err) {
      return c.json({ error: (err as Error).message }, 500);
    }
  });

  app.get('/runs/:id/events', async (c) => {
    const id = c.req.param('id');
    const run = await storage.getRun(id);
    if (!run) return c.json({ error: 'not found' }, 404);
    const after = Number(c.req.query('after') ?? -1);
    return streamSSE(c, async (stream) => {
      const past = await storage.listRunEvents(id, after);
      for (const e of past) {
        await stream.writeSSE({ id: String(e.seq), data: JSON.stringify(e.event) });
      }
      let resolveDone: (() => void) | null = null;
      const done = new Promise<void>((resolve) => {
        resolveDone = resolve;
      });
      const unsubscribe = sseBus.subscribe(id, (env) => {
        stream
          .writeSSE({ id: String(env.seq), data: JSON.stringify(env.event) })
          .catch(() => resolveDone?.());
        if (env.event.type === 'completed' || env.event.type === 'error') {
          // keep stream open; client decides when to disconnect
        }
      });
      stream.onAbort(() => resolveDone?.());
      try {
        await done;
      } finally {
        unsubscribe();
      }
    });
  });

  return app;
}
