# Hard gates: manual procedure

Normative source: `spikes/bench/gates.yaml` (`hard:`, `rules:`). ADR: `docs/adr/0001-client-stack-gates.md`.
This file says how each of the 12 hard gates is checked. It does not change any gate. Pass/fail
is a human verdict on a physical device (M1B-010), unless a gate's platform is a desktop that
M1B-009 uses, which is also checked by hand on that desktop.

## Common rules

- **Who.** A checker who did not write the candidate's code for that gate. Record the name or
  handle as `checked_by`. The checker follows the steps below in order and does not skip any.
- **Builds.** Release (or TestFlight / Play internal-track) builds of the candidate at the pinned
  versions in `gates.yaml` `rules.version_pin`: gpui and gpui-component revs for gpui-kit
  (root `Cargo.toml`), the Flutter SDK (M1B-006) and `flutter_rust_bridge` for Flutter. Record the
  commit of this repo in the notes.
- **Devices.** Physical devices only. Record the model and OS version in the evidence. Simulators
  and emulators do not count, except where a step says so.
- **Test server.** `agui-fixture --scenario transcript-10k --rate 200 --addr <LAN ip>:<port>` on a
  laptop on the same network, unless a step names another scenario.
- **Recording.** Screen-record the whole session for each gate (device recording or screen capture),
  and keep the file under `spikes/bench/results/<candidate>/<device>/evidence/`. Large recordings
  may go to the object store the team uses, but the evidence path must then point to that URL.
- **Verdict.** `pass` is true only if every numbered step passes. Any failed step is a fail, and the
  failing step number goes in `notes`. Re-tests after a fix are new runs: keep the failed record
  (rename it `<gate>.attempt-1.json`) so the history stays visible.
- **Evidence file.** `spikes/bench/results/<candidate>/<device>/<gate>.json`, schema
  `spikes/bench/results.schema.json`, with `kind: "hard"`, `unit: "pass_fail"`, `statistic: "verdict"`,
  `value: null`, `threshold: null`, `samples: []`, `checked_by`, `evidence` (recording and
  screenshot paths), `pass` and `notes` (one line per step: `step N: ok|fail, observation`).
  Check it with `python3 -I spikes/bench/validate_results.py <file>`.
- **Consequence.** Any fail of a gate on gpui-kit iOS or Android makes mobile Flutter (hybrid), with
  no further debate (`rules.default_to_hybrid`). Flutter is rejected only if it fails the same gate.

Platform keys below: `ios`, `android`, `macos`, `windows`, `linux`. The mobile-only gates are
marked **[mobile]**.

---

## 1. ime_japanese (ios, android, macos, windows, linux)

Device: each platform's phone or desktop, with the system Japanese IME on.
Preconditions: Japanese input source added (iOS: Settings > General > Keyboard > Keyboards > Japanese
Romaji; Android: Gboard, Japanese; macOS: Hiragana or Kotoeri; Windows: Microsoft IME; Linux: IBus with
mozc or fcitx5 with mozc). Composer empty.

1. Focus the composer. Type `nihongo` with the IME. The text appears as marked (underlined) text.
2. Press Space. The text converts to kana (`にほんご`), still marked.
3. Press Space again. The candidate window opens with `日本語` among the candidates, and it sits next to the caret.
4. Use the arrow keys to move through the candidates, then press Enter. The composer shows `日本語` committed, with no marked text left.
5. Type `ai`, then Escape. The marked text is removed and nothing is committed.
6. Type `kana` and Enter once more. The caret stays at the end of the committed text, with no duplicate characters.

**Pass:** all six steps behave as described; the marked text is underlined; the candidate window
is next to the caret; no keystroke is lost or doubled.
**Fail:** any missing marked text, a candidate window far from the caret, a lost or doubled key,
or a committed string that differs from the expected text.
**Evidence:** recording of steps 1 to 6; the composer's text after step 6 in a screenshot.

## 2. ime_chinese (ios, android, macos, windows, linux)

Device: as in gate 1. Preconditions: a Pinyin IME on (iOS: Simplified Chinese, Pinyin; Android:
Gboard, Chinese Pinyin; macOS: Pinyin - Simplified; Windows: Microsoft Pinyin; Linux: IBus or
fcitx5 with Pinyin). Composer empty.

1. Type `zhongwen` with the IME. The text appears as marked text.
2. A candidate list opens with `中文` among the candidates. Select it with the number key `1` or by tapping it.
3. The composer shows `中文` committed, with no marked text left.
4. Type `ni` and then press Escape. The marked text is removed.
5. Type `hao` and tap or press the candidate `好`. The composer reads `中文好`.

**Pass:** candidate selection commits the chosen string exactly; Escape cancels; the list stays near the caret.
**Fail:** as gate 1, and any selection that commits the wrong candidate.
**Evidence:** recording of the steps; screenshot of the composer after step 5.

## 3. autocorrect [mobile] (ios, android)

