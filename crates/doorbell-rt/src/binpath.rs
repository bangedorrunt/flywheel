// governed-by: ADR-0012
//! Resolve the external CLI binaries the daemon execs (`toron`, `herdr`).
//!
//! Why this exists: the daemon is routinely launched by launchd/systemd,
//! whose `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin` — the `toron` and `herdr`
//! a *login shell* finds are simply not on it. Every bare-name spawn then
//! returns `NotFound`, which the host reports as an ordinary child death:
//! the watch child restarts on the retry gate forever (`WatchDied` →
//! `RestartWatch`) and each inbox fetch audits `toron spawn/timeout failed`,
//! so the plane delivers nothing while still advertising `mode: hosted`.
//! Observed live 2026-09-13: 250 `RestartWatch` rows in ~83 minutes and an
//! `inbox_error` row per pin per second, with `/health` showing no
//! degradation at all.
//!
//! Resolution order (first hit wins): `PATH` (skipping `.../mise/shims`
//! entries — those are dispatchers, not binaries), then the standard user
//! install dirs (`~/.cargo/bin`, `~/.local/bin`), then beside our own exe
//! (dev checkouts / co-install), then mise's shim dir last.
//!
//! Shims go LAST deliberately. A mise shim is a dispatcher (`shims/toron` is
//! a symlink to the `mise` binary) that must infer a tool version from env
//! and cwd; launched by launchd the daemon has neither the context nor the
//! version pin, so the shim dies with `mise ERROR No version is set for
//! shim: toron` — observed live 2026-09-13 right after "resolving" the CLI.
//! The real `~/.cargo/bin/toron` runs fine under that same minimal env, so
//! the order prefers real binaries and keeps the shim as a last resort.
//!
//! A caller with an explicit env override keeps it: this resolver is only
//! consulted for bare names.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `.../mise/shims` is a dispatcher dir, not a real bin directory.
fn is_mise_shim_dir(dir: &Path) -> bool {
    dir.components()
        .map(|c| c.as_os_str())
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w[0] == "mise" && w[1] == "shims")
}

/// Resolve `name` to an absolute path using the live process environment.
/// `None` means "not found anywhere" — callers keep the bare name so
/// behaviour is unchanged, but must surface the miss (see `/health`
/// `cli_unresolved`).
pub fn resolve_cli(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH");
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_cli_in(
        name,
        path_env.as_deref(),
        exe_dir.as_deref(),
        home.as_deref(),
    )
}

