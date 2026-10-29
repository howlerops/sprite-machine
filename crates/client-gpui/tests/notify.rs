//! M3-013: desktop notifications, deep links and notification settings. The decisions natively
//! (fake sink), the settings view and the toast sink headlessly.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use client_gpui::notify::{
    EN_FTL, EventQueue, Notification, NotificationSink, Notifier, NotifyConfig, NotifyEvent,
    NotifyKind, NotifyPrefs, NotifySettingsView, PrefsHandle, ToastSink, hub_link_thread,
};
use client_gpui::shell::i18n::Catalog;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

const HUB: &str = "https://hub.example.com";

#[derive(Clone, Default)]
struct FakeSink(Arc<Mutex<Vec<Notification>>>);

impl NotificationSink for FakeSink {
    fn show(&mut self, notification: &Notification) {
        self.0.lock().unwrap().push(notification.clone());
    }
}

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("notify-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("notify.json")
}

fn notifier(name: &str) -> (Notifier, FakeSink, NotifyConfig) {
    let config = NotifyConfig {
        prefs_path: temp_path(name),
    };
    let sink = FakeSink::default();
    (Notifier::new(&config, Box::new(sink.clone())), sink, config)
}

// ---- transitions -----------------------------------------------------------------------------------

#[test]
fn each_transition_to_a_notifiable_status_notifies_once() {
    let (mut notifier, sink, _) = notifier("transitions");
    notifier.observe("thr_1", "running");
    assert!(notifier.observe("thr_1", "needs_you").is_some());
    notifier.observe("thr_1", "running");
    assert!(notifier.observe("thr_1", "done").is_some());
    assert!(notifier.observe("thr_2", "failed").is_some());
    let shown = sink.0.lock().unwrap();
    let kinds: Vec<_> = shown
        .iter()
        .map(|n| (n.thread_id.as_str(), n.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("thr_1", NotifyKind::NeedsYou),
            ("thr_1", NotifyKind::Done),
            ("thr_2", NotifyKind::Failed)
        ]
    );
}

#[test]
fn a_refetch_with_the_same_status_does_not_notify_again() {
    let (mut notifier, sink, _) = notifier("refetch");
    assert!(notifier.observe("thr_1", "needs_you").is_some());
    for _ in 0..3 {
        assert!(notifier.observe("thr_1", "needs_you").is_none());
    }
    assert_eq!(sink.0.lock().unwrap().len(), 1);
}

#[test]
fn quiet_statuses_never_notify() {
    let (mut notifier, sink, _) = notifier("quiet");
    for status in ["idle", "running", "", "archived", "surprise"] {
        assert!(notifier.observe("thr_1", status).is_none(), "{status}");
    }
    assert!(sink.0.lock().unwrap().is_empty());
}

#[test]
fn the_seeded_first_fetch_is_a_baseline_not_a_burst() {
    let (mut notifier, sink, _) = notifier("seed");
    notifier.seed([("thr_1", "needs_you"), ("thr_2", "done")]);
    assert!(notifier.observe("thr_1", "needs_you").is_none());
    assert!(notifier.observe("thr_2", "done").is_none());
    assert!(sink.0.lock().unwrap().is_empty());
    // A later change still notifies.
    notifier.observe("thr_2", "running");
    assert!(notifier.observe("thr_2", "done").is_some());
}

#[test]
fn the_focused_thread_does_not_notify_and_is_not_retried() {
    let (mut notifier, sink, _) = notifier("focus");
    notifier.set_focused(Some("thr_1"));
    assert!(notifier.observe("thr_1", "needs_you").is_none());
    assert!(notifier.observe("thr_2", "needs_you").is_some());
    // Leaving the thread does not announce what the user already saw.
    notifier.set_focused(None);
    assert!(notifier.observe("thr_1", "needs_you").is_none());
    assert_eq!(sink.0.lock().unwrap().len(), 1);
}

#[test]
fn a_disabled_kind_does_not_notify_but_the_others_do() {
    let (mut notifier, sink, _) = notifier("disabled");
    notifier.prefs().set(NotifyKind::Done, false).unwrap();
    assert!(notifier.observe("thr_1", "done").is_none());
    assert!(notifier.observe("thr_1", "needs_you").is_some());
    assert!(notifier.observe("thr_2", "failed").is_some());
    assert_eq!(sink.0.lock().unwrap().len(), 2);
    // Switching it on afterwards does not replay the missed one...
    notifier.prefs().set(NotifyKind::Done, true).unwrap();
    assert!(notifier.observe("thr_1", "done").is_some());
}

#[test]
fn observe_all_reports_only_the_changes() {
    let (mut notifier, _, _) = notifier("all");
    let list = [("a", "needs_you"), ("b", "running"), ("c", "done")];
    assert_eq!(notifier.observe_all(list).len(), 2);
    assert_eq!(notifier.observe_all(list).len(), 0);
}

