import { EventEmitter } from 'node:events';
import type { RunEvent } from '@sprite-machine/shared';

export interface SseEnvelope {
  runId: string;
  seq: number;
  event: RunEvent;
}

/**
 * In-memory pub/sub keyed by runId. SSE handlers subscribe per run; webhook
 * intake publishes. Survives only one process — for multi-instance control
 * planes this would be Redis or PG LISTEN/NOTIFY.
 */
export class SseBus extends EventEmitter {
  publish(envelope: SseEnvelope): void {
    this.emit(`run:${envelope.runId}`, envelope);
    this.emit('run:any', envelope);
  }

  subscribe(runId: string, listener: (env: SseEnvelope) => void): () => void {
    const ch = `run:${runId}`;
    this.on(ch, listener);
    return () => this.off(ch, listener);
  }
}
