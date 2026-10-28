//! The NOTIFY queue message and the push payload.
//!
//! A [`NotifyJob`] carries ids only (thread, task, interrupt, option ids). Prompt text, tool
//! input and file names never reach the queue or a push service: the payload built from a job
//! ([`Notification::payload`]) has the ids, the kind, a generic title and, for an approval, the
//! two actions the phone answers with.

use protocol::turn_event::{
    Envelope, Event, InterruptKind, PermissionOptionKind, RunErrorReason, RunOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::webpush::b64_encode;

/// What happened, and the event class a user's prefs switch on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An `Interrupt` is waiting for a person.
    ApprovalNeeded,
    /// A run finished successfully.
    TaskDone,
    /// A run ended in an error.
    TaskFailed,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::ApprovalNeeded, Kind::TaskDone, Kind::TaskFailed];

    /// The stored and wire name (`notification_prefs.event_class`).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ApprovalNeeded => "approval_needed",
            Kind::TaskDone => "task_done",
            Kind::TaskFailed => "task_failed",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    /// The generic title (plan §4.2: "Push payloads carry only ids + a generic title").
    pub fn title(self) -> &'static str {
        match self {
            Kind::ApprovalNeeded => "Approval needed",
            Kind::TaskDone => "Task finished",
            Kind::TaskFailed => "Task failed",
        }
    }
}

/// One queue message. Ids only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyJob {
    pub kind: Kind,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_id: Option<String>,
    /// The id of the interrupt's first allow option, when it is an approval with options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_option_id: Option<String>,
    /// The id of the interrupt's first reject option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_option_id: Option<String>,
    /// The subscriptions still to send, when this job re-queues the failed part of an earlier
    /// send. `None` sends to every subscription of the thread's org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscriptions: Option<Vec<String>>,
    /// How many sends this job has had before (0 for a job from an event).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempt: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Sends of one job before its failed subscriptions are given up on (the queue's own retries).
pub const MAX_ATTEMPTS: u32 = 5;

impl NotifyJob {
    /// The job to queue for the subscriptions that failed temporarily (`pending`), or `None` when
    /// there are none or [`MAX_ATTEMPTS`] sends were made. The subscriptions that were delivered
    /// (or failed for good) are not in it, so they never get the same notification twice.
    pub fn follow_up(&self, pending: Vec<String>) -> Option<NotifyJob> {
        if pending.is_empty() || self.attempt + 1 >= MAX_ATTEMPTS {
            return None;
        }
        Some(NotifyJob {
            subscriptions: Some(pending),
            attempt: self.attempt + 1,
            ..self.clone()
        })
    }
}

/// The jobs for events just appended to a thread, in order. Only these events notify:
///
/// - `Interrupt` (any kind): [`Kind::ApprovalNeeded`]. An approval with options also carries the
///   ids of its first allow and reject options, so the phone can answer it.
/// - `RunFinished{success}`: [`Kind::TaskDone`].
/// - `RunFinished{interrupt}`: [`Kind::ApprovalNeeded`] for the first open interrupt, unless the
///   same batch already holds that interrupt.
/// - `RunError`: [`Kind::TaskFailed`]. A run the user cancelled is not a failure and is skipped.
pub fn jobs_for(appended: &[Envelope]) -> Vec<NotifyJob> {
    let mut jobs: Vec<NotifyJob> = Vec::new();
    for envelope in appended {
        let base = |kind| NotifyJob {
            kind,
            thread_id: envelope.thread_id.clone(),
            task_id: envelope.task_id.clone(),
            interrupt_id: None,
            allow_option_id: None,
            deny_option_id: None,
            subscriptions: None,
            attempt: 0,
        };
        match &envelope.event {
            Event::Interrupt(interrupt) => {
                let mut job = base(Kind::ApprovalNeeded);
                job.interrupt_id = Some(interrupt.interrupt_id.clone());
                if interrupt.kind == InterruptKind::Approval {
                    let find = |allow: bool| {
                        interrupt.options.iter().flatten().find(|option| {
                            matches!(
                                option.kind,
                                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                            ) == allow
                        })
                    };
                    job.allow_option_id = find(true).map(|option| option.option_id.clone());
                    job.deny_option_id = find(false).map(|option| option.option_id.clone());
                }
                jobs.push(job);
            }
            Event::RunFinished(finished) => match &finished.outcome {
                RunOutcome::Success { .. } => jobs.push(base(Kind::TaskDone)),
                RunOutcome::Interrupt { interrupt_ids } => {
                    let Some(first) = interrupt_ids.first() else {
                        continue;
                    };
                    let covered = jobs
                        .iter()
                        .any(|job| job.interrupt_id.as_deref() == Some(first.as_str()));
                    if !covered {
                        let mut job = base(Kind::ApprovalNeeded);
                        job.interrupt_id = Some(first.clone());
                        jobs.push(job);
                    }
                }
            },
            Event::RunError(error) if error.reason == RunErrorReason::Error => {
                jobs.push(base(Kind::TaskFailed));
            }
            _ => {}
        }
    }
    jobs
}

/// How urgently a push service should deliver (RFC 8030 §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    High,
    Normal,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::High => "high",
            Urgency::Normal => "normal",
        }
    }
}

/// How long a push service may hold an undelivered message (RFC 8030 `TTL`), in seconds.
pub const TTL_SECS: u32 = 24 * 60 * 60;

/// What is sent to one subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub job: NotifyJob,
}

impl Notification {
    pub fn new(job: NotifyJob) -> Self {
        Self { job }
    }

    pub fn urgency(&self) -> Urgency {
        match self.job.kind {
            Kind::ApprovalNeeded => Urgency::High,
            _ => Urgency::Normal,
        }
    }

    /// The `Topic` header (RFC 8030 §5.4): a push service replaces an undelivered message that
    /// has the same topic, so there is at most one pending notification per task. The task id is
    /// the topic; it is 32 characters of base64url at most, so a longer or otherwise shaped id is
    /// hashed (a thread without a task id uses the thread id).
    pub fn topic(&self) -> String {
        let id = self.job.task_id.as_deref().unwrap_or(&self.job.thread_id);
        let fits = id.len() <= 32
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if fits {
            id.to_owned()
        } else {
            b64_encode(&Sha256::digest(id.as_bytes()))[..32].to_owned()
        }
    }

    /// The push payload (JSON): ids, the kind, a generic title and, for an approval, the actions.
    ///
    /// ```json
    /// { "kind": "approval_needed", "title": "Approval needed",
    ///   "thread_id": "th_1", "task_id": "tk_1", "interrupt_id": "int_1",
    ///   "actions": [ { "action": "allow", "title": "Allow", "option_id": "allow" },
    ///                { "action": "deny",  "title": "Deny",  "option_id": "reject" } ] }
    /// ```
    pub fn payload(&self) -> Value {
        let job = &self.job;
        let mut body = json!({
            "kind": job.kind.as_str(),
            "title": job.kind.title(),
            "thread_id": job.thread_id,
        });
        body["task_id"] = json!(job.task_id);
        if let Some(interrupt_id) = &job.interrupt_id {
            body["interrupt_id"] = json!(interrupt_id);
            if job.kind == Kind::ApprovalNeeded
                && job.allow_option_id.is_some()
                && job.deny_option_id.is_some()
            {
                body["actions"] = json!([
                    { "action": "allow", "title": "Allow", "option_id": job.allow_option_id },
                    { "action": "deny", "title": "Deny", "option_id": job.deny_option_id },
                ]);
            }
        }
        body
    }
}
