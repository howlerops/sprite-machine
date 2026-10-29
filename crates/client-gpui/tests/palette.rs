//! M3-014: command palette and shortcuts. The command model natively (filtering, ranking,
//! selection wrap, thread order), the shortcut table, the Fluent catalog, and the shell on GPUI's
//! headless platform driven by key strokes against a tiny fake hub.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use client_core::outbox::ManualClock;
use client_gpui::inbox::{InboxModel, SystemClock, parse_threads};
use client_gpui::palette::{
    Command, CommandId, PaletteModel, SHORTCUTS, ShortcutId, ThreadRef, adjacent, build_commands,
    display, fuzzy_score, keystroke, label, shortcut, thread_order,
};
use client_gpui::shell::auth::SessionGrant;
use client_gpui::shell::browser::RecordingBrowser;
use client_gpui::shell::i18n::Catalog;
use client_gpui::shell::nav::Section;
use client_gpui::shell::prefs::Preferences;
use client_gpui::shell::session::{MemoryStore, Session, SessionStore};
use client_gpui::shell::{AppConfig, AppShell};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Focusable as _, Point, TestAppContext,
    WindowBounds, WindowOptions, px, size,
};
use serde_json::{Value, json};

// ---- helpers ---------------------------------------------------------------------------------

fn thread(id: &str, title: &str) -> ThreadRef {
    ThreadRef {
        thread_id: id.to_owned(),
        title: title.to_owned(),
        workspace: Some(format!("/work/{id}")),
    }
}

fn titles(model: &PaletteModel) -> Vec<String> {
    model
        .matches()
        .iter()
        .map(|command| command.title.clone())
        .collect()
}

fn find_command<'a>(commands: &'a [Command], id: &CommandId) -> &'a Command {
    commands
        .iter()
        .find(|command| &command.id == id)
        .expect("command exists")
}

fn row(id: &str, title: &str, status: &str) -> Value {
    json!({
        "thread_id": id, "title": title, "machine_id": "mch_1", "agent": "claude",
        "status": status, "updated_at": "2026-10-28T10:00:00.000Z", "last_seq": 7,
        "workspace": format!("/work/{id}"),
    })
}

// ---- fuzzy matching ----------------------------------------------------------------------------

#[test]
fn fuzzy_matching_is_an_ordered_case_insensitive_subsequence() {
    assert!(fuzzy_score("gti", "Go to Inbox").is_some());
    assert!(fuzzy_score("GO TO", "go to inbox").is_some());
    assert_eq!(fuzzy_score("xyz", "Go to Inbox"), None);
    // Order matters: "ib" is in "Inbox" but "bi" is not.
    assert!(fuzzy_score("ib", "Inbox").is_some());
    assert_eq!(fuzzy_score("bi", "Inbox"), None);
    assert_eq!(fuzzy_score("", "anything"), Some(0));
    // A query longer than the text cannot match.
    assert_eq!(fuzzy_score("inboxes", "inbox"), None);
}

#[test]
fn a_contiguous_prefix_beats_a_word_start_beats_scattered_letters() {
    let prefix = fuzzy_score("new", "New task").expect("prefix");
    let word = fuzzy_score("task", "New task").expect("word start");
    let middle = fuzzy_score("ask", "New task").expect("middle");
    let scattered = fuzzy_score("nsk", "New task").expect("scattered");
    assert!(prefix > word, "{prefix} > {word}");
    assert!(word > middle, "{word} > {middle}");
    assert!(middle > scattered, "{middle} > {scattered}");
}

// ---- the command model -------------------------------------------------------------------------

