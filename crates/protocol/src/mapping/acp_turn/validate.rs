//! `validate_sequence`: the ordering rules of `docs/specs/turn-event.md` §5 (rules 1–8, plus the
//! §3 `seq` contract and the §4 scopes) as a pure check over a list of envelopes.
//!
//! The Thread DO runs the same check on append ([`SequenceValidator::push`]); golden tests run it
//! over whole logs ([`validate_sequence`], [`validate_complete`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::turn_event::{ActivityMode, Envelope, Event, RunOutcome, RunReason, StateScope};

/// Which ordering rule an envelope violates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// §3: `seq` is gap-free (producer-side `seq: 0` logs are exempt).
    Seq,
    /// Rule 1: `UserMessage` → `RunStarted{prompt}` → … → terminal; resume after interrupts.
    TaskShape,
    /// Rule 2: one open run per task, one running task per thread.
    OneOpenRun,
    /// Rule 3: run membership (and the §4 scope of every event).
    RunMembership,
    /// Rule 4: start before content/end; messages closed before the run's terminal event.
    MessageOrder,
    /// Rule 5: tool calls start first, end last, and are closed before the task ends.
    ToolCalls,
    /// Rule 6: interrupts pause the run, and are resolved before the resume or the end.
    Interrupts,
    /// Rule 7: subagents and steps are closed before the task ends.
    SubagentsAndSteps,
    /// Rule 8: snapshot before delta, per state scope and per `activity_id`.
    SnapshotBeforeDelta,
}

impl Rule {
    /// The rule number in §5 (`Seq` is §3 and reports `3`).
    pub fn number(self) -> u8 {
        match self {
            Self::Seq => 3,
            Self::TaskShape => 1,
            Self::OneOpenRun => 2,
            Self::RunMembership => 3,
            Self::MessageOrder => 4,
            Self::ToolCalls => 5,
            Self::Interrupts => 6,
            Self::SubagentsAndSteps => 7,
            Self::SnapshotBeforeDelta => 8,
        }
    }
}

/// The first violation found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceError {
    /// Position of the offending envelope in the list (0-based).
    pub index: usize,
    pub seq: u64,
    pub event_type: &'static str,
    pub rule: Rule,
    pub message: String,
}

impl fmt::Display for SequenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "envelope #{} (seq {}, {}): {:?} (rule {}): {}",
            self.index,
            self.seq,
            self.event_type,
            self.rule,
            self.rule.number(),
            self.message
        )
    }
}

impl std::error::Error for SequenceError {}

/// `Ok` when every envelope satisfies §3 and §5 in order. A log may end inside a task (it is a
/// valid prefix); use [`validate_complete`] to require a task boundary at the end.
pub fn validate_sequence(envelopes: &[Envelope]) -> Result<(), SequenceError> {
    let mut validator = SequenceValidator::new();
    for envelope in envelopes {
        validator.push(envelope)?;
    }
    Ok(())
}

/// [`validate_sequence`], and the log must end between tasks.
pub fn validate_complete(envelopes: &[Envelope]) -> Result<(), SequenceError> {
    let mut validator = SequenceValidator::new();
    for envelope in envelopes {
        validator.push(envelope)?;
    }
    validator.finish()
}

#[derive(Debug, Default)]
struct OpenRun {
    id: String,
    open_messages: BTreeSet<String>,
}

#[derive(Debug)]
struct Paused {
    run_id: String,
}

#[derive(Debug, Default)]
struct Task {
    id: String,
    runs: usize,
    run: Option<OpenRun>,
    paused: Option<Paused>,
    seen_messages: BTreeSet<String>,
    /// `tool_call_id → still open`.
    tool_calls: BTreeMap<String, bool>,
    /// Interrupts raised and not yet resolved.
    open_interrupts: BTreeSet<String>,
    raised_interrupts: BTreeSet<String>,
    subagents: BTreeSet<String>,
    steps: BTreeSet<(String, Option<u64>)>,
}

/// Incremental form of [`validate_sequence`].
#[derive(Debug, Default)]
pub struct SequenceValidator {
    index: usize,
    last_seq: Option<u64>,
    /// Scopes whose `StateSnapshot` was seen (at most four).
    scopes: Vec<StateScope>,
    activities: BTreeSet<String>,
    finished_tasks: BTreeSet<String>,
    task: Option<Task>,
}

#[derive(Clone, Copy)]
enum Scope {
    Thread,
    Task,
    Run,
}

fn scope_of(event: &Event) -> Option<Scope> {
    match event {
        Event::Presence(_) | Event::StateSnapshot(_) | Event::StateDelta(_) => Some(Scope::Thread),
        Event::UserMessage(_) | Event::InterruptResolved(_) => Some(Scope::Task),
        Event::Unknown => None,
        _ => Some(Scope::Run),
    }
}

