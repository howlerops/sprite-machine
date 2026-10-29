//! Decisions around the card that are not about drawing it: answering through `client-core`'s
//! [`Outbox`], following the answer's fate (pending, 409 conflict, failure), the "save as the
//! workspace's preset" choice, and the first-seen time of a resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use client_core::Cache;
use client_core::outbox::{Clock, Intent, IntentState, Outbox, OutboxError};
use client_core::view::{LocalAnswer, ThreadView};
use protocol::tunnel::Outcome;
use protocol::turn_event::{InterruptKind, PermissionOptionKind};
use serde::{Deserialize, Serialize};

use super::model::{ApprovalCard, CardContext, build_card, parse_rfc3339_ms};
use super::tr;

/// Why [`ApprovalController::answer`] refused.
#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    #[error("no approval request {0} in this thread")]
    UnknownInterrupt(String),
    #[error("{0} is not one of the request's options")]
    UnknownOption(String),
    #[error("the request was already answered")]
    NotOpen,
    #[error("the request expired")]
    Expired,
    #[error("an answer is already on its way")]
    AlreadyAnswering,
    #[error("could not queue the answer: {0}")]
    Outbox(#[from] OutboxError),
}

/// A workspace preset the user chose to save from an `allow_always` option.
///
/// The hub has no preset API yet (see the M3-011 report), so the choice is recorded on this
/// device only; `synced` stays `false` until a hub call exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetEntry {
    pub workspace: String,
    pub option_id: String,
    pub option_name: String,
    pub interrupt_id: String,
    pub tool_title: String,
    pub saved_at_ms: u64,
    pub synced: bool,
}

/// The presets chosen on this device, optionally persisted as JSON.
#[derive(Debug, Clone, Default)]
pub struct LocalPresets {
    entries: Vec<PresetEntry>,
    path: Option<PathBuf>,
}

impl LocalPresets {
    /// Memory only.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Loads `path` (a missing file is an empty list) and writes back to it on every change.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            entries,
            path: Some(path),
        })
    }

    pub fn entries(&self) -> &[PresetEntry] {
        &self.entries
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether `workspace` already has a preset for the option `option_name`.
    pub fn has(&self, workspace: &str, option_id: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.workspace == workspace && entry.option_id == option_id)
    }

    fn record(&mut self, entry: PresetEntry) -> io::Result<()> {
        self.entries.push(entry);
        match &self.path {
            Some(path) => {
                let text = serde_json::to_string_pretty(&self.entries).map_err(io::Error::other)?;
                std::fs::write(path, text)
            }
            None => Ok(()),
        }
    }
}

/// Owns the answers this device sent for one thread.
pub struct ApprovalController {
    thread_id: String,
    workspace: String,
    clock: Arc<dyn Clock>,
    presets: LocalPresets,
    remember: BTreeSet<String>,
    saved: BTreeSet<String>,
    /// interrupt id -> local id of the queued answer.
    answers: BTreeMap<String, i64>,
    /// interrupt id -> when this client first saw it resolved (ms).
    resolved_at: BTreeMap<String, u64>,
}