#[test]
fn the_default_commands_cover_pages_new_task_thread_steps_and_recent_threads() {
    let commands = build_commands(&[thread("th_a", "Fix login"), thread("th_b", "")]);
    for section in Section::ALL {
        if section == Section::NewTask {
            continue;
        }
        let command = find_command(&commands, &CommandId::Go(section));
        assert_eq!(command.title, format!("Go to {}", section.title()));
    }
    assert_eq!(
        find_command(&commands, &CommandId::NewTask).title,
        "New task"
    );
    assert_eq!(
        find_command(&commands, &CommandId::OpenThread("th_a".into())).title,
        "Open thread: Fix login"
    );
    assert_eq!(
        find_command(&commands, &CommandId::OpenThread("th_b".into())).title,
        "Open thread: Untitled thread"
    );
    assert!(commands.iter().any(|c| c.id == CommandId::NextThread));
    assert!(commands.iter().any(|c| c.id == CommandId::PreviousThread));
}

#[test]
fn an_empty_query_lists_every_command_in_order_and_selects_the_first() {
    let commands = build_commands(&[thread("th_a", "Fix login")]);
    let model = PaletteModel::new(commands.clone());
    assert_eq!(model.match_count(), commands.len());
    assert_eq!(model.selected_index(), 0);
    assert_eq!(model.selected().map(|c| &c.id), Some(&commands[0].id));
    assert_eq!(model.matches()[1].id, commands[1].id);
}

#[test]
fn typing_filters_and_ranks_and_resets_the_selection() {
    let mut model = PaletteModel::new(build_commands(&[
        thread("th_a", "Fix login bug"),
        thread("th_b", "Write docs"),
    ]));
    model.select_next();
    model.select_next();
    assert_eq!(model.selected_index(), 2);

    model.set_query("inb");
    assert_eq!(
        model.selected_index(),
        0,
        "a new query selects the best match"
    );
    assert_eq!(
        model.selected().map(|c| &c.id),
        Some(&CommandId::Go(Section::Inbox))
    );
    assert!(!titles(&model).iter().any(|t| t.contains("Usage")));

    model.set_query("login");
    assert_eq!(
        model.selected().map(|c| &c.id),
        Some(&CommandId::OpenThread("th_a".into())),
        "a recent thread is found by its title"
    );
    assert_eq!(model.match_count(), 1);

    model.set_query("new");
    assert_eq!(model.selected().map(|c| &c.id), Some(&CommandId::NewTask));

    model.set_query("usage");
    assert_eq!(
        model.selected().map(|c| &c.id),
        Some(&CommandId::Go(Section::Usage))
    );

    model.set_query("zzzz");
    assert_eq!(model.match_count(), 0);
    assert!(model.selected().is_none());

    model.set_query("");
    assert_eq!(
        model.match_count(),
        build_commands(&[thread("a", "x"), thread("b", "y")]).len()
    );
}

#[test]
fn a_better_match_outranks_an_earlier_command() {
    // "Go to Agents" comes before the thread in the list, but the thread's title is the match.
    let mut model = PaletteModel::new(build_commands(&[thread("th_a", "agents rewrite")]));
    model.set_query("rewrite");
    assert_eq!(
        model.selected().map(|c| &c.id),
        Some(&CommandId::OpenThread("th_a".into()))
    );
    // Equal matches keep the command order.
    model.set_query("go to");
    let order: Vec<&CommandId> = model.matches().iter().map(|c| &c.id).collect();
    let pages: Vec<CommandId> = Section::ALL
        .iter()
        .filter(|s| **s != Section::NewTask)
        .map(|s| CommandId::Go(*s))
        .collect();
    assert_eq!(order[..pages.len()], pages.iter().collect::<Vec<_>>()[..]);
}

#[test]
fn the_selection_wraps_in_both_directions() {
    let mut model = PaletteModel::new(build_commands(&[]));
    let count = model.match_count();
    assert!(count > 3);
    model.select_previous();
    assert_eq!(
        model.selected_index(),
        count - 1,
        "up from the first wraps to the last"
    );
    model.select_next();
    assert_eq!(
        model.selected_index(),
        0,
        "down from the last wraps to the first"
    );
    model.select_next();
    assert_eq!(model.selected_index(), 1);
    model.select(count + 5);
    assert_eq!(model.selected_index(), 1, "out of range changes nothing");
    model.select(3);
    assert_eq!(model.selected_index(), 3);

    let mut empty = PaletteModel::new(Vec::new());
    empty.select_next();
    empty.select_previous();
    assert!(empty.selected().is_none());
}

