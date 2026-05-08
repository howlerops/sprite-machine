/**
 * Bundle the control plane into a single .mjs for Docker deployment.
 * better-sqlite3 is externalized — it's a native module that we install
 * via npm in the runtime stage.
 *
 * Output:
 *   apps/control-plane/dist/control-plane.mjs
 *   apps/control-plane/dist/package.json
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
  outfile: path.join(outDir, 'control-plane.mjs'),
  external: ['better-sqlite3'],
  banner: {
    js: `import { createRequire } from 'node:module'; const require = createRequire(import.meta.url);`,
  },
  metafile: true,
});

const sourcePkg = JSON.parse(await readFile(path.join(root, 'package.json'), 'utf8')) as {
  dependencies: Record<string, string>;
  devDependencies?: Record<string, string>;
};
const sqliteVersion =
  sourcePkg.devDependencies?.['better-sqlite3'] ??
  sourcePkg.dependencies['better-sqlite3'] ??
  '^12.9.0';
await writeFile(
  path.join(outDir, 'package.json'),
  JSON.stringify(
    {
      name: 'control-plane-deploy',
      private: true,
      version: '0.0.0',
      type: 'module',
      main: 'control-plane.mjs',
      dependencies: { 'better-sqlite3': sqliteVersion },
    },
    null,
    2,
  ),
);

const bundleSize = Object.values(result.metafile?.outputs ?? {}).reduce((s, o) => s + o.bytes, 0);
console.log(
  `bundled → ${path.join(outDir, 'control-plane.mjs')} (${(bundleSize / 1024).toFixed(1)} KB)`,
);
