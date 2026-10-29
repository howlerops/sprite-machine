//! The new-task composer's model (plan §3, surface 1): what the user has chosen, the draft that
//! survives closing the app, validation, and the `POST /v1/threads` request it becomes. No GPUI
//! in here; the view reads it and forwards input.
//!
//! The hub has no agent-card or workspace listing API yet, so the agent list is the fixed set of
//! harnesses the runner discovers (plan §7.4), narrowed to a machine's advertised `harnesses`
//! once the machines list is loaded, and the workspace is a path on the chosen machine. Mode is
//! the `permission_mode` field of the request; there is no per-card mode/model list to read.

use std::path::{Path, PathBuf};

use client_core::outbox::new_idempotency_key;
use client_core::{CreateThread, CreateThreadError, FirstPrompt, ThreadCreated, create_thread};
use protocol::tunnel::{ContentPart, Workspace};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The harnesses a runner can discover (plan §7.4). Used until the hub lists agents.
pub const AGENTS: [&str; 6] = ["claude", "codex", "opencode", "pi", "gemini", "goose"];

/// Permission modes offered by the composer; `None` leaves the agent's default.
pub const MODES: [&str; 4] = ["default", "acceptEdits", "plan", "bypassPermissions"];

/// How loose a permission mode is (plan §4.10): `plan` < `default` < `acceptEdits` <
/// `bypassPermissions`. `None` for a name that is not a mode.
pub fn mode_looseness(mode: &str) -> Option<u8> {
    match mode {
        "plan" => Some(0),
        "default" => Some(1),
        "acceptEdits" => Some(2),
        "bypassPermissions" => Some(3),
        _ => None,
    }
}

/// How the runner starts the session in the workspace directory (client-api.md §5.2).
pub const SPAWNS: [&str; 2] = ["same_dir", "worktree"];

/// The file the draft is kept in, under the config directory.
pub const DRAFT_FILE: &str = "new-task-draft.json";

/// One machine from `GET /v1/machines`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineOption {
    pub machine_id: String,
    pub name: String,
    pub online: bool,
    /// Harness names the machine advertises; empty means it did not say.
    pub harnesses: Vec<String>,
    pub spawn: Vec<String>,
    /// The machine's local permission mode, the loosest a remote request can get; `None` when
    /// the machine did not say (any mode is offered).
    pub permission_mode: Option<String>,
}

