# Desktop builds (unsigned macOS)

CI builds `client-gpui` for macOS on every push to `v2` that touches `crates/**`, `Cargo.*`,
`scripts/bundle-macos.sh` or `.github/workflows/desktop.yml`. You can also run it by hand:
Actions, "desktop", "Run workflow".

## Download

1. Open the workflow run (Actions tab, workflow "desktop", job `macos-app`).
2. Under Artifacts, download `sprite-machine-macos-unsigned`. Artifacts are kept for 14 days.
3. Unzip. It holds `Sprite-Machine-macos-<arch>-<sha>.zip`, which unpacks to `Sprite Machine.app`.

## Open an unsigned app

The app is not signed or notarized, so Gatekeeper blocks the first launch. Either:

- Right-click (or Control-click) `Sprite Machine.app`, choose Open, then Open again in the dialog; or
- Remove the quarantine flag: `xattr -dr com.apple.quarantine "Sprite Machine.app"`.

## What the build is

- Release build for the runner's architecture only (arm64 on `macos-latest`). A universal
  binary (build both targets, then `lipo -create`) was left out: it roughly doubles build time.
  Intel Macs are not covered yet.
- Assembled by `scripts/bundle-macos.sh` with the template `crates/client-gpui/bundle/Info.plist.in`.
  Bundle id `app.spritemachine.desktop`, minimum macOS 13.0.
  `CFBundleShortVersionString` is `<crate version>+<short sha>`; `CFBundleVersion` is the crate version.
- No icon yet (default app icon).

## Not done yet

- Code signing and notarization: M6-006.
- Universal binary, app icon, dmg packaging, auto-update.
