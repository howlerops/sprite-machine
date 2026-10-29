//! M3-006: attachments (picker, paste, drag-drop), the upload step and inline images.
//!
//! - the model (limits, types, gate) and the HTTP uploader are tested natively, the uploader
//!   against a fake hub that serves `POST /v1/attachments`, the signed `PUT` and `/v1/threads`;
//! - the composers are driven headlessly with a fake file chooser and a fake uploader;
//! - the transcript pane draws image parts with a fake fetcher, and the HTTP fetcher is tested
//!   against the fake hub.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use base64::Engine as _;
use client_core::ThreadView;
use client_core::outbox::Intent;
use client_gpui::compose::attach::{
    AttachmentSet, Gate, HttpUploader, MAX_BYTES, Reject, State as AttachState, UploadError,
    UploadFuture, UploadJob, Uploader, check, format_size, guess_mime, keyed_by_parts, read_file,
};
use client_gpui::compose::{
    AttachBar, ComposeView, ComposerEvent, FileChooser, Status, Target, ThreadComposer,
};
use client_gpui::shell::i18n::Catalog;
use client_gpui::task::media::{
    self, HttpFetcher, ImageFetcher, LoadState, MediaCache, MediaRef, image_refs, sniff,
};
use client_gpui::task::transcript::thread_rows;
use client_gpui::task::{RowKind, TranscriptPane};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, Entity, ExternalPaths,
    FileDropEvent, Focusable as _, Image, ImageFormat, InputEvent as _, Point, Task,
    TestAppContext, WindowBounds, WindowOptions, point, px, size,
};
use protocol::tunnel::ContentPart;
use protocol::turn_event::Envelope;
use serde_json::{Value, json};

fn eid(text: String) -> gpui_kit::ElementId {
    gpui_kit::ElementId::Name(text.into())
}

// ---- fixtures ----------------------------------------------------------------------------------

/// A real 1x1 PNG.
fn png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
        .expect("png")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "client-gpui-attach-{name}-{}-{:08x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write file");
    path
}

fn sparse(dir: &Path, name: &str, len: u64) -> PathBuf {
    let path = dir.join(name);
    std::fs::File::create(&path)
        .expect("create")
        .set_len(len)
        .expect("set_len");
    path
}

fn file_part(name: &str, mime: &str, url: &str) -> ContentPart {
    ContentPart::File {
        name: name.into(),
        mime_type: mime.into(),
        url: url.into(),
        size: Some(1),
        sha256: None,
    }
}

// ---- fake hub ----------------------------------------------------------------------------------

#[derive(Default)]
struct Hub {
    creates: Mutex<Vec<(HeaderMap, Value)>>,
    puts: Mutex<Vec<(String, HeaderMap, Vec<u8>)>>,
    threads: Mutex<Vec<(HeaderMap, Value)>>,
    blobs: Mutex<BTreeMap<String, (String, Vec<u8>)>>,
    /// Answer `POST /v1/attachments` with this status (and an error body) when set.
    refuse_create: Mutex<Option<u16>>,
    /// Answer the signed `PUT` with this status when set.
    refuse_put: Mutex<Option<u16>>,
}

async fn create_attachment(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    hub.creates
        .lock()
        .expect("lock")
        .push((headers.clone(), body.clone()));
    if let Some(status) = *hub.refuse_create.lock().expect("lock") {
        return (
            StatusCode::from_u16(status).expect("status"),
            axum::Json(
                json!({"error": "invalid_request", "message": "an attachment is at most 20 MB"}),
            ),
        )
            .into_response();
    }
    let origin = format!("http://{}", headers["host"].to_str().expect("host"));
    let n = hub.creates.lock().expect("lock").len();
    let id = format!("att_{n}");
    let url = format!("{origin}/v1/attachments/org_1/{id}?exp=9&sig=get");
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "upload_url": format!("{origin}/v1/attachments/org_1/{id}/upload?size=1&mime=x&exp=9&sig=put"),
            "url": url,
            "expires_at": "2099-01-01T00:00:00.000Z",
            "part": {"type": "file", "url": url, "name": body["name"], "mime_type": body["mime_type"], "size": body["size"]},
        })),
    )
        .into_response()
}

async fn upload_attachment(
    State(hub): State<Arc<Hub>>,
    AxPath((_org, id)): AxPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    if let Some(status) = *hub.refuse_put.lock().expect("lock") {
        return StatusCode::from_u16(status)
            .expect("status")
            .into_response();
    }
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    hub.puts
        .lock()
        .expect("lock")
        .push((id.clone(), headers, body.to_vec()));
    hub.blobs
        .lock()
        .expect("lock")
        .insert(id, (mime, body.to_vec()));
    StatusCode::NO_CONTENT.into_response()
}

