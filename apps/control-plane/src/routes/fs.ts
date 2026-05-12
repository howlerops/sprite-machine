import { readdirSync } from 'node:fs';
import path from 'node:path';
import { Hono } from 'hono';

export interface FsRoutesDeps {
  fsRoot: string;
}
export interface FsEntry {
  name: string;
  isDir: boolean;
  path: string;
}

function isSafeChild(root: string, target: string): boolean {
  const r = path.resolve(target);
  return r === root || r.startsWith(root + path.sep);
}

export function buildFsRoutes(deps: FsRoutesDeps) {
  const app = new Hono();
  const { fsRoot } = deps;
  app.get('/fs/list', (c) => {
    const rawPath = c.req.query('path') ?? fsRoot;
    if (rawPath.includes('..')) return c.json({ error: 'path must not contain ..' }, 400);
    const resolved = path.resolve(rawPath);
    if (!isSafeChild(fsRoot, resolved))
      return c.json({ error: 'path is outside allowed root' }, 403);
    let dirents;
    try {
      dirents = readdirSync(resolved, { withFileTypes: true });
    } catch (err) {
      return c.json({ error: (err as Error).message }, 400);
    }
    const entries: FsEntry[] = dirents
      .filter((d) => d.isDirectory() || d.isFile())
      .map((d) => ({ name: d.name, isDir: d.isDirectory(), path: path.join(resolved, d.name) }))
      .sort((a, b) => (a.isDir !== b.isDir ? (a.isDir ? -1 : 1) : a.name.localeCompare(b.name)));
    return c.json({ root: fsRoot, path: resolved, entries });
  });
  return app;
}
