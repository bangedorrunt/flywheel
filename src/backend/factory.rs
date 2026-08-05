//! Backend factory — resolves the active `MultiplexerBackend` and
//! `MailBackend` from config/env and constructs them.
//!
//! governed-by: ADR-0002
//!
//! Selection precedence (flag > env > `.env` > config.toml > default):
//! - `FLYWHEEL_BACKEND` (`herdr` default; `superlogical`/`tmux` future/stub)
//! - `FLYWHEEL_MAIL`    (`agentmail` default; `toron` future/stub)
//!
//! Phase 0: only the default impls are wired (herdr + agentmail). The stub
//! backends (`superlogical`, `tmux`, `toron`) land in `bd-toron-superlogical-
//! stubs` (R0.7); until then an unrecognized name returns a clear
//! `BackendError::Unsupported` so a user who typos the env var gets an
//! actionable error, not a silent fallback to herdr/agentmail.

use crate::backend::{BackendError, MailBackend, MultiplexerBackend, Result};
use crate::config::Config;
use crate::herdr::Client as HerdrClient;
use crate::mail::Mail;

/// Default multiplexer backend name.
pub const DEFAULT_BACKEND: &str = "herdr";
/// Default mail backend name.
pub const DEFAULT_MAIL: &str = "agentmail";

/// Resolve the active multiplexer backend name from config (precedence:
/// `FLYWHEEL_BACKEND` env > `[backend] multiplexer` config field > default).
pub fn resolve_backend_name(cfg: &Config) -> String {
    std::env::var("FLYWHEEL_BACKEND")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.backend.as_ref().map(|b| b.multiplexer.clone()))
        .unwrap_or_else(|| DEFAULT_BACKEND.to_string())
}