async fn read_attachment(
    State(hub): State<Arc<Hub>>,
    AxPath((_org, id)): AxPath<(String, String)>,
) -> axum::response::Response {
    match hub.blobs.lock().expect("lock").get(&id) {
        Some((mime, bytes)) => {
            ([(header::CONTENT_TYPE, mime.clone())], bytes.clone()).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn machines() -> axum::Json<Value> {
    axum::Json(json!({ "machines": [
        { "machine_id": "mch_laptop", "name": "my-laptop", "kind": "local",
          "presence": { "state": "online", "since": "2026-10-27T22:00:00.000Z" },
          "capabilities": { "harnesses": ["claude", "opencode"], "spawn": ["same_dir", "worktree"], "max_sessions": 32 } }
    ]}))
}

async fn create_thread(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    hub.threads.lock().expect("lock").push((headers, body));
    (
        StatusCode::CREATED,
        axum::Json(json!({ "thread_id": "th_01JBX2", "task_id": "tk_x", "status": "started" })),
    )
}

/// Returns the `…/v1` base URL.
fn spawn_hub(hub: Arc<Hub>) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("hub runtime");
        runtime.block_on(async move {
            let app = Router::new()
                .route("/v1/machines", get(machines))
                .route("/v1/threads", post(create_thread))
                .route("/v1/attachments", post(create_attachment))
                .route("/v1/attachments/{org}/{id}/upload", put(upload_attachment))
                .route("/v1/attachments/{org}/{id}", get(read_attachment))
                .with_state(hub);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind hub");
            sender
                .send(listener.local_addr().expect("addr").port())
                .expect("send port");
            axum::serve(listener, app).await.expect("serve");
        });
    });
    format!("http://127.0.0.1:{}/v1", receiver.recv().expect("hub port"))
}

// ---- fake uploader / chooser / fetcher -----------------------------------------------------------

/// Records every upload; names in `fail_once` fail the first time they are tried.
#[derive(Default)]
struct FakeUploader {
    calls: Mutex<Vec<String>>,
    fail_once: Mutex<HashSet<String>>,
}

impl FakeUploader {
    fn failing_once(names: &[&str]) -> Arc<Self> {
        let fake = Self::default();
        fake.fail_once
            .lock()
            .expect("lock")
            .extend(names.iter().map(|name| (*name).to_owned()));
        Arc::new(fake)
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("lock").clone()
    }
}

impl Uploader for FakeUploader {
    fn upload(&self, job: UploadJob) -> UploadFuture {
        self.calls.lock().expect("lock").push(job.name.clone());
        let fail = self.fail_once.lock().expect("lock").remove(&job.name);
        Box::pin(async move {
            job.progress.set(40);
            if fail {
                return Err(UploadError::Transport("connection reset".into()));
            }
            Ok(file_part(
                &job.name,
                &job.mime,
                &format!("https://hub.test/a/{}", job.name),
            ))
        })
    }
}

struct FakeChooser(Vec<PathBuf>);

impl FileChooser for FakeChooser {
    fn choose(&self, _: &mut App) -> Task<Vec<PathBuf>> {
        Task::ready(self.0.clone())
    }
}

/// Serves `images` by URL; anything else is an error.
struct FakeFetcher(BTreeMap<String, Vec<u8>>);

impl ImageFetcher for FakeFetcher {
    fn fetch(&self, url: &str) -> media::FetchFuture {
        let answer = self
            .0
            .get(url)
            .cloned()
            .ok_or_else(|| "HTTP 404".to_owned());
        Box::pin(async move { answer })
    }
}

// ---- model: limits, types, gate ---------------------------------------------------------------------

#[test]
fn the_hubs_size_limit_and_refused_types_are_enforced_before_upload() {
    assert_eq!(MAX_BYTES, 20 * 1024 * 1024, "client-api.md §5.6: 20 MB");
    assert_eq!(check("a.txt", "text/plain", MAX_BYTES), Ok(()));
    assert_eq!(
        check("a.txt", "text/plain", MAX_BYTES + 1),
        Err(Reject::TooLarge {
            name: "a.txt".into(),
            size: MAX_BYTES + 1
        })
    );
    assert_eq!(
        check("a.txt", "text/plain", 0),
        Err(Reject::Empty {
            name: "a.txt".into()
        })
    );
    assert!(matches!(
        check("setup.exe", "application/octet-stream", 10),
        Err(Reject::Type { .. })
    ));
    let mut set = AttachmentSet::new();
    assert!(
        set.add("big.bin", "application/octet-stream", Vec::new())
            .is_err()
    );
    assert!(set.is_empty(), "a refused file is never kept");
    let id = set
        .add("weird.dat", "not a mime", vec![1, 2, 3])
        .expect("added");
    assert_eq!(
        set.get(id).expect("item").mime,
        "application/octet-stream",
        "a mime type the hub would refuse is not sent"
    );
    assert_eq!(guess_mime("Photo.JPG"), "image/jpeg");
    assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
}

#[test]
fn files_are_checked_from_their_metadata_before_they_are_read() {
    let dir = temp_dir("read");
    let big = sparse(&dir, "big.bin", MAX_BYTES + 1);
    assert!(matches!(read_file(&big), Err(Reject::TooLarge { .. })));
    let exe = write(&dir, "setup.exe", b"MZ");
    assert!(matches!(read_file(&exe), Err(Reject::Type { .. })));
    assert!(matches!(read_file(&dir), Err(Reject::Unreadable { .. })));
    assert!(matches!(
        read_file(&dir.join("missing.txt")),
        Err(Reject::Unreadable { .. })
    ));
    let ok = read_file(&write(&dir, "shot.png", &png())).expect("read");
    assert_eq!(
        (ok.name.as_str(), ok.mime.as_str()),
        ("shot.png", "image/png")
    );
    assert_eq!(ok.bytes, png());
}