impl SequenceValidator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks `envelope` against everything pushed before it.
    pub fn push(&mut self, envelope: &Envelope) -> Result<(), SequenceError> {
        let index = self.index;
        self.index += 1;
        let fail = |rule: Rule, message: String| SequenceError {
            index,
            seq: envelope.seq,
            event_type: envelope.event.type_name(),
            rule,
            message,
        };
        self.check(envelope)
            .map_err(|(rule, message)| fail(rule, message))
    }

    /// `Ok` when no task is open (the log ends at a task boundary).
    pub fn finish(&self) -> Result<(), SequenceError> {
        match &self.task {
            Some(task) => Err(SequenceError {
                index: self.index,
                seq: self.last_seq.unwrap_or(0),
                event_type: "(end of log)",
                rule: Rule::TaskShape,
                message: format!("the log ends inside task {}", task.id),
            }),
            None => Ok(()),
        }
    }

    fn check(&mut self, envelope: &Envelope) -> Result<(), (Rule, String)> {
        // §3: gap-free seq. A producer-side log (all seq 0) is exempt.
        if let Some(previous) = self.last_seq
            && !(previous == 0 && envelope.seq == 0)
            && envelope.seq != previous + 1
        {
            return Err((
                Rule::Seq,
                format!("expected seq {} after {previous}", previous + 1),
            ));
        }
        self.last_seq = Some(envelope.seq);

        let Some(scope) = scope_of(&envelope.event) else {
            return Ok(()); // an event type this build does not know: ignored (§2)
        };
        let task_id = envelope.task_id.as_deref();
        let run_id = envelope.run_id.as_deref();
        match scope {
            Scope::Thread if task_id.is_some() || run_id.is_some() => {
                return Err((
                    Rule::RunMembership,
                    "thread-scoped events carry null task_id and run_id".into(),
                ));
            }
            Scope::Task if task_id.is_none() || run_id.is_some() => {
                return Err((
                    Rule::RunMembership,
                    "task-scoped events carry a task_id and a null run_id".into(),
                ));
            }
            Scope::Run if task_id.is_none() || run_id.is_none() => {
                return Err((
                    Rule::RunMembership,
                    "run-scoped events carry a task_id and a run_id".into(),
                ));
            }
            _ => {}
        }

        match &envelope.event {
            Event::Presence(_) => Ok(()),
            Event::StateSnapshot(snapshot) => {
                if !self.scopes.contains(&snapshot.scope) {
                    self.scopes.push(snapshot.scope);
                }
                Ok(())
            }
            Event::StateDelta(delta) => {
                if self.scopes.contains(&delta.scope) {
                    Ok(())
                } else {
                    Err((
                        Rule::SnapshotBeforeDelta,
                        format!(
                            "StateDelta for scope {:?} before its StateSnapshot",
                            delta.scope
                        ),
                    ))
                }
            }
            Event::UserMessage(_) => self.user_message(task_id.unwrap_or_default()),
            event => {
                let task_id = task_id.unwrap_or_default();
                let Some(task) = self.task.as_mut() else {
                    return Err((
                        Rule::TaskShape,
                        "event outside any task (a task opens with UserMessage)".into(),
                    ));
                };
                if task.id != task_id {
                    return Err((
                        Rule::RunMembership,
                        format!("task_id {task_id} is not the running task {}", task.id),
                    ));
                }
                match event {
                    Event::InterruptResolved(resolved) => {
                        if task.open_interrupts.remove(&resolved.interrupt_id) {
                            Ok(())
                        } else {
                            Err((
                                Rule::Interrupts,
                                format!(
                                    "InterruptResolved for {}, which is not an open interrupt",
                                    resolved.interrupt_id
                                ),
                            ))
                        }
                    }
                    Event::RunStarted(started) => {
                        let run_id = run_id.unwrap_or_default();
                        if task.run.is_some() {
                            return Err((Rule::OneOpenRun, "a run is already open".into()));
                        }
                        match started.reason {
                            RunReason::Prompt | RunReason::Retry => {
                                if task.runs > 0 {
                                    return Err((
                                        Rule::TaskShape,
                                        "a prompt run after an earlier run of the task".into(),
                                    ));
                                }
                            }
                            RunReason::Resume => {
                                let Some(paused) = task.paused.take() else {
                                    return Err((
                                        Rule::TaskShape,
                                        "resume run without a run paused on interrupts".into(),
                                    ));
                                };
                                if !task.open_interrupts.is_empty() {
                                    return Err((
                                        Rule::Interrupts,
                                        format!(
                                            "resume before InterruptResolved for {:?}",
                                            task.open_interrupts
                                        ),
                                    ));
                                }
                                if started.parent_run_id.as_deref() != Some(paused.run_id.as_str())
                                {
                                    return Err((
                                        Rule::TaskShape,
                                        format!(
                                            "resume parent_run_id must be the paused run {}",
                                            paused.run_id
                                        ),
                                    ));
                                }
                            }
                        }
                        task.runs += 1;
                        task.run = Some(OpenRun {
                            id: run_id.to_owned(),
                            ..OpenRun::default()
                        });
                        Ok(())
                    }
                    run_event => self.run_event(run_event, run_id.unwrap_or_default()),
                }
            }
        }
    }

    fn user_message(&mut self, task_id: &str) -> Result<(), (Rule, String)> {
        if let Some(task) = &self.task {
            return Err((
                Rule::OneOpenRun,
                format!("task {} is still running", task.id),
            ));
        }
        if self.finished_tasks.contains(task_id) {
            return Err((Rule::TaskShape, format!("task id {task_id} was reused")));
        }
        self.task = Some(Task {
            id: task_id.to_owned(),
            ..Task::default()
        });
        Ok(())
    }

    /// Everything run-scoped except `RunStarted`.
    fn run_event(&mut self, event: &Event, run_id: &str) -> Result<(), (Rule, String)> {
        let Some(task) = self.task.as_mut() else {
            return Err((Rule::TaskShape, "event outside any task".into()));
        };
        let Some(run) = task.run.as_mut() else {
            return Err((
                Rule::RunMembership,
                "run-scoped event while no run is open".into(),
            ));
        };
        if run.id != run_id {
            return Err((
                Rule::RunMembership,
                format!("run_id {run_id} is not the open run {}", run.id),
            ));
        }
        match event {
            Event::TextMessageStart(e) => start_message(task, &e.message_id),
            Event::ReasoningMessageStart(e) => start_message(task, &e.message_id),
            Event::TextMessageContent(e) => content_message(run, &e.message_id),
            Event::ReasoningMessageContent(e) => content_message(run, &e.message_id),
            Event::TextMessageEnd(e) => end_message(run, &e.message_id),
            Event::ReasoningMessageEnd(e) => end_message(run, &e.message_id),
            Event::ToolCallStart(e) => match task.tool_calls.get(&e.tool_call_id) {
                Some(false) => Err((
                    Rule::ToolCalls,
                    format!("ToolCallStart for {} after its ToolCallEnd", e.tool_call_id),
                )),
                Some(true) => Ok(()),
                None => {
                    task.tool_calls.insert(e.tool_call_id.clone(), true);
                    Ok(())
                }
            },
            Event::ToolCallArgs(e) => tool_call_open(task, &e.tool_call_id),
            Event::ToolCallResult(e) => tool_call_open(task, &e.tool_call_id),
            Event::ToolCallEnd(e) => {
                tool_call_open(task, &e.tool_call_id)?;
                task.tool_calls.insert(e.tool_call_id.clone(), false);
                Ok(())
            }
            Event::Activity(activity) => {
                if activity.mode == ActivityMode::Snapshot {
                    self.activities.insert(activity.activity_id.clone());
                    Ok(())
                } else if self.activities.contains(&activity.activity_id) {
                    Ok(())
                } else {
                    Err((
                        Rule::SnapshotBeforeDelta,
                        format!(
                            "{:?} for activity {} before its snapshot",
                            activity.mode, activity.activity_id
                        ),
                    ))
                }
            }
            Event::Interrupt(interrupt) => {
                if !task
                    .raised_interrupts
                    .insert(interrupt.interrupt_id.clone())
                {
                    return Err((
                        Rule::Interrupts,
                        format!("interrupt {} raised twice", interrupt.interrupt_id),
                    ));
                }
                task.open_interrupts.insert(interrupt.interrupt_id.clone());
                Ok(())
            }
            Event::SubagentStarted(e) => {
                if task.subagents.insert(e.subagent_run_id.clone()) {
                    Ok(())
                } else {
                    Err((
                        Rule::SubagentsAndSteps,
                        format!("subagent {} started twice", e.subagent_run_id),
                    ))
                }
            }
            Event::SubagentFinished(e) => finish_subagent(task, &e.subagent_run_id),
            Event::SubagentError(e) => finish_subagent(task, &e.subagent_run_id),
            Event::StepStarted(e) => {
                if task.steps.insert((e.step_id.clone(), e.iteration)) {
                    Ok(())
                } else {
                    Err((
                        Rule::SubagentsAndSteps,
                        format!("step {} started twice", e.step_id),
                    ))
                }
            }
            Event::StepFinished(e) => {
                if task.steps.remove(&(e.step_id.clone(), e.iteration)) {
                    Ok(())
                } else {
                    Err((
                        Rule::SubagentsAndSteps,
                        format!("StepFinished for {}, which is not open", e.step_id),
                    ))
                }
            }
            Event::RunFinished(finished) => match &finished.outcome {
                RunOutcome::Success { .. } => {
                    terminal(task, &mut self.finished_tasks, "RunFinished{success}")?;
                    self.task = None;
                    Ok(())
                }
                RunOutcome::Interrupt { interrupt_ids } => {
                    close_checks_for_pause(run)?;
                    if interrupt_ids.is_empty() {
                        return Err((
                            Rule::Interrupts,
                            "RunFinished{interrupt} lists no interrupt_ids".into(),
                        ));
                    }
                    if let Some(missing) = task
                        .open_interrupts
                        .iter()
                        .find(|id| !interrupt_ids.contains(id))
                    {
                        return Err((
                            Rule::Interrupts,
                            format!("interrupt_ids misses the open interrupt {missing}"),
                        ));
                    }
                    if let Some(unknown) = interrupt_ids
                        .iter()
                        .find(|id| !task.raised_interrupts.contains(*id))
                    {
                        return Err((
                            Rule::Interrupts,
                            format!("interrupt_ids lists {unknown}, which was never raised"),
                        ));
                    }
                    let run_id = run.id.clone();
                    task.run = None;
                    task.paused = Some(Paused { run_id });
                    Ok(())
                }
            },
            Event::RunError(_) => {
                terminal(task, &mut self.finished_tasks, "RunError")?;
                self.task = None;
                Ok(())
            }
            // Checkpoint, Raw: run-scoped, no further ordering rules.
            _ => Ok(()),
        }
    }
}

