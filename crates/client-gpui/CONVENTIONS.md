# client-gpui conventions

Rules for every item that touches `crates/client-gpui/**` (lane `client`, plan §4.4, §7.4).
GPUI is pre-1.0 and changes fast: **copy patterns from the vendored, pinned examples in
`docs/vendor/gpui-component-c8917c3/` (see its `INDEX.md`), never from memory or from newer
upstream docs.** If the vendored code and this file disagree, the vendored code wins; say so
in your report.

## 0. Versions and dependencies

- `gpui` = `gpui-pre =0.3.10`; `gpui-kit` and `gpui-component` = git rev `c8917c3` (root
  `Cargo.toml` `[workspace.dependencies]`). Do not change these in a feature item; a bump is
  its own leader item (re-vendor, re-run `scripts/gpui-render-proof.sh`).
- Import through the umbrella crate: `use gpui_kit::*;` (re-exports `gpui`), plus
  `use gpui_kit::component::*;` — required for the styling extensions such as `.v_flex()` /
  `.h_flex()` (without it: `no method named v_flex`) — and specific widgets like
  `use gpui_kit::component::button::Button;`. Add `use gpui_kit::prelude::*;` too: it brings
  `FluentBuilder` (`.when`, `.when_some`, `.map`), which `use gpui_kit::*` alone does not cover on
  wrapped elements such as the one `.test_support()` returns. Don't import `gpui_component` directly in
  views unless the item needs a type gpui-kit does not re-export.
- No business logic in this crate. View models, sync, caching, the outbound queue and every
  decision live in `crates/client-core` (natively tested). A view reads a view model and turns
  user input into calls on it (§6).

## 1. App and window setup

Copy `docs/vendor/…/examples/hello_world/src/main.rs`:

```rust
gpui_kit::application().run(|cx| {
    gpui_kit::init(cx); // before any window that uses styled components
    gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| cx.new(|cx| AppShell::new(window, cx)))
        .expect("open main window");
});
```

- Always open windows with `gpui_kit::open_window` (it wraps content in the Base `Root`, which
  hosts dialogs, sheets, notifications, tooltips, menus). Never construct `Root` yourself.
- The builder returns your content entity, not a `Root`.
- The binary entry point lives in the desktop app crate later; `client-gpui` stays a library
  exposing `pub fn run(...)` plus the screen modules.

## 2. Module layout: one module per screen

The skeleton already has `src/{shell,compose,inbox,task,diff,approval,agents,notify,palette,
workspace,settings,machines,local,flows,loadout,usage}/mod.rs`. Each screen item owns exactly
its module:

```
src/<screen>/mod.rs      // pub struct <Screen>View + impl Render; re-exports
src/<screen>/<part>.rs   // sub-views (e.g. task/transcript.rs, task/tool_call.rs)
tests/<screen>_*.rs      // headless tests (§8)
```

Shared widgets go in `src/shell/` only when two screens need them; otherwise keep them local.

## 3. Views: struct + `Render`, entities for state

- A stateful view is a struct implementing `Render` (`fn render(&mut self, window: &mut Window,
cx: &mut Context<Self>) -> impl IntoElement`), created with `cx.new(|cx| …)` and held as an
  `Entity<T>`.
