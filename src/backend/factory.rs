//! Backend factory — resolves the active `MultiplexerBackend` and
//! `MailBackend` from config/env and constructs them.
//!
//! governed-by: ADR-0002, ADR-0003
//!
//! Selection precedence (flag > env > `.env` > config.toml > default):
//! - `FLYWHEEL_BACKEND` (`auto` when the rex backend is compiled in, else
//!   `hc`; `superlogical` pins rex without fallback)
//! - `FLYWHEEL_MAIL`    (`toron`; the agentmail backend is deleted, bd-euav)
//!
//! Mail backend routing (ADR-0003 V2/V3 — fixes the BUG1 shared-url misroute):
//! `FLYWHEEL_MAIL=toron` (the default and only backend) → the toron facade
//! (`src/backend/toron.rs` over `mail::Mail`), reading `[mail.toron]`
//! (absent → embedded in-process). The old `[agent_mail]` url is never a
//! routing source.
//!
//! An unrecognized name returns a clear `BackendError::Unsupported` so a
//! user who typos the env var gets an actionable error, not a silent
//! fallback.

use crate::backend::{BackendError, MailSurface, MultiplexerBackend, Result};
use crate::config::Config;
#[cfg(feature = "mail-toron")]
use crate::mail::Mail;

/// Default multiplexer backend name: rex-first with hc fallback when the
/// rex backend is compiled in, else hc.
#[cfg(feature = "backend-superlogical")]
pub const DEFAULT_BACKEND: &str = "auto";
/// Default multiplexer backend name (the rex backend is not compiled in).
#[cfg(not(feature = "backend-superlogical"))]
pub const DEFAULT_BACKEND: &str = "hc";
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

/// Which pane plane a call will actually reach (bd-mknvb).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plane {
    Rex,
    Hc,
}

impl Plane {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rex => "rex",
            Self::Hc => "hc",
        }
    }
}

/// The plane that will ACTUALLY serve, and why (bd-mknvb). ONE derivation for
/// every consumer — the spawn arm, `backend list`, preflight's pane-plane, and
/// the parity e2e — because the configured name is not evidence that rex can
/// answer: `auto` used to pick the rex spawn arm while the transport fell back
/// to hc, so a host without the rex CLI created an hc terminal set and then
/// died in `rex run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlane {
    /// What the env/config asked for (`auto` | `hc` | `superlogical`).
    pub configured: String,
    /// What will serve.
    pub plane: Plane,
    /// The rex CLI answered the presence probe.
    pub rex_reachable: bool,
    /// One sentence: which layer chose this plane, and why.
    pub reason: String,
}

impl ResolvedPlane {
    /// `auto` (or a typo) that had to leave the primary for hc.
    pub fn is_fallback(&self) -> bool {
        self.plane == Plane::Hc && self.configured != "hc"
    }
}

/// The rex CLI presence probe: `$REX_BIN_DIR/rex` when the variable is set
/// (no PATH fall-through — the same contract as `Rex::resolve`), else an
/// executable `rex` on PATH. Presence only: it never spawns rex, so a cheap
/// probe is safe on every door.
fn rex_cli_present(cfg: &Config) -> std::result::Result<std::path::PathBuf, String> {
    if let Some(dir) = cfg.rex_bin_dir() {
        let path = dir.join("rex");
        if executable_file(&path) {
            return Ok(path);
        }
        return Err(format!(
            "rex CLI not found ({} is not an executable file)",
            path.display()
        ));
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join("rex"))
                .find(|p| executable_file(p))
        })
        .ok_or_else(|| "rex CLI not found (no rex on PATH)".to_string())
}

