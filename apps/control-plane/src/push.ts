import type { RunEvent } from '@sprite-machine/shared';
import type { Storage } from './db/types.js';

export interface PushOpts {
  storage: Storage;
  expoAccessToken?: string;
  fetchImpl?: typeof fetch;
}

interface ExpoPushMessage {
  to: string;
  title: string;
  body: string;
  data?: Record<string, unknown>;
  priority?: 'default' | 'high';
  sound?: 'default' | null;
}

export class PushDispatcher {
  private readonly fetchImpl: typeof fetch;
  constructor(private readonly opts: PushOpts) {
    this.fetchImpl = opts.fetchImpl ?? globalThis.fetch;
  }

  async notifyRunEvent(runId: string, runPrompt: string, event: RunEvent): Promise<void> {
    const tmpl = templateFor(event);
    if (!tmpl) return;
    const tokens = await this.opts.storage.listPushTokens();
    if (tokens.length === 0) {
      console.log(
        `[push] no tokens registered, would send: ${tmpl.title} — ${tmpl.body(runPrompt)}`,
      );
      return;
    }
    const messages: ExpoPushMessage[] = tokens.map((t) => ({
      to: t.expoToken,
      title: tmpl.title,
      body: tmpl.body(runPrompt),
      data: { runId, eventType: event.type },
      priority: tmpl.priority,
      sound: 'default',
    }));
    try {
      const headers: Record<string, string> = {
        'content-type': 'application/json',
        accept: 'application/json',
      };
      if (this.opts.expoAccessToken) {
        headers.authorization = `Bearer ${this.opts.expoAccessToken}`;
      }
      const res = await this.fetchImpl('https://exp.host/--/api/v2/push/send', {
        method: 'POST',
        headers,
        body: JSON.stringify(messages),
      });
      const body = await res.text();
      if (!res.ok) {
        console.warn(`[push] expo HTTP ${res.status}: ${body.slice(0, 200)}`);
      } else {
        // body shape: {"data":[{"status":"ok","id":"<ticket>"} or {"status":"error","message":"...","details":{"error":"DeviceNotRegistered"}}]}
        console.log(
          `[push] expo HTTP 200 (${messages.length} msg) tickets=${body.slice(0, 250)}`,
        );
      }
    } catch (err) {
      console.warn(`[push] send failed:`, err);
    }
  }
}

function templateFor(event: RunEvent): {
  title: string;
  body: (prompt: string) => string;
  priority: 'default' | 'high';
} | null {
  switch (event.type) {
    case 'permission_requested':
      return {
        title: 'Approval needed',
        body: (p) => `${event.toolName}: ${truncate(p, 60)}`,
        priority: 'high',
      };
    case 'completed':
    case 'turn_complete':
      return {
        title: 'Run finished a turn',
        body: (p) => truncate(p, 80),
        priority: 'default',
      };
    case 'error':
      return {
        title: 'Run errored',
        body: () => event.message.slice(0, 120),
        priority: 'high',
      };
    default:
      return null;
  }
}

function truncate(s: string, n: number): string {
  return s.length > n ? s.slice(0, n - 1) + '…' : s;
}
