import type { RunEvent, SpriteWebhookEvent } from '@sprite-machine/shared';
import type { RunStore } from './runs.js';

export interface WebhookPusherOptions {
  url: string;
  token?: string;
  spriteId: string;
  store: RunStore;
  fetchImpl?: typeof fetch;
  maxRetries?: number;
  baseBackoffMs?: number;
}

export class WebhookPusher {
  private readonly fetchImpl: typeof fetch;
  private readonly maxRetries: number;
  private readonly baseBackoffMs: number;
  private queue: SpriteWebhookEvent[] = [];
  private flushing = false;
  private retryTimer: NodeJS.Timeout | null = null;
  private readonly listener: (runId: string, event: RunEvent) => void;
  private started = false;

  constructor(private readonly opts: WebhookPusherOptions) {
    this.fetchImpl = opts.fetchImpl ?? globalThis.fetch;
    this.maxRetries = opts.maxRetries ?? 3;
    this.baseBackoffMs = opts.baseBackoffMs ?? 250;
    this.listener = (runId, event) => {
      this.queue.push({ spriteId: this.opts.spriteId, runId, event });
      void this.flush();
    };
  }

  start(): void {
    if (this.started) return;
    this.started = true;
    for (const summary of this.opts.store.list()) {
      const run = this.opts.store.get(summary.id);
      if (!run) continue;
      for (const event of run.events) {
        this.queue.push({ spriteId: this.opts.spriteId, runId: summary.id, event });
      }
    }
    this.opts.store.on('event', this.listener);
    void this.flush();
  }

  stop(): void {
    if (!this.started) return;
    this.started = false;
    this.opts.store.off('event', this.listener);
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.retryTimer = null;
  }

  private scheduleRetry(): void {
    if (this.retryTimer || !this.started) return;
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      void this.flush();
    }, this.baseBackoffMs * 2 ** this.maxRetries);
  }

  private async flush(): Promise<void> {
    if (this.flushing) return;
    this.flushing = true;
    try {
      while (this.queue.length > 0) {
        const next = this.queue.shift();
        if (!next) break;
        const delivered = await this.deliver(next);
        if (!delivered) {
          this.queue.unshift(next);
          this.scheduleRetry();
          break;
        }
      }
    } finally {
      this.flushing = false;
    }
  }

  private async deliver(payload: SpriteWebhookEvent): Promise<boolean> {
    let attempt = 0;
    let lastErr: unknown = null;
    while (attempt <= this.maxRetries) {
      try {
        const headers: Record<string, string> = { 'content-type': 'application/json' };
        if (this.opts.token) headers.authorization = `Bearer ${this.opts.token}`;
        const res = await this.fetchImpl(this.opts.url, {
          method: 'POST',
          headers,
          body: JSON.stringify(payload),
        });
        if (res.ok || res.status === 202) return true;
        lastErr = new Error(`status ${res.status}`);
      } catch (err) {
        lastErr = err;
      }
      attempt += 1;
      if (attempt > this.maxRetries) break;
      await new Promise((r) => setTimeout(r, this.baseBackoffMs * 2 ** (attempt - 1)));
    }
    console.warn(
      `[webhook] will retry after ${this.maxRetries} failed attempts for ${payload.runId}/${payload.event.type}:`,
      lastErr,
    );
    return false;
  }
}