#[test]
fn clicking_a_notification_opens_its_thread_once() {
    let (mut notifier, _, _) = notifier("click");
    let shown = notifier.observe("thr_9", "done").unwrap();
    assert_eq!(
        shown.click_event(),
        NotifyEvent::Open {
            thread_id: "thr_9".into()
        }
    );
    assert_eq!(
        notifier.clicked(&shown.id),
        Some(NotifyEvent::Open {
            thread_id: "thr_9".into()
        })
    );
    assert_eq!(notifier.clicked(&shown.id), None);
    assert_eq!(notifier.clicked("nope"), None);
}

#[test]
fn titles_are_generic_and_carry_no_machine_name() {
    let (mut notifier, _, _) = notifier("titles");
    let titles: Vec<String> = [
        ("thr_machine-laptop", "needs_you"),
        ("thr_b", "done"),
        ("thr_c", "failed"),
    ]
    .into_iter()
    .map(|(id, status)| notifier.observe(id, status).unwrap().title)
    .collect();
    assert_eq!(
        titles,
        ["Needs your attention", "Task finished", "Task failed"]
    );
    let catalog = Catalog::from_source("en", EN_FTL).unwrap();
    for kind in NotifyKind::ALL {
        let title = catalog.tr(&kind.title_id(), &[]);
        assert!(!title.to_lowercase().contains("machine"), "{title}");
        assert!(!title.contains('{'), "{title}");
    }
}

// ---- deep links ------------------------------------------------------------------------------------

#[test]
fn hub_links_parse_to_the_thread_id() {
    for (link, id) in [
        ("https://hub.example.com/t/thr_01HX", "thr_01HX"),
        ("https://hub.example.com/t/thr_01HX/", "thr_01HX"),
        ("https://HUB.example.com/t/a.b-c:d?x=1#frag", "a.b-c:d"),
        ("https://hub.example.com:443/t/abc", "abc"),
    ] {
        assert_eq!(hub_link_thread(link, HUB).as_deref(), Some(id), "{link}");
    }
    assert_eq!(
        hub_link_thread("http://127.0.0.1:8787/t/abc", "http://127.0.0.1:8787").as_deref(),
        Some("abc")
    );
}

#[test]
fn other_hosts_paths_and_schemes_are_rejected() {
    for link in [
        "https://evil.example.com/t/thr_1",
        "https://hub.example.com.evil.com/t/thr_1",
        "https://hub.example.com:8443/t/thr_1",
        "http://hub.example.com/t/thr_1",
        "ftp://hub.example.com/t/thr_1",
        "https://user@hub.example.com/t/thr_1",
        "https://hub.example.com/x/thr_1",
        "https://hub.example.com/t/",
        "https://hub.example.com/t",
        "https://hub.example.com/t/a/b",
        "https://hub.example.com/v1/t/thr_1",
        "https://hub.example.com/t/a%2Fb",
        "https://hub.example.com/t/..",
        "https://hub.example.com/",
        "not a url",
        "",
    ] {
        assert_eq!(hub_link_thread(link, HUB), None, "{link}");
    }
    assert_eq!(
        hub_link_thread("https://hub.example.com/t/a", "garbage"),
        None
    );
}

// ---- prefs -----------------------------------------------------------------------------------------

#[test]
fn prefs_default_on_and_persist_to_the_configured_file() {
    let config = NotifyConfig {
        prefs_path: temp_path("persist"),
    };
    let handle = PrefsHandle::load(&config);
    assert_eq!(handle.get(), NotifyPrefs::default());
    assert!(
        NotifyKind::ALL
            .iter()
            .all(|kind| handle.get().enabled(*kind))
    );
    handle.set(NotifyKind::Failed, false).unwrap();
    assert!(config.prefs_path.exists());
    let reloaded = PrefsHandle::load(&config).get();
    assert!(!reloaded.failed && reloaded.needs_you && reloaded.done);
}

