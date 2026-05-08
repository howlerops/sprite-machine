/**
 * Constant-time string comparison for secret tokens.
 *
 * `crypto.timingSafeEqual` requires equal-length buffers and throws otherwise,
 * which itself leaks length. The lengths of our tokens (USER_TOKEN, etc.) are
 * already public via the env, so the upfront length check is acceptable. The
 * remaining timing channel is closed by `timingSafeEqual` proper.
 */
import { timingSafeEqual } from 'node:crypto';

export function safeEqual(provided: string, expected: string): boolean {
  if (typeof provided !== 'string' || typeof expected !== 'string') return false;
  if (provided.length !== expected.length) return false;
  const a = Buffer.from(provided, 'utf8');
  const b = Buffer.from(expected, 'utf8');
  if (a.length !== b.length) return false;
  return timingSafeEqual(a, b);
}
