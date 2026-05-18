import { spawn, type ChildProcess } from 'node:child_process';
import { nowIso, type RunEvent } from '@sprite-machine/shared';
import type { CreateRunInput, RunStore } from '../runs.js';
import type { Agent, AgentDeps } from './types.js';

/**
 * Base class for harnesses driven by a CLI that emits JSONL on stdout.
 * Subclasses implement: argv-construction, JSON line → RunEvent translation,
 * session-id capture, and resume args.
 */
export abstract class JsonlSubprocessAgent implements Agent {
  protected readonly store: RunStore;
  protected children = new Map<string, ChildProcess>();
  protected cancelled = new Set<string>();

  constructor(deps: AgentDeps) {
    this.store = deps.store;
  }

  abstract readonly harnessName: string;
  protected abstract buildArgs(
    prompt: string,
    opts: SpawnOpts,
  ): { cmd: string; args: string[]; env?: Record<string, string> };
  protected abstract translateLine(line: object, ctx: TranslationCtx): RunEvent[];

  async start(runId: string, input: CreateRunInput): Promise<void> {
    await this.runOnce(runId, input.prompt, {
      ...(input.workdir !== undefined ? { workdir: input.workdir } : {}),
      ...(input.model !== undefined ? { model: input.model } : {}),
      ...(input.permissionMode !== undefined ? { permissionMode: input.permissionMode } : {}),
      ...(input.allowedTools !== undefined ? { allowedTools: input.allowedTools } : {}),
    });
  }

  async sendUserMessage(runId: string, text: string): Promise<void> {
    const run = this.store.get(runId);
    if (!run) throw new Error('run not found');
    if (run.summary.status === 'running' || run.summary.status === 'starting') {
      throw new Error('run is busy; cancel first or wait for current turn to finish');
    }
    await this.store.appendEvent(runId, { type: 'user_message', text, at: nowIso() });
    await this.runOnce(runId, text, {
      ...(run.summary.workdir !== undefined ? { workdir: run.summary.workdir } : {}),
      ...(run.summary.sessionId !== undefined ? { resumeSessionId: run.summary.sessionId } : {}),
    });
  }

  async cancel(runId: string): Promise<void> {
    this.cancelled.add(runId);
    const child = this.children.get(runId);
    if (child) {
      try {
        if (child.pid) process.kill(-child.pid, 'SIGTERM');
        else child.kill('SIGTERM');
      } catch {
        try {
          child.kill('SIGTERM');
        } catch {
          // ignore
        }
      }
    }
    await this.store.setStatus(runId, 'cancelled');
  }

