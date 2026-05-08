import { Hono } from 'hono';
import { z } from 'zod';
import {
  setSessionCookie,
  clearSessionCookie,
  readSessionCookie,
  isEmailAllowed,
} from '../session.js';

export interface AuthRoutesDeps {
  sessionSecret: string;
  allowedEmails: string[];
  cookieSecure: boolean;
}

const LoginBody = z.object({ flyToken: z.string().min(1) });

interface FlyViewerResponse {
  data?: { viewer?: { email?: string | null } | null } | null;
  errors?: Array<{ message?: string }>;
}

const FLY_GRAPHQL = 'https://api.fly.io/graphql';

// Per-IP login rate limiter: 5 attempts per 60s window. Buckets self-expire on
// next observation; not exact, but cheap and good enough for in-process
// brute-force resistance against the Fly token exchange.
const LOGIN_WINDOW_MS = 60_000;
const LOGIN_MAX_ATTEMPTS = 5;
const loginBuckets = new Map<string, { count: number; windowStart: number }>();

function loginRateLimit(ip: string): { allowed: boolean; retryAfterSec: number } {
  const now = Date.now();
  const bucket = loginBuckets.get(ip);
  if (!bucket || now - bucket.windowStart >= LOGIN_WINDOW_MS) {
    loginBuckets.set(ip, { count: 1, windowStart: now });
    return { allowed: true, retryAfterSec: 0 };
  }
  if (bucket.count >= LOGIN_MAX_ATTEMPTS) {
    const retryAfterSec = Math.max(
      1,
      Math.ceil((bucket.windowStart + LOGIN_WINDOW_MS - now) / 1000),
    );
    return { allowed: false, retryAfterSec };
  }
  bucket.count += 1;
  return { allowed: true, retryAfterSec: 0 };
}

/**
 * Exchange a Fly personal access token for a signed session cookie.
 *
 *   POST /login   { flyToken } → 200 { email } + Set-Cookie sm_session
 *                              → 401 invalid token / not allowed
 *                              → 503 + Retry-After if Fly returns 429
 *   POST /logout  → 200 + cleared cookie
 *   GET  /me      → 200 { email } if cookie valid; 401 otherwise
 */
export function buildAuthRoutes(deps: AuthRoutesDeps): Hono {
  const app = new Hono();

  app.post('/login', async (c) => {
    const ip = c.req.header('x-forwarded-for')?.split(',')[0]?.trim() || 'unknown';
    const limit = loginRateLimit(ip);
    if (!limit.allowed) {
      return c.json({ error: 'rate_limited' }, 429, { 'Retry-After': String(limit.retryAfterSec) });
    }

    const body = await c.req.json().catch(() => null);
    const parsed = LoginBody.safeParse(body);
    if (!parsed.success) {
      return c.json({ error: 'invalid request' }, 400);
    }
    const { flyToken } = parsed.data;

    let res: Response;
    try {
      res = await fetch(FLY_GRAPHQL, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          authorization: `Bearer ${flyToken}`,
        },
        body: JSON.stringify({ query: '{ viewer { email } }' }),
      });
    } catch (err) {
      // Sanitized: some undici versions include request headers (incl. the
      // bearer above) on error objects when serialized. Log message only.
      console.error('[auth] fly graphql network error:', (err as Error)?.message ?? 'unknown');
      return c.json({ error: 'upstream_unreachable' }, 502);
    }

    if (res.status === 429) {
      const retryAfter = res.headers.get('retry-after') ?? '30';
      return c.json({ error: 'rate_limited' }, 503, { 'Retry-After': retryAfter });
    }
    if (res.status === 401 || res.status === 403) {
      return c.json({ error: 'invalid_token' }, 401);
    }
    if (!res.ok) {
      const text = await res.text().catch(() => '');
      console.error(`[auth] fly graphql ${res.status}: ${text.slice(0, 300)}`);
      return c.json({ error: 'upstream_error' }, 502);
    }

    let payload: FlyViewerResponse;
    try {
      payload = (await res.json()) as FlyViewerResponse;
    } catch {
      return c.json({ error: 'upstream_error' }, 502);
    }

    if (payload.errors?.length) {
      const message = payload.errors[0]?.message ?? '';
      // Fly returns errors as 200 with an `errors` array on bad auth.
      if (/auth|token|forbidden/i.test(message)) {
        return c.json({ error: 'invalid_token' }, 401);
      }
      return c.json({ error: 'upstream_error', detail: message.slice(0, 200) }, 502);
    }

    const email = payload.data?.viewer?.email;
    if (!email || typeof email !== 'string') {
      return c.json({ error: 'invalid_token' }, 401);
    }

    if (deps.allowedEmails.length === 0) {
      console.warn('[auth] ALLOWED_EMAILS is empty — rejecting all logins');
      return c.json({ error: 'not_allowed' }, 403);
    }
    if (!isEmailAllowed(email, deps.allowedEmails)) {
      return c.json({ error: 'not_allowed' }, 403);
    }

    setSessionCookie(c, email.toLowerCase(), {
      secret: deps.sessionSecret,
      secure: deps.cookieSecure,
    });
    return c.json({ email: email.toLowerCase() });
  });

  app.post('/logout', (c) => {
    clearSessionCookie(c, { secure: deps.cookieSecure });
    return c.json({ ok: true });
  });

  app.get('/me', (c) => {
    const sess = readSessionCookie(c, deps.sessionSecret);
    if (!sess) return c.json({ error: 'unauthorized' }, 401);
    return c.json({ email: sess.email });
  });

  return app;
}
