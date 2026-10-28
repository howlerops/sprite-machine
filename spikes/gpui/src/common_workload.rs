//! The common workload (`GPUI_SPIKE_WORKLOAD=common`): the panes beside the transcript, and the
//! fixture data they start from.
//!
//! - The transcript streams from the feed (`GPUI_SPIKE_FIXTURE_ADDR`, the `transcript-10k` fixture).
//! - agui-fixture serves one scenario per process, so the approval comes from a second fixture:
//!   the `approval` scenario, served in process on its own thread ([`ApprovalFixture`]). Its
//!   answers are recorded by that fixture, as in `tests/approval_http.rs`.
//! - The diff (`diff-5k`) and the terminal output (`terminal`) are generated in process, as their
//!   panes already do.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use agui_fixture::{FixtureConfig, RunningFixture, Scenario, start as start_fixture};
use gpui_kit::component::diff::DiffFile;
use protocol::turn_event::{Envelope, Event, Interrupt};

use crate::approval_model::{ApprovalModel, FIXTURE_THREAD};
use crate::approval_sink::{AnswerSink, HttpAnswerSink};
use crate::diff_pane::diff5k_files;
use crate::feed::spawn_feed_url;
use crate::terminal_pane::fixture_output_chunks;

/// How long an approval stays open (plan §4.1, the fixture's TTL). The card counts down from it.
pub const APPROVAL_TTL_MS: i64 = 600_000;
/// How long to wait for the approval interrupt to reach the log.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);

/// The `approval` fixture, served in process with its own tokio runtime. Its subscription stays
/// open (the fixture emits only to a subscriber), and its envelopes queue on [`Self::feed`].
pub struct ApprovalFixture {
    // Dropped in declaration order: the server first, then the runtime that runs it.
    fixture: RunningFixture,
    feed: Receiver<Envelope>,
    interrupt: Interrupt,
    runtime: tokio::runtime::Runtime,
}

impl ApprovalFixture {
    /// Starts the approval scenario on a free loopback port and waits for its interrupt.
    pub fn start() -> Result<Self, String> {
        let runtime = tokio::runtime::Runtime::new().map_err(|err| err.to_string())?;
        let fixture = runtime
            .block_on(start_fixture(FixtureConfig {
                thread_id: FIXTURE_THREAD.to_owned(),
                ..FixtureConfig::new(Scenario::Approval)
            }))
            .map_err(|err| err.to_string())?;
        let feed = spawn_feed_url(fixture.ws_url(0));
        let deadline = Instant::now() + INTERRUPT_TIMEOUT;
        let interrupt = loop {
            match feed.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(envelope) => {
                    if let Event::Interrupt(interrupt) = envelope.event {
                        break *interrupt;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err("the approval interrupt did not reach the log".to_owned());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("the approval feed closed before its interrupt".to_owned());
                }
            }
        };
        Ok(Self {
            fixture,
            feed,
            interrupt,
            runtime,
        })
    }

    /// The approval interrupt, as the fixture logged it.
    pub fn interrupt(&self) -> &Interrupt {
        &self.interrupt
    }

    /// The approval thread's envelopes that have not been read yet (the subscription).
    pub fn feed(&self) -> &Receiver<Envelope> {
        &self.feed
    }

    /// The HTTP sink that answers the interrupt on this fixture.
    pub fn sink(&self) -> Result<Arc<dyn AnswerSink>, String> {
        let sink = HttpAnswerSink::new(format!("http://{}", self.fixture.addr()))?;
        Ok(Arc::new(sink))
    }

    /// The tokio runtime that serves the fixture.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }
}

/// What the common workload's panes start from.
pub struct CommonInputs {
    /// The `diff-5k` file, parsed for the diff pane.
    pub diff_files: Vec<DiffFile>,
    /// The `terminal` fixture's output chunks, in order (the pane receives them over time).
    pub terminal_output: Vec<Vec<u8>>,
    /// The pending approval card's model.
    pub approval: ApprovalModel,
    /// Where the card's answer goes.
    pub sink: Arc<dyn AnswerSink>,
    /// The card's clock when the window opens (epoch ms). The fixture's expiry is in the past
    /// for a wall clock, so the card counts down from its TTL starting here instead.
    pub approval_clock_start_ms: i64,
}

impl CommonInputs {
    /// The inputs for the common workload, with the approval taken from `approval`.
    pub fn from_fixtures(approval: &ApprovalFixture) -> Result<Self, String> {
        let model = ApprovalModel::from_interrupt(FIXTURE_THREAD, approval.interrupt())?;
        let expires_at = model
            .expires_at_ms()
            .ok_or("the approval interrupt has no expiry")?;
        Ok(Self {
            diff_files: diff5k_files()?,
            terminal_output: fixture_output_chunks()?,
            approval: model,
            sink: approval.sink()?,
            approval_clock_start_ms: expires_at - APPROVAL_TTL_MS,
        })
    }
}

/// The approval card's clock: `start_ms` when the window opens, then real time. [`Self::changed`]
/// reports each new displayed second, so the card repaints once a second and no more.
pub struct CardClock {
    start_ms: i64,
    opened: Instant,
    second: i64,
}

impl CardClock {
    pub fn new(start_ms: i64) -> Self {
        Self {
            start_ms,
            opened: Instant::now(),
            second: start_ms.div_euclid(1_000),
        }
    }

    /// The clock reading, when its displayed second differs from the last one reported.
    pub fn changed(&mut self) -> Option<i64> {
        let elapsed = i64::try_from(self.opened.elapsed().as_millis()).unwrap_or(i64::MAX);
        let now = self.start_ms.saturating_add(elapsed);
        let second = now.div_euclid(1_000);
        if second == self.second {
            return None;
        }
        self.second = second;
        Some(now)
    }
}

#[cfg(test)]
mod tests {
    use super::{APPROVAL_TTL_MS, CardClock};

    #[test]
    fn card_clock_reports_nothing_until_the_displayed_second_changes() {
        let mut clock = CardClock::new(1_000_000);
        // Well under a second has passed since the clock opened.
        assert_eq!(clock.changed(), None);
        // Moving the start back by a second makes the displayed second change at once.
        clock.start_ms -= 1_000;
        assert!(clock.changed().is_some());
        assert_eq!(clock.changed(), None);
    }

    #[test]
    fn card_clock_ttl_is_the_fixture_ttl() {
        assert_eq!(APPROVAL_TTL_MS, 600_000);
    }
}
