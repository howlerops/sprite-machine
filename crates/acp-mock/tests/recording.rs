//! What acp-mock records and advertises: every message it receives (`Recorder`), and the modes
//! and config options its script gives `session/new`, `session/resume` and `session/load`.

use acp_mock::{Recorder, Script};
use serde_json::{Value, json};
use tokio::io::DuplexStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};

const MODES: &str = r#"{
    "currentModeId": "code",
    "availableModes": [{ "id": "code", "name": "Code" }, { "id": "plan", "name": "Plan" }]
}"#;

const OPTIONS: &str = r#"[{
    "id": "model", "name": "Model", "type": "select", "currentValue": "fast",
    "options": [{ "value": "fast", "name": "Fast" }, { "value": "smart", "name": "Smart" }]
}]"#;

/// An agent running in process, and the client's end of the pipe.
struct Client {
    writer: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    recorder: Recorder,
}

fn start(script: &str) -> Client {
    let script = Script::from_json(script).expect("script");
    let recorder = Recorder::new();
    let (client_io, agent_io) = tokio::io::duplex(1 << 16);
    let (agent_r, agent_w) = tokio::io::split(agent_io);
    tokio::spawn(acp_mock::run_recorded(
        script,
        recorder.clone(),
        agent_r,
        agent_w,
    ));
    let (client_r, client_w) = tokio::io::split(client_io);
    Client {
        writer: client_w,
        lines: BufReader::new(client_r).lines(),
        recorder,
    }
}

impl Client {
    /// Sends a request and returns its `result`.
    async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&request).await;
        loop {
            let line = self.lines.next_line().await.expect("read").expect("a line");
            let message: Value = serde_json::from_str(&line).expect("JSON");
            if message["id"] == id {
                assert!(message.get("error").is_none(), "error: {message}");
                return message["result"].clone();
            }
        }
    }

    async fn send(&mut self, message: &Value) {
        let mut bytes = message.to_string();
        bytes.push('\n');
        self.writer
            .write_all(bytes.as_bytes())
            .await
            .expect("write");
    }
}

#[tokio::test]
async fn the_recorder_keeps_every_message_the_agent_received_in_order() {
    let mut client = start(r#"{ "turns": [] }"#);
    client
        .call(1, "initialize", json!({ "protocolVersion": 1 }))
        .await;
    client
        .call(
            2,
            "session/new",
            json!({ "cwd": "/work", "mcpServers": [] }),
        )
        .await;
    // A notification has no response: the next request's response is the sync point.
    client
        .send(&json!({
            "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": "sess_mock_1" }
        }))
        .await;
    client
        .call(
            3,
            "session/load",
            json!({ "sessionId": "sess_mock_1", "cwd": "/work", "mcpServers": [] }),
        )
        .await;

    let messages = client.recorder.messages();
    let methods: Vec<&str> = messages
        .iter()
        .map(|message| message["method"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "session/new",
            "session/cancel",
            "session/load"
        ]
    );
    assert_eq!(messages[1]["params"]["cwd"], "/work");
    assert_eq!(messages[1]["params"]["mcpServers"], json!([]));
}

#[tokio::test]
async fn session_new_resume_and_load_advertise_the_scripts_modes_and_config_options() {
    let script = format!(r#"{{ "modes": {MODES}, "config_options": {OPTIONS}, "turns": [] }}"#);
    let mut client = start(&script);
    let new_session = client
        .call(
            1,
            "session/new",
            json!({ "cwd": "/work", "mcpServers": [] }),
        )
        .await;
    assert_eq!(new_session["sessionId"], "sess_mock_1");
    assert_eq!(new_session["modes"]["currentModeId"], "code");
    assert_eq!(
        new_session["modes"]["availableModes"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(new_session["configOptions"][0]["id"], "model");
    assert_eq!(new_session["configOptions"][0]["currentValue"], "fast");

    let resumed = client
        .call(
            2,
            "session/resume",
            json!({ "sessionId": "sess_mock_1", "cwd": "/work", "mcpServers": [] }),
        )
        .await;
    assert_eq!(resumed["modes"]["currentModeId"], "code");
    assert_eq!(resumed["configOptions"][0]["id"], "model");

    let loaded = client
        .call(
            3,
            "session/load",
            json!({ "sessionId": "sess_mock_1", "cwd": "/work", "mcpServers": [] }),
        )
        .await;
    assert_eq!(loaded["modes"]["currentModeId"], "code");
    assert_eq!(loaded["configOptions"][0]["id"], "model");
}

#[tokio::test]
async fn a_script_without_modes_answers_session_new_without_them() {
    let mut client = start(r#"{ "turns": [] }"#);
    let new_session = client
        .call(
            1,
            "session/new",
            json!({ "cwd": "/work", "mcpServers": [] }),
        )
        .await;
    assert!(new_session.get("modes").is_none(), "{new_session}");
    assert!(new_session.get("configOptions").is_none(), "{new_session}");
}

#[test]
fn a_script_with_malformed_modes_or_options_is_rejected() {
    let bad_modes = r#"{ "modes": { "currentModeId": 7 }, "turns": [] }"#;
    assert!(Script::from_json(bad_modes).is_err());
    let bad_options = r#"{ "config_options": [{ "id": "x" }], "turns": [] }"#;
    assert!(Script::from_json(bad_options).is_err());
}
