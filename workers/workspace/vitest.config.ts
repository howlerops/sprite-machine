import { defineWorkersConfig } from "@cloudflare/vitest-pool-workers/config";

// The test pool does not read wrangler.jsonc: its `containers` entry points at
// images/workspace/Dockerfile (M0-008c), which wrangler validates even when containers are
// not run. Tests use FakeSandbox, so the bindings are declared here instead.
export default defineWorkersConfig({
  test: {
    include: ["test/**/*.test.ts"],
    poolOptions: {
      workers: {
        main: "./src/index.ts",
        isolatedStorage: false,
        miniflare: {
          compatibilityDate: "2026-10-01",
          compatibilityFlags: ["nodejs_compat"],
          durableObjects: { WORKSPACE: { className: "WorkspaceDO", useSQLite: true } },
          r2Buckets: ["BACKUPS"],
          bindings: { DEV_FAKE_SANDBOX: "1", BACKUP_PREFIX: "backups/" },
        },
      },
    },
  },
});
