//! `sm` / `sm --rc [name]`: the interactive terminal session that is also live on the phone
//! (M2B-010, PLAN-v2-ade.md §4.10).
//!
//! The UI is a pure state machine over client-core:
//!
//! - [`app`]: the state. Keys and the thread's messages (client-core [`Incoming`]) go in;
//!   [`app::Action`]s come out (register, prompt, answer, cancel). The thread is a
//!   client-core `ThreadSync`, the reducer the phone's page uses, so what a prompt typed here and
//!   a prompt typed on the phone look like is the same: one `UserMessage` each, in the hub's order.
//! - [`render`]: draws the state into a ratatui buffer. The tests compare it with snapshots.
//! - [`driver`]: the event loop over a [`driver::Backend`], on any ratatui terminal.
//! - [`term`]: the real terminal (raw mode, alternate screen, restored on exit and on panic).
//! - [`session`]: the production backend: the log the screen follows, fed by the session's own
//!   bridge and by the hub's commands, and shared with the hub on `/rc`.
//! - [`host`]: `sm` itself: the agent, the terminal and the tunnel wired to a [`session::Session`].
//!
//! The `autoConnect` setting ([`crate::settings`]) decides whether the session registers with the
//! hub at start; otherwise `/rc` does, and `/rc` again stops sharing.
//!
//! [`Incoming`]: client_core::sync::Incoming

pub mod app;
pub mod driver;
pub mod host;
pub mod render;
pub mod session;
pub mod term;

pub use app::{Action, AnswerResult, App, Key, Remote};
pub use driver::{Backend, Feed, execute, run};
