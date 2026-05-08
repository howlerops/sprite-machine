import path from 'node:path';
import { z } from 'zod';

const ConfigSchema = z.object({
  port: z.coerce.number().default(8787),
  host: z.string().default('127.0.0.1'),
  workspaceRoot: z.string(),
  dataDir: z.string(),
  dbPath: z.string(),
  userToken: z.string().min(8, 'USER_TOKEN must be at least 8 chars'),
  webhookToken: z.string().min(8, 'CONTROL_PLANE_WEBHOOK_TOKEN must be at least 8 chars'),
  publicBaseUrl: z.string().url(),
  templateCheckpointId: z.string().nullable(),
  spritesBackend: z.enum(['local', 'fly']).default('local'),
  spritesApiToken: z.string().optional(),
  workerName: z.string().default('sprite-machine'),
  workerAgentToken: z.string().optional(),
  workerSpriteUrl: z.string().url().optional(),
  expoAccessToken: z.string().optional(),
  idleSuspendAfterMs: z.coerce.number().default(5 * 60 * 1000),
  idleCheckIntervalMs: z.coerce.number().default(30 * 1000),
});

export type Config = z.infer<typeof ConfigSchema>;

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  const workspaceRoot =
    env.SPRITE_MACHINE_ROOT ?? path.resolve(import.meta.dirname, '..', '..', '..');
  const dataDir = env.SPRITE_MACHINE_DATA ?? path.join(workspaceRoot, '.sprite-machine-data');
  const dbPath = env.CONTROL_PLANE_DB_PATH ?? path.join(dataDir, 'control-plane.sqlite');
  const port = env.PORT ?? '8787';
  const host = env.HOST ?? '127.0.0.1';
  const publicBaseUrl = env.PUBLIC_BASE_URL ?? `http://${host}:${port}`;
  return ConfigSchema.parse({
    port,
    host,
    workspaceRoot,
    dataDir,
    dbPath,
    userToken: env.USER_TOKEN ?? 'dev-user-token',
    webhookToken: env.CONTROL_PLANE_WEBHOOK_TOKEN ?? 'dev-webhook-token',
    publicBaseUrl,
    templateCheckpointId: env.TEMPLATE_CHECKPOINT_ID ?? null,
    spritesBackend: env.SPRITES_BACKEND,
    spritesApiToken: env.SPRITES_API_TOKEN,
    workerName: env.SPRITE_WORKER_NAME,
    workerAgentToken: env.WORKER_AGENT_TOKEN,
    workerSpriteUrl: env.WORKER_SPRITE_URL,
    expoAccessToken: env.EXPO_ACCESS_TOKEN,
    idleSuspendAfterMs: env.IDLE_SUSPEND_AFTER_MS,
    idleCheckIntervalMs: env.IDLE_CHECK_INTERVAL_MS,
  });
}
