//! `session_config` (tunnel.md §3.5): the thread's ACP session mode and one config option.
//!
//! The bridge checks both ids against what the session advertises (its `session/new`,
//! `session/load` or `session/resume` response, kept current by the agent's own updates). An
//! unknown mode or option, or a value the option does not take, is refused with
//! `command_result{invalid}` before any ACP call. A valid change is sent at once. It reaches the
//! hub as the agent's `current_mode_update` / `config_option_update`, which the mapper turns into
//! `StateDelta{session}`.
//!
//! The session's `StateSnapshot{session}` is emitted when this process first attaches the session
//! (see [`Bridge::session_snapshot`]) from the response, if the agent advertised modes or config options.

use std::sync::Arc;

use protocol::acp::{
    NewSessionResponse, SessionConfigId, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionValue, SessionConfigSelectOptions, SessionConfigValueId, SessionId,
    SessionModeId,
};
use protocol::mapping::acp_turn::AcpTurnMapper;
use protocol::tunnel::CommandErrorCode;
use protocol::turn_event::Envelope;
use serde_json::Value;

use super::ids::SharedIds;
use super::io::{CommandSource, EventSink};
use super::{Bridge, BridgeError, ThreadState};
use crate::acp::reattach::ReattachError;

impl<S: EventSink, C: CommandSource> Bridge<S, C> {
    /// Applies a `session_config` command. `config` is the option id and its JSON value.
    pub(super) async fn session_config(
        &mut self,
        key: &str,
        thread_id: &str,
        mode_id: Option<String>,
        config: Option<(String, Value)>,
    ) -> Result<(), BridgeError> {
        if mode_id.is_none() && config.is_none() {
            return self
                .reject(
                    key,
                    CommandErrorCode::Invalid,
                    "session_config sets neither a mode nor a config option",
                )
                .await;
        }
        let session = match self.config_session(thread_id).await? {
            Ok(session) => session,
            Err((code, message)) => return self.reject(key, code, message).await,
        };

        // Everything is checked before the first ACP call, so a refused command changes nothing.
        let advertised = self.client.advertised(&session).unwrap_or_default();
        let mode = match mode_id {
            Some(mode) => {
                let known = advertised.modes.as_ref().is_some_and(|modes| {
                    modes
                        .available_modes
                        .iter()
                        .any(|available| &*available.id.0 == mode.as_str())
                });
                if !known {
                    return self
                        .reject(
                            key,
                            CommandErrorCode::Invalid,
                            format!("unknown mode `{mode}`"),
                        )
                        .await;
                }
                Some(SessionModeId::from(mode))
            }
            None => None,
        };
        let options: &[SessionConfigOption] = advertised.config_options.as_deref().unwrap_or(&[]);
        let config = match config {
            Some((id, value)) => {
                let Some(option) = options.iter().find(|option| &*option.id.0 == id.as_str())
                else {
                    return self
                        .reject(
                            key,
                            CommandErrorCode::Invalid,
                            format!("unknown config option `{id}`"),
                        )
                        .await;
                };
                match config_value(option, &value) {
                    Ok(value) => Some((SessionConfigId::from(id), value)),
                    Err(message) => {
                        return self.reject(key, CommandErrorCode::Invalid, message).await;
                    }
                }
            }
            None => None,
        };

        if let Some(mode) = mode
            && let Err(error) = self.client.set_mode(&session, mode).await
        {
            return self
                .reject(
                    key,
                    CommandErrorCode::Invalid,
                    format!("session/set_mode failed: {error}"),
                )
                .await;
        }
        if let Some((id, value)) = config
            && let Err(error) = self.client.set_config_option(&session, id, value).await
        {
            return self
                .reject(
                    key,
                    CommandErrorCode::Invalid,
                    format!("session/set_config_option failed: {error}"),
                )
                .await;
        }
        Ok(())
    }

