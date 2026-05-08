import { serve } from '@hono/node-server';
import { Hono } from 'hono';
import { effectiveMode, loadConfig } from './config.js';
import { bearerAuth } from './auth.js';
import { RunStore } from './runs.js';
import { AgentRouter, type Agent } from './agent.js';
import { WebhookPusher } from './webhook.js';
import { buildRoutes } from './routes.js';

async function main(): Promise<void> {
  const cfg = loadConfig();
  const mode = effectiveMode(cfg);

  const store = new RunStore(cfg.dataDir);
  await store.init();

  // Pick claude impl: prefer CLI (uses your Pro/Max OAuth + installed plugins),
  // fall back to Agent SDK if ANTHROPIC_API_KEY is set, else MockAgent.
  const claudeImpl: 'cli' | 'sdk' | 'mock' =
    process.env.SPRITE_AGENT_CLAUDE_IMPL === 'sdk'
      ? 'sdk'
      : process.env.SPRITE_AGENT_CLAUDE_IMPL === 'mock'
        ? 'mock'
        : mode === 'mock' && !process.env.ANTHROPIC_API_KEY
          ? 'cli' // CLI uses OAuth, not API key — fine without ANTHROPIC_API_KEY
          : 'cli';

  const agent: Agent = new AgentRouter({
    store,
    defaultPermissionMode: cfg.defaultPermissionMode,
    dataDir: cfg.dataDir,
    claudeImpl,
  });

  const pusher = cfg.controlPlaneWebhookUrl
    ? new WebhookPusher({
        url: cfg.controlPlaneWebhookUrl,
        ...(cfg.controlPlaneWebhookToken ? { token: cfg.controlPlaneWebhookToken } : {}),
        spriteId: cfg.spriteId,
        store,
      })
    : null;
  pusher?.start();

  const app = new Hono();
  app.use('*', bearerAuth(cfg.agentToken, ['/healthz']));
  app.route('/', buildRoutes({ store, agent }));

  serve({ fetch: app.fetch, port: cfg.port, hostname: cfg.host }, (info) => {
    console.log(
      `[sprite-agent] mode=${mode} sprite=${cfg.spriteId} listening on http://${info.address}:${info.port}`,
    );
  });
}

main().catch((err) => {
  console.error('[sprite-agent] fatal:', err);
  process.exit(1);
});