Device: iPhone or Android phone. Preconditions: system autocorrect and the suggestion bar on
(iOS: Settings > General > Keyboard > Auto-Correction on; Android: Gboard > Text correction on).

1. Focus the composer. Type `teh` and then Space. The word is corrected to `the` (the system does this; the app must not block it).
2. Undo the correction: on iOS tap the undo bubble that appears next to the caret; on Android tap the
   original word on the suggestion strip. The composer reads `teh` again.
3. Type `hel` and tap the suggestion `hello` in the suggestion bar. The composer reads `hello`.
4. Send the message. The sent text matches the composer.

**Pass:** the correction happens, the undo restores the original, the suggestion bar inserts the
tapped word, and the sent text matches.
**Fail:** the composer blocks corrections, the undo does nothing, the suggestion bar is hidden or
inserts the wrong text, or the sent text differs from the composer.
**Evidence:** recording; screenshot of step 2.

## 4. dictation [mobile for ios, android; desktop macos] (ios, android, macos)

Device: iPhone, Android phone, or Mac. Preconditions: system dictation on (iOS: Settings > General >
Keyboard > Enable Dictation; Android: Gboard voice typing; macOS: Settings > Keyboard > Dictation on).
Composer has the text `Start ` (with a trailing space) and the caret at the end.

1. Tap the dictation control (iOS keyboard microphone; Android voice typing; macOS Fn Fn).
2. Say, clearly: `testing dictation one two three`.
3. Stop dictation. The composer reads `Start testing dictation one two three` (the recognizer may
   vary the capitalisation and punctuation; the words must match).
4. Move the caret to the middle of the text, then dictate `middle` again. The text is inserted at the caret, not at the end.
5. Send the message.

**Pass:** the spoken words land in the composer at the caret, the existing text is kept, and the
message sends.
**Fail:** no text inserted, text inserted at the wrong place, existing text lost, or a crash.
**Evidence:** recording; the recognizer output in the notes.

## 5. image_paste (ios, android, macos, windows, linux)

Device: each platform, with a clipboard image.
Preconditions: copy a PNG screenshot of about 1 MB to the clipboard (take it with the system screenshot tool).

1. Focus the composer and paste (iOS: long-press, Paste; Android: long-press, Paste; desktop: Ctrl/Cmd+V).
2. An attachment chip with the image thumbnail appears in the composer. No text is pasted as an image placeholder.
3. Remove the attachment with its remove control. The chip disappears.
4. Paste again, then send the message.
5. In the task transcript, the message shows the image attachment, and the image opens at full size.

**Pass:** the image becomes an attachment (not text), remove works, and the transcript shows the attachment.
**Fail:** no attachment, the paste is ignored, the app crashes, or the transcript loses the image.
**Evidence:** screenshot of step 2 and of step 5.

## 6. hardware_keyboard (ios, android, macos, windows, linux)

Device: each platform, with a hardware keyboard (Bluetooth keyboard on the phones; the desktop's own
keyboard). Preconditions: a task with one message draft, an approval card pending, and the shortcut list open from its menu.

1. Type a draft, then press Cmd+Enter (Ctrl+Enter on Windows/Linux/Android). The message sends and the composer clears.
2. Type `a`, press Enter. A newline is inserted (not sent).
3. Press Tab repeatedly from the inbox. Focus moves in a visible order: list, task, composer, approval buttons. Shift+Tab moves back.
4. Use the arrow keys in the transcript list. The selection moves one row at a time.
5. Open the shortcut list (Cmd+/ or Ctrl+/ or ?). Every shortcut in the list works as stated.

**Pass:** every listed shortcut works, focus is visible at each Tab stop, and the list matches the actual shortcuts.
**Fail:** a shortcut does nothing, focus is invisible or lost, or the list omits or misstates a shortcut.
**Evidence:** recording of steps 1 to 5; screenshot of the shortcut list.

## 7. screen_reader_approval [mobile] (ios, android, macos)

Device: iPhone (VoiceOver), Android phone (TalkBack), Mac (VoiceOver: Cmd+F5). Preconditions: a
pending approval card with title `Run migration`, a diff summary of `3 files, +12 -4`, and a time left of `10 minutes`.