// ---- thread order ------------------------------------------------------------------------------

fn inbox_with(rows: Vec<Value>) -> InboxModel {
    let mut model = InboxModel::new(Arc::new(SystemClock::new()));
    let generation = model.list_request().generation;
    let (rows, _) = parse_threads(&json!({ "threads": rows }));
    assert!(model.apply_list(generation, rows));
    model
}

#[test]
fn thread_order_follows_the_inbox_sections() {
    let model = inbox_with(vec![
        row("th_run", "Running one", "running"),
        row("th_need", "Needs me", "needs_you"),
        row("th_run2", "Running two", "running"),
    ]);
    let shown: Vec<String> = model.rows().iter().map(|r| r.thread_id.clone()).collect();
    let order: Vec<String> = thread_order(&model)
        .into_iter()
        .map(|thread| thread.thread_id)
        .collect();
    assert_eq!(order.len(), 3);
    // The inbox lists "needs you" first, whatever order the hub answered in.
    assert_eq!(order[0], "th_need");
    assert_eq!(shown.len(), order.len());
    let workspaces: Vec<_> = thread_order(&model)
        .into_iter()
        .map(|t| t.workspace)
        .collect();
    assert_eq!(workspaces[0].as_deref(), Some("/work/th_need"));
}

#[test]
fn adjacent_threads_stop_at_the_ends() {
    let ids = ["a", "b", "c"];
    assert_eq!(adjacent(&ids, Some("a"), 1), Some(1));
    assert_eq!(adjacent(&ids, Some("b"), 1), Some(2));
    assert_eq!(adjacent(&ids, Some("c"), 1), None);
    assert_eq!(adjacent(&ids, Some("c"), -1), Some(1));
    assert_eq!(adjacent(&ids, Some("a"), -1), None);
    // Nothing open: next is the first thread, previous the last.
    assert_eq!(adjacent(&ids, None, 1), Some(0));
    assert_eq!(adjacent(&ids, None, -1), Some(2));
    assert_eq!(adjacent(&ids, Some("gone"), 1), Some(0));
    assert_eq!(adjacent(&[], Some("a"), 1), None);
}

// ---- shortcuts ---------------------------------------------------------------------------------

#[test]
fn the_shortcut_table_has_one_row_per_shortcut_and_no_duplicate_strokes() {
    let ids: HashSet<ShortcutId> = SHORTCUTS.iter().map(|s| s.id).collect();
    assert_eq!(ids.len(), SHORTCUTS.len(), "one row per id");
    for mac in [true, false] {
        // Strokes that share a key context may not collide.
        let global: Vec<&str> = SHORTCUTS
            .iter()
            .filter(|s| {
                !matches!(
                    s.id,
                    ShortcutId::ClosePalette
                        | ShortcutId::RunCommand
                        | ShortcutId::SelectPrevious
                        | ShortcutId::SelectNext
                )
            })
            .map(|s| s.keystroke(mac))
            .collect();
        let unique: HashSet<&&str> = global.iter().collect();
        assert_eq!(unique.len(), global.len(), "duplicate stroke (mac={mac})");
    }
    assert_eq!(shortcut(ShortcutId::OpenPalette).keystroke(true), "cmd-k");
    assert_eq!(shortcut(ShortcutId::OpenPalette).keystroke(false), "ctrl-k");
    assert_eq!(shortcut(ShortcutId::NewTask).keystroke(true), "cmd-n");
    assert_eq!(
        shortcut(ShortcutId::SendTask).keystroke(false),
        "ctrl-enter"
    );
    assert_eq!(
        shortcut(ShortcutId::NextThread).keystroke(false),
        "alt-down"
    );
    assert_eq!(
        shortcut(ShortcutId::PreviousThread).keystroke(false),
        "alt-up"
    );
}