/// Parse the `GET /v1/machines` answer. Entries without a `machine_id` are skipped.
pub fn parse_machines(body: &Value) -> Vec<MachineOption> {
    let strings = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    body.get("machines")
        .and_then(Value::as_array)
        .map(|machines| {
            machines
                .iter()
                .filter_map(|machine| {
                    let machine_id = machine.get("machine_id")?.as_str()?.to_owned();
                    let name = machine
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&machine_id)
                        .to_owned();
                    let online = machine.pointer("/presence/state").and_then(Value::as_str)
                        == Some("online");
                    Some(MachineOption {
                        machine_id,
                        name,
                        online,
                        harnesses: strings(machine.pointer("/capabilities/harnesses")),
                        spawn: strings(machine.pointer("/capabilities/spawn")),
                        permission_mode: machine
                            .pointer("/capabilities/permission_mode")
                            .and_then(Value::as_str)
                            .filter(|mode| mode_looseness(mode).is_some())
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `GET {api}/machines`. `api` is the `…/v1` base URL.
pub async fn fetch_machines(api: &str, token: Option<&str>) -> Result<Vec<MachineOption>, String> {
    let mut request = reqwest::Client::new().get(format!("{}/machines", api.trim_end_matches('/')));
    if let Some(token) = token {
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
    Ok(parse_machines(&body))
}

/// What the user has typed and chosen so far. This is what is saved as the draft.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub machine_id: Option<String>,
    pub agent: String,
    pub workspace_path: String,
    pub spawn: String,
    pub mode: Option<String>,
    pub title: String,
    pub prompt: String,
}

impl Draft {
    fn blank() -> Self {
        Self {
            agent: AGENTS[0].to_owned(),
            spawn: SPAWNS[0].to_owned(),
            ..Self::default()
        }
    }

    /// Nothing worth keeping: no text, no path.
    pub fn is_empty(&self) -> bool {
        self.prompt.trim().is_empty()
            && self.workspace_path.trim().is_empty()
            && self.title.trim().is_empty()
    }
}

/// Why the composer cannot send yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    NoMachine,
    MachineOffline,
    NoAgent,
    AgentNotOnMachine,
    NoWorkspace,
    NoPrompt,
}

/// A validated send: the request and the key to send it under. Retrying sends the same value, so
/// the hub replays its answer instead of creating a second thread.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSend {
    pub idempotency_key: String,
    pub request: CreateThread,
}

/// POST the prepared request.
pub async fn execute(
    api: &str,
    token: Option<&str>,
    prepared: &PreparedSend,
) -> Result<ThreadCreated, CreateThreadError> {
    create_thread(api, token, &prepared.idempotency_key, &prepared.request).await
}

pub struct NewTaskModel {
    draft: Draft,
    machines: Vec<MachineOption>,
    path: Option<PathBuf>,
    pending: Option<PreparedSend>,
    save_error: Option<String>,
    capped_from: Option<String>,
}

impl NewTaskModel {
    /// Open the composer, restoring the draft saved in `config_dir` (when given and readable).
    pub fn open(config_dir: Option<&Path>) -> Self {
        let path = config_dir.map(|dir| dir.join(DRAFT_FILE));
        let draft = path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Draft>(&text).ok())
            .map(|mut draft| {
                if draft.agent.is_empty() {
                    draft.agent = AGENTS[0].to_owned();
                }
                if !SPAWNS.contains(&draft.spawn.as_str()) {
                    draft.spawn = SPAWNS[0].to_owned();
                }
                draft
            })
            .unwrap_or_else(Draft::blank);
        Self {
            draft,
            machines: Vec::new(),
            path,
            pending: None,
            save_error: None,
            capped_from: None,
        }
    }

    pub fn draft(&self) -> &Draft {
        &self.draft
    }

    pub fn machines(&self) -> &[MachineOption] {
        &self.machines
    }

    /// The last error from writing the draft, if any (the draft still works in memory).
    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    pub fn selected_machine(&self) -> Option<&MachineOption> {
        let id = self.draft.machine_id.as_deref()?;
        self.machines
            .iter()
            .find(|machine| machine.machine_id == id)
    }

    /// Agents offered for the chosen machine: what it advertises, else the fixed list.
    pub fn agents(&self) -> Vec<String> {
        match self.selected_machine() {
            Some(machine) if !machine.harnesses.is_empty() => machine.harnesses.clone(),
            _ => AGENTS.iter().map(|agent| (*agent).to_owned()).collect(),
        }
    }

    /// Spawn modes offered for the chosen machine.
    pub fn spawns(&self) -> Vec<String> {
        match self.selected_machine() {
            Some(machine) if !machine.spawn.is_empty() => machine.spawn.clone(),
            _ => SPAWNS.iter().map(|spawn| (*spawn).to_owned()).collect(),
        }
    }

    /// The chosen machine's local permission mode, if it advertises one.
    pub fn local_mode(&self) -> Option<&str> {
        self.selected_machine()?.permission_mode.as_deref()
    }

    /// Permission modes offered for the chosen machine: none looser than its local mode.
    pub fn modes(&self) -> Vec<&'static str> {
        let cap = self.local_mode().and_then(mode_looseness);
        MODES
            .iter()
            .copied()
            .filter(|mode| cap.is_none_or(|cap| mode_looseness(mode).is_some_and(|m| m <= cap)))
            .collect()
    }

    /// The mode the user chose before the machine's cap lowered it, while that is the case: the
    /// composer says so. Cleared when the user picks a mode again.
    pub fn capped_from(&self) -> Option<&str> {
        self.capped_from.as_deref()
    }

    /// Replace the machines list. A saved choice that is still listed is kept; otherwise the
    /// first online machine (else the first) is chosen.
    pub fn set_machines(&mut self, machines: Vec<MachineOption>) {
        let before = self.draft.clone();
        self.machines = machines;
        if self.selected_machine().is_none() {
            self.draft.machine_id = self
                .machines
                .iter()
                .find(|machine| machine.online)
                .or_else(|| self.machines.first())
                .map(|machine| machine.machine_id.clone());
        }
        self.fit_choices();
        if self.draft != before {
            self.changed();
        }
    }

    pub fn set_machine(&mut self, machine_id: &str) {
        if self.machines.iter().any(|m| m.machine_id == machine_id) {
            self.draft.machine_id = Some(machine_id.to_owned());
            self.fit_choices();
            self.changed();
        }
    }

    pub fn set_agent(&mut self, agent: &str) {
        if self.agents().iter().any(|known| known == agent) {
            agent.clone_into(&mut self.draft.agent);
            self.changed();
        }
    }

    pub fn set_spawn(&mut self, spawn: &str) {
        if self.spawns().iter().any(|known| known == spawn) {
            spawn.clone_into(&mut self.draft.spawn);
            self.changed();
        }
    }

    /// `None` leaves the agent's default mode.
    pub fn set_mode(&mut self, mode: Option<&str>) {
        if mode.is_none_or(|mode| self.modes().contains(&mode)) {
            self.draft.mode = mode.map(str::to_owned);
            self.capped_from = None;
            self.changed();
        }
    }

    pub fn set_workspace_path(&mut self, path: &str) {
        if self.draft.workspace_path != path {
            path.clone_into(&mut self.draft.workspace_path);
            self.changed();
        }
    }

    pub fn set_title(&mut self, title: &str) {
        if self.draft.title != title {
            title.clone_into(&mut self.draft.title);
            self.changed();
        }
    }

    pub fn set_prompt(&mut self, prompt: &str) {
        if self.draft.prompt != prompt {
            prompt.clone_into(&mut self.draft.prompt);
            self.changed();
        }
    }

    /// Keep agent and spawn within what the chosen machine offers.
    fn fit_choices(&mut self) {
        let agents = self.agents();
        if !agents.contains(&self.draft.agent) {
            self.draft.agent = agents.first().cloned().unwrap_or_default();
        }
        let spawns = self.spawns();
        if !spawns.contains(&self.draft.spawn) {
            self.draft.spawn = spawns.first().cloned().unwrap_or_default();
        }
        // A chosen mode looser than the machine allows is lowered to the machine's own mode.
        let modes = self.modes();
        if let Some(mode) = self
            .draft
            .mode
            .clone()
            .filter(|mode| !modes.contains(&mode.as_str()))
        {
            self.draft.mode = self.local_mode().map(str::to_owned);
            self.capped_from = Some(mode);
        } else if self.draft.mode.is_some() {
            self.capped_from = None;
        }
    }

    /// Why the composer cannot send, or `None` when it can.
    pub fn problem(&self) -> Option<Invalid> {
        let Some(machine) = self.selected_machine() else {
            return Some(Invalid::NoMachine);
        };
        if !machine.online {
            return Some(Invalid::MachineOffline);
        }
        if self.draft.agent.is_empty() {
            return Some(Invalid::NoAgent);
        }
        if !machine.harnesses.is_empty() && !machine.harnesses.contains(&self.draft.agent) {
            return Some(Invalid::AgentNotOnMachine);
        }
        if self.draft.workspace_path.trim().is_empty() {
            return Some(Invalid::NoWorkspace);
        }
        if self.draft.prompt.trim().is_empty() {
            return Some(Invalid::NoPrompt);
        }
        None
    }

    /// Validate and build the request. The same key and ids are returned until the draft changes
    /// or the send succeeds, so a retry after a failure cannot create a second thread.
    pub fn prepare_send(&mut self) -> Result<PreparedSend, Invalid> {
        if let Some(problem) = self.problem() {
            return Err(problem);
        }
        if let Some(pending) = &self.pending {
            return Ok(pending.clone());
        }
        let draft = &self.draft;
        let title = draft.title.trim();
        let prepared = PreparedSend {
            idempotency_key: new_idempotency_key(),
            request: CreateThread {
                machine_id: draft.machine_id.clone().unwrap_or_default(),
                agent: draft.agent.clone(),
                workspace: Workspace {
                    path: draft.workspace_path.trim().to_owned(),
                    branch: None,
                    spawn: Some(draft.spawn.clone()),
                },
                permission_mode: draft.mode.clone(),
                title: (!title.is_empty()).then(|| title.to_owned()),
                prompt: Some(FirstPrompt {
                    task_id: format!("tk_{:032x}", rand::random::<u128>()),
                    message_id: format!("msg_{:032x}", rand::random::<u128>()),
                    parts: vec![ContentPart::Text {
                        text: draft.prompt.trim_end().to_owned(),
                    }],
                }),
            },
        };
        self.pending = Some(prepared.clone());
        Ok(prepared)
    }

    /// The hub created the thread: forget the prompt and title and the pending request, keep the
    /// choices (machine, agent, workspace) for the next task.
    pub fn sent(&mut self) {
        self.draft.prompt.clear();
        self.draft.title.clear();
        self.changed();
    }

    fn changed(&mut self) {
        self.pending = None;
        self.save_error = self.save().err().map(|error| error.to_string());
    }

    /// Write the draft (or remove the file when it holds nothing worth keeping).
    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if self.draft.is_empty() {
            return match std::fs::remove_file(path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(&self.draft).map_err(std::io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, path)
    }
}
