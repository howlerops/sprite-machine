import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { serve } from '@hono/node-server';
import { serveStatic } from '@hono/node-server/serve-static';
import { Hono } from 'hono';
import { cors } from 'hono/cors';
import { createSpritesClient, type SpritesClient } from '@sprite-machine/sprites-client';
import { loadConfig } from './config.js';
import { bearerOrUnauthorized } from './auth.js';
import { SqliteStorage } from './db/sqlite.js';
import { Orchestrator } from './orchestrator.js';
import { PushDispatcher } from './push.js';
import { SseBus } from './sse-bus.js';
import { buildRunsRoutes } from './routes/runs.js';
import { buildWebhooksRoutes } from './routes/webhooks.js';
import { buildPushRoutes } from './routes/push.js';
import { buildSpritesRoutes } from './routes/sprites.js';
import { IdleScheduler } from './idle-scheduler.js';

async function main(): Promise<void> {
  const cfg = loadConfig();

  // Resolve template checkpoint id (env wins, else .sprite-machine-data/template.json)
  let templateCheckpointId = cfg.templateCheckpointId;
  if (!templateCheckpointId) {
    const tplFile = path.join(cfg.dataDir, 'template.json');
    if (existsSync(tplFile)) {
      try {
        const parsed = JSON.parse(readFileSync(tplFile, 'utf8')) as {
          templateCheckpointId: string;
        };
        templateCheckpointId = parsed.templateCheckpointId;
      } catch (err) {
        console.warn('[control-plane] failed to parse template.json:', err);
      }
    }
  }

  // Inject webhook URL/token into our process env so spawned sidecars inherit
  process.env.CONTROL_PLANE_WEBHOOK_URL = `${cfg.publicBaseUrl}/webhooks/sprite`;
  process.env.CONTROL_PLANE_WEBHOOK_TOKEN = cfg.webhookToken;

  const storage = new SqliteStorage(cfg.dbPath);

  const flyAgentTokens = new Map<string, string>();
  if (cfg.workerAgentToken) flyAgentTokens.set(cfg.workerName, cfg.workerAgentToken);

  const sprites: SpritesClient = createSpritesClient({
    backend: cfg.spritesBackend,
    workspaceRoot: cfg.workspaceRoot,
    ...(cfg.spritesBackend === 'fly' && cfg.spritesApiToken
      ? {
          fly: {
            apiToken: cfg.spritesApiToken,
            workerName: cfg.workerName,
            ...(flyAgentTokens.size > 0 ? { agentTokens: flyAgentTokens } : {}),
          },
        }
      : {}),
  });

  const sseBus = new SseBus();
  const push = new PushDispatcher({
    storage,
    ...(cfg.expoAccessToken !== undefined ? { expoAccessToken: cfg.expoAccessToken } : {}),
  });

  const orchestrator = new Orchestrator({
    storage,
    sprites,
    templateCheckpointId,
    sseBus,
    push,
  });

  const idleScheduler = new IdleScheduler({
    storage,
    sprites,
    idleSuspendAfterMs: cfg.idleSuspendAfterMs,
    idleCheckIntervalMs: cfg.idleCheckIntervalMs,
  });
  idleScheduler.start();

  const app = new Hono();
  app.use('*', cors({ origin: '*', allowHeaders: ['authorization', 'content-type'] }));

  app.get('/healthz', (c) =>
    c.json({
      ok: true,
      role: 'control-plane',
      backend: cfg.spritesBackend,
      template: templateCheckpointId,
    }),
  );

  // Webhook is authed independently with the webhook token
  app.route('/', buildWebhooksRoutes({ orchestrator, webhookToken: cfg.webhookToken }));

  // Static dashboard: the Expo Router web build, served at /app/*. Public
  // (no bearer required for the static assets — users enter their token on
  // the settings screen, which lives in the bundle and stores it in the
  // browser's localStorage; the actual API calls then carry the bearer).
  // Must be mounted BEFORE userApp so its '*' middleware doesn't claim the path.
  const webDir =
    process.env.STATIC_WEB_DIR ?? path.join(cfg.workspaceRoot, 'apps', 'mobile', 'dist-web');
  if (existsSync(webDir)) {
    // Page routes under /app/* (rewritten to filesystem-relative paths). Try the
    // exact path first, then with .html, then fall back to index.html (so dynamic
    // routes like /app/run/<id> get the SPA shell and client-side routing handles
    // the segment).
    app.use(
      '/app/*',
      serveStatic({
        root: webDir,
        rewriteRequestPath: (p) => {
          const stripped = p.replace(/^\/app/, '') || '/';
          // serveStatic itself doesn't try .html suffix; pre-resolve here.
          if (stripped === '/' || stripped === '') return '/index.html';
          // If the path looks like a file (has extension), use as-is.
          if (/\.[a-zA-Z0-9]+$/.test(stripped)) return stripped;
          // Probe filesystem for `<path>.html`, otherwise return SPA shell.
          const htmlPath = path.join(webDir, stripped + '.html');
          if (existsSync(htmlPath)) return stripped + '.html';
          return '/index.html';
        },
      }),
    );
    app.get('/app', (c) => c.redirect('/app/'));
    // Expo emits absolute asset URLs (`/_expo/...`, `/assets/...`, `/favicon.ico`).
    // Serve those at the root, public, so the dashboard JS bundle can load.
    app.use('/_expo/*', serveStatic({ root: webDir }));
    app.use('/assets/*', serveStatic({ root: webDir }));
    app.get('/favicon.ico', serveStatic({ root: webDir, path: '/favicon.ico' }));
    console.log(`[control-plane] dashboard served from ${webDir} at /app/ (+ asset roots)`);
  } else {
    console.log(
      `[control-plane] no web dashboard at ${webDir} (run \`pnpm --filter @sprite-machine/mobile export:web\`)`,
    );
  }

  // Everything else is user-authed
  const userApp = new Hono();
  userApp.use('*', bearerOrUnauthorized(cfg.userToken));
  userApp.route('/', buildRunsRoutes({ orchestrator, storage, sseBus }));
  userApp.route('/', buildPushRoutes({ storage }));
  userApp.route('/', buildSpritesRoutes({ orchestrator, storage, idleScheduler }));
  app.route('/', userApp);

  serve({ fetch: app.fetch, port: cfg.port, hostname: cfg.host }, (info) => {
    console.log(
      `[control-plane] backend=${cfg.spritesBackend} template=${templateCheckpointId ?? 'none'} listening on http://${info.address}:${info.port}`,
    );
    console.log(`[control-plane] webhook url = ${process.env.CONTROL_PLANE_WEBHOOK_URL}`);
  });
}

main().catch((err) => {
  console.error('[control-plane] fatal:', err);
  process.exit(1);
});