#[test]
fn the_gate_waits_for_uploads_and_blocks_on_a_failure_until_retried() {
    let mut set = AttachmentSet::new();
    assert_eq!(set.gate(), Gate::Empty);
    let a = set.add("a.txt", "text/plain", vec![1]).expect("a");
    let b = set.add("b.txt", "text/plain", vec![2]).expect("b");
    assert_eq!(set.gate(), Gate::Waiting);
    assert_eq!(set.pending_ids(), [a, b]);
    set.begin(a).expect("job");
    set.begin(b).expect("job");
    assert!(set.is_uploading());
    set.finish(a, Ok(file_part("a.txt", "text/plain", "https://h/a")));
    assert_eq!(set.gate(), Gate::Waiting, "b is still uploading");
    set.finish(b, Err(UploadError::TooLarge));
    assert_eq!(set.gate(), Gate::Blocked);
    assert!(matches!(
        set.get(b).expect("b").state,
        AttachState::Failed(_)
    ));
    assert!(set.retry(b));
    assert!(!set.retry(a), "only a failed attachment can be retried");
    set.begin(b).expect("job");
    set.finish(b, Ok(file_part("b.txt", "text/plain", "https://h/b")));
    match set.gate() {
        Gate::Ready(parts) => assert_eq!(
            parts,
            [
                file_part("a.txt", "text/plain", "https://h/a"),
                file_part("b.txt", "text/plain", "https://h/b")
            ],
            "parts keep the order the files were attached in"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn another_set_of_files_is_another_idempotency_key() {
    let a = [file_part("a", "text/plain", "https://h/a")];
    let b = [file_part("b", "text/plain", "https://h/b")];
    assert_eq!(keyed_by_parts("idk_1", &a), keyed_by_parts("idk_1", &a));
    assert_ne!(keyed_by_parts("idk_1", &a), keyed_by_parts("idk_1", &b));
    assert!(keyed_by_parts(&"k".repeat(300), &a).len() <= 128);
}

// ---- the HTTP uploader against the fake hub -----------------------------------------------------------

fn job(name: &str, mime: &str, bytes: &[u8]) -> UploadJob {
    UploadJob {
        name: name.into(),
        mime: mime.into(),
        bytes: Arc::new(bytes.to_vec()),
        idempotency_key: "idk_test".into(),
        progress: Default::default(),
    }
}

#[tokio::test]
async fn the_http_uploader_creates_puts_and_answers_the_file_part() {
    let hub = Arc::new(Hub::default());
    let api = spawn_hub(hub.clone());
    let uploader = HttpUploader::new(api, Some("tok".into()));
    let work = job("shot.png", "image/png", &png());
    let progress = work.progress.clone();
    let part = uploader.upload(work).await.expect("uploaded");
    let creates = hub.creates.lock().expect("lock");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].0["authorization"], "Bearer tok");
    assert_eq!(creates[0].0["idempotency-key"], "idk_test");
    assert_eq!(
        creates[0].1,
        json!({"name": "shot.png", "mime_type": "image/png", "size": png().len()})
    );
    let puts = hub.puts.lock().expect("lock");
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].2, png(), "the bytes reach the signed URL");
    assert_eq!(puts[0].1[header::CONTENT_TYPE], "image/png");
    assert!(
        !puts[0].1.contains_key(header::AUTHORIZATION),
        "the signed URL is the credential; the token is not sent to storage"
    );
    match part {
        ContentPart::File {
            name,
            mime_type,
            url,
            size,
            sha256,
        } => {
            assert_eq!(
                (name.as_str(), mime_type.as_str()),
                ("shot.png", "image/png")
            );
            assert!(url.contains("/v1/attachments/org_1/att_1?"), "{url}");
            assert_eq!(size, Some(png().len() as u64));
            assert_eq!(sha256.expect("sha256").len(), 64);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(progress.get(), 100);
}

#[tokio::test]
async fn hub_refusals_become_upload_errors_with_a_reason() {
    let hub = Arc::new(Hub::default());
    let api = spawn_hub(hub.clone());
    let uploader = HttpUploader::new(api.clone(), None);
    *hub.refuse_create.lock().expect("lock") = Some(413);
    assert_eq!(
        uploader
            .upload(job("a.bin", "application/x-bin", b"x"))
            .await,
        Err(UploadError::TooLarge)
    );
    *hub.refuse_create.lock().expect("lock") = Some(403);
    match uploader
        .upload(job("a.bin", "application/x-bin", b"x"))
        .await
    {
        Err(UploadError::Refused { status, code, .. }) => {
            assert_eq!((status, code.as_str()), (403, "invalid_request"));
        }
        other => panic!("{other:?}"),
    }
    *hub.refuse_create.lock().expect("lock") = None;
    *hub.refuse_put.lock().expect("lock") = Some(500);
    assert!(matches!(
        uploader
            .upload(job("a.bin", "application/x-bin", b"x"))
            .await,
        Err(UploadError::Refused { status: 500, .. })
    ));
    let down = HttpUploader::new("http://127.0.0.1:1/v1", None);
    assert!(matches!(
        down.upload(job("a.bin", "application/x-bin", b"x")).await,
        Err(UploadError::Transport(_))
    ));
    assert!(
        !UploadError::TooLarge.detail().is_empty() && !UploadError::NoHub.detail().is_empty(),
        "every error has words"
    );
}

// ---- strings ----------------------------------------------------------------------------------------

#[test]
fn every_attachment_and_media_string_id_is_in_the_catalog() {
    let catalog = Catalog::from_source("en", client_gpui::compose::EN_FTL).expect("parses");
    let mut ids = HashSet::new();
    for file in [
        "src/compose/attach.rs",
        "src/compose/attach_bar.rs",
        "src/task/media.rs",
        "src/task/transcript.rs",
    ] {
        let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(file))
            .expect("source");
        for part in source.split('"').skip(1).step_by(2) {
            if (part.starts_with("compose-attach-") || part.starts_with("compose-media-"))
                && !part.ends_with('-')
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                ids.insert(part.to_owned());
            }
        }
    }
    assert!(ids.len() > 20, "scan found only {} ids", ids.len());
    let args = [
        ("name", "n"),
        ("size", "s"),
        ("limit", "l"),
        ("mime", "m"),
        ("detail", "d"),
        ("status", "t"),
        ("percent", "50"),
        ("number", "1"),
        ("extension", "png"),
        ("message", "x"),
    ];
    for id in &ids {
        assert!(catalog.has(id), "{id} missing from compose.ftl");
        assert!(
            catalog.format(id, &args).is_some(),
            "{id} does not format with its arguments"
        );
    }
    // And the other way: nothing in the attachment section is unused.
    let ftl = client_gpui::compose::EN_FTL;
    let section = ftl
        .split("# Attachments")
        .nth(1)
        .expect("attachment section");
    for line in section.lines() {
        if let Some((id, _)) = line.split_once(" = ") {
            assert!(ids.contains(id.trim()), "{id} is defined but unused");
        }
    }
}

