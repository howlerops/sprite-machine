/**
 * sync-settings: snapshot the user's local harness configs into the repo.
 *
 *   - claude: ~/.claude          → manifest/claude-settings/   (committed; sanitized)
 *   - codex:  ~/.codex           → manifest/codex-settings/    (committed; sanitized)
 *   - pi:     ~/.pi/agent        → manifest/pi-settings/       (committed; sanitized)
 *
 * Auth files (containing tokens) are NOT committed. They go to
 *   .sprite-machine-data/secrets/<harness>/   (gitignored)
 * which `provision-worker` then uploads to the sprite alongside the manifest.
 *
 * Usage:
 *   pnpm sync-settings [--harness claude|codex|pi|all] [--source <path>] [--include-auth]
 *
 * Without --include-auth, auth.json files are skipped (you'd authenticate
 * directly on the sprite with `claude /login`, `codex login`, `pi`-equivalent).
 */
import { homedir } from 'node:os';
import path from 'node:path';
import { existsSync, statSync } from 'node:fs';
import { copyFile, cp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';

interface Args {
  harness: 'claude' | 'codex' | 'pi' | 'all';
  source?: string;
  includeAuth: boolean;
}

interface HarnessSpec {
  name: 'claude' | 'codex' | 'pi';
  defaultSource: string;
  manifestSubdir: string;
  /** files copied verbatim (unsensitive) */
  copyAsIs: string[];
  /** json files passed through secret-stripper */
  copyJsonSanitized: string[];
  /** dirs copied recursively */
  copyDirs: string[];
  /** auth-bearing files routed to .sprite-machine-data/secrets/<name>/ */
  authFiles: string[];
}

const HARNESSES: HarnessSpec[] = [
  {
    name: 'claude',
    defaultSource: path.join(homedir(), '.claude'),
    manifestSubdir: 'claude-settings',
    copyAsIs: ['CLAUDE.md', 'AGENTS.md'],
    copyJsonSanitized: ['settings.json'],
    copyDirs: [],
    authFiles: ['credentials.json', '.credentials.json'],
  },
  {
    name: 'codex',
    defaultSource: path.join(homedir(), '.codex'),
    manifestSubdir: 'codex-settings',
    copyAsIs: ['AGENTS.md', 'config.toml'],
    copyJsonSanitized: [],
    copyDirs: ['agents'],
    authFiles: ['auth.json'],
  },
  {
    name: 'pi',
    defaultSource: path.join(homedir(), '.pi', 'agent'),
    manifestSubdir: 'pi-settings',
    copyAsIs: [],
    copyJsonSanitized: ['settings.json'],
    copyDirs: [],
    authFiles: ['auth.json'],
  },
];

function parseArgs(): Args {
  const args = process.argv.slice(2);
  const out: Args = { harness: 'all', includeAuth: true };
  for (let i = 0; i < args.length; i++) {
    if (args[i] === '--harness' && args[i + 1]) out.harness = args[++i] as Args['harness'];
    else if (args[i] === '--source' && args[i + 1]) out.source = args[++i]!;
    else if (args[i] === '--include-auth') out.includeAuth = true;
    else if (args[i] === '--no-auth') out.includeAuth = false;
  }
  return out;
}

function workspaceRoot(): string {
  return path.resolve(import.meta.dirname, '..', '..', '..');
}

const SECRET_KEYS = new Set(['credentials', 'apiKey', 'oauthToken', 'token', 'access_token']);

function stripSecrets(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(stripSecrets);
  if (value && typeof value === 'object') {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (SECRET_KEYS.has(k)) continue;
      out[k] = stripSecrets(v);
    }
    return out;
  }
  return value;
}

async function syncOne(spec: HarnessSpec, source: string, includeAuth: boolean): Promise<void> {
  if (!existsSync(source)) {
    console.log(`  · ${spec.name}: source not found at ${source}, skipping`);
    return;
  }
  const root = workspaceRoot();
  const manifestDir = path.join(root, 'manifest', spec.manifestSubdir);
  const secretsDir = path.join(root, '.sprite-machine-data', 'secrets', spec.name);

  await rm(manifestDir, { recursive: true, force: true });
  await mkdir(manifestDir, { recursive: true });

  let copied = 0;
  for (const f of spec.copyAsIs) {
    const src = path.join(source, f);
    if (!existsSync(src)) continue;
    await copyFile(src, path.join(manifestDir, f));
    copied += 1;
  }
  for (const f of spec.copyJsonSanitized) {
    const src = path.join(source, f);
    if (!existsSync(src)) continue;
    const raw = await readFile(src, 'utf8');
    try {
      const sanitized = stripSecrets(JSON.parse(raw));
      await writeFile(path.join(manifestDir, f), JSON.stringify(sanitized, null, 2));
    } catch {
      await copyFile(src, path.join(manifestDir, f));
    }
    copied += 1;
  }
  for (const d of spec.copyDirs) {
    const src = path.join(source, d);
    if (!existsSync(src) || !statSync(src).isDirectory()) continue;
    await cp(src, path.join(manifestDir, d), { recursive: true });
    copied += 1;
  }

  if (includeAuth) {
    await rm(secretsDir, { recursive: true, force: true });
    let authCopied = 0;
    for (const f of spec.authFiles) {
      const src = path.join(source, f);
      if (!existsSync(src)) continue;
      if (authCopied === 0) await mkdir(secretsDir, { recursive: true });
      await copyFile(src, path.join(secretsDir, f));
      authCopied += 1;
    }
    if (authCopied > 0) {
      console.log(
        `  · ${spec.name}: ${copied} setting file(s) → manifest/${spec.manifestSubdir}/, ${authCopied} auth file(s) → .sprite-machine-data/secrets/${spec.name}/`,
      );
    } else {
      console.log(`  · ${spec.name}: ${copied} setting file(s) → manifest/${spec.manifestSubdir}/`);
    }
  } else {
    console.log(`  · ${spec.name}: ${copied} setting file(s) → manifest/${spec.manifestSubdir}/`);
  }

  await writeFile(
    path.join(manifestDir, '.gitkeep'),
    `# generated by apps/cli/src/sync-settings.ts at ${new Date().toISOString()}\n`,
  );
}

async function main(): Promise<void> {
  const args = parseArgs();
  console.log(`sync-settings (harness=${args.harness}, includeAuth=${args.includeAuth})`);
  for (const spec of HARNESSES) {
    if (args.harness !== 'all' && args.harness !== spec.name) continue;
    const source = args.source && args.harness === spec.name ? args.source : spec.defaultSource;
    await syncOne(spec, source, args.includeAuth);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
