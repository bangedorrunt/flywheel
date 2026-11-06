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
        }
    };
}

validated_text!(Corr, "correlation id");
validated_text!(AgentPin, "agent pin");
validated_text!(WorkflowName, "workflow name");

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

/// Staged pipeline vocabulary (wave9): `Input` is constructed today
/// (pane drainer); the rest land as failure stages wire up. Pinned by
/// `every_action_stage_spells_itself` so the set cannot rot silently.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "staged: failure::classify_with_action_stage is the first consumer; variants land as stages wire up"
    )
)]
pub(crate) enum ActionStage {
    Normalize,
    Input,
    Execute,
    Output,
    Effect,
    Park,
}

impl ActionStage {
    /// Wire spelling (live: `failure::classify_with_action_stage` maps the
    /// typed stage back onto the stable string field).
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Normalize => "normalize",
            Self::Input => "input",
            Self::Execute => "execute",
            Self::Output => "output",
            Self::Effect => "effect",
            Self::Park => "park",
        }
    }
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
        assert!(WorkflowName::parse("ship").is_ok());
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
    fn every_action_stage_spells_itself() {
        assert_eq!(
            [
                ActionStage::Normalize,
                ActionStage::Input,
                ActionStage::Execute,
                ActionStage::Output,
                ActionStage::Effect,
                ActionStage::Park,
            ]
            .map(|stage| stage.as_str()),
            ["normalize", "input", "execute", "output", "effect", "park"]
        );
    }
}
