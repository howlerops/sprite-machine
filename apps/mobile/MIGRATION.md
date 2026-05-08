# Mobile UI Migration Notes

The mobile app (`@sprite-machine/mobile`) was migrated from a hand-rolled
RN+web UI to **gluestack-ui v5-alpha + NativeWind v4 + Tailwind v3** in
late April 2026. The mix below is intentional and a couple of pieces are
not what you'd expect from a fresh-greenfield install. This file
documents why so we don't re-litigate the choices on every upgrade.

## Stack pin matrix

| Piece                    | What we ship    | What's "current"    | Why we're not on current                                                                                                                                                                                                                                                                          |
| ------------------------ | --------------- | ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `@gluestack-ui/core`     | `5.0.0-alpha.0` | v5 (alpha)          | gluestack moved the v4 CLI/components into a v5 shape and stopped publishing new v4. Upstream `create-gluestack` only emits v5 sources, so the path of least friction is vendoring v5 components and adapting them.                                                                               |
| `@gluestack-ui/utils`    | `5.0.1-alpha.0` | matches core        | Bundled with v5 component sources.                                                                                                                                                                                                                                                                |
| `nativewind`             | `^4.2.3`        | NativeWind v5 alpha | NativeWind v5 requires Metro 0.84+ and a different Babel setup. Expo SDK 53 still pins Metro 0.82.x as a peer of `@expo/cli@0.24.x`, so v5 won't load under our Expo.                                                                                                                             |
| `tailwindcss`            | `^3.4.19`       | Tailwind v4         | NativeWind v4 is built against Tailwind v3's `tailwind.config.js` shape; Tailwind v4's CSS-first config is incompatible.                                                                                                                                                                          |
| `expo`                   | `~53.0.27`      | SDK 54              | We're holding to SDK 53 until SDK 54 lands stable + NativeWind v5 ships stable. The two need to advance together.                                                                                                                                                                                 |
| `metro` (and 12 friends) | `0.82.5`        | 0.84+               | `react-native-css-interop@0.2.3` (NativeWind 4's runtime) declares `metro@^0.84` as a peer. `@expo/cli@0.24.24` calls APIs only available in Metro 0.82.x. The peer mismatch breaks the web export. We pin Metro to 0.82.5 inside `react-native-css-interop` only (see "Scoped overrides" below). |

## The `nativewind-shim.ts`

`apps/mobile/components/nativewind-shim.ts` exists because gluestack v5
components compose with `styled(...)` (a v5 NativeWind primitive that
attaches className-aware props at build time). NativeWind v4 doesn't ship
`styled`; it expects you to call `cssInterop` per component to register
which className-like props should be rewritten.

The shim provides a `styled(...)`-shaped function that calls
`cssInterop(Component, { className: 'style' })` under the hood. We
re-export it as `@gluestack-ui/nativewind-utils/withStyleContext`,
`@gluestack-ui/nativewind-utils/styled`, etc. — the import paths the
vendored v5 components reach for.

**Caveat:** the shim only wires the single `className -> style` mapping.
If a v5 component starts using `tvaClassName` or multi-target mappings
(e.g. mapping `containerClassName -> contentContainerStyle` on FlatList),
we'd need to extend the shim. That risk is documented in
`/Users/jacob/projects/sprite-machine/.omc/research/ui-design-spec.md`
and the critic review; we accept it for now.

## `@ts-nocheck` on `components/ui/**`

The vendored v5 components are written for the v5 type surface
(`@gluestack-ui/core/v5-types`) which we don't ship. Instead of polyfilling
the missing type modules, we put `@ts-nocheck` at the top of every file
in `components/ui/` and exclude that directory from the project's
`tsconfig.json`. The runtime behavior is correct (verified by the
shim + v4 NativeWind interop), but the types reference modules that
don't exist in our `node_modules`. Excluding + nocheck is the smallest
surface that keeps `pnpm typecheck` clean without lying about types
in our first-party code (`app/**`, `src/**`).

`tsconfig.json` keeps `strict`, `noUncheckedIndexedAccess`,
`exactOptionalPropertyTypes`, and `declaration` inherited from
`tsconfig.base.json`. **First-party app code holds the line on strict
flags**; only `components/ui/**` is unchecked.

## Scoped overrides (Metro) — why flat is correct

The root `package.json` uses flat (workspace-wide) overrides for all 13
`metro-*` packages at `0.82.5`. This looks broader than necessary but is
the only form that works reliably, for a subtle pnpm reason.

`react-native-css-interop@0.2.3` declares `metro@^0.84` as a **peer**
dependency. pnpm satisfies peer deps by installing them into the hoisted
`.pnpm/node_modules/` store. If the overrides are scoped (e.g.
`react-native-css-interop>metro` or `@sprite-machine/mobile>metro`),
pnpm still installs a 0.84.x copy at the hoisted level for the peer
requirement AND a 0.82.5 copy under the scoped path. Expo's
`metro.config.js` loader resolves `metro-cache` from the hoisted store
(not the scoped path), picks up 0.84.x, and crashes because
`@expo/cli@0.24.x` uses `metro-cache/src/stores/FileStore` — an internal
path removed in 0.84.

A flat workspace-wide override forces the single hoisted copy to 0.82.5
for every consumer. This is safe here because:

- control-plane, sprite-agent, and cli have zero transitive metro
  dependencies (`pnpm why metro` returns empty for all three).
- The override is therefore functionally mobile-only in practice; it just
  achieves it through the resolution mechanism pnpm actually honors for
  hoisted peer deps.

If a future workspace package genuinely needs Metro 0.84+, the correct
fix is to upgrade `@expo/cli` (and Expo SDK) in `apps/mobile`
simultaneously and then drop the overrides entirely — not to re-scope
them.

If you ever delete `react-native-css-interop` from `apps/mobile` you can
remove the entire `pnpm.overrides` block.

## Upgrade path — when to reconsider

Re-evaluate the whole stack when **all** of the following land:

1. **Expo SDK 54 stable.** This is the gating release for Metro 0.84+ as
   a first-party peer.
2. **NativeWind v5 stable** (no more alpha/beta tags). At that point:
   - Delete `apps/mobile/components/nativewind-shim.ts`.
   - Re-vendor gluestack v5 components from a fresh `create-gluestack`
     run (or upgrade to whatever stable v5 publishes by then).
   - Remove the `pnpm.overrides` block in the root `package.json`.
   - Remove the `exclude: ["components/ui"]` and the per-file
     `@ts-nocheck` once the v5 type modules are available.
3. **Tailwind v4 supported by NativeWind v5.** If yes, migrate
   `tailwind.config.js` to the v4 CSS-first shape simultaneously; this
   is also when we'd swap the design tokens out of `global.css` `@layer
base` blocks into the v4 `@theme` directive.

When in doubt, do steps 1+2 together as one PR; do not partial-upgrade
NativeWind without bumping Expo. Mismatching Metro versions is the most
common cause of "white screen with no error" on web export.

## Out-of-scope deviations from the design spec

These are tracked as follow-ups, not bugs:

- `formatCost()` mixes `$` and `¢` for sub-cent costs. The spec doesn't
  prescribe; we match the previous behavior.
- Toast IDs use `Math.random()`. Acceptable until we have a real
  collision; switch to `crypto.randomUUID()` when toasts get persisted
  cross-session.
- Multi-target `cssInterop` mappings in `nativewind-shim.ts`. See the
  caveat above.

## Web bundle: wterm is inlined into the entry chunk

`apps/mobile/components/shell-panel.web.tsx` statically imports
`@wterm/react`. Metro under Expo SDK 53 emits a single web entry chunk
(`_expo/static/js/web/entry-*.js`) — there is no automatic
`React.lazy()`-driven split, no separate runtime chunk, and no
`SplitChunksPlugin`-equivalent for Metro's web target. We tried
`React.lazy(() => import('@wterm/react'))` and Metro inlined the
`import()` anyway (one chunk in, one chunk out).

The previous build was ~2.10 MB; after dropping the (no-op) lazy wrapper
the bundle is ~2.15 MB (the lazy/Suspense plumbing was net deadweight).
We rely on Metro's **platform-extension resolver** (`shell-panel.web.tsx`
vs `shell-panel.native.tsx`) to keep wterm out of the native bundle —
verified by the fact that native typecheck passes with `Terminal` typed
only on the web side and the iOS/Android bundle never resolves
`@wterm/react`.

A `shell-panel.d.ts` shim sits beside the `.web.tsx`/`.native.tsx`
implementations so TypeScript's Bundler resolution can find a typed
default export when `app/run/[id].tsx` imports `'../../components/shell-panel'`
without an extension. Metro ignores `.d.ts` files; TS uses it.

**When to revisit:** if/when Expo SDK 54 + Metro 0.84 + an enabled
RSC/lazy-chunk pipeline lands, swap back to a true `import()`-driven
split so the wterm/xterm payload only loads when the Shell tab is
opened. Until then, the inlined cost is acceptable for our single-user
dashboard.
