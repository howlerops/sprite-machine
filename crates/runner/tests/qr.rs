//! The session link, the terminal QR, the spacebar toggle and the `autoConnect` setting (M2B-009,
//! PLAN-v2-ade.md §4.10). All pure: no hub, no terminal.

use runner::qr::keys::{Latest, RawMode, pump};
use runner::qr::{QrToggle, block, mobile_url, render, session_url};
use runner::settings::{self, Settings};
use tempfile::TempDir;

const THREAD: &str = "th_01JBX2";

#[test]
fn the_session_url_is_the_hub_deep_link() {
    assert_eq!(
        session_url("https://hub.example", THREAD).unwrap(),
        "https://hub.example/t/th_01JBX2"
    );
    // A trailing slash on the hub does not double up.
    assert_eq!(
        session_url("https://hub.example/", THREAD).unwrap(),
        "https://hub.example/t/th_01JBX2"
    );
    // Loopback http is the dev hub; a plain http hub is refused (the credential would travel in
    // clear), and so is an id that is not a thread id.
    assert_eq!(
        session_url("http://127.0.0.1:8787", THREAD).unwrap(),
        "http://127.0.0.1:8787/t/th_01JBX2"
    );
    assert!(session_url("http://hub.example", THREAD).is_err());
    assert!(session_url("https://hub.example", "").is_err());
    assert!(session_url("https://hub.example", "th bad/../x").is_err());
}

#[test]
fn the_mobile_url_is_the_hub_web_page() {
    assert_eq!(
        mobile_url("https://hub.example/").unwrap(),
        "https://hub.example/"
    );
    assert!(mobile_url("ftp://hub.example").is_err());
}

#[test]
fn the_qr_is_a_multi_line_text_block_that_depends_on_the_data() {
    let url = session_url("https://hub.example", THREAD).unwrap();
    let qr = render(&url).expect("a session URL fits in a QR");
    assert!(qr.lines().count() > 10, "{qr}");
    assert!(
        qr.chars()
            .any(|c| matches!(c, '\u{2580}' | '\u{2584}' | '\u{2588}')),
        "{qr}"
    );
    // Another session has another code.
    let other = session_url("https://hub.example", "th_02ABCD").unwrap();
    assert_ne!(render(&other).unwrap(), qr);
    // Data too large for any QR code gives no block (the URL is still printed).
    assert!(render(&"a".repeat(5000)).is_none());
}

#[test]
fn the_block_has_the_qr_and_the_link() {
    let url = session_url("https://hub.example", THREAD).unwrap();
    let text = block(&url);
    assert!(text.contains(&url), "{text}");
    assert!(text.contains('\u{2580}') || text.contains('\u{2584}') || text.contains('\u{2588}'));
    assert!(text.contains(render(&url).unwrap().lines().next().unwrap()));
}

#[test]
fn space_toggles_the_qr_and_other_keys_do_nothing() {
    let url = session_url("https://hub.example", THREAD).unwrap();
    let mut toggle = QrToggle::default();
    assert!(!toggle.shown(), "the QR is hidden at start");
    assert_eq!(toggle.view(&url), "", "nothing is printed while hidden");

    // Other keys change nothing and are not handled.
    assert!(!toggle.on_key('q'));
    assert!(!toggle.on_key('\n'));
    assert!(!toggle.shown());

    assert!(toggle.on_key(' '), "a space is handled");
    assert!(toggle.shown());
    assert_eq!(
        toggle.view(&url),
        block(&url),
        "the QR shows when toggled on"
    );

    assert!(toggle.on_key(' '));
    assert!(!toggle.shown(), "a second space hides it again");
    assert_eq!(toggle.view(&url), "");
}

#[test]
fn auto_connect_is_off_by_default() {
    let dir = TempDir::new().unwrap();
    // No settings file: the defaults.
    assert_eq!(
        settings::load(dir.path()),
        Settings {
            auto_connect: false
        }
    );
    assert_eq!(
        settings::parse(b""),
        Settings {
            auto_connect: false
        }
    );
    assert_eq!(
        settings::parse(b"{}"),
        Settings {
            auto_connect: false
        }
    );
    // Unparsable, or not a boolean: the defaults too.
    assert_eq!(
        settings::parse(b"{ nope"),
        Settings {
            auto_connect: false
        }
    );
    assert_eq!(
        settings::parse(br#"{"autoConnect": "yes"}"#),
        Settings {
            auto_connect: false
        }
    );
}

#[test]
fn auto_connect_true_is_read_from_the_state_dir() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join(settings::FILE),
        br#"{"autoConnect": true, "allow_remote_terminal": false}"#,
    )
    .unwrap();
    assert_eq!(settings::load(dir.path()), Settings { auto_connect: true });

    std::fs::write(
        dir.path().join(settings::FILE),
        br#"{"autoConnect": false}"#,
    )
    .unwrap();
    assert_eq!(
        settings::load(dir.path()),
        Settings {
            auto_connect: false
        }
    );
}

// ---- the spacebar of `sm rc` (keys read from an injected reader) -------------------------------

fn latest_at(url: &str) -> Latest {
    let latest = Latest::default();
    latest.set(url);
    latest
}

/// The keys in `input` through the key reader; what it wrote.
fn keys(input: &[u8], latest: &Latest) -> String {
    let mut out = Vec::new();
    pump(input, latest, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn the_spacebar_shows_then_hides_the_qr_of_the_latest_url() {
    let url = session_url("https://hub.example", THREAD).unwrap();
    let latest = latest_at(&url);
    // A space shows the block, and the next space hides it (it prints nothing); other keys do
    // nothing.
    assert_eq!(keys(b" ", &latest), block(&url));
    assert_eq!(keys(b"  ", &latest), block(&url));
    assert_eq!(keys(b" x  ", &latest), block(&url).repeat(2));
}

#[test]
fn the_qr_shown_is_the_one_of_the_latest_session() {
    let first = session_url("https://hub.example", THREAD).unwrap();
    let second = session_url("https://hub.example", "th_02ABCD").unwrap();
    let latest = latest_at(&first);
    latest.set(&second);
    assert_eq!(keys(b" ", &latest), block(&second));
}

#[test]
fn other_keys_and_a_space_before_any_session_print_nothing() {
    let url = session_url("https://hub.example", THREAD).unwrap();
    assert_eq!(keys(b"abc\n", &latest_at(&url)), "");
    assert_eq!(keys(b" ", &Latest::default()), "");
}

#[test]
fn raw_mode_is_not_turned_on_without_a_terminal() {
    // No terminal: nothing is changed and there is no guard to restore anything.
    assert!(RawMode::enable(false).unwrap().is_none());
}
