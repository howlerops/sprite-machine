//! The idempotency keys this process applied, until the store records them.
//!
//! The command source commits a key to the store when the bridge reports the frame processed.
//! Until then a repeat of the key must not be applied again, so the bridge remembers it here.
//! Once [`Store::seen_key`] reports the key committed, the store answers for it, and it is
//! pruned: the set holds only the keys still in flight.

use std::collections::HashSet;

use crate::store::{Store, StoreError};

/// The keys applied by this process that the store does not record yet.
#[derive(Debug, Default)]
pub(super) struct AppliedKeys {
    keys: HashSet<String>,
}

impl AppliedKeys {
    /// Records `key`. `false` when this process already applied it.
    pub fn insert(&mut self, key: &str) -> bool {
        self.keys.insert(key.to_owned())
    }

    /// Drops every key the store reports committed. Returns how many were dropped.
    pub fn prune(&mut self, store: &Store) -> Result<usize, StoreError> {
        let mut committed = Vec::new();
        for key in &self.keys {
            if store.seen_key(key)? {
                committed.push(key.clone());
            }
        }
        for key in &committed {
            self.keys.remove(key);
        }
        Ok(committed.len())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use protocol::tunnel::Command;
    use tempfile::TempDir;

    use super::super::io::{CommandSource, IncomingCommand, MemorySink};
    use super::super::{Bridge, Config, SequentialGenerator};
    use super::*;

    /// A command source that commits each frame to the store when it is reported processed, as
    /// the tunnel client does. It never delivers a command itself.
    struct CommittingCommands {
        store: Arc<Store>,
    }

    impl CommandSource for CommittingCommands {
        async fn next(&mut self) -> Option<IncomingCommand> {
            None
        }

        async fn processed(&mut self, seq: u64, idempotency_key: &str) {
            self.store
                .commit_down_frame(seq, Some(idempotency_key))
                .expect("commit");
        }
    }

    #[test]
    fn committed_keys_are_pruned_and_the_rest_are_kept() {
        let dir = TempDir::new().expect("state dir");
        let store = Store::open(dir.path()).expect("store");
        let mut applied = AppliedKeys::default();
        for key in ["k1", "k2", "k3"] {
            assert!(applied.insert(key));
        }
        assert!(!applied.insert("k1"), "a repeat is not new");

        store.commit_down_frame(1, Some("k1")).expect("commit");
        store.commit_down_frame(2, Some("k2")).expect("commit");
        assert_eq!(applied.prune(&store).expect("prune"), 2);
        assert_eq!(applied.keys.len(), 1);
        assert!(applied.keys.contains("k3"));
    }

    #[test]
    fn the_set_stays_bounded_as_commands_keep_coming() {
        let dir = TempDir::new().expect("state dir");
        let store = Store::open(dir.path()).expect("store");
        let mut applied = AppliedKeys::default();
        for seq in 1..=200u64 {
            let key = format!("idk_{seq}");
            assert!(applied.insert(&key));
            store.commit_down_frame(seq, Some(&key)).expect("commit");
            applied.prune(&store).expect("prune");
            assert!(applied.keys.is_empty(), "after {seq} committed commands");
        }
        // A committed key is still a duplicate: the store answers for it.
        assert!(store.seen_key("idk_1").expect("seen"));
    }

    /// The bridge's own command path: a key the source commits is no longer held by the bridge.
    #[tokio::test]
    async fn the_bridge_forgets_a_key_once_its_source_has_committed_it() {
        let dir = TempDir::new().expect("state dir");
        let store = Arc::new(Store::open(dir.path()).expect("store"));
        let (client_io, _agent_io) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(client_io);
        let (sink, _events) = MemorySink::new();
        let mut bridge = Bridge::connect(
            Config::new("mch_1", "agt_test", dir.path()),
            reader,
            writer,
            Arc::clone(&store),
            Arc::new(SequentialGenerator::new()),
            sink,
            CommittingCommands {
                store: Arc::clone(&store),
            },
        );
        // A cancel for a thread this bridge does not know is rejected without touching the agent.
        bridge
            .on_incoming(IncomingCommand {
                seq: 1,
                idempotency_key: "idk_1".to_owned(),
                command: Command::Cancel {
                    thread_id: "th_1".into(),
                    task_id: "tk_1".into(),
                },
            })
            .await
            .expect("applied");

        assert!(store.seen_key("idk_1").expect("seen"));
        assert!(bridge.applied.keys.is_empty());
    }
}
