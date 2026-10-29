//! [`AttachBar`]: the attachments strip both composers embed. It owns the pending files
//! ([`AttachmentSet`]), the injectable [`FileChooser`] behind the "Attach files" button and the
//! injectable [`Uploader`], shows each file as a chip (name, size, state, Retry, Remove) and
//! the reasons files were refused, and tells its composer when a send may go
//! ([`AttachEvent::ReadyToSend`]).
//!
//! Files arrive three ways, all through here: the picker ([`AttachBar::open_picker`]), a paste
//! of an image or of copied files ([`AttachBar::paste`], wired to the composer's text field),
//! and a drop onto the composer ([`AttachBar::add_paths`], wired to `on_drop`).
//!
//! Uploads start when the composer asks to send ([`AttachBar::request_send`]), one thread per
//! file, and are polled with `try_recv` like the composer's other background jobs.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, *};
use gpui_kit::prelude::*;
use gpui_kit::*;
use protocol::tunnel::ContentPart;

use super::attach::{
    AttachmentSet, Gate, Item, NoUploader, Reject, State, UploadError, Uploader, format_size,
    read_file,
};
use super::{tr, tr_args};

/// Opens a file chooser and answers the chosen paths (empty when cancelled). Injectable: the
/// app uses [`SystemChooser`], tests a fake.
pub trait FileChooser {
    fn choose(&self, cx: &mut App) -> Task<Vec<PathBuf>>;
}

/// The platform's open-file dialog (several files, no folders).
pub struct SystemChooser;

impl FileChooser for SystemChooser {
    fn choose(&self, cx: &mut App) -> Task<Vec<PathBuf>> {
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |_| match answer.await {
            Ok(Ok(Some(paths))) => paths,
            _ => Vec::new(),
        })
    }
}

/// Tracks an external file drag over a composer and hands the files over when it is released
/// there. The composer wires three things: `on_drag_move::<ExternalPaths>` calls
/// [`DropZone::moved`], [`DropZone::release_marker`] goes inside it (it needs `.relative()`),
/// and its redraw reads [`DropZone::is_over`].
///
/// Not `on_drop` or `capture_any_mouse_up`: both only fire for a hovered element, and gpui
/// does not count a file drag as mouse input, so after the user typed the composer still
/// counts as "not hovered" and the drop would be lost. The release is read with a raw
/// window-level listener instead.
#[derive(Clone, Default)]
pub struct DropZone {
    over: Rc<RefCell<Option<Vec<PathBuf>>>>,
}

impl DropZone {
    pub fn new() -> Self {
        Self::default()
    }

    /// The drag moved: remember its files while it is inside the zone. Returns whether the
    /// "files are over the composer" state changed (so the view should redraw).
    pub fn moved(&self, event: &DragMoveEvent<ExternalPaths>, cx: &App) -> bool {
        let inside = event.bounds.contains(&event.event.position);
        let was_over = self.is_over();
        *self.over.borrow_mut() = inside.then(|| event.drag(cx).paths().to_vec());
        was_over != inside
    }

    /// `true` while dragged files are over the zone.
    pub fn is_over(&self) -> bool {
        self.over.borrow().is_some()
    }

    /// An invisible element that fills its (relatively positioned) parent and calls
    /// `on_release` with the files when a drag that was over the zone is released. A click
    /// forgets the drag.
    pub fn release_marker(
        &self,
        on_release: impl Fn(Vec<PathBuf>, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        let over = self.over.clone();
        let on_release = Rc::new(on_release);
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let (up_over, up_release) = (over.clone(), on_release.clone());
                window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
                    if phase.capture() {
                        let files = up_over.borrow_mut().take();
                        if let Some(files) = files
                            && cx.has_active_drag()
                        {
                            up_release(files, window, cx);
                        }
                    }
                });
                let down_over = over.clone();
                window.on_mouse_event(move |_: &MouseDownEvent, _, _, _| {
                    down_over.borrow_mut().take();
                });
            },
        )
        .absolute()
        .size_full()
    }
}

