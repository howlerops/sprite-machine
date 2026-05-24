import type {
  CreateRunRequest,
  ResolvePermissionRequest,
  RunEvent,
  RunSummary,
} from '@sprite-machine/shared';

// Mirror types matching the JSON shapes from the control plane
export interface RunRow {
  id: string;
  status: string;
  prompt: string;
  title?: string | null;
  workdir?: string;
  workdirs?: string[] | null;
  model?: string;
  permissionMode?: string;
  metadata?: Record<string, unknown>;
  createdAt: string;
  updatedAt: string;
  lastEventAt?: string | null;
  totalCostUsd: number;
  totalInputTokens: number;
  totalOutputTokens: number;
  sessionId?: string;
  spriteId?: string;
}

export interface RunEventRow {
  runId: string;
  seq: number;
  event: RunEvent;
  createdAt: string;
}

export interface RunDetail extends RunRow {
  events: RunEventRow[];
}

export interface SpriteRow {
  id: string;
  name: string;
  status: string;
  workdir?: string;
  model?: string;
  permissionMode?: string;
  createdAt: string;
  updatedAt: string;
  idleTimeoutMs?: number;
  lastActiveAt?: string;
}

export interface PushTokenRow {
  id: string;
  token: string;
  label?: string;
  createdAt: string;
}

export interface HealthResponse {
  ok: boolean;
  role?: string;
  backend?: string;
  template?: string;
}

export interface ApplyResult {
  applied: string[];
  errors: Array<{ spriteId: string; message: string }>;
}

export interface GithubIntegration {
  connected: boolean;
  username: string | null;
  gitUserName: string | null;
  gitUserEmail: string | null;
  apply?: ApplyResult;
}

export interface AiAuthIntegration {
  codexConnected: boolean;
  piConnected: boolean;
  claudeConnected: boolean;
  updatedAt: string | null;
  apply?: ApplyResult;
}

export interface ApiError extends Error {
  status: number;
}

function makeApiError(status: number, message: string): ApiError {
  const err = new Error(message) as ApiError;
  err.status = status;
  return err;
}

async function apiFetch<T>(
  baseUrl: string,
  token: string | null,
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...(options.headers as Record<string, string> | undefined),
  };
  if (token) {
    headers['Authorization'] = `Bearer ${token}`;
  }

  const res = await fetch(`${baseUrl}${path}`, {
    ...options,
    headers,
    // On web this lets the browser send the signed sm_session cookie. Native
    // fetch ignores credentials but still attaches the bearer header above.
    credentials: 'include',
  });

  if (!res.ok) {
    const text = await res.text().catch(() => res.statusText);
    throw makeApiError(res.status, text);
  }

  return res.json() as Promise<T>;
}

// Health
export function getHealth(baseUrl: string): Promise<HealthResponse> {
  return apiFetch<HealthResponse>(baseUrl, null, '/healthz');
}

// Runs
export function getRuns(baseUrl: string, token: string | null): Promise<{ runs: RunRow[] }> {
  return apiFetch<{ runs: RunRow[] }>(baseUrl, token, '/runs');
}

export function getRun(baseUrl: string, token: string | null, id: string): Promise<RunDetail> {
  return apiFetch<RunDetail>(baseUrl, token, `/runs/${id}`);
}

export function createRun(
  baseUrl: string,
  token: string | null,
  body: CreateRunRequest,
): Promise<RunRow> {
  return apiFetch<RunRow>(baseUrl, token, '/runs', {
    method: 'POST',
    body: JSON.stringify(body),
  });
}

export function sendMessage(
  baseUrl: string,
  token: string | null,
  runId: string,
  text: string,
): Promise<{ accepted: true }> {
  return apiFetch<{ accepted: true }>(baseUrl, token, `/runs/${runId}/messages`, {
    method: 'POST',
    body: JSON.stringify({ text }),
  });
}

export function cancelRun(
  baseUrl: string,
  token: string | null,
  runId: string,
): Promise<{ cancelled: true }> {
  return apiFetch<{ cancelled: true }>(baseUrl, token, `/runs/${runId}/cancel`, {
    method: 'POST',
  });
}

export function deleteRun(
  baseUrl: string,
  token: string | null,
  runId: string,
): Promise<{ deleted: true }> {
  return apiFetch<{ deleted: true }>(baseUrl, token, `/runs/${runId}`, {
    method: 'DELETE',
  });
}

export function resolvePermission(
  baseUrl: string,
  token: string | null,
  runId: string,
  requestId: string,
  body: ResolvePermissionRequest,
): Promise<{ resolved: true }> {
  return apiFetch<{ resolved: true }>(baseUrl, token, `/runs/${runId}/permissions/${requestId}`, {
    method: 'POST',
    body: JSON.stringify(body),
  });
}

// Sprites
export function getSprites(
  baseUrl: string,
  token: string | null,
): Promise<{ sprites: SpriteRow[] }> {
  return apiFetch<{ sprites: SpriteRow[] }>(baseUrl, token, '/sprites');
}

export function getSprite(baseUrl: string, token: string | null, id: string): Promise<SpriteRow> {
  return apiFetch<SpriteRow>(baseUrl, token, `/sprites/${id}`);
}

export function suspendSprite(
  baseUrl: string,
  token: string | null,
  id: string,
): Promise<SpriteRow> {
  return apiFetch<SpriteRow>(baseUrl, token, `/sprites/${id}/suspend`, { method: 'POST' });
}

export function resumeSprite(
  baseUrl: string,
  token: string | null,
  id: string,
): Promise<SpriteRow> {
  return apiFetch<SpriteRow>(baseUrl, token, `/sprites/${id}/resume`, { method: 'POST' });
}