fn start_message(task: &mut Task, message_id: &str) -> Result<(), (Rule, String)> {
    if !task.seen_messages.insert(message_id.to_owned()) {
        return Err((
            Rule::MessageOrder,
            format!("message {message_id} started twice"),
        ));
    }
    if let Some(run) = task.run.as_mut() {
        run.open_messages.insert(message_id.to_owned());
    }
    Ok(())
}

fn content_message(run: &OpenRun, message_id: &str) -> Result<(), (Rule, String)> {
    if run.open_messages.contains(message_id) {
        Ok(())
    } else {
        Err((
            Rule::MessageOrder,
            format!("content for message {message_id}, which is not open"),
        ))
    }
}

fn end_message(run: &mut OpenRun, message_id: &str) -> Result<(), (Rule, String)> {
    if run.open_messages.remove(message_id) {
        Ok(())
    } else {
        Err((
            Rule::MessageOrder,
            format!("end for message {message_id}, which is not open"),
        ))
    }
}

fn tool_call_open(task: &Task, tool_call_id: &str) -> Result<(), (Rule, String)> {
    match task.tool_calls.get(tool_call_id) {
        Some(true) => Ok(()),
        Some(false) => Err((
            Rule::ToolCalls,
            format!("event for tool call {tool_call_id} after its ToolCallEnd"),
        )),
        None => Err((
            Rule::ToolCalls,
            format!("the first event for tool call {tool_call_id} must be ToolCallStart"),
        )),
    }
}