// ---- headless: the thread composer ------------------------------------------------------------------

type Events = Arc<Mutex<Vec<ComposerEvent>>>;

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(900.), px(900.)),
        })),
        ..Default::default()
    }
}

fn open_thread_composer(
    cx: &mut TestAppContext,
    uploader: Arc<dyn Uploader>,
    chosen: Vec<PathBuf>,
) -> (AnyWindowHandle, Entity<ThreadComposer>, Events) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        client_gpui::compose::init(cx);
    });
    let (window, composer) = cx.update(|cx| {
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| ThreadComposer::new(window, cx))
        })
        .expect("window")
    });
    let events: Events = Arc::default();
    let sink = events.clone();
    cx.update(|cx| {
        cx.subscribe(&composer, move |_, event: &ComposerEvent, _| {
            sink.lock().expect("lock").push(event.clone());
        })
        .detach();
    });
    cx.update_window(window, |_, window, cx| {
        let bar = composer.read(cx).attachments().clone();
        bar.update(cx, |bar, _| {
            bar.set_uploader(uploader);
            bar.set_chooser(Rc::new(FakeChooser(chosen)));
        });
        composer.update(cx, |composer, cx| {
            composer.set_thread(&ThreadView::new("th_1"), cx)
        });
        window.render_frame(cx);
    })
    .expect("window");
    (window, composer, events)
}

fn bar_of(cx: &mut TestAppContext, composer: &Entity<ThreadComposer>) -> Entity<AttachBar> {
    cx.update(|cx| composer.read(cx).attachments().clone())
}

/// Let background reads and uploads finish (real time: they run on their own threads).
fn settle(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    bar: &Entity<AttachBar>,
    done: impl Fn(&AttachBar) -> bool,
) {
    for _ in 0..400 {
        cx.run_until_parked();
        // Pump outside the window update so the notifications are flushed before the frame.
        cx.update(|cx| {
            bar.update(cx, |bar, cx| {
                bar.pump(cx);
            });
        });
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("window");
        if cx.update(|cx| done(bar.read(cx))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out");
}

fn type_text(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    composer: &Entity<ThreadComposer>,
    text: &str,
) {
    cx.update_window(window, |_, window, cx| {
        let focus = composer.read(cx).prompt_input().focus_handle(cx);
        window.focus(&focus, cx);
        window.input(text, cx);
        window.render_frame(cx);
    })
    .expect("window");
}

fn chip_names(cx: &mut TestAppContext, bar: &Entity<AttachBar>) -> Vec<String> {
    cx.update(|cx| {
        bar.read(cx)
            .set()
            .items()
            .iter()
            .map(|item| item.name.clone())
            .collect()
    })
}

#[gpui_kit::test]
fn the_picker_adds_chips_with_name_and_size_and_remove_drops_them(cx: &mut TestAppContext) {
    let dir = temp_dir("picker");
    let notes = write(&dir, "notes.md", &vec![b'x'; 1536]);
    let shot = write(&dir, "shot.png", &png());
    let (window, composer, _) =
        open_thread_composer(cx, FakeUploader::failing_once(&[]), vec![notes, shot]);
    let bar = bar_of(cx, &composer);
    cx.update_window(window, |_, window, cx| {
        assert!(
            !window
                .find("attach-add")
                .label()
                .unwrap_or_default()
                .is_empty()
        );
        window.click("attach-add", cx);
    })
    .expect("window");
    settle(cx, window, &bar, |bar| bar.set().items().len() == 2);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let items = bar.read(cx).set().items().to_vec();
        assert_eq!(
            window.find(eid(format!("attach-{}", items[0].id))).label(),
            Some("notes.md, 1.5 KB, ready to upload")
        );
        assert_eq!(
            window.find(eid(format!("attach-{}", items[1].id))).label(),
            Some(format!("shot.png, {} B, ready to upload", png().len()).as_str())
        );
        window.click(eid(format!("attach-remove-{}", items[0].id)), cx);
    })
    .expect("window");
    assert_eq!(chip_names(cx, &bar), ["shot.png"]);
}

