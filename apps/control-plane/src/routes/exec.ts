/**
 * WebSocket exec proxy.
 *
 * Path: `GET /sprites/:id/exec` (Upgrade: websocket)
 *
 * Auth (BEFORE upgrade): Bearer USER_TOKEN header OR signed sm_session cookie.
 * On auth fail we respond 401 + close the socket; the browser never sees the
 * upstream sprites.dev endpoint.
 *
 * Frame protocol mirrors api.sprites.dev's exec endpoint
 * (binary frames: byte0 = stream id; 0=stdin, 1=stdout, 2=stderr, 3=exit, 4=eof).
 * We just pipe bytes both ways.
 *
 * Close codes (custom, 4000-4999):
 *   4001 — sprite suspended; client should resume first
 *   4002 — sprite has no agentBaseUrl yet (cold/never-started)
 *   4003 — upstream WS error
 */
import type { IncomingMessage } from 'node:http';
import type { Duplex } from 'node:stream';
import { WebSocketServer, WebSocket as WsWebSocket } from 'ws';
import type { Storage } from '../db/types.js';
import { readSessionCookieFromHeader, verifyBearer } from '../ws-auth.js';

export interface ExecProxyDeps {
  storage: Storage;
  spritesApiToken: string | null;
  spritesApiBase: string; // e.g. https://api.sprites.dev
  userToken: string;
  sessionSecret: string;
}

const PATH_RE = /^\/sprites\/([^/]+)\/exec(?:\?.*)?$/;

// Backpressure caps for the pending[] buffer that holds client frames received
// before upstream is open. Either bound triggers a 4003 close.
const PENDING_MAX_FRAMES = 64;
const PENDING_MAX_BYTES = 256 * 1024;

interface ExecPair {
  client: WsWebSocket;
  upstream: WsWebSocket;
  pingTimer: NodeJS.Timeout;
}

export function attachExecProxy(deps: ExecProxyDeps): {
  handleUpgrade: (req: IncomingMessage, socket: Duplex, head: Buffer) => Promise<boolean>;
  close: () => void;
} {
  const wss = new WebSocketServer({ noServer: true });
  // Track active client/upstream pairs so SIGTERM/shutdown can force-close
  // them with code 1001. wss.close() on its own only stops accepting new
  // connections — existing pairs would otherwise keep their ping timers and
  // sockets open until the OS tears the process down.
  const activePairs = new Set<ExecPair>();

  async function handleUpgrade(
    req: IncomingMessage,
    socket: Duplex,
    head: Buffer,
  ): Promise<boolean> {
    const url = req.url ?? '';
    const match = PATH_RE.exec(url);
    if (!match) return false;
    const spriteId = decodeURIComponent(match[1] ?? '');

    // Auth BEFORE upgrade. We respond with raw HTTP and destroy the socket on
    // failure so the client never gets a successful WS handshake.
    const ok =
      verifyBearer(req.headers['authorization'], deps.userToken) ||
      !!readSessionCookieFromHeader(req.headers['cookie'], deps.sessionSecret);
    if (!ok) {
      reject(socket, 401, 'Unauthorized');
      return true;
    }

    if (!deps.spritesApiToken) {
      reject(socket, 503, 'Sprites backend not configured');
      return true;
    }

    const sprite = await deps.storage.getSprite(spriteId).catch(() => null);
    if (!sprite) {
      reject(socket, 404, 'Sprite not found');
      return true;
    }
    if (sprite.status === 'suspended') {
      // Upgrade and immediately close with our typed code so the client can
      // surface a "Resume sprite" UI.
      wss.handleUpgrade(req, socket, head, (client) => {
        client.close(4001, 'sprite suspended');
      });
      return true;
    }
    if (!sprite.agentBaseUrl) {
      wss.handleUpgrade(req, socket, head, (client) => {
        client.close(4002, 'sprite has no agentBaseUrl');
      });
      return true;
    }

    // Build upstream URL. We're proxying to api.sprites.dev's exec endpoint,
    // forwarding the query string the client supplied (cmd, env, tty, etc.).
    const wsBase = deps.spritesApiBase.replace(/^http/, 'ws').replace(/\/$/, '');
    const qIdx = url.indexOf('?');
    const query = qIdx >= 0 ? url.slice(qIdx) : '';
    const upstreamUrl = `${wsBase}/v1/sprites/${encodeURIComponent(spriteId)}/exec${query}`;

    wss.handleUpgrade(req, socket, head, (client) => {
      pipe(client, upstreamUrl, deps.spritesApiToken!, activePairs);
    });
    return true;
  }

  return {
    handleUpgrade,
    close: () => {
      // Force-close all active pairs (1001 going-away) and stop the WS server.
      for (const pair of activePairs) {
        clearInterval(pair.pingTimer);
        try {
          if (
            pair.client.readyState === WsWebSocket.OPEN ||
            pair.client.readyState === WsWebSocket.CONNECTING
          ) {
            pair.client.close(1001, 'server_shutdown');
          }
        } catch {
          /* swallow — best-effort during shutdown */
        }
        try {
          if (
            pair.upstream.readyState === WsWebSocket.OPEN ||
            pair.upstream.readyState === WsWebSocket.CONNECTING
          ) {
            pair.upstream.close(1001, 'server_shutdown');
          }
        } catch {
          /* swallow */
        }
      }
      activePairs.clear();
      wss.close();
    },
  };
}

