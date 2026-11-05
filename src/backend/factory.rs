//! Backend factory — resolves the active `MultiplexerBackend` and
//! `MailBackend` from config/env and constructs them.
//!
//! governed-by: ADR-0002, ADR-0003, ADR-0045
//!
//! Plane selection itself is NOT decided here: it lives in
//! `toron_core::backend::plane` (toron ADR-0045), so flywheel, toron, and the
//! MCP server share one table and one construction site. This module supplies
//! the two edge facts flywheel owns — the configured name and its hc socket
//! path — and nothing else.
//!
//! Selection precedence (flag > env > `.env` > config.toml > default):
//! - `FLYWHEEL_BACKEND` (`auto`; `rex`/`superlogical` pin rex without fallback,
//!   `hc` pins hc)
//! - `FLYWHEEL_MAIL`    (`toron`; the agentmail backend is deleted, bd-euav)
//!
//! Mail backend routing (ADR-0003 V2/V3 — fixes the BUG1 shared-url misroute):
//! `FLYWHEEL_MAIL=toron` (the default and only backend) → the toron facade
//! (`src/backend/toron.rs` over `mail::Mail`), reading `[mail.toron]`
//! (absent → embedded in-process). The old `[agent_mail]` url is never a
//! routing source.
//!
//! An unrecognized name returns a clear error so a user who typos the env var
//! gets an actionable message, not a silent fallback.

use crate::backend::{BackendError, MailSurface, MultiplexerBackend, Result};
use crate::config::Config;
#[cfg(feature = "mail-toron")]
use crate::mail::Mail;

pub use toron_core::backend::plane::{Plane, PlaneChoice, PlaneEnv, ResolvedPlane};

/// Default multiplexer backend name. toron-core always carries both planes
/// (toron ADR-0045), so `auto` is the default regardless of flywheel features.
pub const DEFAULT_BACKEND: &str = "auto";
/// Default mail backend name.
pub const DEFAULT_MAIL: &str = "toron";

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

/// The edge snapshot the shared resolver reads. flywheel's configurable hc
/// socket rides here so toron-core does not need a flywheel-specific hook.
fn plane_env(cfg: &Config) -> PlaneEnv {
    PlaneEnv::detect(&resolve_backend_name(cfg), cfg.rex_bin_dir().as_deref())
        .with_hc_socket(Some(cfg.socket_path()))
}

/// Which plane will ACTUALLY serve, and why — one derivation for every door
/// (spawn arm, `backend list`, preflight, parity e2e). The configured name is
/// not evidence that rex can answer: `auto` used to pick the rex spawn arm
/// while the transport fell back to hc, so a host without the rex CLI created
/// an hc terminal set and then died in `rex run` (bd-mknvb).
pub fn resolve_plane(cfg: &Config) -> ResolvedPlane {
    toron_core::backend::plane::resolve(&plane_env(cfg))
}

/// Construct the active multiplexer backend. The construction site is
/// `toron_core::backend::plane::connect`, so `auto` resolves once (and prints
/// one line per process when it cannot stay on rex) before any call is made.
pub fn multiplexer_backend(cfg: &Config) -> Result<Box<dyn MultiplexerBackend>> {
    toron_core::backend::plane::connect(&plane_env(cfg))
        .map_err(|e| BackendError::Other(anyhow::anyhow!("{e}")))
}

/// Construct the active mail backend. Toron only (bd-euav) — any other
/// name is an actionable error (no silent fallback to toron — ADR-0002 §6).
pub fn mail_backend(cfg: &Config) -> Result<Box<dyn MailSurface>> {
    let name = resolve_mail_name(cfg);
    match name.as_str() {
        #[cfg(feature = "mail-toron")]
        "toron" => {
            let mail = Mail::from_config(Some(&cfg.toron_mail_config()))
                .map_err(BackendError::Other)?
                .ok_or_else(|| BackendError::Unsupported {
                    backend: "toron".to_string(),
                    method: "(factory)",
                    reason: "in-process mail facade unavailable".to_string(),
                })?;
            Ok(Box::new(crate::backend::toron::ToronMailBackend::new(mail)))
        }
        other => Err(backend_not_compiled_or_unknown(
            other,
            "mail",
            "FLYWHEEL_MAIL",
        )),
    }
}