#[gpui_kit::test]
fn files_over_the_limit_or_of_a_refused_type_are_rejected_with_a_message(cx: &mut TestAppContext) {
    let dir = temp_dir("reject");
    let big = sparse(&dir, "huge.bin", MAX_BYTES + 1);
    let exe = write(&dir, "setup.exe", b"MZ");
    let ok = write(&dir, "ok.txt", b"fine");
    let uploader = FakeUploader::failing_once(&[]);
    let (window, composer, _) = open_thread_composer(cx, uploader.clone(), vec![big, exe, ok]);
    let bar = bar_of(cx, &composer);
    cx.update_window(window, |_, window, cx| window.click("attach-add", cx))
        .expect("window");
    settle(cx, window, &bar, |bar| bar.rejections().len() == 2);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("attach-rejected-0").label(),
            Some("huge.bin is 20.0 MB, over the 20.0 MB limit, so it was not attached.")
        );
        assert_eq!(
            window.find("attach-rejected-1").label(),
            Some("setup.exe (application/octet-stream) is a type that cannot be attached.")
        );
        window.click("attach-rejected-dismiss-0", cx);
    })
    .expect("window");
    assert_eq!(
        chip_names(cx, &bar),
        ["ok.txt"],
        "the good file is still attached"
    );
    cx.update(|cx| assert_eq!(bar.read(cx).rejections().len(), 1));
    assert!(
        uploader.calls().is_empty(),
        "nothing is uploaded before send"
    );
}

fn drop_files(cx: &mut TestAppContext, window: AnyWindowHandle, paths: Vec<PathBuf>) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let at = point(px(20.), px(20.));
        window.dispatch_event(
            FileDropEvent::Entered {
                position: at,
                paths: ExternalPaths(paths.into_iter().collect()),
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            FileDropEvent::Submit { position: at }.to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    })
    .expect("window");
}

#[gpui_kit::test]
fn dropping_files_on_the_thread_composer_attaches_them(cx: &mut TestAppContext) {
    let dir = temp_dir("drop");
    let a = write(&dir, "a.txt", b"a");
    let b = write(&dir, "b.txt", b"bb");
    let (window, composer, _) = open_thread_composer(cx, FakeUploader::failing_once(&[]), vec![]);
    let bar = bar_of(cx, &composer);
    drop_files(cx, window, vec![a, b]);
    settle(cx, window, &bar, |bar| bar.set().items().len() == 2);
    assert_eq!(chip_names(cx, &bar), ["a.txt", "b.txt"]);
}

#[gpui_kit::test]
fn pasting_an_image_attaches_it_and_text_still_pastes_as_text(cx: &mut TestAppContext) {
    let (window, composer, _) = open_thread_composer(cx, FakeUploader::failing_once(&[]), vec![]);
    let bar = bar_of(cx, &composer);
    // The real path: clipboard, focus in the field, the paste shortcut.
    cx.update(|cx| {
        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Png,
            png(),
        )));
    });
    cx.update_window(window, |_, window, cx| {
        let focus = composer.read(cx).prompt_input().focus_handle(cx);
        window.focus(&focus, cx);
        window.press("ctrl-v", cx);
        window.render_frame(cx);
    })
    .expect("window");
    assert_eq!(chip_names(cx, &bar), ["pasted-image-1.png"]);
    cx.update(|cx| {
        let item = bar.read(cx).set().items()[0].clone();
        assert_eq!(item.mime, "image/png");
        assert!(item.is_image());
        assert_eq!(item.size, png().len() as u64);
        assert_eq!(
            composer.read(cx).prompt_input().read(cx).value().as_ref(),
            ""
        );
    });
    // Text on the clipboard is not an attachment and is left to the field.
    cx.update(|cx| {
        let swallowed = bar.update(cx, |bar, cx| {
            bar.paste(&ClipboardItem::new_string("hello".into()), cx)
        });
        assert!(!swallowed);
    });
    assert_eq!(chip_names(cx, &bar).len(), 1);
}