#[test]
fn shortcut_labels_read_naturally_on_each_platform() {
    assert_eq!(shortcut(ShortcutId::OpenPalette).label(true), "⌘K");
    assert_eq!(shortcut(ShortcutId::OpenPalette).label(false), "Ctrl+K");
    assert_eq!(shortcut(ShortcutId::SendTask).label(true), "⌘↵");
    assert_eq!(shortcut(ShortcutId::SendTask).label(false), "Ctrl+Enter");
    assert_eq!(shortcut(ShortcutId::NextThread).label(true), "⌥↓");
    assert_eq!(shortcut(ShortcutId::NextThread).label(false), "Alt+↓");
    assert_eq!(display("ctrl-shift-tab", false), "Ctrl+Shift+Tab");
    assert_eq!(display("escape", false), "Esc");
}

#[test]
fn commands_carry_their_shortcut_labels() {
    let commands = build_commands(&[]);
    assert_eq!(
        find_command(&commands, &CommandId::NewTask).shortcut,
        Some(ShortcutId::NewTask)
    );
    assert_eq!(
        find_command(&commands, &CommandId::NextThread).shortcut,
        Some(ShortcutId::NextThread)
    );
    assert_eq!(
        find_command(&commands, &CommandId::PreviousThread).shortcut,
        Some(ShortcutId::PreviousThread)
    );
}

// ---- Fluent ------------------------------------------------------------------------------------

/// Message ids `src/palette/*.rs` looks up: `tr("id")`, `tr_args("id", ..)`.
fn ids_used_in_palette_sources() -> HashSet<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/palette");
    let mut ids = HashSet::new();
    for entry in std::fs::read_dir(dir).expect("src/palette") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source");
        // `tr(` / `tr_args(` followed (after whitespace, rustfmt may wrap) by a string literal.
        for call in ["tr(", "tr_args("] {
            for (at, _) in text.match_indices(call) {
                let rest = text[at + call.len()..].trim_start();
                if let Some(rest) = rest.strip_prefix('"')
                    && let Some(end) = rest.find('"')
                {
                    ids.insert(rest[..end].to_owned());
                }
            }
        }
    }
    ids
}

#[test]
fn every_palette_string_id_exists_in_the_english_catalog() {
    let catalog =
        Catalog::from_source("en", client_gpui::palette::EN_FTL).expect("palette.ftl parses");
    let used = ids_used_in_palette_sources();
    assert!(
        used.len() >= 12,
        "scan found only {} ids: {used:?}",
        used.len()
    );
    let missing: Vec<_> = used.iter().filter(|id| !catalog.has(id)).collect();
    assert!(
        missing.is_empty(),
        "ids missing from palette.ftl: {missing:?}"
    );
    // And nothing in the catalog is dead.
    let unused: Vec<_> = Catalog::ids(client_gpui::palette::EN_FTL)
        .into_iter()
        .filter(|id| !used.contains(id))
        .collect();
    assert!(
        unused.is_empty(),
        "palette.ftl ids no code uses: {unused:?}"
    );
    assert_eq!(
        catalog.tr("palette-go-to", &[("section", "Inbox")]),
        "Go to Inbox"
    );
}

// ---- the shell, headless -----------------------------------------------------------------------

type Rows = Arc<Mutex<Vec<Value>>>;

async fn list_threads(State(rows): State<Rows>) -> Json<Value> {
    let threads = rows.lock().expect("lock").clone();
    Json(json!({ "threads": threads, "next_cursor": null }))
}

fn spawn_hub(rows: Rows) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("hub runtime");
        runtime.block_on(async move {
            let app = Router::new()
                .route("/v1/threads", get(list_threads))
                .with_state(rows);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind hub");
            sender
                .send(listener.local_addr().expect("addr").port())
                .expect("send port");
            axum::serve(listener, app).await.expect("serve");
        });
    });
    format!("http://127.0.0.1:{}", receiver.recv().expect("hub port"))
}

fn signed_in(hub: &str) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::default());
    let grant = SessionGrant::from_json(&json!({
        "token": "sm_session_token", "expires_at": 1_900_000_000,
        "user": { "display_name": "Ada" }, "org": { "name": "Acme" }, "role": "member"
    }))
    .expect("grant");
    store
        .save(&Session {
            hub: hub.to_owned(),
            grant,
        })
        .expect("save");
    store
}

