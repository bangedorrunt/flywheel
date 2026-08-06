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
use crate::herdr_shim::Client as HerdrClient;
use crate::mail_shim::Mail;

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
/// actionable error (no silent fallback to the default — ADR-0002 §6). The
/// `backend-herdr` cargo feature gates the herdr impl; when it's off, the
/// name falls through to `other` with a feature-aware error.
pub fn multiplexer_backend(cfg: &Config) -> Result<Box<dyn MultiplexerBackend>> {
    let name = resolve_backend_name(cfg);
    match name.as_str() {
        #[cfg(feature = "backend-herdr")]
        "herdr" => {
            let client = HerdrClient::connect(Some(&cfg.socket_path()))
                .map_err(|e| BackendError::Other(anyhow::anyhow!("{e}")))?;
            Ok(Box::new(crate::backend::herdr::HerdrBackend::new(client)))
        }
        // superlogical/tmux: stub backends (bd-toron-superlogical-stubs). With
        // the feature on, construct the stub (name() enumerable, method
        // defaults give the clear "not yet released / on hold" error). With
        // the feature off, the arm is compiled out and the name falls through
        // to `other` -> "not compiled in".
        #[cfg(feature = "backend-superlogical")]
        "superlogical" => Ok(Box::new(crate::backend::superlogical::SuperlogicalBackend)),
        #[cfg(feature = "backend-tmux")]
        "tmux" => Ok(Box::new(crate::backend::tmux::TmuxBackend)),
        other => Err(backend_not_compiled_or_unknown(
            other,
            "multiplexer",
            "FLYWHEEL_BACKEND",
        )),
    }
}

/// Construct the active mail backend. Default = agentmail; unrecognized →
/// actionable error. The `mail-agentmail` cargo feature gates the agentmail
/// impl; when it's off, the name falls through to `other` with a feature-aware
/// error.
pub fn mail_backend(cfg: &Config) -> Result<Box<dyn MailBackend>> {
    let name = resolve_mail_name(cfg);
    match name.as_str() {
        #[cfg(feature = "mail-agentmail")]
        "agentmail" => {
            let mail = Mail::in_process(cfg.agent_mail.as_ref())
                .map_err(BackendError::Other)?
                .ok_or_else(|| BackendError::Unsupported {
                    backend: "agentmail".to_string(),
                    method: "(factory)",
                    reason: "in-process mail facade unavailable"
                        .to_string(),
                })?;
            Ok(Box::new(
                crate::backend::agentmail::AgentMailMcpBackend::new(mail),
            ))
        }
        // toron: stub mail backend (bd-toron-superlogical-stubs). Feature on ->
        // construct the stub; feature off -> falls through to `other`.
        #[cfg(feature = "mail-toron")]
        "toron" => Ok(Box::new(crate::backend::toron_mail::ToronMailBackend)),
        other => Err(backend_not_compiled_or_unknown(
            other,
            "mail",
            "FLYWHEEL_MAIL",
        )),
    }
}

/// Feature-aware error for a backend name that isn't compiled in or is unknown.
/// The single message covers both cases: a known backend whose cargo feature is
/// off (user must enable it) and a genuinely unknown name — both resolve the
/// same way (set the env var to a compiled backend).
fn backend_not_compiled_or_unknown(name: &str, _kind: &str, env_var: &str) -> BackendError {
    BackendError::Unsupported {
        backend: name.to_string(),
        method: "(factory)",
        reason: format!(
            "backend '{name}' is not compiled in (enable its cargo feature) or unknown; set {env_var} to a compiled backend"
        ),
    }
}

