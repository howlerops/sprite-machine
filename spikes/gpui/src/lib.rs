//! M1B-004a spike: gpui-kit app shell + virtualized transcript pane (feeds M1B-008 benchmarks).
//! M1B-004b spike: the diff pane (gpui-kit `Diff`, syntax highlighted) over the diff-5k fixture.
//! M1B-004c spike: the terminal pane (`alacritty_terminal` model, styled rows, key input) over the
//! `terminal` fixture.
//! M1B-004d spike: the approval card (`Interrupt{approval}`, answers over HTTP to the `approval`
//! fixture).
//! M1B-008a spike: bench hooks (fixture feed over WebSocket, `BENCH_MARKER`, frame and
//! key-to-glyph JSON lines) and the composer.
//! M1B-008b spike: the common workload (`GPUI_SPIKE_WORKLOAD=common`): the transcript beside the
//! diff, the terminal and one approval card in one window.

pub mod approval_model;
pub mod approval_pane;
pub mod approval_sink;
pub mod bench;
pub mod common_workload;
pub mod composer;
pub mod diff_model;
pub mod diff_pane;
pub mod feed;
pub mod terminal_input;
pub mod terminal_model;
pub mod terminal_pane;
pub mod transcript_model;
pub mod transcript_pane;
pub mod transcript_shell;
pub mod workload;
