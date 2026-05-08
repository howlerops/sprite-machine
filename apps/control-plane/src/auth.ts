import type { Context, MiddlewareHandler } from 'hono';
import { isEmailAllowed, readSessionCookie } from './session.js';
import { safeEqual } from './crypto-equal.js';

declare module 'hono' {
  interface ContextVariableMap {
    identity: { kind: 'bearer' } | { kind: 'cookie'; email: string };
  }
}

export interface AuthOptions {
  userToken: string;
  sessionSecret: string;
  allowedEmails: string[];
  exemptPaths?: string[];
}

/**
 * Accepts either:
 *   - `Authorization: Bearer <USER_TOKEN>` (native iOS / CLI), OR
 *   - a valid signed `sm_session` cookie (web dashboard) whose email is still
 *     on the allowlist (re-checked on every request, so allowlist removals
 *     take effect immediately rather than waiting for cookie expiry).
 *
 * Sets `c.var.identity` describing which credential authorized the request so
 * downstream routes can treat e.g. cookie sessions differently if needed.
 */
export function bearerOrCookieOrUnauthorized(opts: AuthOptions): MiddlewareHandler {
  const exempt = opts.exemptPaths ?? [];
  return async (c, next) => {
    if (exempt.includes(c.req.path)) return next();
    if (tryBearer(c, opts.userToken)) return next();
    const sess = readSessionCookie(c, opts.sessionSecret);
    if (sess && isEmailAllowed(sess.email, opts.allowedEmails)) {
      c.set('identity', { kind: 'cookie', email: sess.email });
      return next();
    }
    return c.json({ error: 'unauthorized' }, 401);
  };
}

/**
 * Backwards-compatible alias. Existing callers that only know about the bearer
 * path can keep using `bearerOrUnauthorized(token)`; new callers should prefer
 * `bearerOrCookieOrUnauthorized` so cookie-authed requests also pass.
 */
export function bearerOrUnauthorized(token: string, exemptPaths: string[] = []): MiddlewareHandler {
  return async (c, next) => {
    if (exemptPaths.includes(c.req.path)) return next();
    if (!tryBearer(c, token)) return c.json({ error: 'unauthorized' }, 401);
    return next();
  };
}

function tryBearer(c: Context, token: string): boolean {
  const header = c.req.header('authorization') ?? '';
  if (!safeEqual(header, `Bearer ${token}`)) return false;
  c.set('identity', { kind: 'bearer' });
  return true;
}
