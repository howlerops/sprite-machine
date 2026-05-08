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
  workdir?: string;
  model?: string;
  permissionMode?: string;
  metadata?: Record<string, unknown>;
  createdAt: string;
  updatedAt: string;
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
export function getRuns(baseUrl: string, token: string): Promise<{ runs: RunRow[] }> {
  return apiFetch<{ runs: RunRow[] }>(baseUrl, token, '/runs');
}

export function getRun(baseUrl: string, token: string, id: string): Promise<RunDetail> {
  return apiFetch<RunDetail>(baseUrl, token, `/runs/${id}`);
}

export function createRun(baseUrl: string, token: string, body: CreateRunRequest): Promise<RunRow> {
  return apiFetch<RunRow>(baseUrl, token, '/runs', {
    method: 'POST',
    body: JSON.stringify(body),
  });
}

export function sendMessage(
  baseUrl: string,
  token: string,
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
  token: string,
  runId: string,
): Promise<{ cancelled: true }> {
  return apiFetch<{ cancelled: true }>(baseUrl, token, `/runs/${runId}/cancel`, {
    method: 'POST',
  });
}

export function resolvePermission(
  baseUrl: string,
  token: string,
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
export function getSprites(baseUrl: string, token: string): Promise<{ sprites: SpriteRow[] }> {
  return apiFetch<{ sprites: SpriteRow[] }>(baseUrl, token, '/sprites');
}

export function getSprite(baseUrl: string, token: string, id: string): Promise<SpriteRow> {
  return apiFetch<SpriteRow>(baseUrl, token, `/sprites/${id}`);
}

export function suspendSprite(baseUrl: string, token: string, id: string): Promise<SpriteRow> {
  return apiFetch<SpriteRow>(baseUrl, token, `/sprites/${id}/suspend`, { method: 'POST' });
}

export function resumeSprite(baseUrl: string, token: string, id: string): Promise<SpriteRow> {
  return apiFetch<SpriteRow>(baseUrl, token, `/sprites/${id}/resume`, { method: 'POST' });
}

// Push
export function registerPushToken(
  baseUrl: string,
  token: string,
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

export function getPushTokens(baseUrl: string, token: string): Promise<{ tokens: PushTokenRow[] }> {
  return apiFetch<{ tokens: PushTokenRow[] }>(baseUrl, token, '/push/tokens');
}

// Admin
export function tickIdleScheduler(
  baseUrl: string,
  token: string,
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

// Re-export shared types used by consumers
export type { RunSummary, RunEvent, CreateRunRequest, ResolvePermissionRequest };