/// Rex native door: every real plane name spawns (`auto`, `rex`,
/// `superlogical`, `hc`). A typo fails loud before any side effect instead of
/// silently building panes on the wrong backend.
pub fn require_mux_for_spawn(name: &str) -> Result<()> {
    if matches!(PlaneChoice::parse(name), PlaneChoice::Unknown(_)) {
        return Err(BackendError::Unsupported {
            backend: name.to_string(),
            method: "spawn",
            reason: "unknown multiplexer backend for spawn (want auto, rex, or hc)".to_string(),
        });
    }
    Ok(())
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

/// The backends compiled into this binary. Both pane planes come from
/// toron-core and are always present (toron ADR-0045); mail is feature-gated.
/// Used by `flywheel backend list`.
pub fn compiled_backends() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut out: Vec<&'static str> = vec!["hc", "superlogical"];
    #[cfg(feature = "mail-toron")]
    {
        out.extend(["toron"]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::lock_env;

    /// toron-core carries both planes, so the name default is `auto` in every
    /// feature combination.
    #[test]
    fn defaults_resolve_to_auto_and_toron() {
        let _g = lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL") };
        unsafe { std::env::remove_var("REX_SESSION") };
        let cfg = Config::default();
        assert_eq!(resolve_backend_name(&cfg), "auto");
        assert_eq!(resolve_mail_name(&cfg), "toron");
    }

    /// Env overrides config (the precedence contract — flag wiring is R0.6).
    #[test]
    fn env_overrides_config() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "superlogical") };
        unsafe { std::env::set_var("FLYWHEEL_MAIL", "toron") };
        let cfg = Config::default();
        assert_eq!(resolve_backend_name(&cfg), "superlogical");
        assert_eq!(resolve_mail_name(&cfg), "toron");
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL") };
    }

    /// Empty env falls back to config (env present-but-empty = unset).
    #[test]
    fn empty_env_falls_back_to_config() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "") };
        let cfg = Config {
            backend: Some(crate::config::BackendConfig {
                multiplexer: "superlogical".into(),
                mail: "toron".into(),
            }),
            ..Config::default()
        };
        assert_eq!(resolve_backend_name(&cfg), "superlogical");
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }

    /// bd-mknvb: `auto` on a host without the rex CLI resolves to the HC
    /// FALLBACK with the reason, before any call is made. Mutation: drop the
    /// CLI probe from the shared resolver and this fails while the spawn arm
    /// still says rex.
    #[test]
    fn auto_without_a_rex_cli_resolves_to_the_hc_fallback() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "auto") };
        let bin = tempfile::tempdir().expect("empty rex dir");
        let cfg = Config {
            rex_bin_dir: Some(bin.path().display().to_string()),
            ..Config::default()
        };
        let plane = resolve_plane(&cfg);
        assert_eq!(plane.configured, PlaneChoice::Auto);
        assert_eq!(plane.plane, Plane::Hc, "{}", plane.reason);
        assert!(!plane.rex_reachable);
        assert!(plane.is_fallback());
        assert!(
            plane
                .reason
                .starts_with("auto -> hc fallback: rex CLI not found"),
            "{}",
            plane.reason
        );
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }

    /// An explicit rex pin with no CLI keeps the rex plane and refuses to
    /// construct — never a silent hc ride.
    #[test]
    fn an_explicit_rex_pin_without_a_cli_refuses_instead_of_falling_back() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "rex") };
        let bin = tempfile::tempdir().expect("empty rex dir");
        let cfg = Config {
            rex_bin_dir: Some(bin.path().display().to_string()),
            ..Config::default()
        };
        let plane = resolve_plane(&cfg);
        assert_eq!(plane.plane, Plane::Rex);
        assert!(!plane.rex_reachable);
        assert!(!plane.is_fallback(), "a pin is not a fallback");
        let err = multiplexer_backend(&cfg)
            .err()
            .expect("a missing pinned CLI must refuse");
        assert!(
            err.to_string().contains("rex CLI not found"),
            "names the missing CLI: {err}"
        );
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }

    /// An unknown backend name returns an actionable error naming the choices
    /// (not a silent fallback to hc).
    #[test]
    fn unknown_backend_name_is_refused_not_silent_fallback() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "nonsense") };
        let cfg = Config::default();
        let err = multiplexer_backend(&cfg).err().expect("a typo must refuse");
        let message = err.to_string();
        assert!(message.contains("nonsense"), "names the typo: {message}");
        assert!(
            message.contains("unknown multiplexer backend"),
            "classifies the refusal: {message}"
        );
        assert!(
            message.contains("want auto, rex, or hc"),
            "names the choices: {message}"
        );
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
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

    /// Both planes come from toron-core and are always compiled; mail is
    /// feature-gated. This is what `backend list` surfaces.
    #[test]
    fn compiled_backends_names_both_planes_and_toron() {
        let backends = super::compiled_backends();
        assert!(backends.contains(&"hc"), "hc missing: {backends:?}");
        assert!(
            backends.contains(&"superlogical"),
            "the rex plane is unconditional in toron-core: {backends:?}"
        );
        #[cfg(feature = "mail-toron")]
        assert!(backends.contains(&"toron"), "toron missing: {backends:?}");
        assert!(
            !backends.contains(&"agentmail"),
            "agentmail backend is deleted (bd-euav)"
        );
    }

    /// A missing rex binary is an install error, not "not yet released",
    /// and it must not fall through to the operator's live server.
    #[test]
    fn superlogical_stub_is_honest_unsupported() {
        let _g = lock_env();
        let dir = tempfile::tempdir().expect("empty rex dir");
        let prev = std::env::var_os("REX_BIN_DIR");
        unsafe { std::env::set_var("REX_BIN_DIR", dir.path()) };
        let b = toron_core::backend::superlogical::SuperlogicalBackend;
        assert_eq!(b.name(), "superlogical");
        let m: Box<dyn MultiplexerBackend> = Box::new(b);
        let err = match m.subscribe(&[]) {
            Err(err) => err,
            Ok(_) => panic!("missing rex must not open a stream"),
        };
        unsafe {
            match &prev {
                Some(value) => std::env::set_var("REX_BIN_DIR", value),
                None => std::env::remove_var("REX_BIN_DIR"),
            }
        }
        let message = err.to_string();
        assert!(
            message.contains("rex CLI not installed"),
            "names the fix: {message}"
        );
        assert!(
            !message.contains("not yet released"),
            "a missing binary is not a release excuse: {message}"
        );
    }

    #[cfg(feature = "mail-toron")]
    #[test]
    fn toron_backend_is_real_facade() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_MAIL", "toron") };
        let cfg = Config::default();
        let b = super::mail_backend(&cfg).expect("toron facade constructs");
        assert_eq!(b.name(), "toron");
        match crate::backend::run_mail(b.health()) {
            Ok(_) => {}
            Err(e) => panic!("toron facade health() must delegate to in-process mailbox: {e:?}"),
        }
        unsafe { std::env::remove_var("FLYWHEEL_MAIL") };
    }

    /// c3b.2 AC2: `Arc<dyn MailSurface>` is constructible and callable from
    /// multiple threads — the receiver change to `&self` is what makes the
    /// backend shareable without a serializing Mutex (the durable loop's
    /// natural shape for mail ops from multiple leaves).
    #[cfg(feature = "mail-toron")]
    #[test]
    fn mail_backend_is_arc_shareable() {
        use std::sync::Arc;
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_MAIL", "toron") };
        let cfg = Config::default();
        let b: Box<dyn MailSurface> = super::mail_backend(&cfg).expect("toron facade constructs");
        let arc: Arc<dyn MailSurface> = Arc::from(b);
        let name = arc.name();
        assert_eq!(name, "toron", "name() through &Arc");
        let agent = arc.agent_name();
        assert!(!agent.is_empty(), "agent_name() must not be the empty trap");
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let arc = Arc::clone(&arc);
                std::thread::spawn(move || match crate::backend::run_mail(arc.health()) {
                    Ok(_) => true,
                    Err(e) => {
                        eprintln!("health from thread: {e:?}");
                        false
                    }
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().expect("thread joins"), "health() via shared Arc");
        }
        unsafe { std::env::remove_var("FLYWHEEL_MAIL") };
    }
}

#[cfg(test)]
mod spawn_gate_tests {
    use super::require_mux_for_spawn;
    use crate::backend::BackendError;

    /// Every real plane name passes the gate, including the `rex` spelling.
    #[test]
    fn spawn_gate_passes_every_real_plane() {
        for name in ["hc", "auto", "rex", "superlogical"] {
            assert!(
                require_mux_for_spawn(name).is_ok(),
                "{name} must pass the spawn gate"
            );
        }
        match require_mux_for_spawn("nonsense") {
            Err(BackendError::Unsupported {
                backend, method, ..
            }) => {
                assert_eq!(backend, "nonsense");
                assert_eq!(method, "spawn");
            }
            other => panic!("a typo must be a typed spawn refusal, got {other:?}"),
        }
    }
}
