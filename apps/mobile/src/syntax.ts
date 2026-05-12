// Lightweight regex-based syntax highlighter for fenced code blocks.
//
// This is intentionally heuristic — it is NOT a real parser. Known limitations:
//   • Multi-line strings/template literals are not tracked across lines.
//   • JSX embedded inside TypeScript strings is not handled.
//   • Regex literals in JS/TS may be mis-tokenized as punctuation.
//   • Python triple-quoted strings are not supported.
// These limitations are acceptable for a read-only chat display.
//
// Supported languages: ts, tsx, js, jsx, py, sh, bash, json.
// All other language tags (or an empty tag) fall back to plain text (no coloring).

export type SyntaxKind = 'kw' | 'str' | 'num' | 'comment' | 'punct' | 'text';

export interface SyntaxToken {
  kind: SyntaxKind;
  value: string;
}

// ---------------------------------------------------------------------------
// Language rule tables
// Each entry is [kind, regex] tested left-to-right. The FIRST match wins.
// ---------------------------------------------------------------------------

type LangRule = [SyntaxKind, RegExp];

const JS_TS_KEYWORDS =
  /\b(abstract|as|async|await|break|case|catch|class|const|continue|debugger|declare|default|delete|do|else|enum|export|extends|finally|for|from|function|get|if|implements|import|in|instanceof|interface|keyof|let|module|namespace|new|null|of|override|package|private|protected|public|readonly|return|set|static|super|switch|this|throw|try|type|typeof|undefined|var|void|while|with|yield)\b/;

const PY_KEYWORDS =
  /\b(and|as|assert|async|await|break|class|continue|def|del|elif|else|except|finally|for|from|global|if|import|in|is|lambda|None|nonlocal|not|or|pass|raise|return|True|False|try|while|with|yield)\b/;

const SH_KEYWORDS =
  /\b(case|do|done|elif|else|esac|fi|for|function|if|in|local|return|select|then|until|while)\b/;

const JS_TS_RULES: LangRule[] = [
  ['comment', /\/\/[^\n]*/],
  ['comment', /\/\*[\s\S]*?\*\//],
  ['str', /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`/],
  ['num', /\b0x[\da-fA-F]+\b|\b\d+\.?\d*([eE][+-]?\d+)?\b/],
  ['kw', JS_TS_KEYWORDS],
  ['punct', /[{}()[\]<>.,;:!?=+\-*/%&|^~@#]/],
];

const PY_RULES: LangRule[] = [
  ['comment', /#[^\n]*/],
  ['str', /"""[\s\S]*?"""|'''[\s\S]*?'''|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/],
  ['num', /\b0x[\da-fA-F]+\b|\b\d+\.?\d*([eE][+-]?\d+)?\b/],
  ['kw', PY_KEYWORDS],
  ['punct', /[{}()[\]<>.,;:!?=+\-*/%&|^~@#]/],
];

const SH_RULES: LangRule[] = [
  ['comment', /#[^\n]*/],
  ['str', /"(?:[^"\\$]|\\.|\$[^(])*"|'[^']*'/],
  ['num', /\b\d+\b/],
  ['kw', SH_KEYWORDS],
  ['punct', /[{}()[\]<>;|&]/],
];

const JSON_RULES: LangRule[] = [
  ['str', /"(?:[^"\\]|\\.)*"/],
  ['num', /-?\b\d+\.?\d*([eE][+-]?\d+)?\b/],
  ['kw', /\b(true|false|null)\b/],
  ['punct', /[{}()[\]:,]/],
];

const LANG_MAP: Record<string, LangRule[]> = {
  ts: JS_TS_RULES,
  tsx: JS_TS_RULES,
  js: JS_TS_RULES,
  jsx: JS_TS_RULES,
  py: PY_RULES,
  sh: SH_RULES,
  bash: SH_RULES,
  json: JSON_RULES,
};

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

/**
 * Tokenize `code` using heuristic regexes for the given `lang`.
 * Returns an array of {kind, value} tokens suitable for rendering.
 * Falls back to a single plain-text token when `lang` is unknown/empty.
 */
export function tokenizeCode(code: string, lang: string): SyntaxToken[] {
  const rules = LANG_MAP[lang.toLowerCase()];
  if (!rules) {
    // Unknown language — return as plain text, no coloring.
    return [{ kind: 'text', value: code }];
  }

  const tokens: SyntaxToken[] = [];
  let remaining = code;

  outer: while (remaining.length > 0) {
    for (const [kind, re] of rules) {
      // We need to match at the START of remaining. Anchor with sticky or slice.
      const m = re.exec(remaining);
      if (m && m.index === 0) {
        tokens.push({ kind, value: m[0] });
        remaining = remaining.slice(m[0].length);
        continue outer;
      }
      // If the match is ahead, record everything before it as plain text.
      if (m && m.index > 0) {
        tokens.push({ kind: 'text', value: remaining.slice(0, m.index) });
        tokens.push({ kind, value: m[0] });
        remaining = remaining.slice(m.index + m[0].length);
        continue outer;
      }
    }
    // No rule matched anywhere — consume one character as plain text.
    // remaining.length > 0 is guaranteed by the while condition, so [0] is defined.
    tokens.push({ kind: 'text', value: remaining[0]! });
    remaining = remaining.slice(1);
  }

  return tokens;
}