export function deleteSprite(
  baseUrl: string,
  token: string | null,
  id: string,
): Promise<{ deleted: true }> {
  return apiFetch<{ deleted: true }>(baseUrl, token, `/sprites/${id}`, { method: 'DELETE' });
}

// Integrations
export function getGithubIntegration(
  baseUrl: string,
  token: string | null,
): Promise<GithubIntegration> {
  return apiFetch<GithubIntegration>(baseUrl, token, '/integrations/github');
}

export function saveGithubIntegration(
  baseUrl: string,
  token: string | null,
  body: { token?: string; username: string; gitUserName: string; gitUserEmail: string },
): Promise<GithubIntegration> {
  return apiFetch<GithubIntegration>(baseUrl, token, '/integrations/github', {
    method: 'PUT',
    body: JSON.stringify(body),
  });
}

export function applyGithubIntegration(
  baseUrl: string,
  token: string | null,
): Promise<GithubIntegration> {
  return apiFetch<GithubIntegration>(baseUrl, token, '/integrations/github/apply', {
    method: 'POST',
  });
}

export function getAiAuthIntegration(
  baseUrl: string,
  token: string | null,
): Promise<AiAuthIntegration> {
  return apiFetch<AiAuthIntegration>(baseUrl, token, '/integrations/ai-auth');
}

export function saveAiAuthIntegration(
  baseUrl: string,
  token: string | null,
  body: { codexAuthJson?: string; piAuthJson?: string; claudeCredentialsJson?: string },
): Promise<AiAuthIntegration> {
  return apiFetch<AiAuthIntegration>(baseUrl, token, '/integrations/ai-auth', {
    method: 'PUT',
    body: JSON.stringify(body),
  });
}

export function applyAiAuthIntegration(
  baseUrl: string,
  token: string | null,
): Promise<AiAuthIntegration> {
  return apiFetch<AiAuthIntegration>(baseUrl, token, '/integrations/ai-auth/apply', {
    method: 'POST',
  });
}

// Push
export function registerPushToken(
  baseUrl: string,
  token: string | null,
  pushToken: string,
  label?: string,
): Promise<PushTokenRow> {
  const body: { token: string; label?: string } = { token: pushToken };
  if (label !== undefined) {
    body.label = label;
  }
  return apiFetch<PushTokenRow>(baseUrl, token, '/push/tokens', {
    method: 'POST',
    body: JSON.stringify(body),
  });
}

export function getPushTokens(
  baseUrl: string,
  token: string | null,
): Promise<{ tokens: PushTokenRow[] }> {
  return apiFetch<{ tokens: PushTokenRow[] }>(baseUrl, token, '/push/tokens');
}

// Attachments
export interface AttachmentResponse {
  id: string;
  url: string;
  contentType: string;
  sizeBytes: number;
}

/**
 * Upload an image file as a run attachment.
 * Uses raw fetch (not apiFetch) because the body is multipart/form-data,
 * not JSON. On web, `file` is a browser File; on native it can be a Blob
 * produced from an image picker result.
 */
export async function uploadAttachment(
  baseUrl: string,
  token: string | null,
  runId: string,
  file: File | Blob,
  filename?: string,
): Promise<AttachmentResponse> {
  const form = new FormData();
  if (file instanceof File) {
    form.append('file', file);
  } else {
    // React Native Blob — wrap with an explicit name so the server gets a
    // Content-Disposition filename and can derive the extension.
    form.append('file', file, filename ?? 'image.jpg');
  }

  const headers: Record<string, string> = {};
  if (token) headers['Authorization'] = `Bearer ${token}`;

  const res = await fetch(`${baseUrl}/runs/${runId}/attachments`, {
    method: 'POST',
    headers,
    body: form,
    credentials: 'include',
  });

  if (!res.ok) {
    const text = await res.text().catch(() => res.statusText);
    const err = new Error(text) as Error & { status: number };
    err.status = res.status;
    throw err;
  }

  return res.json() as Promise<AttachmentResponse>;
}

// Admin
export function tickIdleScheduler(
  baseUrl: string,
  token: string | null,
): Promise<{ evaluated: number; suspended: number; errors: number }> {
  return apiFetch<{ evaluated: number; suspended: number; errors: number }>(
    baseUrl,
    token,
    '/admin/idle/tick',
    { method: 'POST' },
  );
}

// Auth (web cookie session)
export function login(baseUrl: string, flyToken: string): Promise<{ email: string }> {
  return apiFetch<{ email: string }>(baseUrl, null, '/login', {
    method: 'POST',
    body: JSON.stringify({ flyToken }),
  });
}

export function logout(baseUrl: string): Promise<{ ok: true }> {
  return apiFetch<{ ok: true }>(baseUrl, null, '/logout', { method: 'POST' });
}

export function getMe(baseUrl: string): Promise<{ email: string }> {
  return apiFetch<{ email: string }>(baseUrl, null, '/me');
}

// File system browser
export interface FsEntry {
  name: string;
  isDir: boolean;
  path: string;
}

export interface FsListResponse {
  root: string;
  path: string;
  entries: FsEntry[];
}

export function listFsDir(
  baseUrl: string,
  token: string | null,
  dirPath?: string,
): Promise<FsListResponse> {
  const qs = dirPath !== undefined ? `?path=${encodeURIComponent(dirPath)}` : '';
  return apiFetch<FsListResponse>(baseUrl, token, `/fs/list${qs}`);
}

// Re-export shared types used by consumers
export type { RunSummary, RunEvent, CreateRunRequest, ResolvePermissionRequest };
