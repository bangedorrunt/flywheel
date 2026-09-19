// governed-by: ADR-0015
//! Injected host authority (P4). Resolve once at `main()`, thread down.
//! Call sites take `&HostAuthority`; env-mutating tests banned for new code.

#[derive(Debug, Clone)]
pub struct HostAuthority {
    pub agent_name: Option<String>,
    pub home: String,
    pub cwd: std::path::PathBuf,
    pub exe: std::path::PathBuf,
}

impl HostAuthority {
    pub fn resolve() -> Self {
        let agent_name = std::env::var("AGENT_NAME")
            .or_else(|_| std::env::var("TORON_AGENT"))
            .or_else(|_| std::env::var("FLYWHEEL_MAIL_AS"))
            .ok();
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("flywheel"));
        Self {
            agent_name,
            home,
            cwd,
            exe,
        }
    }

    #[cfg(test)]
    pub fn for_test(home: &str) -> Self {
        Self {
            agent_name: Some("test-pin".into()),
            home: home.into(),
            cwd: std::path::PathBuf::from("/tmp/fw-test"),
            exe: std::path::PathBuf::from("flywheel"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_constructs_without_env_mutation() {
        let a = HostAuthority::for_test("/tmp/fw-home");
        assert_eq!(a.home, "/tmp/fw-home");
        assert_eq!(a.agent_name.as_deref(), Some("test-pin"));
    }

    #[test]
    fn resolve_never_panics_without_env() {
        let a = HostAuthority::resolve();
        assert!(!a.home.is_empty());
    }
}