/// Pure core of [`resolve_cli`] — all inputs injected so the search order
/// is testable without mutating process env (which the test runner shares).
pub fn resolve_cli_in(
    name: &str,
    path_env: Option<&std::ffi::OsStr>,
    exe_dir: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(paths) = path_env {
        for dir in std::env::split_paths(paths) {
            // Interactive PATH lists mise shims first. A shim is a dispatcher
            // (symlink to `mise`) that dies under launchd with "No version is
            // set". Skip it here so the real cargo/local bin still wins; the
            // shim remains the last-resort search below (bd-gpj0).
            if is_mise_shim_dir(&dir) {
                continue;
            }
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(home) = home {
        for rel in [".cargo/bin", ".local/bin"] {
            let candidate = home.join(rel).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(dir) = exe_dir {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(home) = home {
        let candidate = home.join(".local/share/mise/shims").join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The env var names the daemon honours for an explicit override, kept
/// beside the resolver so the contract is in one place.
pub const TORON_BIN_ENV: &str = "DOORBELL_TORON_BIN";
/// Explicit `herdr` override.
pub const HERDR_BIN_ENV: &str = "DOORBELL_HERDR_BIN";

/// Resolve a daemon CLI: explicit env override wins verbatim (an operator
/// named it, so a typo must fail loudly at spawn rather than be silently
/// second-guessed), else the search, else the bare name for a visible
/// spawn error.
pub fn resolve_cli_or_name(name: &str, env_key: &str) -> (OsString, bool) {
    if let Some(explicit) = std::env::var_os(env_key) {
        return (explicit, true);
    }
    match resolve_cli(name) {
        Some(p) => (p.into_os_string(), true),
        None => (OsString::from(name), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbrt-binpath-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir scratch");
        dir
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir parent");
        fs::write(p, b"#!/bin/sh\n").expect("write stub");
    }

    /// Interactive-shell PATH lists mise shims first. Those are dispatchers,
    /// not binaries — skip them so `~/.cargo/bin/toron` still wins (bd-gpj0).
    #[test]
    fn path_mise_shim_loses_to_home_cargo_bin() {
        let root = scratch("path-shim");
        let home = root.join("home");
        let shim = home.join(".local/share/mise/shims");
        touch(&shim.join("toron"));
        touch(&home.join(".cargo/bin/toron"));
        let path = OsString::from(format!("{}:/usr/bin:/bin", shim.display()));
        let got = resolve_cli_in("toron", Some(path.as_os_str()), None, Some(&home));
        assert_eq!(
            got,
            Some(home.join(".cargo/bin/toron")),
            "a PATH mise shim must not beat the real cargo bin"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn path_wins_over_every_fallback() {
        let root = scratch("path-first");
        let on_path = root.join("bin");
        let beside = root.join("exe");
        let home = root.join("home");
        touch(&on_path.join("toron"));
        touch(&beside.join("toron"));
        touch(&home.join(".cargo/bin/toron"));
        let got = resolve_cli_in(
            "toron",
            Some(OsString::from(on_path.display().to_string()).as_os_str()),
            Some(&beside),
            Some(&home),
        );
        assert_eq!(got, Some(on_path.join("toron")), "PATH is honoured first");
        let _ = fs::remove_dir_all(&root);
    }

    /// The live follow-on failure: the daemon was launched from mise's shim
    /// dir, so "beside our own exe" IS a shim — a dispatcher that dies with
    /// `No version is set for shim` under launchd. A real `~/.cargo/bin`
    /// binary must win over it.
    #[test]
    fn real_home_bin_beats_the_shim_dir_the_daemon_ran_from() {
        let root = scratch("shim-exe");
        let hexim = root.join("home/.local/share/mise/shims");
        let home = root.join("home");
        touch(&hexim.join("toron"));
        touch(&home.join(".cargo/bin/toron"));
        let got = resolve_cli_in("toron", None, Some(&hexim), Some(&home));
        assert_eq!(
            got,
            Some(home.join(".cargo/bin/toron")),
            "a real binary beats the shim dir the daemon was launched from"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Dev checkouts (`target/debug/doorbell` beside `target/debug/toron`)
    /// still resolve co-located when no standard user bin has the tool.
    #[test]
    fn exe_dir_is_used_when_no_standard_home_bin_has_it() {
        let root = scratch("devcheckout");
        let beside = root.join("target/debug");
        let home = root.join("home");
        touch(&beside.join("toron"));
        let got = resolve_cli_in("toron", None, Some(&beside), Some(&home));
        assert_eq!(got, Some(beside.join("toron")));
        let _ = fs::remove_dir_all(&root);
    }

    /// A shim-dir-only install still resolves (last resort), so the search
    /// does not silently drop the tool on machines without cargo bins.
    #[test]
    fn mise_shims_are_the_last_resort_not_a_gap() {
        let root = scratch("shim-only");
        let home = root.join("home");
        touch(&home.join(".local/share/mise/shims/toron"));
        let got = resolve_cli_in("toron", None, None, Some(&home));
        assert_eq!(got, Some(home.join(".local/share/mise/shims/toron")));
        let _ = fs::remove_dir_all(&root);
    }

    /// The live failure: launchd PATH = /usr/bin:/bin:... and no toron on
    /// it; the daemon exe lives in mise's shims dir, and `herdr` lives only
    /// in `~/.local/bin`. Both must resolve from the home fallbacks.
    #[test]
    fn launchd_minimal_path_still_resolves_stack_bins() {
        let root = scratch("launchd");
        let home = root.join("home");
        touch(&home.join(".cargo/bin/toron"));
        touch(&home.join(".local/bin/herdr"));
        let shim = root.join("home/.local/share/mise/shims");
        touch(&shim.join("toron"));
        let minimal = OsString::from("/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(
            resolve_cli_in("toron", Some(minimal.as_os_str()), Some(&shim), Some(&home)),
            Some(home.join(".cargo/bin/toron")),
            "toron resolves to the real binary under launchd's PATH, not the shim"
        );
        assert_eq!(
            resolve_cli_in(
                "herdr",
                Some(minimal.as_os_str()),
                Some(&root.join("nope")),
                Some(&home)
            ),
            Some(home.join(".local/bin/herdr")),
            "herdr resolves under launchd's PATH"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn absent_binary_is_none_not_a_guess() {
        let root = scratch("absent");
        let got = resolve_cli_in(
            "toron",
            Some(OsString::from("/usr/bin:/bin").as_os_str()),
            Some(&root.join("exe")),
            Some(&root.join("home")),
        );
        assert_eq!(got, None, "a missing CLI must not be invented");
        let _ = fs::remove_dir_all(&root);
    }
}
