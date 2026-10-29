//! Plain-Rust model of the agents screen: the agent rows derived from `GET /v1/machines`, the
//! disabled "add BYO agent" state, and the managed-agent launch form with its validation.
//! No GPUI in here.

use std::sync::{Arc, Mutex};

use serde::Deserialize;

/// Harness ids a managed agent can run (plan §4.7 Loadouts: ACP Registry ids).
pub const MANAGED_HARNESSES: [&str; 6] = ["claude", "codex", "opencode", "pi", "gemini", "goose"];

// ---- the source: GET /v1/machines ----------------------------------------------------------------

/// The body of `GET /v1/machines` (client-api §5.1).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MachinesResponse {
    #[serde(default)]
    pub machines: Vec<MachineInfo>,
}

impl MachinesResponse {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// One machine as the hub returns it. `capabilities` is `null` before the runner's first hello.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MachineInfo {
    pub machine_id: String,
    pub name: String,
    #[serde(default)]
    pub kind: String,
    pub presence: PresenceInfo,
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PresenceInfo {
    pub state: String,
    #[serde(default)]
    pub since: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    #[serde(default)]
    pub harnesses: Vec<String>,
}

// ---- agent rows ---------------------------------------------------------------------------------

/// Where an agent comes from (plan §1: `kind ∈ {local, managed, byo}`). Only `Local` can be listed
/// today: the hub has no agent registry yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Local,
    Managed,
    Byo,
}

impl AgentKind {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Managed => "managed",
            Self::Byo => "byo",
        }
    }
}

/// Presence of the machine the agent runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Online,
    Offline,
    Unknown,
}

impl Presence {
    pub fn parse(state: &str) -> Self {
        match state {
            "online" => Self::Online,
            "offline" => Self::Offline,
            _ => Self::Unknown,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
            Self::Unknown => "unknown",
        }
    }
}

/// Health of an agent. The hub has no health probe, so this is derived from presence only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ready,
    Unreachable,
    Unknown,
}

impl Health {
    pub fn slug(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unreachable => "unreachable",
            Self::Unknown => "unknown",
        }
    }
}

/// One row of the agents table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// `<machine_id>/<harness>`, unique.
    pub id: String,
    /// The harness id (`claude`, `opencode`, ...).
    pub name: String,
    pub kind: AgentKind,
    pub machine_id: String,
    pub machine_name: String,
    pub presence: Presence,
    /// Skills the source provides. Always empty today: `GET /v1/machines` carries none.
    pub skills: Vec<String>,
    pub health: Health,
}

/// One row per (machine, harness). A machine that has not said hello yet (no capabilities) or has
/// no harnesses contributes no rows. Sorted online first, then by name, machine, id.
pub fn agents_from_machines(machines: &[MachineInfo]) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = machines
        .iter()
        .flat_map(|machine| {
            let presence = Presence::parse(&machine.presence.state);
            let health = match presence {
                Presence::Online => Health::Ready,
                Presence::Offline => Health::Unreachable,
                Presence::Unknown => Health::Unknown,
            };
            machine
                .capabilities
                .iter()
                .flat_map(|caps| caps.harnesses.iter())
                .map(move |harness| AgentRow {
                    id: format!("{}/{harness}", machine.machine_id),
                    name: harness.clone(),
                    kind: AgentKind::Local,
                    machine_id: machine.machine_id.clone(),
                    machine_name: machine.name.clone(),
                    presence,
                    skills: Vec::new(),
                    health,
                })
        })
        .collect();
    rows.sort_by(|a, b| {
        (
            a.presence != Presence::Online,
            &a.name,
            &a.machine_name,
            &a.id,
        )
            .cmp(&(
                b.presence != Presence::Online,
                &b.name,
                &b.machine_name,
                &b.id,
            ))
    });
    rows
}

// ---- add BYO ------------------------------------------------------------------------------------

/// Whether the A2A edge (M2-004, P5) exists in this build.
pub const A2A_EDGE_AVAILABLE: bool = false;

/// Why "add BYO agent by URL" is disabled, as a Fluent id; `None` when it is enabled.
pub fn byo_disabled_reason() -> Option<&'static str> {
    (!A2A_EDGE_AVAILABLE).then_some("agents-byo-disabled")
}

// ---- managed launch -----------------------------------------------------------------------------

