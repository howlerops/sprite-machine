import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { serve } from '@hono/node-server';
import { serveStatic } from '@hono/node-server/serve-static';
import { Hono } from 'hono';
import { cors } from 'hono/cors';
import { createSpritesClient, type SpritesClient } from '@sprite-machine/sprites-client';
import { loadConfig } from './config.js';
import { bearerOrCookieOrUnauthorized } from './auth.js';
import { safeEqual } from './crypto-equal.js';
import { readSessionCookie } from './session.js';
import { SqliteStorage } from './db/sqlite.js';
import { Orchestrator } from './orchestrator.js';
import { PushDispatcher } from './push.js';
import { SseBus } from './sse-bus.js';
import { buildAuthRoutes } from './routes/auth.js';
import { buildRunsRoutes } from './routes/runs.js';
import { buildAttachmentsRoutes } from './routes/attachments.js';
import { buildWebhooksRoutes } from './routes/webhooks.js';
import { buildPushRoutes } from './routes/push.js';
import { buildSpritesRoutes } from './routes/sprites.js';
import { buildFsRoutes } from './routes/fs.js';
import { buildIntegrationsRoutes } from './routes/integrations.js';
import { attachExecProxy } from './routes/exec.js';
import { IdleScheduler } from './idle-scheduler.js';

// Asset extension allowlist for the /app/* gate. Anything else without a
// session must redirect to /app/login (prevents arbitrary-suffix bypass).
const ASSET_EXT_RE = /\.(js|css|map|woff2?|ttf|otf|png|jpg|jpeg|svg|ico|webp|json|html|txt)$/i;