fn executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Resolve the pane plane (bd-mknvb): `auto` rides rex only when the rex CLI
/// can answer, else hc WITH THE REASON; `superlogical` is an explicit rex
/// pin (a missing CLI fails loud, never a silent fallback); `hc` is hc.
/// A fallback prints ONE line per process — the fallback stays per call (it is
/// the transport contract), the operator hears about it once.
pub fn resolve_plane(cfg: &Config) -> ResolvedPlane {
    let configured = resolve_backend_name(cfg);
    let rex_compiled = cfg!(feature = "backend-superlogical");
    let (plane, rex_reachable, reason) = match configured.as_str() {
        "hc" => (Plane::Hc, false, "hc (configured)".to_string()),
        "superlogical" if rex_compiled => match rex_cli_present(cfg) {
            Ok(path) => (
                Plane::Rex,
                true,
                format!("superlogical: explicit rex pin ({})", path.display()),
            ),
            Err(why) => (
                Plane::Rex,
                false,
                format!("superlogical: explicit rex pin, but {why}"),
            ),
        },
        "superlogical" => (
            Plane::Rex,
            false,
            "superlogical: the rex backend is not compiled into this build (lean hc-only build)"
                .to_string(),
        ),
        "auto" if rex_compiled => match rex_cli_present(cfg) {
            Ok(path) => (
                Plane::Rex,
                true,
                format!("auto -> rex ({})", path.display()),
            ),
            Err(why) => (Plane::Hc, false, format!("auto -> hc fallback: {why}")),
        },
        "auto" => (
            Plane::Hc,
            false,
            "auto -> hc: the rex backend is not compiled into this build (lean hc-only build)"
                .to_string(),
        ),
        other => (
            Plane::Hc,
            false,
            format!(
                "{other}: unknown multiplexer backend (spawn refuses; set FLYWHEEL_BACKEND \
                 to hc|auto|superlogical)"
            ),
        ),
    };
    let resolved = ResolvedPlane {
        configured,
        plane,
        rex_reachable,
        reason,
    };
    if resolved.is_fallback() {
        note_missing_primary_once(&resolved.reason);
    }
    resolved
}

/// One line per process when the primary cannot serve (bd-mknvb).
fn note_missing_primary_once(reason: &str) {
    use std::sync::OnceLock;
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_ok() {
        eprintln!("flywheel: pane plane: {reason} — every pane call in this process rides it");
    }
}

/// Construct the hc backend (connects the client itself, ADR-0005 P1).
#[cfg(feature = "backend-hc")]
fn hc_backend(cfg: &Config) -> Result<Box<dyn MultiplexerBackend>> {
    let backend = toron_core::backend::hc::HcBackend::connect(Some(&cfg.socket_path()))
        .map_err(|e| BackendError::Other(anyhow::anyhow!("{e}")))?;
    Ok(Box::new(backend))
}