/// The request the M4 managed-agents API will take. There is no hub route yet; this shape is a
/// proposal (plan §3 surface 5: ACP agent + model + egress policy + Loadout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedLaunchRequest {
    pub name: String,
    pub harness: String,
    pub model: String,
    pub egress_allow: Vec<String>,
    pub loadout: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchedAgent {
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LaunchError(pub String);

/// The injectable M4 client. The real implementation lands with the M4 managed-agents route.
pub trait ManagedAgentClient: Send + Sync {
    fn launch(&self, request: &ManagedLaunchRequest) -> Result<LaunchedAgent, LaunchError>;
}

/// Records requests and answers with a canned result; for tests and for wiring before M4.
#[derive(Clone)]
pub struct FakeManagedClient {
    pub requests: Arc<Mutex<Vec<ManagedLaunchRequest>>>,
    pub result: Result<LaunchedAgent, LaunchError>,
}

impl FakeManagedClient {
    pub fn ok(agent_id: &str) -> Self {
        Self {
            requests: Arc::default(),
            result: Ok(LaunchedAgent {
                agent_id: agent_id.to_owned(),
            }),
        }
    }

    pub fn failing(message: &str) -> Self {
        Self {
            requests: Arc::default(),
            result: Err(LaunchError(message.to_owned())),
        }
    }
}

impl ManagedAgentClient for FakeManagedClient {
    fn launch(&self, request: &ManagedLaunchRequest) -> Result<LaunchedAgent, LaunchError> {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        self.result.clone()
    }
}

/// The client the shell passes until the managed-agent route exists (M4-007): every launch fails
/// with a clear "not available yet" error.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableManagedClient;

impl ManagedAgentClient for UnavailableManagedClient {
    fn launch(&self, _: &ManagedLaunchRequest) -> Result<LaunchedAgent, LaunchError> {
        Err(LaunchError(
            "Launching managed agents is not available yet.".to_owned(),
        ))
    }
}

/// Launch configuration. `model_key_configured` is an injected flag until settings (model keys)
/// can answer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaunchConfig {
    pub model_key_configured: bool,
}

impl LaunchConfig {
    /// Why launching is disabled, as a Fluent id; `None` when a model key is configured.
    pub fn disabled_reason(&self) -> Option<&'static str> {
        (!self.model_key_configured).then_some("agents-launch-disabled-no-key")
    }
}

/// The raw text of the form.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaunchForm {
    pub name: String,
    pub harness: String,
    pub model: String,
    /// Comma- or whitespace-separated host names.
    pub egress: String,
    pub loadout: String,
}

/// A form problem: a Fluent id plus the offending value, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormError {
    pub id: &'static str,
    pub value: Option<String>,
}

impl LaunchForm {
    /// Check every field; all problems are returned, none means the request is built.
    pub fn validate(&self) -> Result<ManagedLaunchRequest, Vec<FormError>> {
        let mut errors = Vec::new();
        let name = self.name.trim();
        if name.is_empty() {
            errors.push(FormError {
                id: "agents-error-name-required",
                value: None,
            });
        }
        let harness = self.harness.trim();
        if !MANAGED_HARNESSES.contains(&harness) {
            errors.push(FormError {
                id: "agents-error-harness-unknown",
                value: Some(harness.to_owned()),
            });
        }
        let model = self.model.trim();
        if model.is_empty() {
            errors.push(FormError {
                id: "agents-error-model-required",
                value: None,
            });
        }
        let mut egress_allow = Vec::new();
        for host in self
            .egress
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|h| !h.is_empty())
        {
            if valid_host(host) {
                egress_allow.push(host.to_ascii_lowercase());
            } else {
                errors.push(FormError {
                    id: "agents-error-egress-invalid",
                    value: Some(host.to_owned()),
                });
            }
        }
        if errors.is_empty() {
            let loadout = self.loadout.trim();
            Ok(ManagedLaunchRequest {
                name: name.to_owned(),
                harness: harness.to_owned(),
                model: model.to_owned(),
                egress_allow,
                loadout: (!loadout.is_empty()).then(|| loadout.to_owned()),
            })
        } else {
            Err(errors)
        }
    }
}

