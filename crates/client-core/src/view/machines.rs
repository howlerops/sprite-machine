//! Machines and their presence. Presence is published into every thread a machine hosts
//! (turn-event.md §4.8), so the machine view is the latest presence over those threads.

use std::collections::BTreeMap;

use protocol::turn_event::MachinePresence;

use super::ThreadView;

/// One machine, as seen through the threads it hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRow {
    pub machine_id: String,
    /// `None` until a thread hosted here reports presence.
    pub presence: Option<MachinePresence>,
    /// When `presence` began (RFC 3339, so the latest is the lexical maximum).
    pub since: Option<String>,
    /// Threads this machine hosts, in the order given.
    pub thread_ids: Vec<String>,
}

/// Machines sorted by id. For each machine, the presence is the one with the latest `since`.
pub fn machines<'a>(threads: impl IntoIterator<Item = &'a ThreadView>) -> Vec<MachineRow> {
    let mut rows: BTreeMap<String, MachineRow> = BTreeMap::new();
    for thread in threads {
        let Some(machine_id) = thread.machine_id() else {
            continue;
        };
        let row = rows
            .entry(machine_id.to_owned())
            .or_insert_with(|| MachineRow {
                machine_id: machine_id.to_owned(),
                presence: None,
                since: None,
                thread_ids: Vec::new(),
            });
        row.thread_ids.push(thread.thread_id().to_owned());
        if let Some(presence) = thread.presence() {
            let newer = row
                .since
                .as_deref()
                .is_none_or(|since| presence.since.as_str() > since);
            if newer {
                row.presence = Some(presence.state);
                row.since = Some(presence.since.clone());
            }
        }
    }
    rows.into_values().collect()
}
