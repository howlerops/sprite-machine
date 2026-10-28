# Numeric gates on iPhone and Android: measurement procedure

Procedure only. It applies to the five numeric gates in `spikes/bench/gates.yaml`: frame_time_p95,
key_to_glyph_p95, cold_start, rss_peak, binary_size. Thresholds and the common workload are
normative in `gates.yaml`; this file does not change them. The desktop equivalent is
`spikes/bench/run-desktop.sh`. Hard gates are in `spikes/bench/HARD_GATES.md`.

## Common setup (both platforms)

- **Build.** Release builds at the pinned versions (`gates.yaml` `rules.version_pin`). Record the
  commit, the gpui or Flutter version, and `flutter_rust_bridge` where it applies.
- **Device.** Physical phone. Record model (iPhone: `iPhone15,2`-style identifier from
  `xcrun devicectl list devices` or the About screen; Android: `adb shell getprop ro.product.model`) and OS
  version. Charge the phone above 50 %, turn off Low Power Mode and Battery Saver, and set brightness to a fixed level.
  Let the phone cool to room temperature before a series; note any thermal warning.
- **Network.** Wi-Fi, same LAN as the laptop that runs `agui-fixture`:
  `agui-fixture --scenario transcript-10k --rate 200 --addr 0.0.0.0:<port>`, and the app points at
  `ws://<laptop-ip>:<port>/threads/th_fixture/subscribe?after=0`.
- **Workload.** The common workload from `gates.yaml` `workload`: the transcript streaming at 200
  events/s, a 5k-line diff open, a terminal receiving output, and one pending approval card. Scroll
  the transcript during the run (one full scroll per 2 s).
- **Runs.** Five runs per gate per device, with the phone rebooted before each cold-start run.
  Median of 5 is the gated value; p95 is computed within each run (for frame time and key-to-glyph).
- **Recording.** Each run is recorded to a file and the raw export is kept under
  `spikes/bench/results/<candidate>/<device>/raw/run<i>/`.
- **Results.** One JSON per gate at `spikes/bench/results/<candidate>/<device>/<gate>.json`, matching
  `spikes/bench/results.schema.json`, with `statistic` as in `results.schema.json` (median_of_run_p95
  for frame time and key-to-glyph; median_of_runs for cold start and RSS; single for binary size).
  Check with `python3 -I spikes/bench/validate_results.py <file>`. The summary numbers must be
  the same as the raw export's.

Units: frame time and key-to-glyph in ms; cold start in s; RSS in decimal MB (10^6 bytes); binary
size in decimal MB.

## Definitions (same as the desktop run)

- **Frame time.** The interval between consecutive frame presentations, in ms, after the first
  interactive frame. Its p95 is per run; the gated value is the median of the five run p95s.
- **Key-to-glyph.** From the hardware key event reaching the app to the frame that paints the glyph
  in the composer, in ms. Use a Bluetooth keyboard, one key every 200 ms during the run. Its p95 is
  per run; the gated value is the median of the five run p95s.
- **Cold start.** From process launch (the moment the launch is requested) to the first interactive
  frame of the inbox, with cached data and no network (the inbox is shown from the local cache with the
  server unreachable). Seconds.
- **RSS peak.** Peak resident memory during the common workload, in decimal MB. The gated value is the median
  of the five per-run peaks.
- **Binary size.** The release artifact size: the IPA thinned for one device model (iPhone), or the
  universal APK split for the device's ABI (Android). Decimal MB. One measurement.

## iPhone

Tools: Xcode (Instruments and `xctrace`), and the device connected by cable.

- **Find the device.** `xcrun xctrace list devices` gives the UDID to use below.
- **Frame time.** Record the Animation Hitches template:
  `xcrun xctrace record --template 'Animation Hitches' --device <UDID> --launch -- <bundle-id> --output frame.trace`.
  Export the frame intervals with
  `xcrun xctrace export --input frame.trace --xpath '/trace-toc/run/data/table[@schema="hitches"]' --output frame.xml`
  (use the table name that Xcode shows for the template). Compute the intervals between consecutive
  presents, after the first interactive frame. If the template does not give the presents, use the Metal System Trace template.
- **Key-to-glyph.** Use the candidate's bench log (the app writes the key receipt and the glyph
  paint timestamps, the same format as the desktop run). Cross-check with a high-speed camera
  (240 fps or better) filming the key and the screen together: count frames from key press to glyph
  change for 20 presses. Record both, and use the app's log as the gated value.
- **Cold start.** Record the App Launch template for each run, after a reboot:
  `xcrun xctrace record --template 'App Launch' --device <UDID> --launch -- <bundle-id> --output launch.trace`.
  Read the time from process launch to the first frame. Record the value in seconds.
- **RSS peak.** Instruments Allocations, or the memory report in Xcode's debugger, and record the peak
  physical footprint during the run. Name the tool in the notes. The iOS limit is not the same as the
  Android one, so note any memory warning received during a run.
- **Binary size.** Archive the release build, then export a thinned IPA for the device model:
  Xcode Organizer > Distribute App > Ad Hoc (or Development) with the device's variant, or
  `xcodebuild -exportArchive -exportOptionsPlist <plist> -archivePath <archive> -exportPath <dir>`. Measure the
  resulting `.ipa` with `stat -f %z`.

## Android

Tools: `adb` (platform-tools), the phone in developer mode with USB debugging, and Android GPU
Inspector or Perfetto for traces. Use the release build, or a debuggable build of the same commit
where a step says so.

- **Frame time.** Before each run: `adb shell dumpsys gfxinfo <package> reset`. After the run:
  `adb shell dumpsys gfxinfo <package> framestats`. The `FrameCompleted` timestamps (ns) give the
  present intervals; use the rows after the first interactive frame. For a timeline, use Perfetto
  (`actual_frame_timeline`) or Android GPU Inspector's frame timing.
- **Key-to-glyph.** Log key events with `adb shell getevent -lt` (the first column is the event time) while
  pressing a Bluetooth keyboard key. Match them to the app's bench log by key ID, and check the
  glyph timestamp in the Perfetto trace. Cross-check with a high-speed camera as on iPhone.
- **Cold start.** Force-stop, then start with timing:
  `adb shell am force-stop <package>` and `adb shell am start -W -n <package>/<activity>`. The `TotalTime`
  value is the time to the first frame drawn, in ms. Use it as the launch time and record the
  method in the notes. The gate is the inbox with the server unreachable, so run the cold start with
  the network off.
- **RSS peak.** Sample `dumpsys meminfo <package>` every second during the run and take the maximum
  of TOTAL RSS. For a true high-water mark, read `VmHWM` from `/proc/<pid>/status` every 200 ms
  (`adb shell cat /proc/<pid>/status`); this needs a debuggable build or access the shell has to the
  app's `/proc`. Record which one was used, since they differ.
- **Binary size.** Build the App Bundle, then the universal APK for the device ABI with bundletool:
  `bundletool build-apks --bundle=<app.aab> --output=<out.apks> --mode=universal`, extract the universal
  APK, and measure with `stat -c %s`. For a per-ABI measurement, use `--mode=default` and
  `bundletool get-size total --apks=<out.apks> --dimensions=ABI`, which reports the size for the device's ABI.

## Recording the result

For each gate and device, write the JSON with the five raw runs in `samples` (or the pooled
values for frame time and key-to-glyph) and per-run `runs` where the statistic needs them. Put the
device model, OS version and pinned versions in the file. Run
`python3 -I spikes/bench/validate_results.py <file>`; it recomputes the percentiles and the median
and rejects a `pass` that disagrees with `value <= threshold`.