fn open_shell(cx: &mut TestAppContext, rows: Vec<Value>) -> (AnyWindowHandle, Entity<AppShell>) {
    let rows: Rows = Arc::new(Mutex::new(rows));
    let hub = spawn_hub(rows);
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        client_gpui::shell::init(cx);
    });
    let store = signed_in(&hub);
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1000.), px(700.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            move |window, cx| {
                cx.new(|cx| {
                    AppShell::new(
                        AppConfig {
                            hub,
                            browser: Arc::new(RecordingBrowser::new()),
                            store,
                            prefs: Preferences::default(),
                            prefs_path: None,
                            poll_speedup: 200,
                            link_thread: String::new(),
                            clock: Arc::new(ManualClock::new(1_793_138_416_000)),
                        },
                        window,
                        cx,
                    )
                })
            },
        )
        .expect("open window")
    })
}

/// Pumps the inbox (its refresh timer runs on the test clock) until `id` is laid out.
fn wait_for(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
    for _ in 0..400 {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        let shown = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find(id).is_some()
            })
            .expect("window");
        if shown {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{id} never shown");
}

/// Presses `key`, then draws.
fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &'static str) {
    cx.update_window(window, |_, window, cx| {
        window.press(key, cx);
        cx.refresh_windows();
        window.render_frame(cx);
    })
    .expect("window");
    cx.run_until_parked();
}

fn type_text(cx: &mut TestAppContext, window: AnyWindowHandle, text: &'static str) {
    cx.update_window(window, |_, window, cx| {
        window.input(text, cx);
        window.render_frame(cx);
    })
    .expect("window");
    cx.run_until_parked();
}

fn palette_shown(cx: &mut TestAppContext, window: AnyWindowHandle) -> bool {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find("palette").is_some()
    })
    .expect("window")
}

fn item_label(cx: &mut TestAppContext, window: AnyWindowHandle, ix: usize) -> Option<String> {
    cx.update_window(window, |_, window, _| {
        window
            .try_find(format!("palette-item-{ix}"))
            .and_then(|item| item.label().map(str::to_owned))
    })
    .expect("window")
}

fn current_section(cx: &mut TestAppContext, view: &Entity<AppShell>) -> Section {
    cx.update(|cx| view.read(cx).model().nav.current())
}

#[gpui_kit::test]
fn the_open_shortcut_shows_the_palette_and_escape_closes_it(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    assert!(!palette_shown(cx, window));

    press(cx, window, "secondary-k");
    assert!(palette_shown(cx, window), "the shortcut opens the palette");
    cx.update(|cx| assert!(view.read(cx).palette().read(cx).is_open()));
    // The field has focus, so typing goes to it.
    cx.update_window(window, |_, window, cx| {
        let field = view.read(cx).palette().read(cx).input().focus_handle(cx);
        assert!(field.is_focused(window), "the search field is focused");
    })
    .expect("window");

    press(cx, window, "escape");
    assert!(!palette_shown(cx, window), "Escape closes it");
    cx.update(|cx| assert!(!view.read(cx).palette().read(cx).is_open()));
    // Closing runs nothing.
    assert_eq!(current_section(cx, &view), Section::Inbox);

    // Once closed, the shortcut works again; pressing it while open closes it.
    press(cx, window, "secondary-k");
    assert!(palette_shown(cx, window));
    press(cx, window, "secondary-k");
    assert!(!palette_shown(cx, window));
}

#[gpui_kit::test]
fn the_palette_lists_commands_with_their_shortcuts(cx: &mut TestAppContext) {
    let (window, _view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-k");
    // New task is first and shows its shortcut.
    let expected = format!("New task, shortcut {}", label(ShortcutId::NewTask));
    assert_eq!(item_label(cx, window, 0), Some(expected));
    assert_eq!(item_label(cx, window, 1).as_deref(), Some("Go to Inbox"));
    let next = format!(
        "Open next thread, shortcut {}",
        label(ShortcutId::NextThread)
    );
    let found = (0..20).any(|ix| item_label(cx, window, ix).as_deref() == Some(next.as_str()));
    assert!(found, "the thread shortcut is listed next to its command");
    // The strokes the palette shows are the ones that are bound.
    assert_eq!(
        keystroke(ShortcutId::OpenPalette),
        if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        }
    );
}

