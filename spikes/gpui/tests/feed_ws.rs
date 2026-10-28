//! The fixture feed over a real WebSocket: the `transcript-10k` log, streamed by agui-fixture on
//! port 0 from `ws://<addr>/threads/th_fixture/subscribe?after=0`, folds into the same rows as
//! the in-memory fixture.
use std::time::{Duration, Instant};

use agui_fixture::{FixtureConfig, Scenario, complete_log, start};
use gpui_spike::feed::spawn_feed;
use gpui_spike::transcript_model::build_rows;
use gpui_spike::transcript_shell::fixture_rows;

const STREAM_TIMEOUT: Duration = Duration::from_secs(60);

#[test]
fn feed_streams_the_transcript_fixture_into_the_in_memory_rows() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let fixture = runtime
        .block_on(start(FixtureConfig::new(Scenario::Transcript10k)))
        .expect("start transcript-10k fixture on port 0");
    let expected = complete_log(Scenario::Transcript10k, "th_fixture")
        .expect("fixture log")
        .len();

    let feed = spawn_feed(&fixture.addr().to_string());
    let mut received = Vec::with_capacity(expected);
    let deadline = Instant::now() + STREAM_TIMEOUT;
    while received.len() < expected {
        let left = deadline.saturating_duration_since(Instant::now());
        match feed.recv_timeout(left) {
            Ok(envelope) => received.push(envelope),
            Err(err) => panic!(
                "feed stopped after {} of {expected} events: {err}",
                received.len()
            ),
        }
    }

    assert_eq!(build_rows(&received), fixture_rows());
}
