# Local dev loop

This is the local toolchain for the v2 workspace on Linux. The cloud sandbox uses
[`scripts/cloud-setup.sh`](../scripts/cloud-setup.sh); a Linux dev box uses
[`scripts/setup-dev.sh`](../scripts/setup-dev.sh), which installs the same tools
plus ImageMagick for screenshots. Plan references are in
[PLAN-v2-ade.md §4.2](PLAN-v2-ade.md) (dev and test loop) and §4.4 (desktop).

## One-time setup

```sh
bash scripts/setup-dev.sh
```

The script is idempotent. A second run changes nothing and exits 0. It installs:

- **Rust targets**: `x86_64-unknown-linux-musl`, `wasm32-unknown-unknown`, `wasm32-wasip2`.
- **apt packages**: `musl-tools`, `lld`, `clang`, `mesa-vulkan-drivers` (lavapipe),
  `libvulkan1`, `vulkan-tools`, `xvfb`, `xauth`, `imagemagick`, `libxkbcommon-dev`,
  `libxkbcommon-x11-dev`, `libwayland-dev`, `libfontconfig-dev`, `libssl-dev`.
- **Cargo tools**, pinned and checked by version: `sccache` 0.10.0,
  `cargo-nextest` 0.9.150, `worker-build` 0.8.7.
- **wrangler** 4.154.0 (`npm install -g wrangler@4.154.0`).

It ends by checking for a Vulkan `llvmpipe` device and exits 1 if there is none.
Check it yourself with `vulkaninfo --summary | grep -i llvmpipe`.

## Build settings

Use these for every cargo command in the workspace:

```sh
export CARGO_TARGET_DIR=/home/user/.cargo-target-shared   # one target dir shared by all worktrees
export CARGO_BUILD_JOBS=2                                 # 4 vCPU box; GPUI builds take 10-20 min at 2 jobs
export RUSTFLAGS='-C link-arg=-fuse-ld=lld'               # lld linker
export RUSTC_WRAPPER=sccache                              # optional: compile cache (sccache 0.10.0)
```

- **Shared `CARGO_TARGET_DIR`**: every worktree builds into the same directory, so
  dependencies compile once. Keep it outside the repo. Cargo locks it, so parallel
  builds of different branches wait on each other.
- **`CARGO_BUILD_JOBS=2`**: keeps two parallel Rust builds inside the 4 vCPUs.
- **lld**: `lld` is installed by the script and linking is noticeably faster with it.
- **sccache**: set `RUSTC_WRAPPER=sccache` to cache rustc output across clean builds.
  Check it with `sccache --show-stats`.

## GPUI render proof

```sh
bash scripts/gpui-render-proof.sh
```

The script:

1. clones `longbridge/gpui-component` at the commit pinned in the script
   (shallow fetch, into `${TMPDIR:-/tmp}/gpui-component-<rev>`);
2. builds the `hello_world` example with the settings above;
3. runs it under `xvfb-run -s "-screen 0 1280x800x24"` with
   `VK_ICD_FILENAMES` pointing at lavapipe (`lvp_icd.json`);
4. waits for the window to paint, captures the root window with `import`, and
   kills the app with a `trap`. `xvfb-run` stops its Xvfb when the script exits;
5. fails if the screenshot has fewer than 2 distinct colours (`identify -format %k`).

The output is `spikes/gpui/screenshots/hello_world.png`, which is committed. To use
another example, change `EXAMPLE` and the pinned `GPUI_COMPONENT_REV` in the script.
Bump the pin on purpose, not on each run.

## Hub (`wrangler dev`)

The hub is a Rust Worker compiled to wasm32 with `worker-build`, and it runs locally
under `wrangler dev`. Once its `wrangler.toml` exists (added with the hub tracer
item), run from the hub crate:

```sh
cd crates/hub
wrangler dev --local
```

Durable Objects and their SQLite storage run inside workerd on your machine. There is
**no database setup**: no Postgres, no D1 migration and no Docker daemon. Cloudflare
containers are off locally, since this box has no Docker.

`wrangler dev` is the only wrangler command this loop uses. Do not run `wrangler deploy`
or `wrangler delete` from a dev session.

## Tests

```sh
cargo nextest run          # the workspace test runner (installed by setup-dev.sh)
```

## Troubleshooting

- **`vulkaninfo` shows no llvmpipe**: check that `mesa-vulkan-drivers` is installed and
  that `/usr/share/vulkan/icd.d/lvp_icd.json` exists.
- **Screenshot is all one colour**: the app did not paint. Read the log printed on
  failure, and check `VK_ICD_FILENAMES`.
- **`wrangler --version` prints a proxy warning**: harmless. Your proxy is used for
  fetches.