async function main(): Promise<void> {
  const cfg = loadConfig();

  // Resolve template metadata (env wins, else .sprite-machine-data/template.json)
  let templateCheckpointId = cfg.templateCheckpointId;
  let workerName = cfg.workerName;
  let workerAgentToken = cfg.workerAgentToken;
  if (!templateCheckpointId || !workerAgentToken) {
    const tplFile = path.join(cfg.dataDir, 'template.json');
    if (existsSync(tplFile)) {
      try {
        const parsed = JSON.parse(readFileSync(tplFile, 'utf8')) as {
          templateCheckpointId?: string | null;
          agentToken?: string;
          spriteName?: string;
        };
        if (!templateCheckpointId) templateCheckpointId = parsed.templateCheckpointId ?? null;
        if (!workerAgentToken && parsed.agentToken) workerAgentToken = parsed.agentToken;
        if (workerName === cfg.workerName && parsed.spriteName) workerName = parsed.spriteName;
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
  if (workerAgentToken) flyAgentTokens.set(workerName, workerAgentToken);

  const sprites: SpritesClient = createSpritesClient({
    backend: cfg.spritesBackend,
    workspaceRoot: cfg.workspaceRoot,
    ...(cfg.spritesBackend === 'fly' && cfg.spritesApiToken
      ? {
          fly: {
            apiToken: cfg.spritesApiToken,
            workerName,
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
  // CORS: in prod we lock to a same-origin allowlist (publicBaseUrl's origin
  // plus optional CORS_ALLOWED_ORIGINS env list); reflecting arbitrary origins
  // with credentials=true is a CSRF amplifier. In dev we keep the permissive
  // behavior so Expo web on :19006 can hit control-plane on :8787.
  let corsOriginFn: (origin: string) => string | null | undefined;
  if (cfg.cookieSecure) {
    const allowed = new Set<string>();
    try {
      const u = new URL(cfg.publicBaseUrl);
      allowed.add(`${u.protocol}//${u.host}`);
    } catch {
      // publicBaseUrl is validated as URL by zod; this is unreachable in practice.
    }
    const extra = (process.env.CORS_ALLOWED_ORIGINS ?? '')
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
    for (const o of extra) allowed.add(o);
    console.log(`[control-plane] CORS allowlist: ${Array.from(allowed).join(', ') || '(empty)'}`);
    corsOriginFn = (origin: string) => (allowed.has(origin) ? origin : null);
  } else {
    corsOriginFn = (origin: string) => origin ?? '*';
  }
  app.use(
    '*',
    cors({
      origin: corsOriginFn,
      allowHeaders: ['authorization', 'content-type'],
      credentials: true,
    }),
  );

  app.get('/healthz', (c) =>
    c.json({
      ok: true,
      role: 'control-plane',
      backend: cfg.spritesBackend,
      template: templateCheckpointId,
    }),
  );

  // Public auth routes (login, logout, me)
  app.route(
    '/',
    buildAuthRoutes({
      sessionSecret: cfg.sessionSecret,
      allowedEmails: cfg.allowedEmails,
      cookieSecure: cfg.cookieSecure,
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
    // Dashboard auth gate: HTML page requests under /app/* must have a valid
    // sm_session cookie OR an Authorization: Bearer header. Asset requests
    // (anything with a file extension) and the public /app/login screen are
    // exempt so the bundle and login can load without a cookie.
    app.use('/app/*', async (c, next) => {
      const reqPath = c.req.path;
      const stripped = reqPath.replace(/^\/app/, '') || '/';
      // Allow login page + known asset extensions unauthenticated. The
      // explicit allowlist replaces the previous `\.[a-zA-Z0-9]+$/` regex,
      // which let attackers reach the SPA shell at e.g. /app/foo.bar (any
      // extension) and bypass the gate.
      if (
        stripped === '/login' ||
        stripped === '/login/' ||
        stripped === '/login.html' ||
        ASSET_EXT_RE.test(stripped)
      ) {
        return next();
      }
      const authHeader = c.req.header('authorization') ?? '';
      const hasBearer = safeEqual(authHeader, `Bearer ${cfg.userToken}`);
      const sess = readSessionCookie(c, cfg.sessionSecret);
      if (hasBearer || sess) return next();
      return c.redirect('/app/login', 302);
    });

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
    // Root → dashboard. Without this, hitting the bare hostname 404s.
    app.get('/', (c) => c.redirect('/app/'));
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

  // Everything else is user-authed (bearer header OR signed session cookie)
  const userApp = new Hono();
  userApp.use(
    '*',
    bearerOrCookieOrUnauthorized({
      userToken: cfg.userToken,
      sessionSecret: cfg.sessionSecret,
      allowedEmails: cfg.allowedEmails,
    }),
  );
  userApp.route('/', buildRunsRoutes({ orchestrator, storage, sseBus }));
  userApp.route('/', buildAttachmentsRoutes({ storage, dataDir: cfg.dataDir }));
  userApp.route('/', buildPushRoutes({ storage }));
  userApp.route('/', buildSpritesRoutes({ orchestrator, storage, idleScheduler }));
  userApp.route(
    '/',
    buildIntegrationsRoutes({ orchestrator, storage, sessionSecret: cfg.sessionSecret }),
  );
  userApp.route('/', buildFsRoutes({ fsRoot: process.env.FS_ROOT ?? cfg.workspaceRoot }));
  app.route('/', userApp);

  // WS exec proxy — attaches to the underlying http.Server's `upgrade` event
  // BEFORE the WS handshake, so we can authenticate the request and reject
  // unauthorized clients without ever upgrading.
  const execProxy = attachExecProxy({
    storage,
    spritesApiToken: cfg.spritesApiToken ?? null,
    spritesApiBase: 'https://api.sprites.dev',
    userToken: cfg.userToken,
    sessionSecret: cfg.sessionSecret,
  });

  const server = serve({ fetch: app.fetch, port: cfg.port, hostname: cfg.host }, (info) => {
    console.log(
      `[control-plane] backend=${cfg.spritesBackend} template=${templateCheckpointId ?? 'none'} listening on http://${info.address}:${info.port}`,
    );
    console.log(`[control-plane] webhook url = ${process.env.CONTROL_PLANE_WEBHOOK_URL}`);
  });

  server.on('upgrade', (req, socket, head) => {
    void execProxy.handleUpgrade(req, socket, head).then((handled) => {
      if (!handled) {
        socket.write('HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n');
        socket.destroy();
      }
    });
  });

  // Shutdown: force-close active exec WS pairs (1001) before closing the HTTP
  // server. Without this, in-flight WS sessions would keep their ping timers
  // and sockets alive until the OS tears the process down.
  let shuttingDown = false;
  const shutdown = (signal: string): void => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.log(`[control-plane] received ${signal}, shutting down…`);
    try {
      execProxy.close();
    } catch (err) {
      console.error('[control-plane] execProxy.close error:', err);
    }
    server.close(() => {
      process.exit(0);
    });
    // Hard-exit fallback so a stuck connection can't block forever.
    setTimeout(() => process.exit(0), 5000).unref();
  };
  process.on('SIGTERM', () => shutdown('SIGTERM'));
  process.on('SIGINT', () => shutdown('SIGINT'));
}

main().catch((err) => {
  console.error('[control-plane] fatal:', err);
  process.exit(1);
});