1. Turn the screen reader on. Open the approval card.
2. Move through the card. The reader announces, in order: the title, the diff summary, and the time left.
3. Move to Allow and activate it (double-tap or the reader's activate gesture). The card shows it is resolved, and the reader announces the new state.
4. Repeat steps 1 to 3 with a second pending approval, activating Deny instead.
5. Confirm the buttons have their own labels (`Allow` and `Deny`), not `button`.

**Pass:** all three fields are announced in order; both buttons are labelled and activatable; the resolved state is announced.
**Fail:** a field is not announced, a button is unlabelled or cannot be activated, or the focus gets stuck.
**Evidence:** recording of the reader output (screen recording with audio); notes list the exact spoken text.

## 8. dynamic_type_200 [mobile] (ios, android)

Device: iPhone and Android phone. Preconditions: the task view and an approval card visible with
a long title (120 characters) and a 5k-line diff summary.

1. iOS: Settings > Accessibility > Display & Text Size > Larger Text, then turn on Larger Accessibility Sizes and
   set the largest size. Android: Settings > Display > Font size, then the largest value; Display size set to its largest value.
   Record the effective font scale: on Android `adb shell settings get system font_scale` (if run from a
   laptop) or the value shown in Settings. 200 % means a font scale of 2.0 on Android, or the iOS size at
   which the system reports the same scale; record which one was used.
2. Open the task view. Text reflows; nothing is clipped on the right edge and no two elements overlap.
3. Open the approval card. Title, diff summary, time left, Allow and Deny are all visible or
   reachable by scrolling. No button is cut off.
4. Rotate the device to landscape and back. The layout still reflows.

**Pass:** no clipping, no overlap, and every control reachable.
**Fail:** clipped or overlapping text, a control that cannot be reached, or a layout that breaks on rotation.
**Evidence:** screenshots of steps 2 and 3, with the font scale in the notes.

## 9. background_resume [mobile] (ios, android)

Device: iPhone and Android phone on Wi-Fi with the laptop. Preconditions: a task streaming on the
fixture (`transcript-10k` at 200 ev/s; this produces well over 10 minutes of events). Record the
phone's last event sequence number (`seq`) in the app's debug view or in the fixture log.

1. Foreground the app with the task streaming. Note the last `seq` shown: `S0`. Note the time.
2. Put the app in the background (home button or swipe-up) for 10 minutes, measured with a stopwatch.
   Do not force-quit it.
3. Foreground the app. It catches up from `?after=S0` automatically: a catch-up request is visible in the debug view or in the fixture log.
4. Wait until the transcript reaches the latest `seq` on the server.
5. Verify the sequence: the seq numbers from `S0 + 1` up to the server's latest seq are present once each.
   Check it with the debug view (count and first and last seq) against the fixture log: no gap, no duplicate.

**Pass:** the catch-up request starts from `S0`, and the transcript holds every seq from `S0 + 1` up to
the latest, each exactly once.
**Fail:** a gap, a duplicate, a catch-up that starts from zero or from the wrong seq, or a stalled stream.
**Evidence:** the debug view (or a screenshot) showing first and last seq and the count; the fixture log excerpt for the same range.

## 10. push_tap_killed [mobile] (ios, android)

Device: iPhone and Android phone. Preconditions: push notifications allowed for the app; a
pending approval on a known task (title `Deploy staging`). Start the approval from the fixture or hub so that a push is sent.

1. Force-quit the app (iOS: swipe up from the app switcher; Android: swipe it away from recents and wait 10 seconds). Confirm it is not running.
2. Send the approval request. The push notification arrives on the device (lock screen is fine).
3. Tap the notification.
4. The app launches and opens the `Deploy staging` task, with the approval card visible and pending.
5. Repeat steps 1 to 4 with the device unlocked and then with the device locked.

**Pass:** each tap from a killed app opens the right task, and the approval card is visible and actionable.
**Fail:** the inbox opens instead, the wrong task opens, the approval is missing, or no push arrives.
**Evidence:** recording of step 3 to 4 for both states; screenshot of the notification.

## 11. stacked_screens [mobile] (ios, android)

Device: iPhone and Android phone. Preconditions: the inbox with at least two tasks, one of them with a diff.

1. Open a task from the inbox. Scroll the transcript to the middle, and type `draft` in the composer without sending.
2. Open the diff from the task. Scroll it and select a file.
3. Go back with the native back gesture (iOS: swipe from the left edge; Android: back gesture). The task returns with the scroll position and the `draft` text intact.
4. Go back again. The inbox returns with its scroll position intact.
5. Open the task again from the inbox. The `draft` text is still there.

**Pass:** each back gesture returns to the previous screen with its state (scroll, draft, selection) preserved.
**Fail:** a back gesture leaves the app, a screen restarts from the top, or the draft is lost.
**Evidence:** recording of steps 1 to 5.

## 12. testflight_upload [mobile] (ios)

Device: an iPhone for the install step. Preconditions: an Apple Developer account with the app
record; the release build of the candidate at the pinned versions.

1. Archive the release build (Xcode: Product > Archive, or `xcodebuild archive` with the release scheme).
2. Distribute to App Store Connect: Xcode Organizer > Distribute App > App Store Connect > Upload. Or upload with the Transporter app.
3. Wait for App Store Connect to show the build as Processing complete (no email error). Record the build number.
4. In TestFlight, add the build to the internal testing group. Install it on the iPhone from the TestFlight app.
5. Launch the installed build. It starts and shows the inbox.

**Pass:** the upload is accepted, processing completes without error, and the installed build launches.
**Fail:** the upload is rejected, processing fails, or the installed build cannot be installed or does not launch.
**Evidence:** screenshot of the App Store Connect build page (version, build number, status); screenshot of the TestFlight install.