#[gpui_kit::test]
fn send_uploads_first_then_the_prompt_carries_text_and_file_parts(cx: &mut TestAppContext) {
    let dir = temp_dir("send");
    let notes = write(&dir, "notes.md", b"# notes");
    let shot = write(&dir, "shot.png", &png());
    let uploader = FakeUploader::failing_once(&[]);
    let (window, composer, events) = open_thread_composer(cx, uploader.clone(), vec![notes, shot]);
    let bar = bar_of(cx, &composer);
    cx.update_window(window, |_, window, cx| window.click("attach-add", cx))
        .expect("window");
    settle(cx, window, &bar, |bar| bar.set().items().len() == 2);
    type_text(cx, window, &composer, "look at these");
    cx.update_window(window, |_, window, cx| window.click("thread-send", cx))
        .expect("window");
    // Nothing is sent until the uploads are done.
    assert!(events.lock().expect("lock").is_empty());
    settle(cx, window, &bar, |_| {
        !events.lock().expect("lock").is_empty()
    });
    assert_eq!(uploader.calls().len(), 2);
    let events = events.lock().expect("lock").clone();
    assert_eq!(events.len(), 1, "{events:?}");
    let ComposerEvent::Intent(Intent::Prompt {
        thread_id, parts, ..
    }) = &events[0]
    else {
        panic!("{:?}", events[0]);
    };
    assert_eq!(thread_id, "th_1");
    assert_eq!(
        parts,
        &[
            ContentPart::Text {
                text: "look at these".into()
            },
            file_part("notes.md", "text/markdown", "https://hub.test/a/notes.md"),
            file_part("shot.png", "image/png", "https://hub.test/a/shot.png"),
        ]
    );
    cx.update(|cx| {
        assert!(bar.read(cx).is_empty(), "sent attachments are cleared");
        assert_eq!(
            composer.read(cx).prompt_input().read(cx).value().as_ref(),
            ""
        );
    });
}

#[gpui_kit::test]
fn a_failed_upload_blocks_send_until_it_is_retried(cx: &mut TestAppContext) {
    let dir = temp_dir("fail");
    let a = write(&dir, "a.txt", b"a");
    let b = write(&dir, "b.txt", b"b");
    let uploader = FakeUploader::failing_once(&["b.txt"]);
    let (window, composer, events) = open_thread_composer(cx, uploader.clone(), vec![a, b]);
    let bar = bar_of(cx, &composer);
    cx.update_window(window, |_, window, cx| window.click("attach-add", cx))
        .expect("window");
    settle(cx, window, &bar, |bar| bar.set().items().len() == 2);
    type_text(cx, window, &composer, "go");
    cx.update_window(window, |_, window, cx| window.click("thread-send", cx))
        .expect("window");
    settle(cx, window, &bar, |bar| bar.is_blocked());
    let failed_id = cx.update(|cx| bar.read(cx).set().items()[1].id);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("attach-blocked").visible());
        assert_eq!(
            window
                .find(eid(format!("attach-status-{failed_id}")))
                .label(),
            Some("upload failed: could not reach the hub (connection reset)")
        );
        assert!(
            window
                .find(eid(format!("attach-retry-{failed_id}")))
                .visible()
        );
    })
    .expect("window");
    assert!(
        events.lock().expect("lock").is_empty(),
        "the send is blocked"
    );
    // Send again while still failed: still blocked, and the good upload is not repeated.
    cx.update_window(window, |_, window, cx| window.click("thread-send", cx))
        .expect("window");
    assert!(events.lock().expect("lock").is_empty());
    assert_eq!(uploader.calls(), ["a.txt", "b.txt"]);
    // Retry uploads only the failed one; then send goes through.
    cx.update_window(window, |_, window, cx| {
        window.click(eid(format!("attach-retry-{failed_id}")), cx);
    })
    .expect("window");
    settle(cx, window, &bar, |bar| {
        matches!(bar.set().gate(), Gate::Ready(_))
    });
    assert_eq!(uploader.calls(), ["a.txt", "b.txt", "b.txt"]);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_all("attach-blocked").is_empty());
        window.click("thread-send", cx);
    })
    .expect("window");
    let events = events.lock().expect("lock").clone();
    let ComposerEvent::Intent(Intent::Prompt { parts, .. }) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(parts.len(), 3, "text and both files");
}

#[gpui_kit::test]
fn text_without_attachments_still_sends_as_a_plain_send_event(cx: &mut TestAppContext) {
    let (window, composer, events) =
        open_thread_composer(cx, FakeUploader::failing_once(&[]), vec![]);
    type_text(cx, window, &composer, "just text");
    cx.update_window(window, |_, window, cx| window.click("thread-send", cx))
        .expect("window");
    assert_eq!(
        events.lock().expect("lock").as_slice(),
        [ComposerEvent::Send {
            thread_id: "th_1".into(),
            text: "just text".into()
        }]
    );
}

// ---- headless: the new-task composer against the fake hub ------------------------------------------------

fn open_new_task(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<ComposeView>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        client_gpui::compose::init(cx);
    });
    cx.update(|cx| {
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| ComposeView::new(None, window, cx))
        })
        .expect("open window")
    })
}

fn wait_view(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<ComposeView>,
    done: impl Fn(&mut gpui_kit::Window, &mut App) -> bool,
) {
    for _ in 0..400 {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| view.pump(window, cx));
        })
        .expect("window");
        cx.update(|cx| {
            let bar = view.read(cx).attachments().clone();
            bar.update(cx, |bar, cx| {
                bar.pump(cx);
            });
        });
        let finished = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                done(window, cx)
            })
            .expect("window");
        if finished {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out");
}