#[gpui_kit::test]
fn typing_filters_the_list_and_enter_runs_the_selected_command(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-k");
    type_text(cx, window, "usage");
    assert_eq!(item_label(cx, window, 0).as_deref(), Some("Go to Usage"));
    assert_eq!(
        item_label(cx, window, 1),
        None,
        "only matching commands are listed"
    );

    press(cx, window, "enter");
    assert!(
        !palette_shown(cx, window),
        "running a command closes the palette"
    );
    assert_eq!(current_section(cx, &view), Section::Usage);
}

#[gpui_kit::test]
fn a_query_with_no_match_shows_the_empty_state_and_enter_does_nothing(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-k");
    type_text(cx, window, "qqqq");
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("palette-empty").label(),
            Some("No matching commands")
        );
    })
    .expect("window");
    press(cx, window, "enter");
    assert!(
        palette_shown(cx, window),
        "nothing to run: the palette stays"
    );
    assert_eq!(current_section(cx, &view), Section::Inbox);
}

#[gpui_kit::test]
fn the_arrow_keys_move_the_selection_and_wrap(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-k");
    let selected = |cx: &mut TestAppContext| {
        cx.update(|cx| view.read(cx).palette().read(cx).model().selected_index())
    };
    assert_eq!(selected(cx), 0);
    press(cx, window, "down");
    assert_eq!(selected(cx), 1);
    press(cx, window, "up");
    press(cx, window, "up");
    let count = cx.update(|cx| view.read(cx).palette().read(cx).model().match_count());
    assert_eq!(
        selected(cx),
        count - 1,
        "up from the first wraps to the last"
    );
    press(cx, window, "down");
    assert_eq!(selected(cx), 0);

    // Down twice, then Enter, runs the third command (Go to Agents).
    press(cx, window, "down");
    press(cx, window, "down");
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("palette-item-2").selected(),
            Some(true),
            "the selected row is marked"
        );
        assert_eq!(window.find("palette-item-0").selected(), Some(false));
    })
    .expect("window");
    press(cx, window, "enter");
    assert_eq!(current_section(cx, &view), Section::Agents);
}

#[gpui_kit::test]
fn clicking_a_command_runs_it(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-k");
    type_text(cx, window, "machines");
    cx.update_window(window, |_, window, cx| {
        window.click("palette-item-0", cx);
        window.render_frame(cx);
    })
    .expect("window");
    assert_eq!(current_section(cx, &view), Section::Machines);
    assert!(!palette_shown(cx, window));
}

#[gpui_kit::test]
fn a_recent_thread_is_opened_from_the_palette_by_its_title(cx: &mut TestAppContext) {
    let (window, view) = open_shell(
        cx,
        vec![
            row("th_a", "Fix login", "running"),
            row("th_b", "Write the release notes", "running"),
        ],
    );
    wait_for(cx, window, "inbox-open-th_a");

    press(cx, window, "secondary-k");
    type_text(cx, window, "release");
    assert_eq!(
        item_label(cx, window, 0).as_deref(),
        Some("Open thread: Write the release notes")
    );
    press(cx, window, "enter");
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), Some("th_b")));
    assert_eq!(current_section(cx, &view), Section::Inbox);
}

#[gpui_kit::test]
fn the_new_task_shortcut_shows_the_composer_with_the_prompt_focused(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-n");
    assert_eq!(current_section(cx, &view), Section::NewTask);
    cx.update_window(window, |_, window, cx| {
        let prompt = view
            .read(cx)
            .compose()
            .read(cx)
            .prompt_input()
            .focus_handle(cx);
        assert!(prompt.is_focused(window), "the prompt has the cursor");
    })
    .expect("window");
    // What the user types goes into the prompt.
    type_text(cx, window, "Upgrade axum");
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).compose().read(cx).model().draft().prompt,
            "Upgrade axum"
        );
    });
}