    /// The session of a known thread. A session this process has not attached yet (the thread
    /// is idle after a restart) is attached now. `Err` is the `command_result` to send.
    async fn config_session(
        &mut self,
        thread_id: &str,
    ) -> Result<Result<SessionId, (CommandErrorCode, String)>, BridgeError> {
        if let Some(session) = self
            .threads
            .get(thread_id)
            .and_then(|thread| thread.session.clone())
        {
            return Ok(Ok(session));
        }
        let known = self.threads.contains_key(thread_id) || self.store.thread(thread_id)?.is_some();
        if !known {
            return Ok(Err((
                CommandErrorCode::UnknownThread,
                format!("unknown thread {thread_id}"),
            )));
        }
        let (harness, cwd) = self.thread_defaults(thread_id, None, None)?;
        if let Some(error) = self.ensure_agent().await? {
            return Ok(Err((
                CommandErrorCode::Invalid,
                format!("could not start the agent: {error}"),
            )));
        }
        let mcp_servers = self.mcp_servers_for(thread_id);
        let attached = match self
            .reattacher
            .attach(&self.client, thread_id, &harness, &cwd, &mcp_servers)
            .await
        {
            Ok(attached) => attached,
            Err(ReattachError::Store(error)) => return Err(error.into()),
            Err(error) => {
                return Ok(Err((
                    CommandErrorCode::Invalid,
                    format!("could not attach the session: {error}"),
                )));
            }
        };
        let session = attached.session;
        let ids = Arc::clone(&self.ids);
        self.threads
            .entry(thread_id.to_owned())
            .or_insert_with(|| ThreadState {
                harness: harness.clone(),
                cwd: cwd.clone(),
                mapper: AcpTurnMapper::new(thread_id.to_owned(), SharedIds(ids)),
                session: None,
                task: None,
            });
        self.by_session
            .insert(session.0.to_string(), thread_id.to_owned());
        if let Some(thread) = self.threads.get_mut(thread_id) {
            thread.session = Some(session.clone());
        }
        if let Some(response) = attached.response {
            let snapshot = self.session_snapshot(thread_id, response);
            self.emit(thread_id, snapshot).await?;
        }
        if let Err(message) = self.apply_initial_mode(&session).await {
            return Ok(Err((CommandErrorCode::Invalid, message)));
        }
        Ok(Ok(session))
    }

    /// Puts a freshly attached session in [`Config::initial_mode`]. `Err` is why it could not.
    pub(super) async fn apply_initial_mode(&mut self, session: &SessionId) -> Result<(), String> {
        let Some(mode) = self.config.initial_mode.clone() else {
            return Ok(());
        };
        if self.moded.contains(&*session.0) {
            return Ok(());
        }
        let known = self
            .client
            .advertised(session)
            .and_then(|advertised| advertised.modes)
            .is_some_and(|modes| {
                modes
                    .available_modes
                    .iter()
                    .any(|available| &*available.id.0 == mode.as_str())
            });
        if !known {
            return Err(format!(
                "the agent does not offer the permission mode `{mode}`"
            ));
        }
        self.client
            .set_mode(session, SessionModeId::from(mode.clone()))
            .await
            .map_err(|error| format!("could not set the permission mode `{mode}`: {error}"))?;
        self.moded.insert(session.0.to_string());
        Ok(())
    }

    /// The `StateSnapshot{session}` of what the agent answered to `session/new`, `session/resume`
    /// or `session/load` (§4.6), or nothing when it advertised neither modes nor config options.
    /// Without a snapshot the mapper makes one at the session's first update, so the snapshot
    /// still comes before any delta.
    pub(super) fn session_snapshot(
        &mut self,
        thread_id: &str,
        response: NewSessionResponse,
    ) -> Vec<Envelope> {
        if response.modes.is_none() && response.config_options.is_none() {
            return Vec::new();
        }
        let ts = self.ts();
        match self.threads.get_mut(thread_id) {
            Some(thread) => thread.mapper.on_session_new(&response, &ts),
            None => Vec::new(),
        }
    }
}

/// The ACP value for `value` of `option`: a boolean for a boolean option, a listed value id for
/// a select option. `Err` is the reason the value does not fit.
fn config_value(
    option: &SessionConfigOption,
    value: &Value,
) -> Result<SessionConfigOptionValue, String> {
    match (&option.kind, value) {
        (SessionConfigKind::Boolean(_), Value::Bool(flag)) => {
            Ok(SessionConfigOptionValue::boolean(*flag))
        }
        (SessionConfigKind::Select(select), Value::String(text))
            if select_values(&select.options)
                .iter()
                .any(|known| &*known.0 == text.as_str()) =>
        {
            Ok(SessionConfigOptionValue::value_id(text.clone()))
        }
        (SessionConfigKind::Select(_), Value::String(text)) => Err(format!(
            "`{text}` is not a value of config option `{}`",
            option.id.0
        )),
        _ => Err(format!(
            "the value does not fit config option `{}`",
            option.id.0
        )),
    }
}

/// Every value id a select option lists, grouped or not.
fn select_values(options: &SessionConfigSelectOptions) -> Vec<&SessionConfigValueId> {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().map(|option| &option.value).collect()
        }
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(|option| &option.value))
            .collect(),
        // The schema marks the enum non-exhaustive; an unknown shape lists nothing.
        _ => Vec::new(),
    }
}