/// What the strip tells its composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// A send was requested and every attachment is now uploaded: ask again with
    /// [`AttachBar::request_send`] to get the parts.
    ReadyToSend,
}

type Answer = Result<ContentPart, UploadError>;

pub struct AttachBar {
    set: AttachmentSet,
    uploader: Arc<dyn Uploader>,
    chooser: Rc<dyn FileChooser>,
    jobs: Vec<(u64, Receiver<Answer>)>,
    rejections: Vec<Reject>,
    /// A send is waiting for the uploads.
    send_wanted: bool,
    /// The last send was refused because an upload failed.
    blocked: bool,
    polling: bool,
    pasted: u32,
}

impl EventEmitter<AttachEvent> for AttachBar {}

/// Run `future` on its own thread and runtime; the answer is read with `try_recv`.
fn run_off_thread<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> Receiver<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let _ = sender.send(runtime.block_on(future));
        }
    });
    receiver
}

impl Default for AttachBar {
    fn default() -> Self {
        Self::new()
    }
}

impl AttachBar {
    /// Signed out (every upload fails) with the system file chooser.
    pub fn new() -> Self {
        Self {
            set: AttachmentSet::new(),
            uploader: Arc::new(NoUploader),
            chooser: Rc::new(SystemChooser),
            jobs: Vec::new(),
            rejections: Vec::new(),
            send_wanted: false,
            blocked: false,
            polling: false,
            pasted: 0,
        }
    }

    pub fn set_uploader(&mut self, uploader: Arc<dyn Uploader>) {
        self.uploader = uploader;
    }

    pub fn set_chooser(&mut self, chooser: Rc<dyn FileChooser>) {
        self.chooser = chooser;
    }