fn finish_subagent(task: &mut Task, subagent_run_id: &str) -> Result<(), (Rule, String)> {
    if task.subagents.remove(subagent_run_id) {
        Ok(())
    } else {
        Err((
            Rule::SubagentsAndSteps,
            format!("subagent {subagent_run_id} finished but was not started"),
        ))
    }
}

/// Messages close before any terminal event, including the one that pauses on interrupts.
fn close_checks_for_pause(run: &OpenRun) -> Result<(), (Rule, String)> {
    if run.open_messages.is_empty() {
        Ok(())
    } else {
        Err((
            Rule::MessageOrder,
            format!(
                "messages {:?} are still open at the run's end",
                run.open_messages
            ),
        ))
    }
}

/// The checks for `RunFinished{success}` and `RunError`, which end the task.
fn terminal(
    task: &mut Task,
    finished_tasks: &mut BTreeSet<String>,
    what: &str,
) -> Result<(), (Rule, String)> {
    if let Some(run) = &task.run {
        close_checks_for_pause(run)?;
    }
    if let Some((id, _)) = task.tool_calls.iter().find(|(_, open)| **open) {
        return Err((
            Rule::ToolCalls,
            format!("tool call {id} is still open at {what}"),
        ));
    }
    if !task.open_interrupts.is_empty() {
        return Err((
            Rule::Interrupts,
            format!(
                "interrupts {:?} are unresolved at {what}",
                task.open_interrupts
            ),
        ));
    }
    if let Some(id) = task.subagents.iter().next() {
        return Err((
            Rule::SubagentsAndSteps,
            format!("subagent {id} is still open at {what}"),
        ));
    }
    if let Some((id, _)) = task.steps.iter().next() {
        return Err((
            Rule::SubagentsAndSteps,
            format!("step {id} is still open at {what}"),
        ));
    }
    finished_tasks.insert(task.id.clone());
    Ok(())
}
