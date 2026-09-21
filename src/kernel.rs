// governed-by: ADR-0016
//! Typed domain contracts for the functional effect kernel.
//!
//! This module is intentionally small during migration. Boundary adapters
//! construct these values; reducers and interpreters can then stop passing
//! unvalidated strings for identity, lifecycle, and effect state.

use std::fmt;
use std::path::PathBuf;

macro_rules! validated_text {
    ($name:ident, $label:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn parse(value: impl Into<String>) -> Result<Self, KernelError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(KernelError::Empty($label));
                }
                if value.chars().any(char::is_control) {
                    return Err(KernelError::ControlCharacter($label));
                }
                Ok(Self(value))
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

validated_text!(Corr, "correlation id");
validated_text!(AgentPin, "agent pin");
validated_text!(WorkflowName, "workflow name");
validated_text!(EffectKey, "effect key");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectPath(PathBuf);

impl ProjectPath {
    pub(crate) fn parse(path: impl Into<PathBuf>) -> Result<Self, KernelError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(KernelError::Empty("project path"));
        }
        if path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        {
            return Err(KernelError::ControlCharacter("project path"));
        }
        Ok(Self(path))
    }

    pub(crate) fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresenceSource {
    Unknown,
    Presence,
    HerdrEvidence,
    Endpoint,
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presence {
    Unknown,
    Busy(PresenceSource),
    Idle(PresenceSource),
    Dead(PresenceSource),
}

impl Presence {
    pub(crate) fn is_promptable(self) -> bool {
        matches!(self, Self::Idle(PresenceSource::Presence))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vehicle {
    Open,
    Loop,
    Swarm,
    Join,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionStage {
    Normalize,
    Input,
    Execute,
    Output,
    Effect,
    Park,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunStatus {
    Planned,
    Running,
    Suspended,
    Succeeded,
    Failed,
    Unknown,
}

impl RunStatus {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryState {
    Prepared,
    Claimed,
    Running,
    Accepted,
    Delivered,
    Shed,
    Failed,
    Unknown,
}

impl DeliveryState {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Delivered | Self::Shed | Self::Failed | Self::Unknown
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeliveryTransitionError {
    TerminalRegression {
        from: DeliveryState,
        to: DeliveryState,
    },
    Illegal {
        from: DeliveryState,
        to: DeliveryState,
    },
}

/// Pure effect lifecycle transition. Terminal outcomes never move backwards,
/// and a provider cannot report delivery before the intent was claimed/running.
pub(crate) fn advance_delivery(
    from: DeliveryState,
    to: DeliveryState,
) -> Result<DeliveryState, DeliveryTransitionError> {
    if from.is_terminal() {
        return if from == to {
            Ok(from)
        } else {
            Err(DeliveryTransitionError::TerminalRegression { from, to })
        };
    }
    let legal = matches!(
        (from, to),
        (DeliveryState::Prepared, DeliveryState::Claimed)
            | (DeliveryState::Claimed, DeliveryState::Running)
            | (DeliveryState::Running, DeliveryState::Accepted)
            | (DeliveryState::Running, DeliveryState::Delivered)
            | (DeliveryState::Running, DeliveryState::Shed)
            | (DeliveryState::Running, DeliveryState::Failed)
            | (DeliveryState::Running, DeliveryState::Unknown)
            | (DeliveryState::Accepted, DeliveryState::Delivered)
            | (DeliveryState::Accepted, DeliveryState::Shed)
            | (DeliveryState::Accepted, DeliveryState::Unknown)
    );
    if legal {
        Ok(to)
    } else {
        Err(DeliveryTransitionError::Illegal { from, to })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SinkResult {
    Accepted { delivery_id: String },
    Delivered { delivery_id: String },
    Shed { reason: String },
    Unavailable { retry_after_secs: Option<u64> },
    Unknown { evidence: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KernelError {
    Empty(&'static str),
    ControlCharacter(&'static str),
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(label) => write!(f, "{label} must not be empty"),
            Self::ControlCharacter(label) => write!(f, "{label} contains a control character"),
        }
    }
}

impl std::error::Error for KernelError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_identifiers_reject_empty_and_control_values() {
        assert!(Corr::parse("").is_err());
        assert!(AgentPin::parse("pin\n").is_err());
        assert_eq!(WorkflowName::parse("ship").unwrap().as_str(), "ship");
    }

    #[test]
    fn project_paths_are_non_empty_and_control_free() {
        assert!(ProjectPath::parse("").is_err());
        assert!(ProjectPath::parse("project\n").is_err());
        assert_eq!(
            ProjectPath::parse(".").unwrap().as_path(),
            std::path::Path::new(".")
        );
    }

    #[test]
    fn lifecycle_terminal_predicates_are_explicit() {
        assert!(RunStatus::Succeeded.is_terminal());
        assert!(!RunStatus::Running.is_terminal());
        assert!(DeliveryState::Unknown.is_terminal());
        assert!(!DeliveryState::Accepted.is_terminal());
    }

    #[test]
    fn delivery_lifecycle_rejects_unclaimed_delivery_and_terminal_regression() {
        assert!(advance_delivery(DeliveryState::Prepared, DeliveryState::Delivered).is_err());
        assert_eq!(
            advance_delivery(DeliveryState::Running, DeliveryState::Delivered).unwrap(),
            DeliveryState::Delivered
        );
        assert!(matches!(
            advance_delivery(DeliveryState::Delivered, DeliveryState::Running),
            Err(DeliveryTransitionError::TerminalRegression { .. })
        ));
        assert_eq!(
            advance_delivery(DeliveryState::Delivered, DeliveryState::Delivered).unwrap(),
            DeliveryState::Delivered
        );
    }

    #[test]
    fn only_authoritative_idle_presence_is_promptable() {
        assert!(Presence::Idle(PresenceSource::Presence).is_promptable());
        assert!(!Presence::Idle(PresenceSource::HerdrEvidence).is_promptable());
        assert!(!Presence::Unknown.is_promptable());
    }
}
