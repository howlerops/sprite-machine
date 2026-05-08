/**
 * Bundle the sprite-agent into a single .mjs file for deployment to a sprite.
 * The Anthropic Agent SDK is externalized — it has a vendored native binary
 * that breaks when bundled, so we install it via npm on the sprite.
 *
 * Output: packages/sprite-agent/dist/sprite-agent.mjs
 *         packages/sprite-agent/dist/package.json   (deployable package.json)
 */
import { build } from 'esbuild';
import path from 'node:path';
import { mkdir, readFile, writeFile } from 'node:fs/promises';

const root = path.resolve(import.meta.dirname, '..');
const outDir = path.join(root, 'dist');
await mkdir(outDir, { recursive: true });

const result = await build({
  entryPoints: [path.join(root, 'src', 'index.ts')],
  bundle: true,
  platform: 'node',
  target: 'node22',
  format: 'esm',
  outfile: path.join(outDir, 'sprite-agent.mjs'),
  external: ['@anthropic-ai/claude-agent-sdk'],
  banner: {
    js: `import { createRequire } from 'node:module'; const require = createRequire(import.meta.url);`,
  },
  metafile: true,
});

const sourcePkg = JSON.parse(await readFile(path.join(root, 'package.json'), 'utf8')) as {
  dependencies: Record<string, string>;
};
const sdkVersion = sourcePkg.dependencies['@anthropic-ai/claude-agent-sdk'] ?? '*';
await writeFile(
  path.join(outDir, 'package.json'),
  JSON.stringify(
    {
      name: 'sprite-agent-deploy',
      private: true,
      version: '0.0.0',
      type: 'module',
      main: 'sprite-agent.mjs',
      dependencies: {
        '@anthropic-ai/claude-agent-sdk': sdkVersion,
      },
    },
    null,
    2,
  ),
);

const bundleSize = Object.values(result.metafile?.outputs ?? {}).reduce((s, o) => s + o.bytes, 0);
console.log(
  `bundled → ${path.join(outDir, 'sprite-agent.mjs')} (${(bundleSize / 1024).toFixed(1)} KB)`,
);