    pub fn set(&self) -> &AttachmentSet {
        &self.set
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// `true` while uploads run or a send waits for them.
    pub fn is_busy(&self) -> bool {
        self.send_wanted || !self.jobs.is_empty()
    }

    /// The reasons files were refused, until dismissed or the next batch.
    pub fn rejections(&self) -> &[Reject] {
        &self.rejections
    }

    /// `true` when the last send was refused for a failed upload.
    pub fn is_blocked(&self) -> bool {
        self.blocked
    }

    /// Add a file already in memory. A refusal is recorded and shown.
    pub fn add_bytes(
        &mut self,
        name: &str,
        mime: &str,
        bytes: Vec<u8>,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let added = match self.set.add(name, mime, bytes) {
            Ok(id) => Some(id),
            Err(reject) => {
                self.rejections.push(reject);
                None
            }
        };
        self.blocked = false;
        cx.notify();
        added
    }

    /// Add the files at `paths` (the picker's answer, or a drop). They are read off the UI
    /// thread; a folder, a missing file, a program or a file over 20 MB is refused with a
    /// message and never read.
    pub fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { paths.iter().map(|path| read_file(path)).collect::<Vec<_>>() })
                .await;
            this.update(cx, |this, cx| {
                this.rejections.clear();
                for result in read {
                    match result {
                        Ok(picked) => {
                            this.add_bytes(&picked.name, &picked.mime, picked.bytes, cx);
                        }
                        Err(reject) => this.rejections.push(reject),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The "Attach files" button: ask the chooser, add what it answers.
    pub fn open_picker(&mut self, cx: &mut Context<Self>) {
        let chosen = self.chooser.choose(cx);
        cx.spawn(async move |this, cx| {
            let paths = chosen.await;
            this.update(cx, |this, cx| this.add_paths(paths, cx)).ok();
        })
        .detach();
    }

    /// A paste into the composer's text field. Images and copied files become attachments.
    /// Returns `true` when the paste was taken (the field must not insert it): copied files
    /// always, an image only when the clipboard has no text to paste with it.
    pub fn paste(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        let mut has_text = false;
        let mut images = 0;
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::String(_) => has_text = true,
                ClipboardEntry::Image(image) => {
                    self.pasted += 1;
                    let name = tr_args(
                        "compose-attach-pasted-name",
                        &[
                            ("number", &self.pasted.to_string()),
                            ("extension", image.format.extension()),
                        ],
                    );
                    self.rejections.clear();
                    self.add_bytes(&name, image.format.mime_type(), image.bytes.clone(), cx);
                    images += 1;
                }
                ClipboardEntry::ExternalPaths(external) => {
                    paths.extend(external.paths().iter().cloned());
                }
            }
        }
        let files = !paths.is_empty();
        if files {
            self.add_paths(paths, cx);
        }
        files || (images > 0 && !has_text)
    }

    pub fn remove(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.set.remove(id) {
            self.jobs.retain(|(job, _)| *job != id);
            self.blocked = self.blocked && self.has_failed();
            cx.notify();
        }
    }

    /// Upload a failed attachment again.
    pub fn retry(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.set.retry(id) {
            self.start_pending(cx);
            self.blocked = self.has_failed();
            cx.notify();
        }
    }

    fn has_failed(&self) -> bool {
        self.set
            .items()
            .iter()
            .any(|item| matches!(item.state, State::Failed(_)))
    }

    /// Forget every attachment (the message was sent).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set.clear();
        self.jobs.clear();
        self.rejections.clear();
        self.send_wanted = false;
        self.blocked = false;
        cx.notify();
    }

    pub fn dismiss_rejection(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.rejections.len() {
            self.rejections.remove(index);
            cx.notify();
        }
    }

    /// The composer wants to send. Starts the uploads that have not started and answers
    /// whether the send can go now ([`Gate::Ready`], with the parts), must wait
    /// ([`Gate::Waiting`]; [`AttachEvent::ReadyToSend`] follows) or is blocked by a failed
    /// upload ([`Gate::Blocked`]; Retry or Remove it).
    pub fn request_send(&mut self, cx: &mut Context<Self>) -> Gate {
        self.blocked = false;
        self.start_pending(cx);
        let gate = self.set.gate();
        match &gate {
            Gate::Waiting => self.send_wanted = true,
            Gate::Blocked => {
                self.send_wanted = false;
                self.blocked = true;
            }
            _ => self.send_wanted = false,
        }
        cx.notify();
        gate
    }

    fn start_pending(&mut self, cx: &mut Context<Self>) {
        for id in self.set.pending_ids() {
            if let Some(job) = self.set.begin(id) {
                let uploader = self.uploader.clone();
                self.jobs.push((
                    id,
                    run_off_thread(async move { uploader.upload(job).await }),
                ));
            }
        }
        if !self.jobs.is_empty() {
            self.watch(cx);
        }
    }

    fn watch(&mut self, cx: &mut Context<Self>) {
        if self.polling {
            return;
        }
        self.polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        let running = this.pump(cx);
                        this.polling = running;
                        running
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Move finished uploads into the set. Returns whether any upload is still running.
    pub fn pump(&mut self, cx: &mut Context<Self>) -> bool {
        let mut finished: Vec<(u64, Answer)> = Vec::new();
        self.jobs
            .retain(|(id, receiver)| match receiver.try_recv() {
                Ok(answer) => {
                    finished.push((*id, answer));
                    false
                }
                Err(TryRecvError::Empty) => true,
                Err(TryRecvError::Disconnected) => {
                    finished.push((
                        *id,
                        Err(UploadError::Transport(
                            "the upload did not finish".to_owned(),
                        )),
                    ));
                    false
                }
            });
        for (id, answer) in finished {
            self.set.finish(id, answer);
        }
        if self.send_wanted && self.jobs.is_empty() {
            match self.set.gate() {
                Gate::Ready(_) | Gate::Empty => {
                    self.send_wanted = false;
                    cx.emit(AttachEvent::ReadyToSend);
                }
                Gate::Blocked => {
                    self.send_wanted = false;
                    self.blocked = true;
                }
                Gate::Waiting => {}
            }
        }
        // Progress moves while a job runs, so the strip redraws every tick.
        cx.notify();
        !self.jobs.is_empty()
    }

    fn status_text(item: &Item) -> String {
        match &item.state {
            State::Pending => tr("compose-attach-status-pending"),
            State::Uploading => tr_args(
                "compose-attach-status-uploading",
                &[("percent", &item.progress.get().to_string())],
            ),
            State::Uploaded(_) => tr("compose-attach-status-uploaded"),
            State::Failed(detail) => tr_args("compose-attach-status-failed", &[("detail", detail)]),
        }
    }

    fn render_chip(&self, item: &Item, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let id = item.id;
        let size = format_size(item.size);
        let status = Self::status_text(item);
        let failed = matches!(item.state, State::Failed(_));
        let tone = if failed {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };
        let a11y = tr_args(
            "compose-attach-chip-a11y",
            &[("name", &item.name), ("size", &size), ("status", &status)],
        );
        h_flex()
            .id(SharedString::from(format!("attach-{id}")))
            .role(Role::ListItem)
            .test_support()
            .aria_label(a11y)
            .gap_2()
            .items_center()
            .px_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(div().child(format!("{} ({size})", item.name)))
            .child(
                div()
                    .id(SharedString::from(format!("attach-status-{id}")))
                    .test_support()
                    .aria_label(status.clone())
                    .text_color(tone)
                    .child(status),
            )
            .when(failed, |el| {
                el.child(
                    Button::new(SharedString::from(format!("attach-retry-{id}")))
                        .label(tr("compose-attach-retry"))
                        .accessibility_label(tr_args(
                            "compose-attach-retry-a11y",
                            &[("name", &item.name)],
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| this.retry(id, cx))),
                )
            })
            .child(
                Button::new(SharedString::from(format!("attach-remove-{id}")))
                    .label(tr("compose-attach-remove"))
                    .accessibility_label(tr_args(
                        "compose-attach-remove-a11y",
                        &[("name", &item.name)],
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove(id, cx))),
            )
    }
}

impl Render for AttachBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chips: Vec<_> = self
            .set
            .items()
            .iter()
            .map(|item| self.render_chip(item, cx))
            .collect();
        let danger = cx.theme().danger;
        let muted = cx.theme().muted_foreground;
        v_flex()
            .id("attachments")
            .role(Role::Group)
            .test_support()
            .aria_label(tr("compose-attach-region-a11y"))
            .gap_1()
            .children(self.rejections.iter().enumerate().map(|(index, reject)| {
                let message = reject.message();
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .id(SharedString::from(format!("attach-rejected-{index}")))
                            .role(Role::Status)
                            .test_support()
                            .aria_label(message.clone())
                            .text_color(danger)
                            .child(message.clone()),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "attach-rejected-dismiss-{index}"
                        )))
                        .label(tr("compose-attach-rejected-dismiss"))
                        .accessibility_label(tr_args(
                            "compose-attach-rejected-dismiss-a11y",
                            &[("message", &message)],
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.dismiss_rejection(index, cx);
                        })),
                    )
            }))
            .when(!chips.is_empty(), |el| {
                el.child(
                    h_flex()
                        .id("attach-chips")
                        .role(Role::List)
                        .test_support()
                        .aria_label(tr("compose-attach-region-a11y"))
                        .gap_2()
                        .flex_wrap()
                        .children(chips),
                )
            })
            .when(self.blocked, |el| {
                el.child(
                    div()
                        .id("attach-blocked")
                        .role(Role::Status)
                        .test_support()
                        .aria_label(tr("compose-attach-blocked"))
                        .text_color(danger)
                        .child(tr("compose-attach-blocked")),
                )
            })
            .when(self.send_wanted, |el| {
                el.child(
                    div()
                        .id("attach-uploading-note")
                        .role(Role::Status)
                        .test_support()
                        .aria_label(tr("compose-attach-uploading-note"))
                        .text_color(muted)
                        .child(tr("compose-attach-uploading-note")),
                )
            })
            .child(
                h_flex().child(
                    Button::new("attach-add")
                        .label(tr("compose-attach-add"))
                        .accessibility_label(tr("compose-attach-add-a11y"))
                        .on_click(cx.listener(|this, _, _, cx| this.open_picker(cx))),
                ),
            )
    }
}
