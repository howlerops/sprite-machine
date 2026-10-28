//! A scriptable mock ACP v1 agent for tests.
//!
//! `acp-mock` speaks JSON-RPC 2.0 over newline-delimited JSON, like an ACP agent on
//! stdio. What it sends is driven by a JSON [`Script`]. Use it in-process with
//! [`run`] over any tokio duplex pipe, or as the `acp-mock` binary with
//! `acp-mock --script <path.json>` on stdin and stdout.
//!
//! # Handled methods
//!
//! `initialize`, `session/new`, `session/load`, `session/resume`, `session/prompt`, the
//! `session/cancel` notification, `session/set_mode` and `session/set_config_option`. Each
//! session counts its prompts: the Nth `session/prompt` of a session runs `turns[N-1]`. A prompt
//! past the end of the script fails with a JSON-RPC error, so a test notices a script that is
//! too short. `session/set_mode` and `session/set_config_option` answer from the session's
//! current modes and options (the script's at start), reject an unknown mode, option or value
//! with `-32602`, and send `current_mode_update` / `config_option_update` before the answer.
//!
//! `initialize` advertises the script's `mcp_capabilities` (`http` and `sse`, both off when the
//! script says nothing), so a client can tell which remote MCP servers the agent takes.
//!
//! [`run_recorded`] keeps every message the agent received in a [`Recorder`], so a test can
//! check what the client sent (for example `mcpServers` on `session/new`).
//!
//! # Script format
//!
//! ```json
//! {
//!   "modes": { "currentModeId": "code", "availableModes": [{ "id": "code", "name": "Code" }] },
//!   "config_options": [],
//!   "mcp_capabilities": { "http": true, "sse": true },
//!   "turns": [
//!     {
//!       "steps": [
//!         { "text": "Reading the file." },
//!         { "thought": "The parser drops the newline." },
//!         { "tool_call": { "toolCallId": "call_1", "title": "Edit parser.rs",
//!                          "kind": "edit", "status": "in_progress" } },
//!         { "tool_call_update": { "toolCallId": "call_1", "status": "completed",
//!             "content": [{ "type": "diff", "path": "/work/parser.rs",
//!                           "oldText": "a", "newText": "b" }] } },
//!         { "plan": [{ "content": "Fix it", "priority": "high", "status": "pending" }] },
//!         { "usage": { "used": 100, "size": 1000 } },
//!         { "available_commands": [{ "name": "review", "description": "Review" }] },
//!         { "current_mode": "code" },
//!         { "config_option": [{ "id": "model", "name": "Model", "type": "select",
//!             "currentValue": "opus", "options": [{ "value": "opus", "name": "Opus" }] }] },
//!         { "request_permission": { "toolCall": { "toolCallId": "call_2",
//!               "title": "Run tests", "kind": "execute" },
//!             "options": [{ "optionId": "allow_once", "name": "Allow",
//!                           "kind": "allow_once" },
//!                         { "optionId": "reject_once", "name": "Reject",
//!                           "kind": "reject_once" }] },
//!           "expect": "allow" },
//!         { "elicitation": { "mode": "form", "message": "Which branch?",
//!             "requestedSchema": { "type": "object", "properties": {} } },
//!           "expect": "accept" },
//!         { "elicitation": { "mode": "url", "message": "Sign in to continue",
//!             "elicitationId": "e1", "url": "https://example.test/auth" } },
//!         { "terminal": { "command": "cargo test", "toolCallId": "call_3" } },
//!         { "leak_secret": true },
//!         { "stop": "end_turn" }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! `modes` and `config_options` are optional. They are the ACP `SessionModeState` and the list of
//! `SessionConfigOption`, returned by `session/new`, `session/resume` and `session/load`.
//! `mcp_capabilities` is optional too: the ACP `McpCapabilities` (`http`, `sse`) that `initialize`
//! returns.
//!
//! Each step has exactly one step key, plus an optional `expect` on the two client
//! requests. Payloads are ACP v1 JSON, checked against the `protocol::acp` types when the
//! script loads, so an invalid payload fails in [`Script::from_json`].
//!
//! - `text`, `thought`: an `agent_message_chunk` or `agent_thought_chunk` with that text.
//! - `config_option`: the list of `SessionConfigOption` of a `config_option_update`. It only
//!   sends the update; `session/set_config_option` answers from the session's own values.
//! - `tool_call`, `tool_call_update`: the ACP `tool_call` or `tool_call_update` fields.
//! - `plan`: the array of plan entries.
//! - `usage`: the `usage_update` fields (`used`, `size`, optional `cost`).
//! - `available_commands`: the array of commands. `current_mode`: a mode id string.
//! - `request_permission`: the ACP request without `sessionId`. The client must answer
//!   with `expect`: `any`, `cancelled`, `allow` (an allow kind), `reject` (a reject kind),
//!   or an option id. A different answer fails the prompt.
//! - `elicitation`: the ACP `elicitation/create` request without `sessionId`, with
//!   `mode` `form` or `url`. `expect` is `any`, `accept`, `decline` or `cancel`.
//! - `terminal`: `command`, and an optional `toolCallId`. Calls the client's
//!   `terminal/create`, reports the terminal as a tool call, then calls
//!   `terminal/wait_for_exit`, `terminal/output` and `terminal/release`.
//! - `leak_secret`: `true` emits a text chunk containing [`FAKE_API_KEY`].
//! - `stop`: the stop reason that ends the turn (`end_turn`, `cancelled`, ...). It must
//!   be the last step. A turn without it ends with `end_turn`.
//!
//! A `session/cancel` during a turn ends the prompt with `stopReason: "cancelled"`. This
//! includes a turn that is waiting for a permission, elicitation or terminal answer.
//!
//! # Example
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! let script = acp_mock::Script::from_json(r#"{"turns":[{"steps":[{"text":"hi"}]}]}"#)?;
//! acp_mock::run(script, tokio::io::stdin(), tokio::io::stdout()).await
//! # }
//! ```

mod agent;
mod script;

pub use agent::{Recorder, run, run_recorded};
pub use script::{FAKE_API_KEY, Script, Step, Turn};

/// Crate name, used by the skeleton smoke test.
pub const CRATE: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE, "acp-mock");
    }
}
