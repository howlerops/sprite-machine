//! `seq`: per thread, assigned by the Thread DO, gap-free (§3).

use std::fmt;

use super::Envelope;

/// Assigns envelope `seq` for one thread: the first stamped envelope gets `1`, every later
/// one the previous `seq + 1`. The Thread DO persists [`ThreadSeq::last`] with each append.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadSeq {
    last: u64,
}

/// Why an envelope could not be stamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampError {
    /// The envelope already carries a `seq`; producers must send `seq: 0`.
    AlreadyStamped { seq: u64 },
}

impl fmt::Display for StampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyStamped { seq } => {
                write!(f, "envelope already has seq {seq}; producers send seq 0")
            }
        }
    }
}

impl std::error::Error for StampError {}

impl ThreadSeq {
    /// A thread with no events yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Continues a thread whose last stored event has `seq == last`.
    pub fn resume(last: u64) -> Self {
        Self { last }
    }

    /// The last assigned `seq` (0 when the thread is empty).
    pub fn last(&self) -> u64 {
        self.last
    }

    /// Takes the next `seq` (`last + 1`).
    pub fn next_seq(&mut self) -> u64 {
        self.last += 1;
        self.last
    }

    /// Assigns the next `seq` to a producer-side envelope (`seq == 0`) and returns it.
    pub fn stamp(&mut self, mut envelope: Envelope) -> Result<Envelope, StampError> {
        if envelope.seq != 0 {
            return Err(StampError::AlreadyStamped { seq: envelope.seq });
        }
        envelope.seq = self.next_seq();
        Ok(envelope)
    }
}