#[gpui_kit::test]
fn the_new_task_uploads_attachments_to_the_hub_then_creates_the_thread_with_their_parts(
    cx: &mut TestAppContext,
) {
    let hub = Arc::new(Hub::default());
    let api = spawn_hub(hub.clone());
    let dir = temp_dir("newtask");
    let shot = write(&dir, "shot.png", &png());
    let (window, view) = open_new_task(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.set_target(
                Some(Target {
                    api: api.clone(),
                    token: Some("tok".to_owned()),
                }),
                window,
                cx,
            );
        });
    })
    .expect("window");
    wait_view(cx, window, &view, |window, cx| {
        view.read(cx).model().selected_machine().is_some()
            && window.find("agent-opencode").visible()
    });
    let bar = cx.update(|cx| view.read(cx).attachments().clone());
    cx.update_window(window, |_, window, cx| {
        let workspace = view.read(cx).workspace_input().focus_handle(cx);
        window.focus(&workspace, cx);
        window.input("/Users/me/src/app", cx);
        let prompt = view.read(cx).prompt_input().focus_handle(cx);
        window.focus(&prompt, cx);
        window.input("Fix the layout in the screenshot", cx);
        window.render_frame(cx);
    })
    .expect("window");
    // Dropped onto the composer.
    drop_files(cx, window, vec![shot]);
    wait_view(cx, window, &view, |_, cx| {
        bar.read(cx).set().items().len() == 1
    });
    cx.update_window(window, |_, window, cx| {
        assert!(
            hub.creates.lock().expect("lock").is_empty(),
            "no upload before send"
        );
        window.click("compose-send", cx);
    })
    .expect("window");
    wait_view(cx, window, &view, |_, cx| {
        matches!(view.read(cx).status(), Status::Sent { .. })
    });
    let creates = hub.creates.lock().expect("lock");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].0["authorization"], "Bearer tok");
    assert_eq!(creates[0].1["name"], "shot.png");
    let puts = hub.puts.lock().expect("lock");
    assert_eq!(puts[0].2, png());
    let threads = hub.threads.lock().expect("lock");
    assert_eq!(threads.len(), 1);
    let parts = threads[0].1["prompt"]["parts"].as_array().expect("parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[0],
        json!({"type": "text", "text": "Fix the layout in the screenshot"})
    );
    assert_eq!(parts[1]["type"], "file");
    assert_eq!(parts[1]["name"], "shot.png");
    assert_eq!(parts[1]["mime_type"], "image/png");
    assert!(
        parts[1]["url"]
            .as_str()
            .expect("url")
            .contains("/v1/attachments/org_1/att_1"),
        "{parts:?}"
    );
    cx.update(|cx| {
        assert!(
            bar.read(cx).is_empty(),
            "attachments clear once the task started"
        )
    });
}

#[gpui_kit::test]
fn signed_out_the_new_task_composer_cannot_upload_and_says_so(cx: &mut TestAppContext) {
    let (window, view) = open_new_task(cx);
    let bar = cx.update(|cx| view.read(cx).attachments().clone());
    cx.update(|cx| {
        bar.update(cx, |bar, cx| {
            bar.add_bytes("a.txt", "text/plain", b"a".to_vec(), cx);
            bar.request_send(cx);
        });
    });
    settle(cx, window, &bar, |bar| bar.is_blocked());
    cx.update(|cx| {
        let item = bar.read(cx).set().items()[0].clone();
        assert_eq!(
            item.state,
            AttachState::Failed("Sign in to attach files.".into())
        );
    });
}

// ---- inline images ---------------------------------------------------------------------------------------

fn thread_with_image(url: &str) -> ThreadView {
    let mut thread = ThreadView::new("th_1");
    let event = json!({"type": "ToolCallResult", "tool_call_id": "t",
    "content": [
        {"type": "text", "text": "Took a screenshot"},
        {"type": "file", "name": "screen.png", "mime_type": "image/png", "url": url, "size": 67}
    ]});
    let envelope: Envelope = serde_json::from_value(json!({
        "v": 1, "seq": 1, "thread_id": "th_1", "task_id": "tk_1", "run_id": "run_1",
        "ts": "2026-10-27T22:00:00.000Z", "event": event
    }))
    .expect("envelope");
    thread.apply(&envelope).expect("apply");
    thread
}

#[test]
fn image_parts_become_taller_image_rows_after_the_tool_text() {
    let thread = thread_with_image("https://hub.test/a/screen.png?sig=1");
    let rows = thread_rows(&thread);
    let at = rows
        .iter()
        .position(|row| row.kind == RowKind::Image)
        .expect("an image row");
    let media = rows[at].media.as_ref().expect("media");
    assert_eq!(media.url, "https://hub.test/a/screen.png?sig=1");
    assert_eq!(media.name, "screen.png");
    assert_eq!(rows[at].height(), media::THUMB_ROW_HEIGHT);
    assert!(rows[at].height() > rows[0].height());
    assert!(
        rows[..at].iter().any(|row| row.text == "Took a screenshot"),
        "the image follows the tool's text"
    );
    // A non-image file, a file: URL and an svg stay text.
    let parts = [
        file_part("a.pdf", "application/pdf", "https://h/a.pdf"),
        file_part("b.png", "image/png", "file:///etc/passwd"),
        file_part("c.svg", "image/svg+xml", "https://h/c.svg"),
    ];
    assert!(image_refs(&parts).is_empty());
}

