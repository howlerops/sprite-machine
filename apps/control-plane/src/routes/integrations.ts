import { createCipheriv, createDecipheriv, createHash, randomBytes } from 'node:crypto';
import { Hono } from 'hono';
import { z } from 'zod';
import type { Orchestrator } from '../orchestrator.js';
import type { Storage } from '../db/types.js';

export interface IntegrationsRoutesDeps {
  storage: Storage;
  orchestrator: Orchestrator;
  sessionSecret: string;
}

const GithubBody = z.object({
  token: z.string().min(1).optional(),
  username: z.string().min(1),
  gitUserName: z.string().min(1),
  gitUserEmail: z.string().email(),
});

const KEY = 'github';

type StoredGithub = {
  username: string;
  gitUserName: string;
  gitUserEmail: string;
  tokenEnc?: string;
};

function key(secret: string): Buffer {
  return createHash('sha256').update(`sprite-machine-integrations:${secret}`).digest();
}

function encrypt(secret: string, plain: string): string {
  const iv = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', key(secret), iv);
  const data = Buffer.concat([cipher.update(plain, 'utf8'), cipher.final()]);
  const tag = cipher.getAuthTag();
  return Buffer.concat([iv, tag, data]).toString('base64');
}

function decrypt(secret: string, enc: string): string {
  const raw = Buffer.from(enc, 'base64');
  const iv = raw.subarray(0, 12);
  const tag = raw.subarray(12, 28);
  const data = raw.subarray(28);
  const decipher = createDecipheriv('aes-256-gcm', key(secret), iv);
  decipher.setAuthTag(tag);
  return Buffer.concat([decipher.update(data), decipher.final()]).toString('utf8');
}

function publicGithub(row: StoredGithub | null) {
  return {
    connected: !!row?.tokenEnc,
    username: row?.username ?? null,
    gitUserName: row?.gitUserName ?? null,
    gitUserEmail: row?.gitUserEmail ?? null,
  };
}

export function buildIntegrationsRoutes(deps: IntegrationsRoutesDeps): Hono {
  const app = new Hono();

  app.get('/integrations/github', async (c) => {
    const row = await deps.storage.getIntegration(KEY);
    return c.json(publicGithub((row?.value as StoredGithub | undefined) ?? null));
  });

  app.put('/integrations/github', async (c) => {
    const parsed = GithubBody.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);

    const existing = (await deps.storage.getIntegration(KEY))?.value as StoredGithub | undefined;
    const tokenEnc = parsed.data.token
      ? encrypt(deps.sessionSecret, parsed.data.token)
      : existing?.tokenEnc;
    if (!tokenEnc) return c.json({ error: 'token required' }, 400);

    const stored: StoredGithub = {
      username: parsed.data.username,
      gitUserName: parsed.data.gitUserName,
      gitUserEmail: parsed.data.gitUserEmail,
      tokenEnc,
    };
    await deps.storage.setIntegration(KEY, stored);

    const token = decrypt(deps.sessionSecret, tokenEnc);
    const apply = await deps.orchestrator.applyGitHubIntegration({
      token,
      username: stored.username,
      gitUserName: stored.gitUserName,
      gitUserEmail: stored.gitUserEmail,
    });
    return c.json({ ...publicGithub(stored), apply });
  });

  app.post('/integrations/github/apply', async (c) => {
    const row = (await deps.storage.getIntegration(KEY))?.value as StoredGithub | undefined;
    if (!row?.tokenEnc) return c.json({ error: 'not_connected' }, 404);
    const apply = await deps.orchestrator.applyGitHubIntegration({
      token: decrypt(deps.sessionSecret, row.tokenEnc),
      username: row.username,
      gitUserName: row.gitUserName,
      gitUserEmail: row.gitUserEmail,
    });
    return c.json({ ...publicGithub(row), apply });
  });

  app.delete('/integrations/github', async (c) => {
    await deps.storage.removeIntegration(KEY);
    return c.json({ connected: false });
  });

  return app;
}
