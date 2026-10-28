//! The event loop: keys from the terminal and messages from the [`Backend`] go to the [`App`];
//! the [`Action`]s it returns are carried out on the backend; the screen is drawn after each
//! step. The terminal is any ratatui backend, so the tests run it on a `TestBackend`.

use client_core::sync::Incoming;
use client_core::{Connectivity, ThreadSync};
use protocol::tunnel::Outcome;
use ratatui::Terminal;
use tokio::sync::mpsc;

use super::app::{Action, AnswerResult, App, Key};
use super::render::render;

/// What a backend tells the UI.
#[derive(Debug)]
pub enum Feed {
    /// A message of the thread, in the order the hub (or the local host) gave it.
    Thread(Incoming),
    /// The source of the thread changed (the session was shared, or stopped being shared): the
    /// messages that follow rebuild the thread from `seq` 1.
    Reset,
    Connectivity(Connectivity),
}

/// The session's host: what the UI asks of it, and what it reports back. The production one is
/// [`super::host::Host`]; the tests use a fake hub.
#[allow(async_fn_in_trait)]
pub trait Backend {
    /// Registers this session with the hub. Returns the session link.
    async fn register(&mut self) -> Result<String, String>;
    /// Stops sharing the session.
    async fn unregister(&mut self);
    async fn prompt(
        &mut self,
        task_id: String,
        message_id: String,
        text: String,
    ) -> Result<(), String>;
    /// Answers an interrupt. When the session is shared this is the hub's rule: the first answer
    /// wins and a second one is a `409`, reported as [`AnswerResult::AlreadyAnswered`].
    async fn answer(&mut self, interrupt_id: &str, outcome: Outcome) -> AnswerResult;
    async fn cancel(&mut self, task_id: &str) -> Result<(), String>;
    /// The next message for the UI; `None` when the host has ended. Cancel safe.
    async fn next(&mut self) -> Option<Feed>;
}

/// Runs the UI until the user quits, the keys end, or the host ends.
pub async fn run<B, T>(
    app: &mut App,
    backend: &mut B,
    terminal: &mut Terminal<T>,
    keys: &mut mpsc::Receiver<Key>,
) -> Result<(), T::Error>
where
    B: Backend,
    T: ratatui::backend::Backend,
{
    let thread_id = app.view().thread_id().to_owned();
    let start = app.start();
    draw(app, terminal)?;
    if !execute(start, app, backend).await {
        return Ok(());
    }
    loop {
        draw(app, terminal)?;
        // Keys first: typing stays responsive while the thread streams.
        tokio::select! {
            biased;
            key = keys.recv() => {
                let Some(key) = key else { return Ok(()) };
                let actions = app.key(key);
                draw(app, terminal)?;
                if !execute(actions, app, backend).await {
                    return Ok(());
                }
            }
            feed = backend.next() => {
                let Some(feed) = feed else { return Ok(()) };
                match feed {
                    Feed::Thread(incoming) => app.incoming(incoming),
                    Feed::Reset => app.replace_sync(ThreadSync::new(thread_id.clone())),
                    Feed::Connectivity(state) => app.set_connectivity(state),
                }
            }
        }
    }
}

fn draw<T: ratatui::backend::Backend>(
    app: &App,
    terminal: &mut Terminal<T>,
) -> Result<(), T::Error> {
    terminal.draw(|frame| render(app, frame.area(), frame.buffer_mut()))?;
    Ok(())
}

/// Carries out `actions` in order. `false` once the user quit.
pub async fn execute<B: Backend>(actions: Vec<Action>, app: &mut App, backend: &mut B) -> bool {
    for action in actions {
        match action {
            Action::Quit => return false,
            Action::Register => {
                let result = backend.register().await;
                app.registered(result);
            }
            Action::Unregister => {
                backend.unregister().await;
                app.unregistered();
            }
            Action::Prompt {
                task_id,
                message_id,
                text,
            } => {
                if let Err(error) = backend.prompt(task_id, message_id, text).await {
                    app.set_notice(format!("message not sent: {error}"));
                }
            }
            Action::Answer {
                interrupt_id,
                outcome,
            } => {
                let result = backend.answer(&interrupt_id, outcome).await;
                app.answered(&interrupt_id, result);
            }
            Action::Cancel { task_id } => {
                if let Err(error) = backend.cancel(&task_id).await {
                    app.set_notice(format!("cancel failed: {error}"));
                }
            }
        }
    }
    true
}