/// The backends compiled into this binary (feature-gated). Used by
/// `flywheel backend list` (R0.10) and by tests to assert the feature matrix.
///
/// clippy-clean pattern: each feature contributes a cfg-gated slice; the vec
/// concatenates them (no `push`-after-`Vec::new` lint).
pub fn compiled_backends() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    #[cfg(feature = "backend-herdr")]
    {
        out.extend(["herdr"]);
    }
    #[cfg(feature = "backend-superlogical")]
    {
        out.extend(["superlogical"]);
    }
    #[cfg(feature = "backend-tmux")]
    {
        out.extend(["tmux"]);
    }
    #[cfg(feature = "mail-agentmail")]
    {
        out.extend(["agentmail"]);
    }
    #[cfg(feature = "mail-toron")]
    {
        out.extend(["toron"]);
    }
    out
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
    /// With `backend-superlogical` OFF, selecting superlogical yields the
    /// feature-aware "not compiled in" error (no silent fallback). Gated to
    /// the feature-off build (the stub itself is covered by --all-features).
    #[cfg(not(feature = "backend-superlogical"))]
    #[test]
    fn stubbed_backend_not_compiled_in_default_build() {
        let _g = _lock_env();
        std::env::set_var("FLYWHEEL_BACKEND", "superlogical");
        let cfg = Config::default();
        let res = multiplexer_backend(&cfg);
        match res {
            Err(BackendError::Unsupported { reason, .. }) => {
                assert!(
                    reason.contains("not compiled in"),
                    "reason should say not-compiled (feature off): {reason}"
                );
            }
            Err(e) => panic!("expected Unsupported, got error: {e}"),
            Ok(_) => panic!("expected Unsupported, got Ok (silent fallback)"),
        }
        std::env::remove_var("FLYWHEEL_BACKEND");
    }

    /// The default build compiles herdr + agentmail (the `default` feature set
    /// is `["serve", "backend-herdr", "mail-agentmail"]`). This is the
    /// feature-matrix contract — compiled_backends() must reflect the actual
    /// cfg gates (it's what `flywheel backend list` will surface in R0.10).
    #[test]
    fn default_build_compiles_herdr_and_agentmail() {
        let backends = super::compiled_backends();
        assert!(backends.contains(&"herdr"), "herdr missing: {backends:?}");
        assert!(
            backends.contains(&"agentmail"),
            "agentmail missing: {backends:?}"
        );
        // superlogical/tmux/toron are opt-in: present IFF their feature is on
        #[cfg(not(feature = "backend-superlogical"))]
        assert!(
            !backends.contains(&"superlogical"),
            "superlogical should be opt-in (feature off)"
        );
        #[cfg(not(feature = "backend-tmux"))]
        assert!(!backends.contains(&"tmux"), "tmux should be opt-in");
        #[cfg(not(feature = "mail-toron"))]
        assert!(!backends.contains(&"toron"), "toron should be opt-in");
    }

    /// A backend name with its feature off produces the feature-aware error
    /// (no silent fallback). Simulated via backend_not_compiled_or_unknown
    /// directly, since a unit test can't toggle cargo cfg at runtime.
    #[test]
    fn not_compiled_backend_error_is_actionable() {
        let e = super::backend_not_compiled_or_unknown(
            "superlogical",
            "multiplexer",
            "FLYWHEEL_BACKEND",
        );
        let s = e.to_string();
        assert!(s.contains("superlogical"), "names the backend: {s}");
        assert!(
            s.contains("cargo feature"),
            "mentions enabling the feature: {s}"
        );
        assert!(s.contains("FLYWHEEL_BACKEND"), "points at the env var: {s}");
    }

    /// Stub contract (runs only in `--all-features` builds where the stub
    /// feature is on): the stub is name()-enumerable and every operation
    /// returns an honest Unsupported naming it (no Ok, no panic).
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn superlogical_stub_is_honest_unsupported() {
        let b = crate::backend::superlogical::SuperlogicalBackend;
        assert_eq!(b.name(), "superlogical");
        let mut m: Box<dyn MultiplexerBackend> = Box::new(b);
        match m.workspace_list() {
            Err(BackendError::Unsupported { backend, .. }) => assert_eq!(backend, "superlogical"),
            Ok(_) => panic!("stub must not Ok"),
            Err(e) => panic!("expected Unsupported, got {e:?}"),
        }
    }

    #[cfg(feature = "mail-toron")]
    #[test]
    fn toron_stub_is_honest_unsupported() {
        let b = crate::backend::toron_mail::ToronMailBackend;
        assert_eq!(b.name(), "toron");
        let mut m: Box<dyn MailBackend> = Box::new(b);
        match m.health() {
            Err(BackendError::Unsupported { backend, .. }) => assert_eq!(backend, "toron"),
            Ok(_) => panic!("stub must not Ok"),
            Err(e) => panic!("expected Unsupported, got {e:?}"),
        }
    }
}