/// Auto selection: rex first, always. A set `REX_SESSION` only records
/// that this process runs inside a rex terminal; it never decides WHETHER
/// rex is tried — `auto` builds the rex primary regardless of
/// `REX_SESSION` or any hc socket. hc serves only as transport
/// fallback (see [`FallbackBackend`]). `FLYWHEEL_BACKEND=hc` selects
/// hc explicitly (the honest escape hatch).
///
/// Construct the active multiplexer backend. Default is `auto` (rex-primary
/// when `backend-superlogical` is compiled; otherwise hc). An
/// unrecognized name is an actionable error (no silent fallback —
/// ADR-0002 §6). The `backend-hc` cargo feature gates the hc impl;
/// when it's off, the name falls through to `other` with a feature-aware
/// error.
pub fn multiplexer_backend(cfg: &Config) -> Result<Box<dyn MultiplexerBackend>> {
    let name = resolve_backend_name(cfg);
    match name.as_str() {
        #[cfg(feature = "backend-hc")]
        "hc" => hc_backend(cfg),
        #[cfg(feature = "backend-superlogical")]
        "superlogical" => Ok(Box::new(toron_core::backend::superlogical::SuperlogicalBackend)),
        #[cfg(feature = "backend-superlogical")]
        "auto" => {
            let primary: Box<dyn MultiplexerBackend> =
                Box::new(toron_core::backend::superlogical::SuperlogicalBackend);
            #[cfg(feature = "backend-hc")]
            let fallback: Option<Box<dyn MultiplexerBackend>> =
                match toron_core::backend::hc::HcBackend::connect(Some(&cfg.socket_path())) {
                    Ok(backend) => Some(Box::new(backend)),
                    Err(e) => {
                        eprintln!("auto backend: hc unavailable ({e}); rex-only");
                        None
                    }
                };
            #[cfg(not(feature = "backend-hc"))]
            let fallback: Option<Box<dyn MultiplexerBackend>> = None;
            Ok(Box::new(crate::backend::fallback::FallbackBackend::new(
                primary, fallback,
            )))
        }
        other => Err(backend_not_compiled_or_unknown(
            other,
            "multiplexer",
            "FLYWHEEL_BACKEND",
        )),
    }
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

/// Rex native door: `hc`, `auto`, and `superlogical` all spawn. Any other
/// selection fails loud before any side effect instead of silently building
/// panes on the wrong backend.
pub fn require_mux_for_spawn(name: &str) -> Result<()> {
    if matches!(name, "hc" | "auto" | "superlogical") {
        return Ok(());
    }
    Err(BackendError::Unsupported {
        backend: name.to_string(),
        method: "spawn",
        reason: "unknown multiplexer backend for spawn (want hc, auto, or superlogical)"
            .to_string(),
    })
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
    #[cfg_attr(
        not(any(
            feature = "backend-hc",
            feature = "backend-superlogical",
            feature = "mail-toron"
        )),
        allow(unused_mut)
    )]
    let mut out: Vec<&'static str> = Vec::new();
    #[cfg(feature = "backend-hc")]
    {
        out.extend(["hc"]);
    }
    #[cfg(feature = "backend-superlogical")]
    {
        out.extend(["superlogical"]);
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
    use crate::config::lock_env;

    /// Defaults resolve to hc + toron when nothing is set (the build
    /// without the rex backend; with it, the default is auto).
    #[cfg(not(feature = "backend-superlogical"))]
    #[test]
    fn defaults_resolve_to_hc_and_toron() {
        let _g = lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL") };
        let cfg = Config::default();
        assert_eq!(resolve_backend_name(&cfg), "hc");
        assert_eq!(resolve_mail_name(&cfg), "toron");
    }

    /// With the rex backend compiled in, the default name is auto. Auto
    /// constructs rex-first with hc transport fallback regardless of
    /// `REX_SESSION`: rex resolves lazily per call, and a down hc server
    /// leaves a primary-only backend behind a warning.
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn defaults_resolve_to_auto_and_toron() {
        let _g = lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL") };
        unsafe { std::env::remove_var("REX_SESSION") };
        let cfg = Config::default();
        assert_eq!(resolve_backend_name(&cfg), "auto");
        assert_eq!(resolve_mail_name(&cfg), "toron");
        let backend = multiplexer_backend(&cfg).expect("auto constructs");
        assert_eq!(backend.name(), "auto");
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL") };
        unsafe { std::env::remove_var("REX_SESSION") };
    }

    /// Auto is rex-primary on every door: `REX_SESSION` set, empty, or
    /// unset all construct the same rex-first backend. A hosted bot, a cron
    /// run, and the doorbell have no `REX_SESSION` and still resolve rex.
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn auto_ignores_rex_session_env_and_selects_rex_primary() {
        let _g = lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        for session in ["session:fake-1", "", "   "] {
            unsafe { std::env::set_var("REX_SESSION", session) };
            let cfg = Config::default();
            let backend = multiplexer_backend(&cfg).expect("auto constructs");
            assert_eq!(
                backend.name(),
                "auto",
                "REX_SESSION={session:?} must still select rex-first"
            );
        }
        unsafe { std::env::remove_var("REX_SESSION") };
        let cfg = Config::default();
        let backend = multiplexer_backend(&cfg).expect("auto constructs");
        assert_eq!(backend.name(), "auto", "unset REX_SESSION still rex-first");
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
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

    /// An unknown backend name returns an actionable Unsupported (not a silent
    /// fallback to hc). Verifies the factory error path without needing a
    /// live socket (the name is bogus so it fails at the match, not connect).
    #[test]
    fn unknown_backend_name_is_unsupported_not_silent_fallback() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "nonsense") };
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
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }

    /// A backend compiled out (superlogical without its feature) returns an
    /// actionable Unsupported (no silent fallback).
    /// With `backend-superlogical` OFF, selecting superlogical yields the
    /// feature-aware "not compiled in" error (no silent fallback). Gated to
    /// the feature-off build (the stub itself is covered by --all-features).
    #[cfg(not(feature = "backend-superlogical"))]
    #[test]
    fn stubbed_backend_not_compiled_in_default_build() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "superlogical") };
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
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }

    /// The default build compiles hc + toron (the `default`
    /// feature set is `["serve", "backend-hc", "mail-toron"]`).
    /// This is the feature-matrix contract —
    /// compiled_backends() must reflect the actual cfg gates (it's what
    /// `flywheel backend list` will surface in R0.10).
    #[test]
    fn default_build_compiles_hc_and_toron() {
        let backends = super::compiled_backends();
        assert!(backends.contains(&"hc"), "hc missing: {backends:?}");
        #[cfg(feature = "mail-toron")]
        assert!(backends.contains(&"toron"), "toron missing: {backends:?}");
        assert!(
            !backends.contains(&"agentmail"),
            "agentmail backend is deleted (bd-euav)"
        );
        #[cfg(not(feature = "backend-superlogical"))]
        assert!(
            !backends.contains(&"superlogical"),
            "superlogical should be opt-in (feature off)"
        );
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

    /// A missing rex binary is an install error, not "not yet released",
    /// and it must not fall through to the operator's live server.
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn superlogical_stub_is_honest_unsupported() {
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

    /// c3b.2 AC2: `Arc<dyn MailBackend>` is constructible and callable from
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
    use super::{Plane, resolve_plane};
    use crate::backend::BackendError;
    use crate::config::{Config, lock_env};

    /// Rex native door: hc, auto, and superlogical all pass the gate.
    /// Unknown names still refuse loud before any side effect, naming the
    /// method and the fix. No live socket needed: the gate decides on the
    /// name alone.
    #[test]
    fn spawn_gate_passes_hc_and_rex_modes() {
        for name in ["hc", "auto", "superlogical"] {
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
            Err(e) => panic!("expected Unsupported for nonsense, got error: {e}"),
            Ok(()) => panic!("expected Unsupported for nonsense, got Ok"),
        }
    }

    /// bd-mknvb: `auto` on a host without the rex CLI resolves to the HC
    /// FALLBACK with the reason. The measured failure was the rex spawn arm
    /// being chosen from the configured name while the transport fell back:
    /// an hc terminal set was built, then `rex run` died.
    /// Mutation: drop the CLI probe from `resolve_plane`'s auto arm (always
    /// rex) and this test fails — the spawn arm follows this value.
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn auto_without_a_rex_cli_resolves_to_the_hc_fallback() {
        let bin = crate::backend::rex_test_guard::RexBinGuard::empty();
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        let cfg = Config {
            rex_bin_dir: Some(bin.dir_path().display().to_string()),
            ..Config::default()
        };
        let plane = resolve_plane(&cfg);
        assert_eq!(plane.configured, "auto");
        assert_eq!(plane.plane, Plane::Hc, "{}", plane.reason);
        assert!(!plane.rex_reachable);
        assert!(plane.is_fallback());
        assert!(
            plane
                .reason
                .starts_with("auto -> hc fallback: rex CLI not found"),
            "the fallback must name the missing CLI: {}",
            plane.reason
        );
    }

    /// The other side of the probe: with a rex CLI in `REX_BIN_DIR`, `auto`
    /// stays on rex and says so.
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn auto_with_a_rex_cli_resolves_to_rex() {
        let bin = crate::backend::rex_test_guard::RexBinGuard::install("exit 0");
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        let cfg = Config {
            rex_bin_dir: Some(bin.dir_path().display().to_string()),
            ..Config::default()
        };
        let plane = resolve_plane(&cfg);
        assert_eq!(plane.plane, Plane::Rex, "{}", plane.reason);
        assert!(plane.rex_reachable);
        assert!(!plane.is_fallback());
        assert!(
            plane.reason.starts_with("auto -> rex ("),
            "{}",
            plane.reason
        );
    }

    /// An EXPLICIT rex pin is not a fallback, even with the CLI gone: the
    /// spawn arm must refuse loud (nothing created), never quietly ride hc.
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn an_explicit_rex_pin_never_reports_a_fallback() {
        let bin = crate::backend::rex_test_guard::RexBinGuard::empty();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "superlogical") };
        let cfg = Config {
            rex_bin_dir: Some(bin.dir_path().display().to_string()),
            ..Config::default()
        };
        let plane = resolve_plane(&cfg);
        assert_eq!(
            plane.plane,
            Plane::Rex,
            "an explicit pin is rex even when the CLI is gone"
        );
        assert!(!plane.rex_reachable);
        assert!(
            !plane.is_fallback(),
            "a pin is not a fallback — the spawn arm must fail loud"
        );
        assert!(
            plane
                .reason
                .contains("explicit rex pin, but rex CLI not found"),
            "{}",
            plane.reason
        );
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }

    /// Mutation: if `rex_cli_present` ignored `cfg.rex_bin_dir` and only read
    /// the process-global `REX_BIN_DIR`, a parallel test that cleared the env
    /// would make this fail — config is the race-proof path.
    #[cfg(feature = "backend-superlogical")]
    #[test]
    fn config_rex_bin_dir_outranks_a_poisoned_env() {
        let bin = crate::backend::rex_test_guard::RexBinGuard::install("exit 0");
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        unsafe { std::env::remove_var("REX_BIN_DIR") };
        let cfg = Config {
            rex_bin_dir: Some(bin.dir_path().display().to_string()),
            ..Config::default()
        };
        let plane = resolve_plane(&cfg);
        assert_eq!(plane.plane, Plane::Rex, "{}", plane.reason);
        assert!(plane.rex_reachable, "{}", plane.reason);
    }

    /// `FLYWHEEL_BACKEND=hc` is a choice, not a fallback: no line, no probe.
    #[test]
    fn explicit_hc_never_reports_a_fallback() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "hc") };
        let plane = resolve_plane(&Config::default());
        assert_eq!(plane.plane, Plane::Hc);
        assert!(!plane.rex_reachable);
        assert!(!plane.is_fallback());
        assert_eq!(plane.reason, "hc (configured)");
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }
}
