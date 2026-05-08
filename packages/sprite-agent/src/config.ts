import { z } from 'zod';

const Mode = z.enum(['auto', 'sdk', 'mock']);

const ConfigSchema = z.object({
  port: z.coerce.number().default(8080),
  host: z.string().default('0.0.0.0'),
  spriteId: z.string().default('local-sprite'),
  agentToken: z.string().default('dev-token'),
  anthropicApiKey: z.string().optional(),
  controlPlaneWebhookUrl: z.string().url().optional(),
  controlPlaneWebhookToken: z.string().optional(),
  dataDir: z.string().default('./.sprite-agent-data'),
  mode: Mode.default('auto'),
  defaultPermissionMode: z
    .enum(['default', 'acceptEdits', 'bypassPermissions', 'plan'])
    .default('acceptEdits'),
  defaultWorkdir: z.string().default(process.cwd()),
});

export type Config = z.infer<typeof ConfigSchema>;

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  return ConfigSchema.parse({
    port: env.PORT,
    host: env.HOST,
    spriteId: env.SPRITE_ID,
    agentToken: env.SPRITE_AGENT_TOKEN,
    anthropicApiKey: env.ANTHROPIC_API_KEY,
    controlPlaneWebhookUrl: env.CONTROL_PLANE_WEBHOOK_URL,
    controlPlaneWebhookToken: env.CONTROL_PLANE_WEBHOOK_TOKEN,
    dataDir: env.SPRITE_AGENT_DATA_DIR,
    mode: env.SPRITE_AGENT_MODE,
    defaultPermissionMode: env.SPRITE_AGENT_PERMISSION_MODE,
    defaultWorkdir: env.SPRITE_AGENT_WORKDIR,
  });
}

export function effectiveMode(cfg: Config): 'sdk' | 'mock' {
  if (cfg.mode === 'sdk') return 'sdk';
  if (cfg.mode === 'mock') return 'mock';
  return cfg.anthropicApiKey ? 'sdk' : 'mock';
}
