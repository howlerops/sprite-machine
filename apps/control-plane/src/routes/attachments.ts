import { createReadStream, mkdirSync } from 'node:fs';
import { writeFile } from 'node:fs/promises';
import path from 'node:path';
import { Hono } from 'hono';
import { randomUUID } from 'node:crypto';
import type { Storage } from '../db/types.js';

const MAX_FILE_BYTES = 8 * 1024 * 1024; // 8 MB

// Extensions we are willing to serve. Restricting to well-known image types
// prevents storing arbitrary blobs and limits Content-Type surface.
const ALLOWED_MIME_PREFIX = 'image/';

export interface AttachmentsRoutesDeps {
  storage: Storage;
  dataDir: string;
}

export function buildAttachmentsRoutes(deps: AttachmentsRoutesDeps): Hono {
  const app = new Hono();
  const { storage, dataDir } = deps;

  // POST /runs/:id/attachments
  // Accepts multipart/form-data with a `file` field (image/*, max 8 MB).
  // Returns { id, url, contentType, sizeBytes }.
  app.post('/runs/:id/attachments', async (c) => {
    const runId = c.req.param('id');

    // Verify the run exists (ownership is implicit — the route is gated by the
    // same bearerOrCookieOrUnauthorized middleware applied to all /runs/* paths).
    const run = await storage.getRun(runId);
    if (!run) return c.json({ error: 'run not found' }, 404);

    let body: Record<string, File | string>;
    try {
      body = (await c.req.parseBody()) as Record<string, File | string>;
    } catch {
      return c.json({ error: 'failed to parse multipart body' }, 400);
    }

    const file = body['file'];
    if (!file || typeof file === 'string') {
      return c.json({ error: 'missing `file` field in multipart body' }, 400);
    }

    if (!file.type.startsWith(ALLOWED_MIME_PREFIX)) {
      return c.json({ error: `only image/* content types are accepted, got: ${file.type}` }, 415);
    }

    if (file.size > MAX_FILE_BYTES) {
      return c.json({ error: `file exceeds 8 MB limit (got ${file.size} bytes)` }, 413);
    }

    // Derive a safe extension from the MIME type (e.g. image/jpeg → jpg).
    const ext = mimeToExt(file.type);
    const attachmentId = randomUUID();
    const filename = `${attachmentId}.${ext}`;
    const attachmentDir = path.join(dataDir, 'attachments', runId);
    const filePath = path.join(attachmentDir, filename);

    mkdirSync(attachmentDir, { recursive: true });
    const buffer = Buffer.from(await file.arrayBuffer());
    await writeFile(filePath, buffer);

    const row = await storage.createAttachment({
      id: attachmentId,
      runId,
      contentType: file.type,
      sizeBytes: file.size,
      path: filePath,
    });

    const url = `/runs/${runId}/attachments/${attachmentId}`;
    return c.json({ id: row.id, url, contentType: row.contentType, sizeBytes: row.sizeBytes }, 201);
  });

  // GET /runs/:id/attachments/:attachmentId
  // Streams the stored file with the correct Content-Type.
  app.get('/runs/:id/attachments/:attachmentId', async (c) => {
    const runId = c.req.param('id');
    const attachmentId = c.req.param('attachmentId');

    const row = await storage.getAttachment(runId, attachmentId);
    if (!row) return c.json({ error: 'attachment not found' }, 404);

    // Stream from disk using Node's createReadStream. The @hono/node-server
    // adapter accepts a ReadableStream (web-standard), so we pipe through it.
    const nodeStream = createReadStream(row.path);
    const webStream = new ReadableStream({
      start(controller) {
        nodeStream.on('data', (chunk) => controller.enqueue(chunk));
        nodeStream.on('end', () => controller.close());
        nodeStream.on('error', (err) => controller.error(err));
      },
      cancel() {
        nodeStream.destroy();
      },
    });

    return new Response(webStream, {
      status: 200,
      headers: {
        'Content-Type': row.contentType,
        'Content-Length': String(row.sizeBytes),
        'Cache-Control': 'private, max-age=86400',
      },
    });
  });

  return app;
}

// Map common image MIME types to file extensions.
function mimeToExt(mime: string): string {
  const map: Record<string, string> = {
    'image/jpeg': 'jpg',
    'image/jpg': 'jpg',
    'image/png': 'png',
    'image/gif': 'gif',
    'image/webp': 'webp',
    'image/svg+xml': 'svg',
    'image/bmp': 'bmp',
    'image/tiff': 'tiff',
    'image/heic': 'heic',
    'image/heif': 'heif',
    'image/avif': 'avif',
  };
  return map[mime] ?? 'bin';
}