#[gpui_kit::test]
fn the_new_task_command_in_the_palette_does_the_same(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-k");
    type_text(cx, window, "new task");
    press(cx, window, "enter");
    assert_eq!(current_section(cx, &view), Section::NewTask);
    type_text(cx, window, "From the palette");
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).compose().read(cx).model().draft().prompt,
            "From the palette"
        );
    });
}

#[gpui_kit::test]
fn the_send_shortcut_submits_the_composer_when_it_has_focus(cx: &mut TestAppContext) {
    let (window, _view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-n");
    type_text(cx, window, "Upgrade axum");
    press(cx, window, "secondary-enter");
    cx.update_window(window, |_, window, _| {
        // Validation ran: the form is missing its workspace, which is what Send reports.
        assert_eq!(
            window.find("compose-status").label(),
            Some("Choose a machine.")
        );
    })
    .expect("window");
}

#[gpui_kit::test]
fn the_send_shortcut_does_nothing_outside_the_composer(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-enter");
    assert_eq!(current_section(cx, &view), Section::Inbox);
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("compose-status").is_none());
    })
    .expect("window");
}

#[gpui_kit::test]
fn the_thread_shortcuts_open_the_adjacent_thread_in_inbox_order(cx: &mut TestAppContext) {
    let (window, view) = open_shell(
        cx,
        vec![
            row("th_run", "Running one", "running"),
            row("th_need", "Needs me", "needs_you"),
            row("th_run2", "Running two", "running"),
        ],
    );
    wait_for(cx, window, "inbox-open-th_need");
    // The inbox shows "needs you" first; the shortcut walks the same order.
    let order = ["th_need", "th_run", "th_run2"];
    let open =
        |cx: &mut TestAppContext| cx.update(|cx| view.read(cx).open_thread_id().map(str::to_owned));

    assert_eq!(open(cx), None);
    press(cx, window, "alt-down");
    assert_eq!(
        open(cx).as_deref(),
        Some(order[0]),
        "next with none open: the first"
    );
    press(cx, window, "alt-down");
    assert_eq!(open(cx).as_deref(), Some(order[1]));
    press(cx, window, "alt-down");
    assert_eq!(open(cx).as_deref(), Some(order[2]));
    press(cx, window, "alt-down");
    assert_eq!(open(cx).as_deref(), Some(order[2]), "stops at the last");
    press(cx, window, "alt-up");
    assert_eq!(open(cx).as_deref(), Some(order[1]));
    press(cx, window, "alt-up");
    press(cx, window, "alt-up");
    assert_eq!(open(cx).as_deref(), Some(order[0]), "stops at the first");
    assert_eq!(current_section(cx, &view), Section::Inbox);
}

#[gpui_kit::test]
fn the_next_thread_command_in_the_palette_opens_the_first_thread(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, vec![row("th_a", "Fix login", "running")]);
    wait_for(cx, window, "inbox-open-th_a");
    press(cx, window, "secondary-k");
    type_text(cx, window, "next thread");
    press(cx, window, "enter");
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), Some("th_a")));
}

#[gpui_kit::test]
fn shortcuts_keep_working_after_a_page_takes_the_focused_prompt_away(cx: &mut TestAppContext) {
    let (window, view) = open_shell(cx, Vec::new());
    press(cx, window, "secondary-n");
    // From inside the prompt the palette still opens.
    press(cx, window, "secondary-k");
    assert!(palette_shown(cx, window));
    press(cx, window, "escape");
    // Leaving the page removes the prompt; the next shortcut must still reach the shell.
    press(cx, window, "ctrl-tab");
    assert_eq!(current_section(cx, &view), Section::Agents);
    press(cx, window, "ctrl-shift-tab");
    assert_eq!(current_section(cx, &view), Section::NewTask);
}
