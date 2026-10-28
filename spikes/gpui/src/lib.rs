//! M1B-004a spike: gpui-kit app shell + virtualized transcript pane (feeds M1B-008 benchmarks).
//! M1B-004b spike: the diff pane (gpui-kit `Diff`, syntax highlighted) over the diff-5k fixture.
//! M1B-004c spike: the terminal pane (`alacritty_terminal` model, styled rows, key input) over the
//! `terminal` fixture.

pub mod diff_model;
pub mod diff_pane;
pub mod terminal_input;
pub mod terminal_model;
pub mod terminal_pane;
pub mod transcript_model;
pub mod transcript_pane;
pub mod transcript_shell;