#[test]
fn the_cache_loads_sniffs_and_reports_failures() {
    let mut images = BTreeMap::new();
    images.insert("https://h/ok.png".to_owned(), png());
    images.insert(
        "https://h/page.png".to_owned(),
        b"<html>login</html>".to_vec(),
    );
    let mut cache = MediaCache::new(Arc::new(FakeFetcher(images)));
    assert!(cache.ensure("https://h/ok.png"));
    assert!(!cache.ensure("https://h/ok.png"), "one fetch per url");
    cache.ensure("https://h/page.png");
    cache.ensure("https://h/gone.png");
    assert!(matches!(
        cache.state("https://h/ok.png"),
        Some(LoadState::Loading)
    ));
    for _ in 0..200 {
        if !cache.pump().1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(cache.state("https://h/ok.png").expect("state").is_ready());
    assert!(
        matches!(cache.state("https://h/page.png"), Some(LoadState::Failed(d)) if d == "not an image"),
        "a wrong Content-Type is caught by sniffing the bytes"
    );
    assert!(
        matches!(cache.state("https://h/gone.png"), Some(LoadState::Failed(d)) if d == "HTTP 404")
    );
    assert_eq!(sniff(&png()), Some(ImageFormat::Png));
}

#[tokio::test]
async fn the_http_fetcher_gets_bytes_and_reports_a_missing_image() {
    let hub = Arc::new(Hub::default());
    let api = spawn_hub(hub.clone());
    hub.blobs
        .lock()
        .expect("lock")
        .insert("att_9".into(), ("image/png".into(), png()));
    let origin = api.trim_end_matches("/v1");
    let got = HttpFetcher
        .fetch(&format!(
            "{origin}/v1/attachments/org_1/att_9?exp=9&sig=get"
        ))
        .await
        .expect("bytes");
    assert_eq!(got, png());
    assert_eq!(
        HttpFetcher
            .fetch(&format!("{origin}/v1/attachments/org_1/missing"))
            .await,
        Err("HTTP 404".to_owned())
    );
}

fn open_pane(
    cx: &mut TestAppContext,
    images: BTreeMap<String, Vec<u8>>,
) -> (AnyWindowHandle, Entity<TranscriptPane>) {
    cx.update(gpui_kit::init);
    let (window, pane) = cx.update(|cx| {
        gpui_kit::open_window(window_options(), cx, |_, cx| {
            cx.new(|_| TranscriptPane::new())
        })
        .expect("open test window")
    });
    cx.update(|cx| {
        pane.update(cx, |pane, _| {
            pane.set_image_fetcher(Arc::new(FakeFetcher(images)));
        });
    });
    (window, pane)
}

fn settle_pane(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    pane: &Entity<TranscriptPane>,
    url: &str,
) {
    for _ in 0..400 {
        cx.run_until_parked();
        cx.update(|cx| pane.update(cx, |pane, cx| pane.pump_images(cx)));
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .expect("window");
        let done = cx.update(|cx| {
            pane.read(cx)
                .image_state(url)
                .is_some_and(|state| !matches!(state, LoadState::Loading))
        });
        if done {
            // One more frame so the state reaches the screen.
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .expect("window");
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out");
}

#[gpui_kit::test]
fn an_image_part_renders_inline_and_a_click_opens_it_full_size(cx: &mut TestAppContext) {
    let url = "https://hub.test/a/screen.png?sig=1";
    let (window, pane) = open_pane(cx, BTreeMap::from([(url.to_owned(), png())]));
    let thread = thread_with_image(url);
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    settle_pane(cx, window, &pane, url);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(media::READY_ID).label(),
            Some("Image screen.png")
        );
        assert!(window.find_all(media::UNAVAILABLE_ID).is_empty());
        assert!(window.find_all(media::PANEL_ID).is_empty());
        assert_eq!(
            window.find(media::THUMB_ID).label(),
            Some("Open image screen.png full size")
        );
        window.click(media::THUMB_ID, cx);
        window.render_frame(cx);
        assert!(window.find(media::PANEL_ID).visible());
        assert_eq!(
            window.find(media::PANEL_ID).label(),
            Some("Image screen.png")
        );
        assert_eq!(
            window.find_all(media::READY_ID).len(),
            2,
            "thumbnail and full size"
        );
        window.click(media::CLOSE_ID, cx);
        window.render_frame(cx);
        assert!(window.find_all(media::PANEL_ID).is_empty());
    })
    .expect("window");
    cx.update(|cx| assert!(pane.read(cx).open_image_ref().is_none()));
}

#[gpui_kit::test]
fn an_image_that_cannot_load_shows_a_fallback_note_instead(cx: &mut TestAppContext) {
    let url = "https://hub.test/a/screen.png?sig=expired";
    let (window, pane) = open_pane(cx, BTreeMap::new());
    let thread = thread_with_image(url);
    cx.update(|cx| pane.update(cx, |pane, cx| pane.set_thread(&thread, cx)));
    settle_pane(cx, window, &pane, url);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(media::UNAVAILABLE_ID).label(),
            Some("Image screen.png could not be loaded.")
        );
        assert!(window.find_all(media::READY_ID).is_empty());
        // It can still be opened; the panel says the same.
        window.click(media::THUMB_ID, cx);
        window.render_frame(cx);
        assert!(window.find(media::PANEL_ID).visible());
        assert_eq!(window.find_all(media::UNAVAILABLE_ID).len(), 2);
    })
    .expect("window");
    let state = cx.update(|cx| pane.read(cx).image_state(url).cloned());
    assert!(state.expect("state").is_failed());
}

#[test]
fn media_refs_compare_by_value() {
    let a = MediaRef {
        name: "a".into(),
        mime: "image/png".into(),
        url: "https://h/a".into(),
    };
    assert_eq!(a.clone(), a);
}
