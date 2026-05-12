/**
 * Signed session cookie for the web dashboard.
 *
 * Format (URL-safe): `v1|<base64url(email)>.<expMs>.<base64url(hmac)>`
 *  where hmac = HMAC-SHA256(SESSION_SECRET, `v1|${b64email}.${expMs}`).
 *
 * The `v1|` prefix is a schema version marker so future migrations can be
 * introduced without ambiguity; `verifySession` rejects anything that does
 * not start with a known prefix.
 *
 * CSRF posture: cookie is httpOnly + sameSite=lax. The login POST is same-origin
 * (login screen is served from `/app/`). We do not perform unsafe state-changing
 * GETs, so SameSite=Lax is sufficient without an explicit CSRF token.
 */
import { createHmac, timingSafeEqual } from 'node:crypto';
import type { Context } from 'hono';
import { setCookie, getCookie, deleteCookie } from 'hono/cookie';

export const SESSION_COOKIE = 'sm_session';
export const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000; // 30 days
const SESSION_VERSION_PREFIX = 'v1|';

function b64urlEncode(buf: Buffer | string): string {
  const b = typeof buf === 'string' ? Buffer.from(buf, 'utf8') : buf;
  return b.toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function b64urlDecode(s: string): Buffer {
  const padded = s.replace(/-/g, '+').replace(/_/g, '/') + '==='.slice((s.length + 3) % 4);
  return Buffer.from(padded, 'base64');
}

export function signSession(email: string, secret: string, expMs?: number): string {
  const exp = expMs ?? Date.now() + SESSION_TTL_MS;
  const b64email = b64urlEncode(email);
  const payload = `${SESSION_VERSION_PREFIX}${b64email}.${exp}`;
  const mac = createHmac('sha256', secret).update(payload).digest();
  return `${payload}.${b64urlEncode(mac)}`;
}

export interface VerifiedSession {
  email: string;
  expMs: number;
}

export function verifySession(value: string | undefined, secret: string): VerifiedSession | null {
  if (!value) return null;
  if (!value.startsWith(SESSION_VERSION_PREFIX)) return null;
  const versioned = value.slice(SESSION_VERSION_PREFIX.length);
  const parts = versioned.split('.');
  if (parts.length !== 3) return null;
  const [b64email, expStr, sigB64] = parts as [string, string, string];
  const expMs = Number(expStr);
  if (!Number.isFinite(expMs) || expMs <= Date.now()) return null;

  const expected = createHmac('sha256', secret)
    .update(`${SESSION_VERSION_PREFIX}${b64email}.${expStr}`)
    .digest();
  let provided: Buffer;
  try {
    provided = b64urlDecode(sigB64);
  } catch {
    return null;
  }
  if (provided.length !== expected.length) return null;
  if (!timingSafeEqual(provided, expected)) return null;

  let email: string;
  try {
    email = b64urlDecode(b64email).toString('utf8');
  } catch {
    return null;
  }
  return { email, expMs };
}

export interface SessionCookieOptions {
  secret: string;
  secure: boolean;
}

export function setSessionCookie(c: Context, email: string, opts: SessionCookieOptions): void {
  const maxAge = Math.floor(SESSION_TTL_MS / 1000);
  const value = signSession(email, opts.secret);
  setCookie(c, SESSION_COOKIE, value, {
    httpOnly: true,
    secure: opts.secure,
    sameSite: 'Lax',
    path: '/',
    maxAge,
    partitioned: true,
  });
}

export function clearSessionCookie(c: Context, opts: { secure: boolean }): void {
  deleteCookie(c, SESSION_COOKIE, {
    path: '/',
    secure: opts.secure,
    sameSite: 'Lax',
  });
}

export function readSessionCookie(c: Context, secret: string): VerifiedSession | null {
  const raw = getCookie(c, SESSION_COOKIE);
  return verifySession(raw, secret);
}

/**
 * Domain-wildcard allowlist matcher. Each entry is either an exact email or a
 * `*@domain` pattern. Comparison is case-insensitive. The `*` is only honored
 * as a leading-prefix wildcard; mid-string `*` is not recognized.
 */
export function isEmailAllowed(email: string, entries: string[]): boolean {
  const target = email.trim().toLowerCase();
  if (!target) return false;
  for (const raw of entries) {
    const entry = raw.trim().toLowerCase();
    if (!entry) continue;
    if (entry.startsWith('*@')) {
      const domain = entry.slice(2);
      if (!domain) continue;
      if (target.endsWith(`@${domain}`)) return true;
    } else if (entry === target) {
      return true;
    }
  }
  return false;
}