#[test]
fn unreadable_prefs_fall_back_to_the_defaults() {
    let path = temp_path("corrupt");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(NotifyPrefs::load(&path), NotifyPrefs::default());
    std::fs::write(&path, r#"{"done": false}"#).unwrap();
    let prefs = NotifyPrefs::load(&path);
    assert!(!prefs.done && prefs.needs_you && prefs.failed);
}

#[test]
fn a_prefs_file_that_cannot_be_written_reports_the_error_but_applies() {
    let blocker = temp_path("blocked");
    std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
    std::fs::write(&blocker, "file").unwrap();
    // The parent "directory" is a file.
    let config = NotifyConfig {
        prefs_path: blocker.join("sub").join("notify.json"),
    };
    let handle = PrefsHandle::load(&config);
    assert!(handle.set(NotifyKind::Done, false).is_err());
    assert!(!handle.get().done);
}

// ---- strings ---------------------------------------------------------------------------------------

#[test]
fn every_string_id_the_notify_module_uses_is_in_the_catalog() {
    let catalog = Catalog::from_source("en", EN_FTL).expect("notify.ftl parses");
    let mut ids = BTreeSet::new();
    for source in [
        include_str!("../src/notify/view.rs"),
        include_str!("../src/notify/model.rs"),
    ] {
        for line in source.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut rest = line;
            while let Some(start) = rest.find("\"notify-") {
                rest = &rest[start + 1..];
                if let Some(end) = rest.find('"') {
                    ids.insert(rest[..end].to_owned());
                    rest = &rest[end..];
                }
            }
        }
    }
    for kind in NotifyKind::ALL {
        ids.insert(kind.title_id());
        ids.insert(format!("notify-kind-{}", kind.slug()));
        ids.insert(format!("notify-kind-{}-desc", kind.slug()));
    }
    // Element ids (not strings) that share the prefix.
    for element_id in [
        "notify-settings",
        "notify-save-error",
        "notify-toggle-",
        "notify-row-",
    ] {
        ids.remove(element_id);
    }
    ids.retain(|id| !id.ends_with('-') && !id.contains('{') && id != "notify-");
    assert!(ids.len() >= 12, "scan found only {} ids", ids.len());
    let missing: Vec<_> = ids.iter().filter(|id| !catalog.has(id)).collect();
    assert!(
        missing.is_empty(),
        "ids missing from notify.ftl: {missing:?}"
    );
    // And nothing in the catalog is dead.
    let unused: Vec<_> = Catalog::ids(EN_FTL)
        .into_iter()
        .filter(|id| !ids.contains(id) && id != "notify-save-failed")
        .collect();
    assert!(unused.is_empty(), "unused ids in notify.ftl: {unused:?}");
}

// ---- headless --------------------------------------------------------------------------------------

fn open<V: gpui_kit::Render + 'static>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> Entity<V> + 'static,
) -> (AnyWindowHandle, Entity<V>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(600.), px(500.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            build,
        )
        .expect("open window")
    })
}

#[gpui_kit::test]
fn the_settings_view_toggles_and_saves_each_kind(cx: &mut TestAppContext) {
    let config = NotifyConfig {
        prefs_path: temp_path("view"),
    };
    let prefs = PrefsHandle::load(&config);
    let shared = prefs.clone();
    let (window, _view) = open(cx, move |_, cx| cx.new(|_| NotifySettingsView::new(prefs)));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("notify-settings-title").label(),
            Some("Notifications")
        );
        for (slug, name) in [
            ("needs_you", "Needs you"),
            ("done", "Done"),
            ("failed", "Failed"),
        ] {
            assert_eq!(
                window.find(format!("notify-row-{slug}")).label(),
                Some(format!("{name} notifications, on").as_str())
            );
        }
        window.click("notify-toggle-done", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("notify-row-done").label(),
            Some("Done notifications, off")
        );
        assert_eq!(
            window.find("notify-row-failed").label(),
            Some("Failed notifications, on")
        );
    })
    .expect("window");
    assert!(!shared.get().done && shared.get().failed && shared.get().needs_you);
    assert!(!PrefsHandle::load(&config).get().done, "saved to the file");
    // The notifier built on the same handle obeys the switch.
    let mut notifier = Notifier::with_prefs(shared, Box::new(FakeSink::default()));
    assert!(notifier.observe("thr_1", "done").is_none());
    assert!(notifier.observe("thr_1", "failed").is_some());
}

#[gpui_kit::test]
fn the_toast_sink_shows_queued_notifications_in_the_window(cx: &mut TestAppContext) {
    let events = EventQueue::default();
    let sink = ToastSink::new(events.clone());
    let config = NotifyConfig {
        prefs_path: temp_path("toast"),
    };
    let mut notifier = Notifier::new(&config, Box::new(sink.clone()));
    let (window, _view) = open(cx, |_, cx| {
        cx.new(|_| {
            NotifySettingsView::new(PrefsHandle::load(&NotifyConfig {
                prefs_path: temp_path("toast-view"),
            }))
        })
    });
    notifier.observe("thr_1", "needs_you");
    notifier.observe("thr_2", "done");
    notifier.observe("thr_2", "done");
    assert_eq!(sink.pending(), 2);
    cx.update_window(window, |_, window, cx| {
        assert_eq!(window.notifications(cx).len(), 0);
        assert_eq!(sink.flush(window, cx), 2);
        window.render_frame(cx);
        assert_eq!(window.notifications(cx).len(), 2);
        assert_eq!(sink.flush(window, cx), 0, "nothing is shown twice");
    })
    .expect("window");
    assert_eq!(sink.pending(), 0);
    assert!(events.drain().is_empty(), "no click, no event");
}