/// Resolve the active mail backend name from config (precedence:
/// `FLYWHEEL_MAIL` env > `[backend] mail` config field > default).
pub fn resolve_mail_name(cfg: &Config) -> String {
    std::env::var("FLYWHEEL_MAIL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.backend.as_ref().map(|b| b.mail.clone()))
        .unwrap_or_else(|| DEFAULT_MAIL.to_string())
}

/// Construct the active multiplexer backend. Default = herdr; unrecognized →
/// actionable error (no silent fallback to the default — ADR-0002 §6).
pub fn multiplexer_backend(cfg: &Config) -> Result<Box<dyn MultiplexerBackend>> {
    let name = resolve_backend_name(cfg);
    match name.as_str() {
        "herdr" => {
            let client = HerdrClient::connect(Some(&cfg.socket_path()))
                .map_err(BackendError::Other)?;
            Ok(Box::new(crate::backend::herdr::HerdrBackend::new(client)))
        }
        // R0.7 lands stubs for these; until then, an actionable error.
        "superlogical" | "tmux" => Err(BackendError::Unsupported {
            backend: name.clone(),
            method: "(factory)",
            reason: "{name} backend not yet wired (stub lands in bd-toron-superlogical-stubs); set FLYWHEEL_BACKEND=herdr"
                .replacen("{name}", &name, 1),
        }),
        other => Err(BackendError::Unsupported {
            backend: other.to_string(),
            method: "(factory)",
            reason: "unknown multiplexer backend '{other}'; set FLYWHEEL_BACKEND=herdr (or superlogical|tmux once stubbed)"
                .replacen("{other}", other, 1),
        }),
    }
}

/// Construct the active mail backend. Default = agentmail; unrecognized →
/// actionable error.
pub fn mail_backend(cfg: &Config) -> Result<Box<dyn MailBackend>> {
    let name = resolve_mail_name(cfg);
    match name.as_str() {
        "agentmail" => {
            let mail = Mail::from_config(cfg.agent_mail.as_ref())
                .map_err(BackendError::Other)?
                .ok_or_else(|| BackendError::Unsupported {
                    backend: "agentmail".to_string(),
                    method: "(factory)",
                    reason: "agent_mail config unset (set [agent_mail] url/token in config.toml \
                             or FLYWHEEL_AGENTMAIL_URL/FLYWHEEL_AGENTMAIL_TOKEN env)"
                        .to_string(),
                })?;
            Ok(Box::new(
                crate::backend::agentmail::AgentMailMcpBackend::new(mail),
            ))
        }
        // R0.7 lands the toron stub; until then, an actionable error.
        "toron" => Err(BackendError::Unsupported {
            backend: name.clone(),
            method: "(factory)",
            reason: "toron backend not yet wired (stub lands in bd-toron-superlogical-stubs / real impl at R1); set FLYWHEEL_MAIL=agentmail".to_string(),
        }),
        other => Err(BackendError::Unsupported {
            backend: other.to_string(),
            method: "(factory)",
            reason: "unknown mail backend '{other}'; set FLYWHEEL_MAIL=agentmail (or toron once wired)"
                .replacen("{other}", other, 1),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Mutex;

    // Env-var mutation is process-global; serialize the tests that touch it so
    // parallel test runs don't race (classic env-test hazard). This lock is
    // std-only (no serial_test dep).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn _lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Defaults resolve to herdr + agentmail when nothing is set.
    #[test]
    fn defaults_resolve_to_herdr_and_agentmail() {
        let _g = _lock_env();
        std::env::remove_var("FLYWHEEL_BACKEND");
        std::env::remove_var("FLYWHEEL_MAIL");
        let cfg = Config::default();
        assert_eq!(resolve_backend_name(&cfg), "herdr");
        assert_eq!(resolve_mail_name(&cfg), "agentmail");
    }

    /// Env overrides config (the precedence contract — flag wiring is R0.6).
    #[test]
    fn env_overrides_config() {
        let _g = _lock_env();
        std::env::set_var("FLYWHEEL_BACKEND", "superlogical");
        std::env::set_var("FLYWHEEL_MAIL", "toron");
        let cfg = Config::default();
        assert_eq!(resolve_backend_name(&cfg), "superlogical");
        assert_eq!(resolve_mail_name(&cfg), "toron");
        std::env::remove_var("FLYWHEEL_BACKEND");
        std::env::remove_var("FLYWHEEL_MAIL");
    }

    /// Empty env falls back to config (env present-but-empty = unset).
    #[test]
    fn empty_env_falls_back_to_config() {
        let _g = _lock_env();
        std::env::set_var("FLYWHEEL_BACKEND", "");
        let cfg = Config {
            backend: Some(crate::config::BackendConfig {
                multiplexer: "tmux".into(),
                mail: "agentmail".into(),
            }),
            ..Config::default()
        };
        assert_eq!(resolve_backend_name(&cfg), "tmux");
        std::env::remove_var("FLYWHEEL_BACKEND");
    }

    /// An unknown backend name returns an actionable Unsupported (not a silent
    /// fallback to herdr). Verifies the factory error path without needing a
    /// live socket (the name is bogus so it fails at the match, not connect).
    #[test]
    fn unknown_backend_name_is_unsupported_not_silent_fallback() {
        let _g = _lock_env();
        std::env::set_var("FLYWHEEL_BACKEND", "nonsense");
        let cfg = Config::default();
        let res = multiplexer_backend(&cfg);
        match res {
            Err(BackendError::Unsupported {
                backend, method, ..
            }) => {
                assert_eq!(backend, "nonsense");
                assert_eq!(method, "(factory)");
            }
            Err(e) => panic!("expected Unsupported for unknown backend, got error: {e}"),
            Ok(_) => panic!("expected Unsupported for unknown backend, got Ok (silent fallback)"),
        }
        std::env::remove_var("FLYWHEEL_BACKEND");
    }

    /// A stubbed-but-unwired backend (superlogical) returns an actionable
    /// Unsupported pointing at the stub bead (no silent fallback).
    #[test]
    fn stubbed_backend_is_unsupported_with_stub_bead_pointer() {
        let _g = _lock_env();
        std::env::set_var("FLYWHEEL_BACKEND", "superlogical");
        let cfg = Config::default();
        let res = multiplexer_backend(&cfg);
        match res {
            Err(BackendError::Unsupported { reason, .. }) => {
                assert!(
                    reason.contains("bd-toron-superlogical-stubs"),
                    "reason: {reason}"
                );
            }
            Err(e) => panic!("expected Unsupported for superlogical, got error: {e}"),
            Ok(_) => panic!("expected Unsupported for superlogical, got Ok (silent fallback)"),
        }
        std::env::remove_var("FLYWHEEL_BACKEND");
    }
}
