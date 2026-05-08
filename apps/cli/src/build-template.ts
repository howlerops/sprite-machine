/**
 * build-template: create a sprite, provision Claude Code + OMC plugins from
 * manifest/, sanity-check the sidecar, then checkpoint. Outputs the
 * TEMPLATE_CHECKPOINT_ID to .sprite-machine-data/template.json.
 *
 * Usage:
 *   pnpm --filter @sprite-machine/cli build-template [--backend local|fly]
 *                                                    [--keep] [--mode mock|sdk|auto]
 *
 * Local backend: produces a real working sprite (child process running the
 * sidecar) that the control plane can talk to. Skips heavy plugin installs;
 * relies on host's Claude Code being on PATH.
 *
 * Fly backend: not yet implemented (FlySpritesClient TODOs). Enabled when
 * SPRITES_API_TOKEN is set and --backend fly is passed.
 */
import { existsSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import {
  createSpritesClient,
  type SpriteHandle,
  type SpritesClient,
} from '@sprite-machine/sprites-client';

interface Args {
  backend: 'local' | 'fly';
  keep: boolean;
  mode: 'mock' | 'sdk' | 'auto';
}

interface PluginManifest {
  marketplaces: Array<{ name: string; git?: string; builtin?: boolean }>;
  plugins: string[];
}

function parseArgs(): Args {
  const args = process.argv.slice(2);
  const out: Args = { backend: 'local', keep: false, mode: 'auto' };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === '--backend' && args[i + 1]) out.backend = args[++i] as Args['backend'];
    else if (a === '--keep') out.keep = true;
    else if (a === '--mode' && args[i + 1]) out.mode = args[++i] as Args['mode'];
  }
  return out;
}

function workspaceRoot(): string {
  return path.resolve(import.meta.dirname, '..', '..', '..');
}

async function provision(client: SpritesClient, sprite: SpriteHandle, args: Args): Promise<void> {
  const root = workspaceRoot();
  const manifestPath = path.join(root, 'manifest', 'plugins.json');
  if (!existsSync(manifestPath)) {
    throw new Error(`missing manifest: ${manifestPath}`);
  }
  const manifest = JSON.parse(await readFile(manifestPath, 'utf8')) as PluginManifest;

  const settingsDir = path.join(root, 'manifest', 'claude-settings');
  if (existsSync(settingsDir)) {
    await client.uploadDir(sprite.id, settingsDir, '~/.claude');
    console.log(`  ✓ uploaded claude-settings (${settingsDir} → ~/.claude)`);
  } else {
    console.log('  ⚠ no manifest/claude-settings/ — run `pnpm sync-settings` first');
  }

  const onSprite = async (label: string, fn: () => Promise<void>): Promise<void> => {
    process.stdout.write(`  · ${label} ... `);
    try {
      await fn();
      console.log('done');
    } catch (err) {
      console.log(`failed: ${(err as Error).message}`);
      throw err;
    }
  };

  if (args.backend === 'local') {
    // Local mode: nothing to install on the "sprite". Plugin install is a
    // no-op since the child process inherits the host's PATH and Claude
    // Code installation. The point is to exercise the workflow.
    console.log(`  · skipping plugin install (local backend uses host's claude)`);
  } else {
    await onSprite('install nodejs + pnpm', async () => {
      const r = await client.exec(sprite.id, [
        'bash',
        '-lc',
        'curl -fsSL https://nodejs.org/dist/v22.13.0/node-v22.13.0-linux-x64.tar.xz -o /tmp/n.txz && mkdir -p /opt/node && tar -xJf /tmp/n.txz -C /opt/node --strip-components=1 && /opt/node/bin/npm i -g pnpm',
      ]);
      if (r.exitCode !== 0) throw new Error(r.stderr || 'install failed');
    });
    await onSprite('install @anthropic-ai/claude-code', async () => {
      const r = await client.exec(sprite.id, [
        '/opt/node/bin/npm',
        'i',
        '-g',
        '@anthropic-ai/claude-code',
      ]);
      if (r.exitCode !== 0) throw new Error(r.stderr);
    });
    for (const mp of manifest.marketplaces) {
      if (mp.builtin || !mp.git) continue;
      await onSprite(`add marketplace ${mp.name}`, async () => {
        const r = await client.exec(sprite.id, ['claude', 'plugin', 'marketplace', 'add', mp.git!]);
        if (r.exitCode !== 0) throw new Error(r.stderr);
      });
    }
    for (const pluginRef of manifest.plugins) {
      await onSprite(`install plugin ${pluginRef}`, async () => {
        const r = await client.exec(sprite.id, ['claude', 'plugin', 'install', pluginRef]);
        if (r.exitCode !== 0) throw new Error(r.stderr);
      });
    }
    console.log(`  · TODO: bundle + upload sprite-agent itself + systemd unit`);
  }

  // sanity-check sidecar
  process.stdout.write('  · sidecar /healthz ... ');
  if (!sprite.agentBaseUrl) {
    console.log('skipped (no agentBaseUrl yet)');
  } else {
    const res = await fetch(`${sprite.agentBaseUrl}/healthz`);
    if (!res.ok) throw new Error(`/healthz returned ${res.status}`);
    console.log('OK');
  }
}

async function main(): Promise<void> {
  const args = parseArgs();
  const root = workspaceRoot();
  const dataDir = process.env.SPRITE_MACHINE_DATA ?? path.join(root, '.sprite-machine-data');
  await mkdir(dataDir, { recursive: true });

  console.log(`build-template (backend=${args.backend}, mode=${args.mode})`);
  console.log(`  data dir: ${dataDir}`);

  const client = createSpritesClient({
    backend: args.backend,
    workspaceRoot: root,
    local: { sidecarMode: args.mode },
  });

  console.log('  · creating sprite ...');
  const sprite = await client.create({ name: 'template-builder' });
  console.log(`    sprite: ${sprite.id} (${sprite.status}) ${sprite.agentBaseUrl ?? ''}`);

  try {
    await provision(client, sprite, args);

    console.log('  · checkpointing ...');
    const checkpoint = await client.checkpoint(sprite.id, 'template-v1');
    console.log(`    checkpoint: ${checkpoint.id}`);

    const out = path.join(dataDir, 'template.json');
    await writeFile(
      out,
      JSON.stringify(
        {
          templateCheckpointId: checkpoint.id,
          builtAt: checkpoint.createdAt,
          backend: args.backend,
        },
        null,
        2,
      ),
    );
    console.log(`  ✓ wrote ${out}`);
  } finally {
    if (!args.keep) {
      console.log('  · destroying builder sprite ...');
      await client.destroy(sprite.id);
    } else {
      console.log(`  · keeping builder sprite ${sprite.id} (--keep)`);
    }
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