  private async runOnce(runId: string, prompt: string, opts: SpawnOpts): Promise<void> {
    await this.store.setStatus(runId, 'starting');

    const ctx: TranslationCtx = { capturedSessionId: null };

    const { cmd, args, env } = this.buildArgs(prompt, opts);

    const timeoutMs = Number(process.env.SUBPROCESS_HARNESS_TIMEOUT_MS ?? 5 * 60_000);

    return await new Promise((resolve) => {
      const child = spawn(cmd, args, {
        env: { ...process.env, ...(env ?? {}) },
        stdio: ['pipe', 'pipe', 'pipe'],
        cwd: opts.workdir,
        detached: true,
      });
      this.children.set(runId, child);

      let stdoutBuf = '';
      let stderrBuf = '';
      let started = false;
      let timedOut = false;
      let timeoutReason: 'startup' | 'overall' = 'overall';
      let sawOutput = false;
      let sawErrorEvent = false;

      const killForTimeout = (reason: 'startup' | 'overall') => {
        timedOut = true;
        timeoutReason = reason;
        try {
          if (child.pid) process.kill(-child.pid, 'SIGTERM');
          else child.kill('SIGTERM');
        } catch {
          try {
            child.kill('SIGTERM');
          } catch {
            // ignore
          }
        }
      };

      const timer = setTimeout(() => killForTimeout('overall'), timeoutMs);
      const startupTimeoutMs = Number(process.env.SUBPROCESS_HARNESS_STARTUP_TIMEOUT_MS ?? 45_000);
      const startupTimer = setTimeout(
        () => {
          if (!sawOutput) killForTimeout('startup');
        },
        Math.min(startupTimeoutMs, timeoutMs),
      );

      void this.store.setStatus(runId, 'running').then(() => {
        started = true;
      });

      const flushLines = async (): Promise<void> => {
        const lines = stdoutBuf.split('\n');
        stdoutBuf = lines.pop() ?? '';
        for (const line of lines) {
          if (!line.trim()) continue;
          let parsed: object;
          try {
            parsed = JSON.parse(line);
          } catch {
            continue;
          }
          for (const ev of this.translateLine(parsed, ctx)) {
            if (ev.type === 'session_started') {
              ctx.capturedSessionId = ev.sessionId;
            }
            if (ev.type === 'error') sawErrorEvent = true;
            await this.store.appendEvent(runId, ev);
          }
        }
      };

      child.stdout.on('data', (chunk: Buffer) => {
        sawOutput = true;
        clearTimeout(startupTimer);
        stdoutBuf += chunk.toString();
        void flushLines();
      });

      child.stderr.on('data', (chunk: Buffer) => {
        sawOutput = true;
        clearTimeout(startupTimer);
        stderrBuf += chunk.toString();
      });

      child.on('error', async (err) => {
        await this.store.appendEvent(runId, {
          type: 'error',
          message: `${this.harnessName} subprocess error: ${err.message}`,
          at: nowIso(),
        });
        await this.store.setStatus(runId, 'failed');
        this.children.delete(runId);
        resolve();
      });

      child.on('exit', async (code, signal) => {
        clearTimeout(timer);
        clearTimeout(startupTimer);
        // If cancellation was requested, don't emit follow-on error/status events
        // that would override the 'cancelled' status the cancel() method already set.
        if (this.cancelled.has(runId)) {
          this.cancelled.delete(runId);
          this.children.delete(runId);
          return resolve();
        }
        await flushLines();
        if (timedOut) {
          const seconds = Math.round(
            (timeoutReason === 'startup' ? Math.min(startupTimeoutMs, timeoutMs) : timeoutMs) /
              1000,
          );
          await this.store.appendEvent(runId, {
            type: 'error',
            message:
              timeoutReason === 'startup'
                ? `${this.harnessName} did not produce any output after ${seconds}s. The CLI may be waiting on auth or a startup lock. Check the integration/auth settings, then try again.`
                : `${this.harnessName} stopped responding after ${seconds}s. This is usually a CLI/session hang, not a problem with your prompt. Try sending the message again, or choose another harness.`,
            at: nowIso(),
          });
          // Keep the chat recoverable instead of leaving it in a failed terminal state.
          await this.store.setStatus(runId, 'awaiting_input');
          this.children.delete(runId);
          return resolve();
        }
        if (signal === 'SIGTERM' && !timedOut) {
          // External SIGTERM (not cancel(), not timeout) — treat as cancelled
          await this.store.setStatus(runId, 'cancelled');
          this.children.delete(runId);
          return resolve();
        }
        if (code !== 0 && code !== null) {
          await this.store.appendEvent(runId, {
            type: 'error',
            message: `${this.harnessName} exit ${code}: ${stderrBuf.slice(-400) || stdoutBuf.slice(-400)}`,
            at: nowIso(),
          });
          await this.store.setStatus(runId, 'failed');
          this.children.delete(runId);
          return resolve();
        }
        const finalStatus = this.store.get(runId)?.summary.status;
        if (finalStatus !== 'failed' && finalStatus !== 'cancelled') {
          await this.store.setStatus(runId, sawErrorEvent ? 'awaiting_input' : 'idle');
        }
        if (started === false) {
          // ensure 'running' was set even for very fast runs
        }
        this.children.delete(runId);
        resolve();
      });

      // Send prompt via stdin if argv didn't include it
      if (opts.promptViaStdin) {
        child.stdin.end(prompt);
      } else {
        child.stdin.end();
      }
    });
  }
}

export interface SpawnOpts {
  workdir?: string;
  model?: string;
  resumeSessionId?: string;
  permissionMode?: 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan';
  allowedTools?: string[];
  promptViaStdin?: boolean;
}

export interface TranslationCtx {
  capturedSessionId: string | null;
}