- Small stateless pieces implement `RenderOnce` (upstream's "stateless design" rule) or are
  plain functions returning `impl IntoElement`.
- Build elements as **one fluent builder chain**; use `.when(cond, |el| …)`,
  `.when_some(opt, |el, v| …)`, `.map(|el| …)` instead of mutable temporaries (upstream
  AGENTS.md "GPUI builder style").
- Mutate state in listeners: `cx.listener(|this, event, window, cx| { …; cx.notify(); })`.
  Call `cx.notify()` after every state change that affects rendering.
- Observe other entities with `cx.observe(&entity, |this, entity, cx| …)` / `cx.subscribe`;
  store the returned `Subscription` in the struct (dropping it unsubscribes).
- Name GPUI contexts `cx`; never `ctx` (upstream rule). Name `window` `window`.
- Give every interactive or asserted element a stable `.id("…")` (string ids, kebab or snake
  case, unique within the screen) so tests can find it.

## 4. Actions and keybindings

Copy `stories/command_story.rs`:

- Declare actions with `actions!(<namespace>, [Name, …])` (or `#[action(namespace = …)]` for
  actions with data). Namespace = the screen module name.
- Register bindings once at init: `cx.bind_keys([KeyBinding::new("cmd-k", OpenPalette, None)])`
  — use `cmd-` on macOS and `ctrl-` elsewhere via `secondary-` where GPUI supports it.
- Handle with `.on_action(cx.listener(Self::open_palette))` on the element that owns focus.
- Plan §3 shortcuts: ⌘K palette, ⌘N new task, ⌘↵ send, task navigation. Keep a table of every
  binding in `src/palette/mod.rs` rustdoc.

## 5. Theming

- Read colours and sizes only through the theme: `cx.theme()` (`ActiveTheme` trait) — e.g.
  `cx.theme().foreground`, `.border`, `.muted_foreground`. **No hex literals in views.**
- Switch light/dark with `Theme::change(ThemeMode::Dark, Some(window), cx)`
  (`examples/system_monitor/src/main.rs`).
- Use the component size system (`Sizable`: `xs/sm/md/lg`) rather than ad-hoc pixel sizes.

## 6. Binding to client-core view models

- `client-core` view models are plain Rust (no GPUI). The view holds an `Entity<…>` wrapper or
  a cheap handle and re-renders on change:

  ```rust
  pub struct TaskView { vm: client_core::task::TaskViewModel, _sub: Subscription }
  // on new data from client-core: self.vm.apply(event); cx.notify();
  ```

- Every user intent goes to the view model (`vm.send_prompt(text)`, `vm.answer(interrupt_id,
option_id)`); the view never builds protocol messages or touches the network.
- Async work: `cx.spawn(async move |this, cx| { … this.update(cx, |this, cx| { …; cx.notify() }) })`
  (see `skills/gpui-kit/references/gpui/async.md`). Never block the UI thread; never use tokio
  in views (client-core owns its runtime boundary).

## 7. Lists and text

- **Transcripts and any list that can exceed ~100 rows use a virtual list**:
  `v_virtual_list` (`stories/virtual_list_story.rs`) with measured or fixed item sizes, or
  `List` + delegate (`stories/list_story.rs`). Never render a 10k-event transcript as plain
  children.
- Text input: `InputState` entity + `Input::new(&state)` (`stories/input_story.rs`); multi-line
  composer = input in multi-line mode; code/diff = the editor (`stories/editor_story.rs`,
  `stories/diff_story.rs`).
- Markdown: `examples/stream-markdown/` for streamed assistant text.

## 8. Tests (headless, required for every screen item)

Pattern verified at this pin in `crates/client-gpui/tests/headless_smoke.rs`:

```rust
use gpui_kit::test::{TestSupportExt, TestWindowExt};

#[gpui_kit::test]
fn clicking_the_button_updates_the_status(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| gpui_kit::open_window(options_400x300(), cx,
        |_, cx| cx.new(|_| Counter { clicks: 0 })).expect("open test window"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("bump", cx);
        window.render_frame(cx);
        assert_eq!(window.find("status").label(), Some("Clicked 1 times"));
    }).expect("window is open");
}
```

- `dev-dependencies` enable `test-support` on `gpui` and `gpui-kit` (already in Cargo.toml).
- Find elements by `.id(…)`; expose readable state with `.test_support().aria_label(…)` (and a
  `role`) — this doubles as the screen-reader label (plan §3 cross-cutting a11y).
- Interactions: `window.click`, `window.input("text", cx)`, `window.key("cmd-enter", cx)` /
  keystrokes, `window.scroll`; then `render_frame` before asserting.
- `.test_support()` is a no-op without the `test-support` feature, so views may call it
  unconditionally; call `.aria_label(…)` after `.id(…)`.
- **Virtual lists** (verified in `spikes/gpui/tests/transcript_headless.rs`): `v_virtual_list` needs
  every item size up front (fixed-height rows, or a measuring pass for wrapped text). The list
  itself is not observable: wrap it in an element with `.id("transcript").test_support()` and
  scroll that (`window.scroll("transcript", ScrollDelta::Pixels(point(px(0.), px(-dy))), cx)`;
  negative y scrolls down). `find_all(id)` matches only the last id segment, so give each row a
  unique wrapper id (`("row", ix)`) plus an inner element with a shared id (`"transcript-row"`);
  `find_all("transcript-row")` then counts only the rows actually laid out (assert it is ≪ the
  item count), and `.visible()` drops rows drawn just outside the viewport.
- Drive views from `client-core` fixtures (TurnEvent examples in
  `crates/protocol/schemas/turn-event/examples/`), not from a live hub.
- Tests run on the GPUI test platform (no display, no GPU). Pixel output is checked only by
  `scripts/gpui-render-proof.sh` (xvfb + lavapipe) and the M1B benchmarks.

## 9. Build hygiene

- `CARGO_TARGET_DIR=/home/user/.cargo-target-shared`, `CARGO_BUILD_JOBS=2`; a cold GPUI build
  takes 6+ minutes — run it in the foreground with a long timeout, don't start a second build
  in parallel (the client lane is capped at 1 for this reason).
- `cargo clippy -p client-gpui --all-targets -- -D warnings` and
  `cargo test -p client-gpui` must pass; no `#[allow]` to silence clippy.
- Strings shown to users go through the Fluent catalog once it exists (plan §3, i18n); until
  then keep them as `const` / `SharedString` in one place per screen.
