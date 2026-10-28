# MVP-000b report: CI runs the tracer bullet

## Summary

Added a `tracer` job to `.github/workflows/rust.yml`. It runs on the same `push`/`pull_request` triggers to `v2` as the other jobs, on `ubuntu-latest`, with a 30 minute timeout. It runs `cargo run -p cli -- e2e tracer` and fails the workflow if that command fails. It uses no Cloudflare credentials. The existing `linux` and `macos` jobs are unchanged.

## Files changed

- `.github/workflows/rust.yml`: new `tracer` job (32 lines added, no existing lines changed).
- `docs/ledger/reports/MVP-000b.md`: this report.

## Job steps

1. `actions/checkout@v4`.
2. Install lld and the GPUI apt deps (same package list as the `linux` job).
3. Toolchain from `rust-toolchain.toml` (`rustup show active-toolchain || rustup toolchain install`, as in the other jobs). This also installs the `wasm32-unknown-unknown` target the hub build needs.
4. `Swatinem/rust-cache@v2`.
5. `taiki-e/install-action@v2` with `tool: worker-build@0.8.7` (prebuilt binary via cargo-binstall, no source build).
6. `actions/setup-node@v4` with Node 22.
7. `npm install -g wrangler@4.154.0`.
8. `cargo run -p cli -- e2e tracer`.

Job-level env: `CLOUDFLARE_API_TOKEN: ''` and `WRANGLER_SEND_METRICS: 'false'`. No account id is set and no `secrets.*` reference appears in the job.

## Verify output

```
$ grep -q 'e2e tracer' .github/workflows/rust.yml        -> exit 0
$ python3 -c "...assert j['runs-on']=='ubuntu-latest'; assert 'secrets.' not in s; assert '4.154.0' in s and '0.8.7' in s; print('ok')"
ok
$ npx --yes @action-validator/cli@0.6.0 .github/workflows/rust.yml   -> exit 0, no output
```

Cargo builds were not run for this item, per the brief. The CI job itself has not run yet; it will first run on the next push or PR to `v2`.

## Acceptance criteria

1. **Met.** `tracer` job on `ubuntu-latest`: checkout, toolchain from `rust-toolchain.toml`, lld and the same apt deps, `Swatinem/rust-cache@v2`, worker-build 0.8.7 via `taiki-e/install-action@v2`, Node 22 via `actions/setup-node@v4`, `wrangler@4.154.0` installed globally, then `cargo run -p cli -- e2e tracer`. A failing command fails the job.
2. **Met.** No `secrets.*` reference in the job (checked by the python assertion). `CLOUDFLARE_API_TOKEN: ''` is set, no account id is set, and `WRANGLER_SEND_METRICS: 'false'` is set.
3. **Met.** `timeout-minutes: 30`. Same `on:` triggers as the other jobs (the file-level `push`/`pull_request` to `v2`).

## Notes

- The attribution line in the commit message is `Co-Authored-By: Claude Haiku 5.5`, as the session attribution reminder specifies. The brief named Opus 5.5. The reminder was treated as the governing instruction for attribution.
- `cargo run -p cli` will compile the workspace in CI. The `rust-cache` step and the apt deps mirror the `linux` job, so this should not need extra packages. Whether the first cold run stays under 30 minutes is not verified here; if it does not, raise the timeout or split the cache key.

## Discovered work

```yaml
- id: MVP-000b-d1
  title: Confirm the tracer CI job passes on a real runner
  note: First run on GitHub Actions is unverified. Check the cold-cache build time against the 30 minute timeout.
  kind: follow-up
```
