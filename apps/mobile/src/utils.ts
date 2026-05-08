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

export type StatusColor = {
  bg: string;
  text: string;
  label: string;
};

export function statusColor(status: string): StatusColor {
  switch (status) {
    case 'running':
    case 'starting':
      return { bg: '#DBEAFE', text: '#1D4ED8', label: status };
    case 'awaiting_input':
      return { bg: '#FEF3C7', text: '#D97706', label: 'waiting' };
    case 'completed':
      return { bg: '#D1FAE5', text: '#065F46', label: 'done' };
    case 'failed':
      return { bg: '#FEE2E2', text: '#991B1B', label: 'failed' };
    case 'cancelled':
      return { bg: '#F3F4F6', text: '#6B7280', label: 'cancelled' };
    case 'queued':
    case 'idle':
    default:
      return { bg: '#F3F4F6', text: '#374151', label: status };
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
