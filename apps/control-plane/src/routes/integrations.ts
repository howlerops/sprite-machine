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

const GITHUB_KEY = 'github';
const AI_AUTH_KEY = 'ai-auth';

type StoredGithub = {
  username: string;
  gitUserName: string;
  gitUserEmail: string;
  tokenEnc?: string;
};

type StoredAiAuth = {
  codexAuthEnc?: string;
  piAuthEnc?: string;
  claudeCredentialsEnc?: string;
  updatedAt?: string;
};

const AiAuthBody = z.object({
  codexAuthJson: z.string().min(2).optional(),
  piAuthJson: z.string().min(2).optional(),
  claudeCredentialsJson: z.string().min(2).optional(),
});

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

function publicAiAuth(row: StoredAiAuth | null) {
  return {
    codexConnected: !!row?.codexAuthEnc,
    piConnected: !!row?.piAuthEnc,
    claudeConnected: !!row?.claudeCredentialsEnc,
    updatedAt: row?.updatedAt ?? null,
  };
}

function parseJsonField(name: string, value: string | undefined): void {
  if (!value) return;
  try {
    JSON.parse(value);
  } catch {
    throw new Error(`${name} must be valid JSON`);
  }
}

export function buildIntegrationsRoutes(deps: IntegrationsRoutesDeps): Hono {
  const app = new Hono();

  app.get('/integrations/github', async (c) => {
    const row = await deps.storage.getIntegration(GITHUB_KEY);
    return c.json(publicGithub((row?.value as StoredGithub | undefined) ?? null));
  });

  app.put('/integrations/github', async (c) => {
    const parsed = GithubBody.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);

    const existing = (await deps.storage.getIntegration(GITHUB_KEY))?.value as
      | StoredGithub
      | undefined;
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
    await deps.storage.setIntegration(GITHUB_KEY, stored);

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
    const row = (await deps.storage.getIntegration(GITHUB_KEY))?.value as StoredGithub | undefined;
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
    await deps.storage.removeIntegration(GITHUB_KEY);
    return c.json({ connected: false });
  });

  app.get('/integrations/ai-auth', async (c) => {
    const row = await deps.storage.getIntegration(AI_AUTH_KEY);
    return c.json(publicAiAuth((row?.value as StoredAiAuth | undefined) ?? null));
  });

  app.put('/integrations/ai-auth', async (c) => {
    const parsed = AiAuthBody.safeParse(await c.req.json().catch(() => null));
    if (!parsed.success) return c.json({ error: parsed.error.format() }, 400);
    try {
      const existing = (await deps.storage.getIntegration(AI_AUTH_KEY))?.value as
        | StoredAiAuth
        | undefined;
      parseJsonField('Codex auth JSON', parsed.data.codexAuthJson);
      parseJsonField('Pi auth JSON', parsed.data.piAuthJson);
      parseJsonField('Claude credentials JSON', parsed.data.claudeCredentialsJson);

      const codexAuthEnc = parsed.data.codexAuthJson
        ? encrypt(deps.sessionSecret, parsed.data.codexAuthJson)
        : existing?.codexAuthEnc;
      const piAuthEnc = parsed.data.piAuthJson
        ? encrypt(deps.sessionSecret, parsed.data.piAuthJson)
        : existing?.piAuthEnc;
      const claudeCredentialsEnc = parsed.data.claudeCredentialsJson
        ? encrypt(deps.sessionSecret, parsed.data.claudeCredentialsJson)
        : existing?.claudeCredentialsEnc;
      const stored: StoredAiAuth = {
        ...(codexAuthEnc ? { codexAuthEnc } : {}),
        ...(piAuthEnc ? { piAuthEnc } : {}),
        ...(claudeCredentialsEnc ? { claudeCredentialsEnc } : {}),
        updatedAt: new Date().toISOString(),
      };
      if (!stored.codexAuthEnc && !stored.piAuthEnc && !stored.claudeCredentialsEnc) {
        return c.json({ error: 'at least one auth JSON is required' }, 400);
      }
      await deps.storage.setIntegration(AI_AUTH_KEY, stored);
      const apply = await deps.orchestrator.applyAiAuthIntegration({
        ...(stored.codexAuthEnc
          ? { codexAuthJson: decrypt(deps.sessionSecret, stored.codexAuthEnc) }
          : {}),
        ...(stored.piAuthEnc ? { piAuthJson: decrypt(deps.sessionSecret, stored.piAuthEnc) } : {}),
        ...(stored.claudeCredentialsEnc
          ? { claudeCredentialsJson: decrypt(deps.sessionSecret, stored.claudeCredentialsEnc) }
          : {}),
      });
      return c.json({ ...publicAiAuth(stored), apply });
    } catch (err) {
      return c.json({ error: (err as Error).message }, 400);
    }
  });

  app.post('/integrations/ai-auth/apply', async (c) => {
    const row = (await deps.storage.getIntegration(AI_AUTH_KEY))?.value as StoredAiAuth | undefined;
    if (!row?.codexAuthEnc && !row?.piAuthEnc && !row?.claudeCredentialsEnc) {
      return c.json({ error: 'not_connected' }, 404);
    }
    const apply = await deps.orchestrator.applyAiAuthIntegration({
      ...(row.codexAuthEnc ? { codexAuthJson: decrypt(deps.sessionSecret, row.codexAuthEnc) } : {}),
      ...(row.piAuthEnc ? { piAuthJson: decrypt(deps.sessionSecret, row.piAuthEnc) } : {}),
      ...(row.claudeCredentialsEnc
        ? { claudeCredentialsJson: decrypt(deps.sessionSecret, row.claudeCredentialsEnc) }
        : {}),
    });
    return c.json({ ...publicAiAuth(row), apply });
  });

  app.delete('/integrations/ai-auth', async (c) => {
    await deps.storage.removeIntegration(AI_AUTH_KEY);
    return c.json({ codexConnected: false, piConnected: false, claudeConnected: false });
  });

  return app;
}
