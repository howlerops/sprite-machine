/**
 * provision-worker: bring up the production worker sprite end-to-end with
 * multi-harness support (claude / codex / pi).
 *
 *   - get-or-create sprite "sprite-machine" (idempotent)
 *   - upload bundled sidecar + claude-settings + codex-settings + pi-settings
 *     + auth files from .sprite-machine-data/secrets/
 *   - npm install the Anthropic Agent SDK on the sprite (for claude harness)
 *   - install bun (for codex + pi CLIs)
 *   - bun install -g @openai/codex
 *   - bun install -g @oh-my-pi/pi-coding-agent
 *   - install OMC plugins via the preinstalled `claude` CLI (manifest/plugins.json)
 *   - install pi plugins via `pi install <source>` (manifest/pi-plugins.json)
 *   - register a Sprites service that starts the sidecar on boot
 *   - wait for /healthz on the sprite's public URL
 *   - checkpoint and write template.json
 *
 * Idempotency: tries hard to preserve existing agentToken and skip re-installs
 * by passing flags like --skip-plugins.
 *
 * Usage:
 *   pnpm provision-worker [--name sprite-machine]
 *                         [--webhook-url URL]
 *                         [--webhook-token TOKEN]
 *                         [--anthropic-key KEY]
 *                         [--openai-key KEY]
 *                         [--skip-plugins]   # skips OMC + pi plugins
 *                         [--skip-bun]       # skips installing bun + codex/pi CLIs
 *                         [--skip-checkpoint]
 */
import { randomBytes } from 'node:crypto';
import { existsSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { FlySpritesClient } from '@sprite-machine/sprites-client';

interface Args {
  name: string;
  webhookUrl?: string;
  webhookToken?: string;
  anthropicKey?: string;
  openaiKey?: string;
  skipPlugins: boolean;
  skipBun: boolean;
  skipCheckpoint: boolean;
}

interface PluginManifest {
  marketplaces?: Array<{ name: string; git?: string; builtin?: boolean }>;
  plugins: string[];
}

function parseArgs(): Args {
  const args = process.argv.slice(2);
  const out: Args = {
    name: 'sprite-machine',
    skipPlugins: false,
    skipBun: false,
    skipCheckpoint: false,
  };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === '--name' && args[i + 1]) out.name = args[++i]!;
    else if (a === '--webhook-url' && args[i + 1]) out.webhookUrl = args[++i]!;
    else if (a === '--webhook-token' && args[i + 1]) out.webhookToken = args[++i]!;
    else if (a === '--anthropic-key' && args[i + 1]) out.anthropicKey = args[++i]!;
    else if (a === '--openai-key' && args[i + 1]) out.openaiKey = args[++i]!;
    else if (a === '--skip-plugins') out.skipPlugins = true;
    else if (a === '--skip-bun') out.skipBun = true;
    else if (a === '--skip-checkpoint') out.skipCheckpoint = true;
  }
  return out;
}

function workspaceRoot(): string {
  return path.resolve(import.meta.dirname, '..', '..', '..');
}

async function loadEnvFile(envPath: string): Promise<void> {
  if (!existsSync(envPath)) return;
  const txt = await readFile(envPath, 'utf8');
  for (const line of txt.split('\n')) {
    const m = /^\s*([A-Z_][A-Z0-9_]*)\s*=\s*(.*)$/i.exec(line);
    if (!m) continue;
    const k = m[1]!;
    let v = m[2] ?? '';
    if ((v.startsWith('"') && v.endsWith('"')) || (v.startsWith("'") && v.endsWith("'"))) {
      v = v.slice(1, -1);
    }
    if (!(k in process.env)) process.env[k] = v;
  }
}

async function ensureBundle(): Promise<{ bundlePath: string; pkgPath: string }> {
  const root = workspaceRoot();
  const dist = path.join(root, 'packages', 'sprite-agent', 'dist');
  const bundlePath = path.join(dist, 'sprite-agent.mjs');
  const pkgPath = path.join(dist, 'package.json');
  if (!existsSync(bundlePath) || !existsSync(pkgPath)) {
    console.log('  · building sidecar bundle ...');
    await runHere(['pnpm', '--filter', '@sprite-machine/sprite-agent', 'build']);
  }
  return { bundlePath, pkgPath };
}

