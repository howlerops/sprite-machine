/**
 * Heuristic auto-title for runs. Takes the first non-empty line of a prompt,
 * strips leading markdown list / heading / quote markers, trims, and truncates
 * to 60 characters. Falls back to "Untitled run" if the result is blank.
 *
 * V1 — pure heuristic, zero deps. V2 (LLM summarize) is out of scope.
 */
const MAX_LEN = 60;
const FALLBACK = 'Untitled run';

// Strips markdown markers like `# `, `## `, `- `, `* `, `> `, `1. `, `2) `, etc.
const LEADING_MARKER_RE = /^\s*(?:#{1,6}\s+|[-*+>]\s+|\d+[.)]\s+)+/;

export function deriveTitle(prompt: string): string {
  if (!prompt) return FALLBACK;
  const firstLine = prompt
    .split(/\r?\n/)
    .map((l) => l.trim())
    .find((l) => l.length > 0);
  if (!firstLine) return FALLBACK;
  const stripped = firstLine.replace(LEADING_MARKER_RE, '').trim();
  if (!stripped) return FALLBACK;
  if (stripped.length <= MAX_LEN) return stripped;
  return stripped.slice(0, MAX_LEN - 1) + '…';
}