function reject(socket: Duplex, status: number, reason: string): void {
  socket.write(
    `HTTP/1.1 ${status} ${reason}\r\n` + 'Connection: close\r\n' + 'Content-Length: 0\r\n' + '\r\n',
  );
  socket.destroy();
}

function pipe(
  client: WsWebSocket,
  upstreamUrl: string,
  apiToken: string,
  activePairs: Set<ExecPair>,
): void {
  let upstream: WsWebSocket;
  try {
    upstream = new WsWebSocket(upstreamUrl, {
      headers: { Authorization: `Bearer ${apiToken}` },
    });
  } catch (err) {
    console.error('[exec-proxy] upstream connect failed:', err);
    client.close(4003, 'upstream_error');
    return;
  }

  // 30s ping to both sides.
  const pingTimer = setInterval(() => {
    if (client.readyState === WsWebSocket.OPEN) client.ping();
    if (upstream.readyState === WsWebSocket.OPEN) upstream.ping();
  }, 30_000);

  const pair: ExecPair = { client, upstream, pingTimer };
  activePairs.add(pair);

  let upstreamOpen = false;
  // Buffer client frames that arrive before upstream is open. Cap at
  // PENDING_MAX_FRAMES OR PENDING_MAX_BYTES — whichever is hit first triggers
  // a 4003 close so a slow/never-opening upstream can't OOM the proxy.
  const pending: Array<{ data: WsWebSocket.RawData; binary: boolean }> = [];
  let pendingBytes = 0;

  function frameSize(data: WsWebSocket.RawData): number {
    if (typeof data === 'string') return Buffer.byteLength(data, 'utf8');
    if (Buffer.isBuffer(data)) return data.length;
    if (Array.isArray(data)) return data.reduce((n, b) => n + b.length, 0);
    if (data instanceof ArrayBuffer) return data.byteLength;
    return 0;
  }

  upstream.on('open', () => {
    upstreamOpen = true;
    for (const m of pending) {
      upstream.send(m.data, { binary: m.binary });
    }
    pending.length = 0;
    pendingBytes = 0;
  });

  upstream.on('message', (data, isBinary) => {
    if (client.readyState === WsWebSocket.OPEN) {
      client.send(data, { binary: isBinary });
    }
  });

  upstream.on('close', (code, reason) => {
    cleanup();
    if (client.readyState === WsWebSocket.OPEN || client.readyState === WsWebSocket.CONNECTING) {
      // Pass through close code if it's in user range; otherwise normal.
      const safeCode = code >= 1000 && code <= 4999 ? code : 1011;
      client.close(safeCode, reason.toString().slice(0, 120));
    }
  });

  upstream.on('error', (err) => {
    // Log the URL (without query/token) so we can correlate failures.
    const sanitizedUrl = upstreamUrl.split('?')[0];
    console.error(
      `[exec-proxy] upstream error: ${err.message} url=${sanitizedUrl} upstreamState=${upstream.readyState}`,
    );
    if (client.readyState === WsWebSocket.OPEN) {
      // Pass a slightly more specific close reason so the Shell UI can hint.
      const reason = err.message.includes('400')
        ? 'sprite_unavailable'
        : err.message.includes('401') || err.message.includes('403')
          ? 'sprite_unauthorized'
          : 'upstream_error';
      client.close(4003, reason);
    }
  });

  client.on('message', (data, isBinary) => {
    if (!upstreamOpen) {
      const size = frameSize(data);
      if (pending.length >= PENDING_MAX_FRAMES || pendingBytes + size > PENDING_MAX_BYTES) {
        client.close(4003, 'backpressure');
        return;
      }
      pending.push({ data, binary: isBinary });
      pendingBytes += size;
      return;
    }
    upstream.send(data, { binary: isBinary });
  });

  client.on('close', () => {
    cleanup();
    if (
      upstream.readyState === WsWebSocket.OPEN ||
      upstream.readyState === WsWebSocket.CONNECTING
    ) {
      upstream.close();
    }
  });

  client.on('error', (err) => {
    console.error('[exec-proxy] client error:', err.message);
  });

  function cleanup(): void {
    clearInterval(pingTimer);
    activePairs.delete(pair);
  }
}