async function runHere(cmd: string[]): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const child = spawn(cmd[0]!, cmd.slice(1), { stdio: 'inherit', cwd: workspaceRoot() });
    child.on('exit', (code) =>
      code === 0 ? resolve() : reject(new Error(`${cmd.join(' ')} exit ${code}`)),
    );
    child.on('error', reject);
  });
}

async function step<T>(label: string, fn: () => Promise<T>): Promise<T> {
  process.stdout.write(`  · ${label} ... `);
  try {
    const r = await fn();
    console.log('done');
    return r;
  } catch (err) {
    console.log(`failed`);
    throw err;
  }
}

async function preserveAgentToken(workspaceRootPath: string): Promise<string> {
  const tplPath = path.join(workspaceRootPath, '.sprite-machine-data', 'template.json');
  if (existsSync(tplPath)) {
    try {
      const tpl = JSON.parse(await readFile(tplPath, 'utf8')) as { agentToken?: string };
      if (typeof tpl.agentToken === 'string' && tpl.agentToken.length >= 16) return tpl.agentToken;
    } catch {
      // fall through
    }
  }
  return randomBytes(24).toString('hex');
}

async function main(): Promise<void> {
  const root = workspaceRoot();
  await loadEnvFile(path.join(root, '.env'));

  const args = parseArgs();
  const apiToken = process.env.SPRITES_API_TOKEN;
  if (!apiToken) throw new Error('SPRITES_API_TOKEN not set (put it in .env at repo root)');

  console.log(`provision-worker (sprite=${args.name})`);

  // Bundle
  const { bundlePath, pkgPath } = await ensureBundle();
  const bundleBytes = await readFile(bundlePath);
  const pkgBytes = await readFile(pkgPath);

  // Sprite handle
  const sprites = new FlySpritesClient({ apiToken, workerName: args.name });
  let sprite = await step('get-or-create sprite', async () => sprites.create());
  console.log(`    name=${sprite.id} status=${sprite.status} url=${sprite.agentBaseUrl}`);
  if (sprite.status !== 'running') {
    sprite = await step('wake sprite', () => sprites.resume(sprite.id));
  }

  const agentToken = await preserveAgentToken(root);

  await step('mkdir /opt/sprite-agent', async () => {
    const r = await sprites.exec(sprite.id, [
      'sudo',
      'bash',
      '-c',
      'mkdir -p /opt/sprite-agent /var/sprite-agent-data && chown -R sprite:sprite /opt/sprite-agent /var/sprite-agent-data',
    ]);
    if (r.exitCode !== 0) throw new Error(r.stderr || 'mkdir failed');
  });

  await step('upload sidecar bundle', () =>
    sprites.writeFile(sprite.id, '/opt/sprite-agent/sprite-agent.mjs', bundleBytes),
  );
  await step('upload deploy package.json', () =>
    sprites.writeFile(sprite.id, '/opt/sprite-agent/package.json', pkgBytes),
  );
  await step('npm install (Anthropic SDK for claude harness)', async () => {
    const r = await sprites.exec(
      sprite.id,
      ['bash', '-c', 'cd /opt/sprite-agent && npm install --omit=dev --no-audit --no-fund 2>&1'],
      { timeoutMs: 5 * 60_000 },
    );
    if (r.exitCode !== 0)
      throw new Error(`npm install failed: ${r.stdout.slice(-400)} ${r.stderr.slice(-400)}`);
  });

  // Install bun + codex + pi for the codex/pi harnesses
  if (!args.skipBun) {
    await step('install bun', async () => {
      const r = await sprites.exec(
        sprite.id,
        [
          'bash',
          '-lc',
          'command -v bun >/dev/null 2>&1 && echo "bun already installed: $(bun --version)" || curl -fsSL https://bun.sh/install | bash 2>&1',
        ],
        { timeoutMs: 3 * 60_000 },
      );
      if (r.exitCode !== 0) throw new Error(r.stderr.slice(-400) || r.stdout.slice(-400));
    });

    await step('bun install -g @openai/codex', async () => {
      const r = await sprites.exec(
        sprite.id,
        ['bash', '-lc', 'export PATH="$HOME/.bun/bin:$PATH" && bun install -g @openai/codex 2>&1'],
        { timeoutMs: 5 * 60_000 },
      );
      if (r.exitCode !== 0) throw new Error(r.stderr.slice(-400) || r.stdout.slice(-400));
    });

    await step('npm install -g @mariozechner/pi-coding-agent@0.67.1', async () => {
      // Pin to 0.67.1 (the laptop version) so the sprite's pi auth.json format
      // matches the synced one. Newer versions (0.73+) clobber the synced
      // auth.json on first launch with an empty `{}`.
      // Override via env: PI_VERSION=...
      const r = await sprites.exec(
        sprite.id,
        [
          'bash',
          '-lc',
          'npm install -g @mariozechner/pi-coding-agent@${PI_VERSION:-0.67.1} 2>&1 && ln -sf "$(npm prefix -g)/bin/pi" /home/sprite/.local/bin/pi',
        ],
        { timeoutMs: 5 * 60_000 },
      );
      if (r.exitCode !== 0) throw new Error(r.stderr.slice(-400) || r.stdout.slice(-400));
    });
  } else {
    console.log('  · --skip-bun set, skipping bun + codex + pi installs');
  }

  // Sync settings + auth for each harness/tool.
  for (const harness of ['claude', 'codex', 'pi', 'github'] as const) {
    const subdir =
      harness === 'claude'
        ? 'claude-settings'
        : harness === 'github'
          ? 'git-settings'
          : `${harness}-settings`;
    const remoteDir =
      harness === 'claude'
        ? '~/.claude'
        : harness === 'codex'
          ? '~/.codex'
          : harness === 'pi'
            ? '~/.pi/agent'
            : '~';
    const settingsDir = path.join(root, 'manifest', subdir);
    if (existsSync(settingsDir)) {
      await step(`sync ${harness} settings → ${remoteDir}`, () =>
        sprites.uploadDir(sprite.id, settingsDir, remoteDir),
      );
    }
    const secretsDir = path.join(root, '.sprite-machine-data', 'secrets', harness);
    if (existsSync(secretsDir)) {
      const authRemoteDir = harness === 'github' ? '~/.config/gh' : remoteDir;
      await step(`sync ${harness} auth → ${authRemoteDir}`, () =>
        sprites.uploadDir(sprite.id, secretsDir, authRemoteDir),
      );
      if (harness === 'github') {
        await step('configure gh git credentials', async () => {
          const r = await sprites.exec(sprite.id, [
            'bash',
            '-lc',
            'chmod 700 ~/.config/gh && chmod 600 ~/.config/gh/hosts.yml && gh auth setup-git',
          ]);
          if (r.exitCode !== 0) throw new Error(r.stderr || r.stdout || 'gh auth setup-git failed');
        });
      }
    }
  }

  // OMC plugins (claude)
  if (!args.skipPlugins) {
    const claudeManifestPath = path.join(root, 'manifest', 'plugins.json');
    if (existsSync(claudeManifestPath)) {
      const m = JSON.parse(await readFile(claudeManifestPath, 'utf8')) as PluginManifest;
      for (const mp of m.marketplaces ?? []) {
        if (mp.builtin || !mp.git) continue;
        await step(`claude marketplace add ${mp.name}`, async () => {
          const r = await sprites.exec(
            sprite.id,
            ['bash', '-lc', `claude plugin marketplace add ${mp.git} 2>&1 || true`],
            { timeoutMs: 60_000 },
          );
          if (r.exitCode !== 0 && !r.stdout.includes('already')) {
            throw new Error(r.stdout.slice(-400) || r.stderr.slice(-400));
          }
        });
      }
      for (const pluginRef of m.plugins) {
        await step(`claude plugin install ${pluginRef}`, async () => {
          const r = await sprites.exec(
            sprite.id,
            ['bash', '-lc', `claude plugin install ${pluginRef} 2>&1 || true`],
            { timeoutMs: 120_000 },
          );
          if (r.exitCode !== 0 && !r.stdout.includes('already')) {
            throw new Error(r.stdout.slice(-400) || r.stderr.slice(-400));
          }
        });
      }
    }

    const piManifestPath = path.join(root, 'manifest', 'pi-plugins.json');
    if (existsSync(piManifestPath) && !args.skipBun) {
      const m = JSON.parse(await readFile(piManifestPath, 'utf8')) as PluginManifest;
      for (const pluginRef of m.plugins) {
        await step(`pi install ${pluginRef}`, async () => {
          const r = await sprites.exec(
            sprite.id,
            [
              'bash',
              '-lc',
              `export PATH="$HOME/.bun/bin:$PATH" && pi install ${pluginRef} 2>&1 || true`,
            ],
            { timeoutMs: 120_000 },
          );
          if (r.exitCode !== 0 && !r.stdout.includes('already')) {
            throw new Error(r.stdout.slice(-400) || r.stderr.slice(-400));
          }
        });
      }
    }
  } else {
    console.log('  · --skip-plugins set, skipping plugin install');
  }

  // Env + startup script (PATH includes ~/.bun/bin so codex/pi resolve)
  const envFile = [
    `SPRITE_ID=${args.name}`,
    `SPRITE_AGENT_TOKEN=${agentToken}`,
    `PORT=8080`,
    `HOST=0.0.0.0`,
    `SPRITE_AGENT_MODE=auto`,
    `SPRITE_AGENT_DATA_DIR=/var/sprite-agent-data`,
    `SUBPROCESS_HARNESS_TIMEOUT_MS=${process.env.SUBPROCESS_HARNESS_TIMEOUT_MS ?? '600000'}`,
    args.webhookUrl ? `CONTROL_PLANE_WEBHOOK_URL=${args.webhookUrl}` : '',
    args.webhookToken ? `CONTROL_PLANE_WEBHOOK_TOKEN=${args.webhookToken}` : '',
    args.anthropicKey ? `ANTHROPIC_API_KEY=${args.anthropicKey}` : '',
    args.openaiKey ? `OPENAI_API_KEY=${args.openaiKey}` : '',
  ]
    .filter(Boolean)
    .join('\n');

  const startSh = `#!/bin/bash
set -euo pipefail
# Sprites bun globals + the host's claude/codex live in these dirs.
export PATH="/.sprite/languages/bun/bin:$HOME/.bun/bin:$HOME/.local/bin:$PATH"
set -a
source /opt/sprite-agent/env
set +a
mkdir -p "$SPRITE_AGENT_DATA_DIR"
cd /opt/sprite-agent
exec node sprite-agent.mjs
`;

  await step('write env + start.sh', async () => {
    await sprites.writeFile(sprite.id, '/opt/sprite-agent/env', envFile);
    await sprites.writeFile(sprite.id, '/opt/sprite-agent/start.sh', startSh);
    const r = await sprites.exec(sprite.id, ['chmod', '+x', '/opt/sprite-agent/start.sh']);
    if (r.exitCode !== 0) throw new Error(r.stderr);
  });

  await step('register Sprites service: sprite-agent', () =>
    sprites.putService(sprite.id, 'sprite-agent', {
      cmd: '/bin/bash',
      args: ['/opt/sprite-agent/start.sh'],
      http_port: 8080,
    }),
  );

  // stop+start to ensure new env is loaded
  await step('stop+start service (env reload)', async () => {
    try {
      await sprites.stopService(sprite.id, 'sprite-agent');
    } catch {
      // ok if not running
    }
    await new Promise((r) => setTimeout(r, 1000));
    await sprites.startService(sprite.id, 'sprite-agent');
  });

  // healthz
  const baseUrl = sprite.agentBaseUrl!;
  await step(`wait for ${baseUrl}/healthz`, async () => {
    const deadline = Date.now() + 60_000;
    while (Date.now() < deadline) {
      try {
        const r = await fetch(`${baseUrl}/healthz`);
        if (r.ok) return;
      } catch {
        // not yet
      }
      await new Promise((r) => setTimeout(r, 1500));
    }
    throw new Error('healthz never came up within 60s');
  });

  // Checkpoint
  let checkpointId: string | null = null;
  if (!args.skipCheckpoint) {
    const cp = await step('checkpoint sprite', () =>
      sprites.checkpoint(sprite.id, 'worker-provisioned-multi-harness'),
    );
    checkpointId = cp.id;
    console.log(`    checkpoint id: ${cp.id}`);
  }

  // template.json
  const dataDir = process.env.SPRITE_MACHINE_DATA ?? path.join(root, '.sprite-machine-data');
  await mkdir(dataDir, { recursive: true });
  const tplPath = path.join(dataDir, 'template.json');
  await writeFile(
    tplPath,
    JSON.stringify(
      {
        backend: 'fly',
        spriteName: sprite.id,
        spriteUrl: sprite.agentBaseUrl,
        agentToken,
        templateCheckpointId: checkpointId,
        builtAt: new Date().toISOString(),
        harnesses: ['claude', 'codex', 'pi', 'mock'],
      },
      null,
      2,
    ),
  );
  console.log(`  ✓ wrote ${tplPath}`);
  console.log(`\nWorker ready: ${sprite.agentBaseUrl}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