impl ApprovalController {
    pub fn new(
        thread_id: impl Into<String>,
        workspace: impl Into<String>,
        clock: Arc<dyn Clock>,
        presets: LocalPresets,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            workspace: workspace.into(),
            clock,
            presets,
            remember: BTreeSet::new(),
            saved: BTreeSet::new(),
            answers: BTreeMap::new(),
            resolved_at: BTreeMap::new(),
        }
    }

    pub fn presets(&self) -> &LocalPresets {
        &self.presets
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// Ticks or unticks "save as this workspace's preset" on one card.
    pub fn set_remember_preset(&mut self, interrupt_id: &str, remember: bool) {
        if remember {
            self.remember.insert(interrupt_id.to_owned());
        } else {
            self.remember.remove(interrupt_id);
        }
    }

    pub fn remember_preset(&self, interrupt_id: &str) -> bool {
        self.remember.contains(interrupt_id)
    }

    /// Records the hub's own time for a resolution (for example the envelope `ts`), replacing
    /// the first-seen time.
    pub fn set_resolved_at(&mut self, interrupt_id: &str, at_ms: u64) {
        self.resolved_at.insert(interrupt_id.to_owned(), at_ms);
    }

    /// Queues the answer: `option_id` is sent exactly as the agent offered it. When the option
    /// is `allow_always` and the card's preset box is ticked, the preset is also recorded.
    /// Returns the outbox local id.
    pub fn answer(
        &mut self,
        view: &mut ThreadView,
        outbox: &mut Outbox,
        cache: &Cache,
        interrupt_id: &str,
        option_id: &str,
    ) -> Result<i64, AnswerError> {
        let now_ms = self.clock.now_ms();
        let (title, option, expired) = {
            let interrupt = view
                .tasks()
                .iter()
                .find_map(|task| task.transcript().interrupt(interrupt_id))
                .filter(|found| found.kind() == InterruptKind::Approval)
                .ok_or_else(|| AnswerError::UnknownInterrupt(interrupt_id.to_owned()))?;
            if !interrupt.is_open() {
                return Err(AnswerError::NotOpen);
            }
            let option = interrupt
                .options()
                .iter()
                .find(|option| option.option_id == option_id)
                .cloned()
                .ok_or_else(|| AnswerError::UnknownOption(option_id.to_owned()))?;
            let expired =
                parse_rfc3339_ms(&interrupt.interrupt.expires_at).is_some_and(|at| now_ms >= at);
            (interrupt.interrupt.title.clone(), option, expired)
        };
        if expired {
            return Err(AnswerError::Expired);
        }
        if let Some(local_id) = self.answers.get(interrupt_id)
            && matches!(
                outbox.get(*local_id).map(|entry| &entry.state),
                Some(IntentState::Queued | IntentState::Done { .. })
            )
        {
            return Err(AnswerError::AlreadyAnswering);
        }
        let local_id = outbox.enqueue(
            cache,
            Intent::Approval {
                thread_id: self.thread_id.clone(),
                interrupt_id: interrupt_id.to_owned(),
                outcome: Outcome::Selected {
                    option_id: option.option_id.clone(),
                },
                resolved_by: None,
            },
            now_ms,
        )?;
        self.answers.insert(interrupt_id.to_owned(), local_id);
        view.set_local_answer(interrupt_id, Some(LocalAnswer::Pending));
        if option.kind == PermissionOptionKind::AllowAlways
            && self.remember.contains(interrupt_id)
            && !self.presets.has(&self.workspace, &option.option_id)
        {
            let entry = PresetEntry {
                workspace: self.workspace.clone(),
                option_id: option.option_id.clone(),
                option_name: option.name.clone(),
                interrupt_id: interrupt_id.to_owned(),
                tool_title: title,
                saved_at_ms: now_ms,
                synced: false,
            };
            if self.presets.record(entry).is_ok() {
                self.saved.insert(interrupt_id.to_owned());
            }
        }
        Ok(local_id)
    }

    /// Copies the fate of this device's answers from the outbox into the view: still queued,
    /// delivered, lost to another device (409), or refused.
    pub fn sync(&mut self, outbox: &Outbox, view: &mut ThreadView) {
        let now_ms = self.clock.now_ms();
        let tracked: Vec<(String, i64)> = self
            .answers
            .iter()
            .map(|(id, local)| (id.clone(), *local))
            .collect();
        for (interrupt_id, local_id) in tracked {
            let Some(entry) = outbox.get(local_id) else {
                self.answers.remove(&interrupt_id);
                view.set_local_answer(&interrupt_id, None);
                continue;
            };
            match &entry.state {
                IntentState::Queued => {
                    view.set_local_answer(&interrupt_id, Some(LocalAnswer::Pending));
                }
                IntentState::Done { .. } => {
                    view.set_local_answer(&interrupt_id, None);
                }
                IntentState::Conflict { resolved_by, .. } => {
                    self.resolved_at
                        .entry(interrupt_id.clone())
                        .or_insert(now_ms);
                    match resolved_by {
                        Some(winner) => {
                            view.set_local_answer(&interrupt_id, None);
                            view.note_conflict(&interrupt_id, winner.clone());
                        }
                        None => {
                            view.set_local_answer(
                                &interrupt_id,
                                Some(LocalAnswer::Failed {
                                    message: tr("approval-conflict-unknown"),
                                }),
                            );
                        }
                    }
                }
                IntentState::Rejected { status, error } => {
                    view.set_local_answer(
                        &interrupt_id,
                        Some(LocalAnswer::Failed {
                            message: format!("{error} (HTTP {status})"),
                        }),
                    );
                }
                IntentState::Exhausted { last_error } => {
                    view.set_local_answer(
                        &interrupt_id,
                        Some(LocalAnswer::Failed {
                            message: last_error.clone(),
                        }),
                    );
                }
            }
        }
    }

    /// The cards of every approval interrupt in the thread, in transcript order, as of the
    /// injected clock. The first time a resolution is seen its time is stamped.
    pub fn cards(&mut self, view: &ThreadView) -> Vec<ApprovalCard> {
        let now_ms = self.clock.now_ms();
        let mut out = Vec::new();
        for task in view.tasks() {
            let transcript = task.transcript();
            for interrupt in transcript.interrupts() {
                if !interrupt.is_open() {
                    self.resolved_at
                        .entry(interrupt.id().to_owned())
                        .or_insert(now_ms);
                }
                let context = CardContext {
                    now_ms,
                    resolved_at_ms: self.resolved_at.get(interrupt.id()).copied(),
                    remember_preset: self.remember.contains(interrupt.id()),
                    preset_saved: self.saved.contains(interrupt.id()),
                };
                if let Some(card) = build_card(interrupt, transcript, context) {
                    out.push(card);
                }
            }
        }
        out
    }
}
