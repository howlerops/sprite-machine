//! The GPUI side: the toast / OS-notification sink and the settings view.

use std::sync::{Arc, Mutex};

use gpui_kit::TestSupportExt as _;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::{Notification as Toast, NotificationType};
use gpui_kit::component::{WindowExt as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::model::{Notification, NotificationSink, NotifyEvent, NotifyKind, PrefsHandle};
use super::{tr, tr_args};

/// Events waiting for the shell: clicks on notifications. Cloning shares the queue.
#[derive(Clone, Default)]
pub struct EventQueue(Arc<Mutex<Vec<NotifyEvent>>>);

impl EventQueue {
    pub fn push(&self, event: NotifyEvent) {
        self.0.lock().expect("event queue").push(event);
    }

    /// Take everything queued, oldest first. The shell calls this each frame / on a timer and
    /// maps `Open { thread_id }` to `AppShell::open_thread`.
    pub fn drain(&self) -> Vec<NotifyEvent> {
        std::mem::take(&mut *self.0.lock().expect("event queue"))
    }
}

/// Key type for toast ids: the same thread and kind replace each other.
struct NotifyToast;

/// The real sink. [`NotificationSink::show`] only queues (it has no window); the shell calls
/// [`ToastSink::flush`] with its window, which pushes each as a gpui-component notification
/// delivered in-app **and** to the OS notification center. A click queues the
/// [`NotifyEvent::Open`] on the [`EventQueue`]. Cloning shares the queue, so the shell keeps one
/// clone and hands the other to the [`super::Notifier`].
#[derive(Clone)]
pub struct ToastSink {
    pending: Arc<Mutex<Vec<Notification>>>,
    events: EventQueue,
}

impl ToastSink {
    pub fn new(events: EventQueue) -> Self {
        Self {
            pending: Arc::default(),
            events,
        }
    }

    /// Notifications queued and not yet flushed.
    pub fn pending(&self) -> usize {
        self.pending.lock().expect("toast queue").len()
    }

    /// Show everything queued in `window`; returns how many.
    pub fn flush(&self, window: &mut Window, cx: &mut App) -> usize {
        let queued = std::mem::take(&mut *self.pending.lock().expect("toast queue"));
        let count = queued.len();
        for notification in queued {
            window.push_notification(self.toast(notification), cx);
        }
        count
    }

    fn toast(&self, notification: Notification) -> Toast {
        let events = self.events.clone();
        let kind = match notification.kind {
            NotifyKind::NeedsYou => NotificationType::Warning,
            NotifyKind::Done => NotificationType::Success,
            NotifyKind::Failed => NotificationType::Error,
        };
        let click = notification.click_event();
        Toast::new()
            .id1::<NotifyToast>(SharedString::from(notification.id))
            .title(SharedString::from(notification.title))
            .with_type(kind)
            .in_app_and_system()
            .on_click(move |_, _, _| events.push(click.clone()))
    }
}

impl NotificationSink for ToastSink {
    fn show(&mut self, notification: &Notification) {
        self.pending
            .lock()
            .expect("toast queue")
            .push(notification.clone());
    }
}

/// The notification settings: one switch per kind, saved on every change.
pub struct NotifySettingsView {
    prefs: PrefsHandle,
    error: Option<String>,
}

impl NotifySettingsView {
    pub fn new(prefs: PrefsHandle) -> Self {
        Self { prefs, error: None }
    }

    /// The save error of the last change, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Flip a kind (what a click on its switch does).
    pub fn set(&mut self, kind: NotifyKind, on: bool, cx: &mut Context<Self>) {
        self.error = self.prefs.set(kind, on).err();
        cx.notify();
    }

    fn row(&self, kind: NotifyKind, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let on = self.prefs.get().enabled(kind);
        let name = tr(&format!("notify-kind-{}", kind.slug()));
        let a11y = tr_args(
            if on {
                "notify-kind-on-a11y"
            } else {
                "notify-kind-off-a11y"
            },
            &[("name", &name)],
        );
        let slug = kind.slug();
        v_flex()
            .id(format!("notify-row-{slug}"))
            .test_support()
            .aria_label(a11y)
            .gap_1()
            .child(
                Checkbox::new(format!("notify-toggle-{slug}"))
                    .label(name)
                    .checked(on)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set(kind, *checked, cx);
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .child(tr(&format!("notify-kind-{slug}-desc"))),
            )
    }
}

impl Render for NotifySettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = tr("notify-settings-title");
        let error = self
            .error
            .as_deref()
            .map(|reason| tr_args("notify-save-failed", &[("reason", reason)]));
        v_flex()
            .id("notify-settings")
            .test_support()
            .aria_label(tr("notify-settings-region-a11y"))
            .gap_3()
            .p_4()
            .child(
                div()
                    .id("notify-settings-title")
                    .test_support()
                    .aria_label(title.clone())
                    .text_lg()
                    .child(title),
            )
            .child(div().text_sm().child(tr("notify-settings-intro")))
            .children(NotifyKind::ALL.map(|kind| self.row(kind, cx)))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .id("notify-save-error")
                        .test_support()
                        .aria_label(message.clone())
                        .text_sm()
                        .child(message),
                )
            })
    }
}
