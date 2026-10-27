# Vendored gpui-component @ c8917c3 (GPUI-000)

Pinned source: https://github.com/longbridge/gpui-component at
`c8917c3661ed1090870f403c312b39e0d8a62e0b` (gpui-kit / gpui-component 0.8.1, GPUI snapshot
`gpui-pre =0.3.10`). Licence: Apache-2.0 (`LICENSE-APACHE`). Read-only reference for workers:
**copy patterns from here, never from memory or newer docs**. `AGENTS.md` is upstream's own
architecture/style guide; `skills/gpui-kit/` is upstream's agent skill (GPUI entity, element,
action, focus, async and test references under `skills/gpui-kit/references/gpui/`).

Our rules for using this are in `crates/client-gpui/CONVENTIONS.md`.

| Widget / topic                 | Example file(s)                                                                                                  |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------- |
| App shell / window setup       | `examples/hello_world/src/main.rs`, `examples/window_title/`, `stories/shell_story.rs`                           |
| Sidebar navigation             | `examples/sidebar/`, `stories/sidebar_story.rs`                                                                  |
| Dock / panes / resizable split | `examples/dock/src/main.rs`, `stories/dock_story.rs`, `stories/resizable_story.rs`                               |
| Virtual list (transcripts)     | `stories/virtual_list_story.rs` (`v_virtual_list`), `stories/list_story.rs` (`List` + delegate)                  |
| Chat / message stream          | `stories/message_story.rs`, `stories/message_scroller_story.rs`, `examples/stream-markdown/`                     |
| Markdown rendering             | `examples/markdown/`, `examples/stream-markdown/`                                                                |
| Text input (composer)          | `stories/input_story.rs`, `examples/input/`, `stories/input_group_story.rs`                                      |
| Code editor (diff/file view)   | `stories/editor_story.rs`, `examples/editor/`, `examples/large-text/`                                            |
| Diff view                      | `stories/diff_story.rs`                                                                                          |
| Dropdown / select              | `stories/select_story.rs`, `stories/combobox_story.rs`, `stories/dropdown_button_story.rs`                       |
| Dialog / alert / sheet         | `stories/dialog_story.rs`, `stories/alert_dialog_story.rs`, `stories/sheet_story.rs`, `examples/dialog_overlay/` |
| Button                         | `stories/button_story.rs`                                                                                        |
| Tabs                           | `stories/tabs_story.rs`                                                                                          |
| Notification / toast           | `stories/notification_story.rs`                                                                                  |
| Tree (file explorer)           | `stories/tree_story.rs`                                                                                          |
| Table                          | `stories/table_story.rs`, `stories/data_table_story.rs`, `examples/table_in_scrollable/`                         |
| Command palette + keybindings  | `stories/command_story.rs` (`actions!`, `cx.bind_keys`, `KeyBinding::new`), `stories/kbd_story.rs`               |
| Settings forms                 | `stories/settings_story.rs`, `stories/form_story.rs`                                                             |
| Theme switching                | `examples/system_monitor/src/main.rs` (`Theme::change(ThemeMode::Dark, …)`)                                      |
| Focus management               | `examples/focus_trap/`, `skills/gpui-kit/references/gpui/focus-handle.md`                                        |
| Headless UI tests              | `skills/gpui-kit/references/gpui/test*.md`, `crates/client-gpui/tests/headless_smoke.rs` (ours)                  |
| FPS / perf probe               | `examples/fps_monitor/`                                                                                          |
