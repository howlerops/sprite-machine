import type { RunRow } from './api';

export function relativeTime(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

export function truncate(text: string, maxLen: number): string {
  if (text.length <= maxLen) return text;
  return text.slice(0, maxLen - 1) + '…';
}

/**
 * Status badge styling, returned as Tailwind/NativeWind className strings
 * referencing semantic tokens declared in apps/mobile/global.css.
 *
 * Consumers (`(tabs)/index.tsx`, `run/[id].tsx`) compose `bgClass` on the
 * pill background and `textClass` on the label.
 */
export type StatusColor = {
  bgClass: string;
  textClass: string;
  dotClass: string;
  label: string;
};

export function statusColor(status: string): StatusColor {
  switch (status) {
    case 'running':
    case 'starting':
      return {
        bgClass: 'bg-status-running-muted',
        textClass: 'text-status-running-foreground',
        dotClass: 'bg-status-running',
        label: status,
      };
    case 'awaiting_input':
      return {
        bgClass: 'bg-status-warning-muted',
        textClass: 'text-status-warning-foreground',
        dotClass: 'bg-status-warning',
        label: 'waiting',
      };
    case 'completed':
      return {
        bgClass: 'bg-status-success-muted',
        textClass: 'text-status-success-foreground',
        dotClass: 'bg-status-success',
        label: 'done',
      };
    case 'failed':
      return {
        bgClass: 'bg-destructive/10',
        textClass: 'text-destructive',
        dotClass: 'bg-destructive',
        label: 'failed',
      };
    case 'cancelled':
      return { bgClass: 'bg-muted', textClass: 'text-muted-foreground', dotClass: 'bg-muted-foreground', label: 'cancelled' };
    case 'queued':
    case 'idle':
    default:
      return { bgClass: 'bg-muted', textClass: 'text-muted-foreground', dotClass: 'bg-muted-foreground', label: status };
  }
}

export function formatCost(usd: number): string {
  if (usd === 0) return '';
  if (usd < 0.01) return `$${(usd * 100).toFixed(2)}¢`;
  return `$${usd.toFixed(3)}`;
}

export function isActiveRun(run: RunRow): boolean {
  return run.status === 'running' || run.status === 'starting' || run.status === 'queued';
}
