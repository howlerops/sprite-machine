import type { MiddlewareHandler } from 'hono';

export function bearerAuth(token: string, exemptPaths: string[] = []): MiddlewareHandler {
  return async (c, next) => {
    if (exemptPaths.some((p) => c.req.path === p)) return next();
    const header = c.req.header('authorization') ?? '';
    const expected = `Bearer ${token}`;
    if (header !== expected) return c.json({ error: 'unauthorized' }, 401);
    return next();
  };
}
