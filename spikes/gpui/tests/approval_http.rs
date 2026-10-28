//! The real HTTP answer sink against agui-fixture's `approval` scenario on port 0: the first answer
//! is accepted (200, names the fixture user), a second answer with another idempotency key gets
//! 409 `already_resolved` naming the same user. No GPUI here.
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread::sleep;
use std::time::{Duration, Instant};

use agui_fixture::{FixtureConfig, Scenario, start};
use gpui_spike::approval_model::{AnswerError, AnswerRequest, FIXTURE_THREAD, selected_body};
use gpui_spike::approval_sink::{AnswerSink, HttpAnswerSink};

const INTERRUPT_ID: &str = "int_00001";

fn request(idempotency_key: &str) -> AnswerRequest {
    AnswerRequest {
        thread_id: FIXTURE_THREAD.to_owned(),
        interrupt_id: INTERRUPT_ID.to_owned(),
        idempotency_key: idempotency_key.to_owned(),
        body: selected_body("allow"),
    }
}

/// Opens the thread's subscription. The fixture starts its emitter when a subscriber connects,
/// so the interrupt only appears in the log after this.
fn subscribe(addr: SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect to fixture");
    let upgrade = format!(
        "GET /threads/{FIXTURE_THREAD}/subscribe?after=0 HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(upgrade.as_bytes()).expect("send upgrade");
    let mut reader = BufReader::new(stream.try_clone().expect("clone socket"));
    let mut status = String::new();
    reader.read_line(&mut status).expect("read status line");
    assert!(
        status.starts_with("HTTP/1.1 101"),
        "subscription upgrades: {status}"
    );
    stream
}

#[test]
fn approval_http_sink_is_accepted_once_then_409_names_the_winner() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let fixture = runtime
        .block_on(start(FixtureConfig {
            thread_id: FIXTURE_THREAD.to_owned(),
            ..FixtureConfig::new(Scenario::Approval)
        }))
        .expect("start approval fixture on port 0");
    let _subscription = subscribe(fixture.addr());
    let sink = HttpAnswerSink::new(format!("http://{}", fixture.addr())).expect("sink");

    // Until the run reaches its interrupt the fixture answers 404 `unknown_interrupt`; the answer
    // is retried until the interrupt is in the log.
    let deadline = Instant::now() + Duration::from_secs(10);
    let first = loop {
        match sink.answer(&request("click-1")) {
            Err(AnswerError::Http { status: 404, .. }) if Instant::now() < deadline => {
                sleep(Duration::from_millis(10));
            }
            other => break other,
        }
    };
    let resolver = first.expect("the first answer is accepted");
    assert_eq!(resolver.name.as_deref(), Some("Fixture user"));

    match sink.answer(&request("click-2")) {
        Err(AnswerError::AlreadyResolved { resolved_by }) => {
            assert_eq!(resolved_by.name.as_deref(), Some("Fixture user"));
        }
        other => panic!("the second answer gets 409 already_resolved, got {other:?}"),
    }
}
