//! Which workload the spike renders, from `GPUI_SPIKE_WORKLOAD`.
//!
//! - unset or `transcript`: the transcript pane alone (M1B-004a, M1B-008a).
//! - `common`: the common workload of `spikes/bench/gates.yaml` in one window: the transcript
//!   streaming beside a 5,000-line diff, a terminal receiving output, and one pending approval card
//!   (see [`crate::common_workload`]).

/// The environment variable that selects the workload.
pub const WORKLOAD_ENV: &str = "GPUI_SPIKE_WORKLOAD";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    Transcript,
    Common,
}

impl Workload {
    /// The workload named by `value` (the variable's value, `None` when it is unset).
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("transcript") => Ok(Self::Transcript),
            Some("common") => Ok(Self::Common),
            Some(other) => Err(format!(
                "unknown {WORKLOAD_ENV} `{other}`; expected `transcript` or `common`"
            )),
        }
    }

    /// The workload named by the environment.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var(WORKLOAD_ENV) {
            Ok(value) => Self::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::parse(None),
            Err(err) => Err(format!("{WORKLOAD_ENV}: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Workload;

    #[test]
    fn workload_unset_or_transcript_is_the_transcript_alone() {
        assert_eq!(Workload::parse(None), Ok(Workload::Transcript));
        assert_eq!(
            Workload::parse(Some("transcript")),
            Ok(Workload::Transcript)
        );
    }

    #[test]
    fn workload_common_is_the_common_workload() {
        assert_eq!(Workload::parse(Some("common")), Ok(Workload::Common));
    }

    #[test]
    fn workload_rejects_unknown_names() {
        let err = Workload::parse(Some("Common")).expect_err("names are exact");
        assert!(err.contains("GPUI_SPIKE_WORKLOAD"), "{err}");
        assert!(Workload::parse(Some("")).is_err());
    }
}