/// A bare host name or `*.host` wildcard: no scheme, path, port or spaces.
fn valid_host(host: &str) -> bool {
    let host = host.strip_prefix("*.").unwrap_or(host);
    host.contains('.')
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Where a launch attempt stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LaunchState {
    #[default]
    Idle,
    Invalid(Vec<FormError>),
    Launched(LaunchedAgent),
    Failed(String),
}

/// Submit the form: refuse without a model key, validate, then call the client.
pub fn submit(
    config: LaunchConfig,
    form: &LaunchForm,
    client: &dyn ManagedAgentClient,
) -> LaunchState {
    if config.disabled_reason().is_some() {
        return LaunchState::Idle;
    }
    match form.validate() {
        Err(errors) => LaunchState::Invalid(errors),
        Ok(request) => match client.launch(&request) {
            Ok(agent) => LaunchState::Launched(agent),
            Err(error) => LaunchState::Failed(error.0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentKind, AgentRow, FakeManagedClient, Health, LaunchConfig, LaunchForm, LaunchState,
        LaunchedAgent, MachinesResponse, Presence, agents_from_machines, byo_disabled_reason,
        submit,
    };

    const HUB_BODY: &str = r#"{"machines":[
      {"machine_id":"mch_b","name":"build-box","kind":"local",
       "presence":{"state":"offline","since":"2026-10-27T22:00:00.000Z"},
       "capabilities":{"harnesses":["claude"],"spawn":["same_dir"],"max_sessions":4}},
      {"machine_id":"mch_a","name":"my-laptop","kind":"local",
       "presence":{"state":"online","since":"2026-10-27T22:00:00.000Z"},
       "capabilities":{"harnesses":["opencode","claude"],"permission_mode":"default"}},
      {"machine_id":"mch_c","name":"fresh","kind":"local",
       "presence":{"state":"offline","since":"2026-10-27T22:00:00.000Z"},
       "capabilities":null}]}"#;

    fn rows() -> Vec<AgentRow> {
        agents_from_machines(&MachinesResponse::parse(HUB_BODY).unwrap().machines)
    }

    #[test]
    fn one_row_per_machine_harness_online_first() {
        let ids: Vec<_> = rows().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["mch_a/claude", "mch_a/opencode", "mch_b/claude"]);
    }

    #[test]
    fn presence_and_health_come_from_the_machine() {
        let rows = rows();
        assert_eq!(rows[0].presence, Presence::Online);
        assert_eq!(rows[0].health, Health::Ready);
        assert_eq!(rows[2].presence, Presence::Offline);
        assert_eq!(rows[2].health, Health::Unreachable);
        assert_eq!(rows[0].machine_name, "my-laptop");
        assert!(rows.iter().all(|r| r.skills.is_empty()));
        assert!(rows.iter().all(|r| r.kind == AgentKind::Local));
    }

    #[test]
    fn unknown_presence_states_are_unknown_health() {
        assert_eq!(Presence::parse("degraded"), Presence::Unknown);
    }

    #[test]
    fn valid_form_builds_a_normalized_request() {
        let form = LaunchForm {
            name: " reviewer ".into(),
            harness: "claude".into(),
            model: "claude-opus".into(),
            egress: "API.github.com, *.npmjs.org".into(),
            loadout: "".into(),
        };
        let request = form.validate().unwrap();
        assert_eq!(request.name, "reviewer");
        assert_eq!(request.egress_allow, ["api.github.com", "*.npmjs.org"]);
        assert_eq!(request.loadout, None);
    }

    #[test]
    fn invalid_form_reports_every_problem() {
        let form = LaunchForm {
            harness: "nope".into(),
            egress: "https://evil.example/x good.example.com".into(),
            ..LaunchForm::default()
        };
        let ids: Vec<_> = form.validate().unwrap_err().iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            [
                "agents-error-name-required",
                "agents-error-harness-unknown",
                "agents-error-model-required",
                "agents-error-egress-invalid"
            ]
        );
    }

    fn good_form() -> LaunchForm {
        LaunchForm {
            name: "a".into(),
            harness: "codex".into(),
            model: "m".into(),
            ..LaunchForm::default()
        }
    }

    #[test]
    fn submit_without_a_key_never_calls_the_client() {
        let client = FakeManagedClient::ok("ag_1");
        let state = submit(LaunchConfig::default(), &good_form(), &client);
        assert_eq!(state, LaunchState::Idle);
        assert!(client.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn submit_with_a_key_calls_the_client_once() {
        let client = FakeManagedClient::ok("ag_1");
        let config = LaunchConfig {
            model_key_configured: true,
        };
        let state = submit(config, &good_form(), &client);
        assert_eq!(
            state,
            LaunchState::Launched(LaunchedAgent {
                agent_id: "ag_1".into()
            })
        );
        assert_eq!(client.requests.lock().unwrap().len(), 1);
        let failing = FakeManagedClient::failing("boom");
        assert_eq!(
            submit(config, &good_form(), &failing),
            LaunchState::Failed("boom".into())
        );
    }

    #[test]
    fn byo_is_disabled_until_the_a2a_edge_exists() {
        assert_eq!(byo_disabled_reason(), Some("agents-byo-disabled"));
    }
}
