/**
 * Auth helpers usable from a raw `http.IncomingMessage` (i.e. before the WS
 * upgrade completes, where Hono's Context is unavailable).
 */
import { safeEqual } from './crypto-equal.js';
import { verifySession, SESSION_COOKIE, type VerifiedSession } from './session.js';

export function verifyBearer(
  authHeader: string | string[] | undefined,
  userToken: string,
): boolean {
  if (!authHeader) return false;
  const value = Array.isArray(authHeader) ? (authHeader[0] ?? '') : authHeader;
  return safeEqual(value, `Bearer ${userToken}`);
}

/**
 * Parse the raw `Cookie:` header and verify the `sm_session` cookie value.
 * Returns the verified session, or null on missing/invalid cookie.
 */
export function readSessionCookieFromHeader(
  cookieHeader: string | string[] | undefined,
  secret: string,
): VerifiedSession | null {
  if (!cookieHeader) return null;
  const raw = Array.isArray(cookieHeader) ? cookieHeader.join('; ') : cookieHeader;
  for (const pair of raw.split(';')) {
    const eq = pair.indexOf('=');
    if (eq < 0) continue;
    const name = pair.slice(0, eq).trim();
    if (name !== SESSION_COOKIE) continue;
    const value = decodeURIComponent(pair.slice(eq + 1).trim());
    return verifySession(value, secret);
  }
  return null;
}
