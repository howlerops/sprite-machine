# ADR 0001: client-stack gates (written before the prototypes)

- Status: accepted (gates only; the decisions are ADR 0001a desktop, M1B-009, and 0001b mobile, M1B-011)
- Desktop decided: ADR 0001a (gpui-kit, provisional on a GPU frame-time run)
- Date: 2026-10-27
- Plan: `docs/PLAN-v2-ade.md` §4.4, §7.3, §7.4
- Gates file (normative): `spikes/bench/gates.yaml`

## Context

v2 needs a native client on macOS, Windows, Linux, iOS and Android. The candidates are
**gpui-kit** (gpui-component on GPUI), **Flutter**, and a **hybrid** (gpui-kit desktop + Flutter
mobile over the shared Rust `client-core`). gpui-kit is production-grade on desktop but its
mobile path is experimental (one GPUI view per app lifetime, incomplete IME, accessibility and
lifecycle hooks, no webview). Prototypes are cheap to bias, so the gates are fixed first.

## Decision

1. The gates in `spikes/bench/gates.yaml` are fixed **before** any prototype (M1B-004\*, M1B-007)
   exists. Changing a threshold afterwards needs a new ADR.
2. **Numeric gates** (common agui-fixture workload, 200 events/s, median of 5 runs):
   p95 frame time ≤ 16.7 ms on iPhone and desktop, ≤ 20 ms on Android; p95 key-to-glyph
   ≤ 60 ms; cold start ≤ 1.5 s (iPhone, desktop) / ≤ 2.5 s (Android); peak RSS ≤ 300 MB;
   IPA / APK ≤ 50 MB, desktop binary ≤ 80 MB.
3. **Hard gates** (pass/fail, on physical devices): Japanese and Chinese IME, autocorrect,
   dictation, image paste, hardware keyboard, VoiceOver/TalkBack on an approval card,
   Dynamic Type 200 %, 10-minute background then resume from `seq`, push tap from a killed app,
   stacked native screens, an accepted TestFlight upload.
4. **Desktop and mobile are decided separately.** Desktop needs no devices and is decided in P2
   (M1B-009) from Linux + macOS runs; mobile is decided in P7 (M1B-011) on devices.
5. **Default to hybrid**: any hard-gate failure of gpui-kit on iOS or Android means mobile is
   Flutter. Flutter is rejected only if it fails the same gate. A numeric miss above 25 %
   counts as a hard failure.
6. **Version pin**: results are valid only for the pinned gpui / gpui-component revs and Flutter /
   flutter_rust_bridge versions recorded with them; a bump requires a re-run (M1B-008).

## Consequences

- The interim mobile web page (P2) covers phones until P7, so the mobile decision is not on
  the critical path.
- Most UI logic lives in `client-core` (natively tested), which keeps a later switch of the
  mobile stack cheap.
- Benchmarks (M1B-008) must emit `spikes/bench/results/<candidate>/<device>/<gate>.json` with the
  pinned versions, so the decision ADRs can cite raw evidence.
