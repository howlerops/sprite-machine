import { EventEmitter } from 'node:events';
import { mkdir, readFile, readdir, writeFile, appendFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import path from 'node:path';
import {
  type Harness,
  type RunEvent,
  type RunStatus,
  type RunSummary,
  newId,
  nowIso,
} from '@sprite-machine/shared';

export interface PendingPermission {
  requestId: string;
  toolName: string;
  input: unknown;
  resolve: (decision: { allow: boolean; reason?: string }) => void;
}

export interface Run {
  summary: RunSummary;
  events: RunEvent[];
  abortController: AbortController | null;
  pendingPermissions: Map<string, PendingPermission>;
}

export interface CreateRunInput {
  prompt: string;
  harness?: Harness | undefined;
  workdir?: string | undefined;
  model?: string | undefined;
  permissionMode?: 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan' | undefined;
  allowedTools?: string[] | undefined;
  metadata?: Record<string, unknown> | undefined;
}

type StoreEvents = {
  event: (runId: string, event: RunEvent) => void;
  statusChange: (runId: string, status: RunStatus) => void;
};

export class RunStore extends EventEmitter {
  private runs = new Map<string, Run>();

  constructor(private readonly dataDir: string) {
    super();
  }

  override emit<K extends keyof StoreEvents>(
    eventName: K,
    ...args: Parameters<StoreEvents[K]>
  ): boolean {
    return super.emit(eventName, ...args);
  }

  override on<K extends keyof StoreEvents>(eventName: K, listener: StoreEvents[K]): this {
    return super.on(eventName, listener as (...args: unknown[]) => void);
  }

  override off<K extends keyof StoreEvents>(eventName: K, listener: StoreEvents[K]): this {
    return super.off(eventName, listener as (...args: unknown[]) => void);
  }

  async init(): Promise<void> {
    await mkdir(this.runsDir(), { recursive: true });
    if (!existsSync(this.runsDir())) return;
    const entries = await readdir(this.runsDir());
    for (const entry of entries) {
      if (!entry.endsWith('.json')) continue;
      const runId = entry.replace(/\.json$/, '');
      try {
        const summaryRaw = await readFile(path.join(this.runsDir(), entry), 'utf8');
        const summary = JSON.parse(summaryRaw) as RunSummary;
        const events: RunEvent[] = [];
        const eventsPath = this.eventsPath(runId);
        if (existsSync(eventsPath)) {
          const txt = await readFile(eventsPath, 'utf8');
          for (const line of txt.split('\n')) {
            if (!line.trim()) continue;
            try {
              events.push(JSON.parse(line) as RunEvent);
            } catch {
              // skip malformed line
            }
          }
        }
        // On boot, any in-flight runs become idle (the process died, abort lost)
        const wasInFlight = summary.status === 'running' || summary.status === 'starting';
        if (wasInFlight) summary.status = 'idle';
        this.runs.set(runId, {
          summary,
          events,
          abortController: null,
          pendingPermissions: new Map(),
        });
      } catch (err) {
        console.error(`[runs] failed to load ${entry}:`, err);
      }
    }
  }

  async create(input: CreateRunInput): Promise<Run> {
    const id = newId('run');
    const summary: RunSummary = {
      id,
      status: 'queued',
      harness: input.harness ?? 'pi',
      prompt: input.prompt,
      ...(input.workdir !== undefined ? { workdir: input.workdir } : {}),
      createdAt: nowIso(),
      updatedAt: nowIso(),
      totalCostUsd: 0,
      totalInputTokens: 0,
      totalOutputTokens: 0,
    };
    const run: Run = {
      summary,
      events: [],
      abortController: null,
      pendingPermissions: new Map(),
    };
    this.runs.set(id, run);
    await this.persistSummary(run);
    return run;
  }

  get(runId: string): Run | undefined {
    return this.runs.get(runId);
  }

  list(): RunSummary[] {
    return Array.from(this.runs.values()).map((r) => r.summary);
  }

  async setStatus(runId: string, status: RunStatus): Promise<void> {
    const run = this.runs.get(runId);
    if (!run) return;
    if (run.summary.status === status) return;
    run.summary.status = status;
    run.summary.updatedAt = nowIso();
    await this.persistSummary(run);
    this.emit('statusChange', runId, status);
    await this.appendEvent(runId, { type: 'status', status, at: nowIso() });
  }

  async appendEvent(runId: string, event: RunEvent): Promise<void> {
    const run = this.runs.get(runId);
    if (!run) return;
    run.events.push(event);
    if (event.type === 'turn_complete') {
      if (typeof event.costUsd === 'number') run.summary.totalCostUsd += event.costUsd;
      if (typeof event.inputTokens === 'number') run.summary.totalInputTokens += event.inputTokens;
      if (typeof event.outputTokens === 'number')
        run.summary.totalOutputTokens += event.outputTokens;
    }
    if (event.type === 'session_started') run.summary.sessionId = event.sessionId;
    run.summary.updatedAt = nowIso();
    await appendFile(this.eventsPath(runId), JSON.stringify(event) + '\n');
    await this.persistSummary(run);
    this.emit('event', runId, event);
  }

  setAbortController(runId: string, ac: AbortController | null): void {
    const run = this.runs.get(runId);
    if (run) run.abortController = ac;
  }

  registerPendingPermission(runId: string, p: PendingPermission): void {
    const run = this.runs.get(runId);
    if (!run) return;
    run.pendingPermissions.set(p.requestId, p);
  }

  resolvePendingPermission(
    runId: string,
    requestId: string,
    decision: { allow: boolean; reason?: string },
  ): boolean {
    const run = this.runs.get(runId);
    if (!run) return false;
    const p = run.pendingPermissions.get(requestId);
    if (!p) return false;
    p.resolve(decision);
    run.pendingPermissions.delete(requestId);
    return true;
  }

  private async persistSummary(run: Run): Promise<void> {
    await writeFile(
      path.join(this.runsDir(), `${run.summary.id}.json`),
      JSON.stringify(run.summary, null, 2),
    );
  }

  private runsDir(): string {
    return path.join(this.dataDir, 'runs');
  }

  private eventsPath(runId: string): string {
    return path.join(this.runsDir(), `${runId}.events.ndjson`);
  }
}
