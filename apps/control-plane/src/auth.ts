import type { MiddlewareHandler } from 'hono';

export function bearerOrUnauthorized(token: string, exemptPaths: string[] = []): MiddlewareHandler {
  return async (c, next) => {
    if (exemptPaths.includes(c.req.path)) return next();
    const header = c.req.header('authorization') ?? '';
    if (header !== `Bearer ${token}`) return c.json({ error: 'unauthorized' }, 401);
    return next();
  };
}
