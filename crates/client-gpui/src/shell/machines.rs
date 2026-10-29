//! The shell's own `GET /v1/machines` feed (client-api §5.1). One fetch serves the Agents page
//! (harness rows and presence), the new-task composer (machine and harness choices) and the
//! open thread's runner cap, so none of them fetches by itself.

use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde_json::Value;

use crate::agents::MachineInfo;
use crate::compose::{MachineOption, Target, parse_machines};

/// How often the machines list is refreshed while signed in (presence changes show up within it).
pub const REFRESH_EVERY: Duration = Duration::from_secs(30);

/// One answer of `GET /v1/machines`, in the two shapes the pages need.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachinesSnapshot {
    pub info: Vec<MachineInfo>,
    pub options: Vec<MachineOption>,
}

impl MachinesSnapshot {
    /// Parse the response body. Entries the agents table cannot read are left out of `info`.
    pub fn parse(body: &Value) -> Self {
        let info = body
            .get("machines")
            .and_then(Value::as_array)
            .map(|machines| {
                machines
                    .iter()
                    .filter_map(|machine| serde_json::from_value(machine.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            info,
            options: parse_machines(body),
        }
    }
}

/// `GET {api}/machines`.
pub async fn fetch(target: &Target) -> Result<MachinesSnapshot, String> {
    let mut request =
        reqwest::Client::new().get(format!("{}/machines", target.api.trim_end_matches('/')));
    if let Some(token) = &target.token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "GET /v1/machines answered HTTP {}",
            status.as_u16()
        ));
    }
    let body: Value = response.json().await.map_err(|error| error.to_string())?;
    Ok(MachinesSnapshot::parse(&body))
}

pub type FetchAnswer = Result<MachinesSnapshot, String>;

/// Run one fetch on its own thread and runtime; the answer is read with `try_recv` so no foreign
/// thread wakes a GPUI task.
pub fn fetch_off_thread(target: Target) -> Receiver<FetchAnswer> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            let _ = sender.send(runtime.block_on(fetch(&target)));
        }
    });
    receiver
}

/// What the shell knows about the machines.
#[derive(Default)]
pub struct MachinesFeed {
    pub target: Option<Target>,
    pub snapshot: MachinesSnapshot,
    /// Whether any answer has arrived for the current target.
    pub loaded: bool,
    /// Why the last fetch failed; the previous snapshot stays.
    pub error: Option<String>,
    pub rx: Option<Receiver<FetchAnswer>>,
}

impl MachinesFeed {
    /// Forget everything: a new hub or account.
    pub fn reset(&mut self, target: Option<Target>) {
        *self = Self {
            target,
            ..Self::default()
        };
    }
}
