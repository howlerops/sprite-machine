import { z } from 'zod';

export const RunStatus = z.enum([
  'queued',
  'starting',
  'running',
  'awaiting_input',
  'idle',
  'completed',
  'failed',
  'cancelled',
]);
export type RunStatus = z.infer<typeof RunStatus>;

export const Harness = z.enum(['claude', 'codex', 'pi', 'mock']);
export type Harness = z.infer<typeof Harness>;

export const CreateRunRequest = z.object({
  prompt: z.string().min(1),
  harness: Harness.optional(),
  workdir: z.string().optional(),
  workdirs: z.array(z.string()).optional(),
  model: z.string().optional(),
  permissionMode: z.enum(['default', 'acceptEdits', 'bypassPermissions', 'plan']).optional(),
  allowedTools: z.array(z.string()).optional(),
  metadata: z.record(z.string(), z.unknown()).optional(),
});
export type CreateRunRequest = z.infer<typeof CreateRunRequest>;

export const SendMessageRequest = z.object({
  text: z.string().min(1),
});
export type SendMessageRequest = z.infer<typeof SendMessageRequest>;

export const RunEvent = z.discriminatedUnion('type', [
  z.object({ type: z.literal('status'), status: RunStatus, at: z.string() }),
  z.object({
    type: z.literal('session_started'),
    sessionId: z.string(),
    model: z.string().optional(),
    at: z.string(),
  }),
  z.object({ type: z.literal('user_message'), text: z.string(), at: z.string() }),
  z.object({ type: z.literal('assistant_text'), text: z.string(), at: z.string() }),
  z.object({ type: z.literal('assistant_delta'), text: z.string(), at: z.string() }),
  z.object({
    type: z.literal('tool_use'),
    toolUseId: z.string(),
    name: z.string(),
    input: z.unknown(),
    at: z.string(),
  }),
  z.object({
    type: z.literal('tool_result'),
    toolUseId: z.string(),
    name: z.string().optional(),
    output: z.unknown(),
    isError: z.boolean().optional(),
    at: z.string(),
  }),
  z.object({
    type: z.literal('permission_requested'),
    requestId: z.string(),
    toolName: z.string(),
    input: z.unknown(),
    at: z.string(),
  }),
  z.object({
    type: z.literal('permission_resolved'),
    requestId: z.string(),
    decision: z.enum(['allow', 'deny']),
    reason: z.string().optional(),
    at: z.string(),
  }),
  z.object({
    type: z.literal('turn_complete'),
    costUsd: z.number().optional(),
    inputTokens: z.number().optional(),
    outputTokens: z.number().optional(),
    at: z.string(),
  }),
  z.object({ type: z.literal('error'), message: z.string(), at: z.string() }),
  z.object({ type: z.literal('completed'), at: z.string() }),
]);
export type RunEvent = z.infer<typeof RunEvent>;

export const RunSummary = z.object({
  id: z.string(),
  status: RunStatus,
  harness: Harness,
  sessionId: z.string().optional(),
  prompt: z.string(),
  workdir: z.string().optional(),
  workdirs: z.array(z.string()).optional(),
  createdAt: z.string(),
  updatedAt: z.string(),
  totalCostUsd: z.number().default(0),
  totalInputTokens: z.number().default(0),
  totalOutputTokens: z.number().default(0),
});
export type RunSummary = z.infer<typeof RunSummary>;

export const SpriteWebhookEvent = z.object({
  spriteId: z.string(),
  runId: z.string(),
  event: RunEvent,
});
export type SpriteWebhookEvent = z.infer<typeof SpriteWebhookEvent>;

export const ResolvePermissionRequest = z.object({
  decision: z.enum(['allow', 'deny']),
  reason: z.string().optional(),
});
export type ResolvePermissionRequest = z.infer<typeof ResolvePermissionRequest>;

export function nowIso(): string {
  return new Date().toISOString();
}

export function newId(prefix: string): string {
  const r = Math.random().toString(36).slice(2, 10);
  const t = Date.now().toString(36);
  return `${prefix}_${t}${r}`;
}
