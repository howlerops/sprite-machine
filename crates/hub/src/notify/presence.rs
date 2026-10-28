//! "Machine back" notifications (M2B-014, plan §4.10 "Sleep and disconnects"): a machine that
//! comes back online while commands are queued for it announces itself once per window.
//!
//! This file is the pure decision, with the clock injected, so it runs natively. The Machine DO
//! calls [`machine_back_thread`] on each presence change. The push itself is not wired yet: it
//! needs its own notification class in `job.rs` and `prefs.rs`.

use serde::{Deserialize, Serialize};

/// At most one "machine back" notification per machine in this many milliseconds. A machine that
/// flaps (online, offline, online) within the window sends one push, not one per reconnect.
pub const MACHINE_BACK_DEBOUNCE_MS: u64 = 60_000;

/// When this machine last announced itself. One per machine, so the Machine DO keeps a single
/// value (serialized) and loads it before each decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineBackDebounce {
    last_sent_ms: Option<u64>,
}

impl MachineBackDebounce {
    /// Admits a notification at `now_ms` unless one was admitted less than
    /// [`MACHINE_BACK_DEBOUNCE_MS`] ago. A clock that went backwards does not block: the window
    /// restarts at `now_ms`.
    pub fn admit(&mut self, now_ms: u64) -> bool {
        if let Some(last) = self.last_sent_ms
            && now_ms >= last
            && now_ms - last < MACHINE_BACK_DEBOUNCE_MS
        {
            return false;
        }
        self.last_sent_ms = Some(now_ms);
        true
    }
}

/// The thread to name in a "machine back" push, or `None` when there is nothing to announce.
///
/// A push is due when presence goes from anything but `online` to `online` and at least one
/// thread has commands waiting for the machine (`queued_threads`, the outbox's thread ids in
/// order; the first one is named). Only a due push consumes the debounce window, so a reconnect
/// with nothing queued does not hide a later one that has work.
pub fn machine_back_thread<'a>(
    from: &str,
    to: &str,
    queued_threads: &'a [String],
    debounce: &mut MachineBackDebounce,
    now_ms: u64,
) -> Option<&'a str> {
    if to != "online" || from == "online" {
        return None;
    }
    let thread = queued_threads.first()?;
    debounce.admit(now_ms).then_some(thread.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued() -> Vec<String> {
        vec!["th_1".to_owned(), "th_2".to_owned()]
    }

    #[test]
    fn the_first_return_is_admitted_and_the_window_holds_the_next_one_back() {
        let mut debounce = MachineBackDebounce::default();
        assert!(debounce.admit(1_000));
        assert!(!debounce.admit(1_000 + MACHINE_BACK_DEBOUNCE_MS - 1));
        assert!(debounce.admit(1_000 + MACHINE_BACK_DEBOUNCE_MS));
    }

    #[test]
    fn a_flapping_machine_sends_one_push_per_window() {
        let mut debounce = MachineBackDebounce::default();
        let threads = queued();
        let mut sent = Vec::new();
        // Online, offline, online, asleep, online, offline, online: every 5 s, then at 70 s.
        let steps = [
            ("offline", "online", 0),
            ("online", "offline", 5_000),
            ("offline", "online", 10_000),
            ("online", "asleep", 15_000),
            ("asleep", "online", 20_000),
            ("online", "offline", 25_000),
            ("offline", "online", 70_000),
        ];
        for (from, to, now) in steps {
            if let Some(thread) = machine_back_thread(from, to, &threads, &mut debounce, now) {
                sent.push((now, thread.to_owned()));
            }
        }
        assert_eq!(
            sent,
            vec![(0, "th_1".to_owned()), (70_000, "th_1".to_owned())]
        );
    }

    #[test]
    fn a_reconnect_with_nothing_queued_does_not_use_up_the_window() {
        let mut debounce = MachineBackDebounce::default();
        let nothing: Vec<String> = Vec::new();
        assert_eq!(
            machine_back_thread("offline", "online", &nothing, &mut debounce, 0),
            None
        );
        // The window is still free for a machine that has work when it comes back.
        assert_eq!(
            machine_back_thread("offline", "online", &queued(), &mut debounce, 1_000),
            Some("th_1")
        );
    }

    #[test]
    fn only_a_change_to_online_is_announced() {
        let mut debounce = MachineBackDebounce::default();
        let threads = queued();
        // Already online: a repeated hello is not a comeback.
        assert_eq!(
            machine_back_thread("online", "online", &threads, &mut debounce, 0),
            None
        );
        // Going away is not announced.
        assert_eq!(
            machine_back_thread("online", "asleep", &threads, &mut debounce, 0),
            None
        );
        assert_eq!(
            machine_back_thread("online", "offline", &threads, &mut debounce, 0),
            None
        );
        // None of those used the window.
        assert!(debounce.admit(0));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_block_forever() {
        let mut debounce = MachineBackDebounce::default();
        assert!(debounce.admit(500_000));
        assert!(debounce.admit(1_000));
        assert!(!debounce.admit(1_000 + 1));
    }

    #[test]
    fn the_debounce_state_round_trips_through_json() {
        let mut debounce = MachineBackDebounce::default();
        assert!(debounce.admit(42));
        let text = serde_json::to_string(&debounce).unwrap();
        let back: MachineBackDebounce = serde_json::from_str(&text).unwrap();
        assert_eq!(back, debounce);
        assert!(!back.clone().admit(42 + 1));
    }
}
