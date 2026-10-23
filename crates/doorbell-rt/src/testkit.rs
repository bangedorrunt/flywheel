// governed-by: ADR-0012
//! bd-w7g8: the one test-process env contract for this workspace.
//!
//! A test that spawns a binary hands it the operator's whole environment
//! unless it says otherwise: `HOME`, the `XDG_*` roots, `TMPDIR`, and whatever
//! ambient daemon URL happens to be exported. The child then resolves the LIVE
//! store — `index.sqlite`, `operator_nsec`, `~/.local/state/flywheel` — because
//! nothing told it a test was running.
//!
//! The marker is the point. toron already refuses to guess a dev build from
//! the binary's path (`CACHEDIR.TAG` above it); a child is under test when the
//! harness SAYS so. So this sets the marker and then every root the child
//! resolves, all inside one temp root, plus a herdr socket path that does not
//! exist. A hermetic test cannot dial the real herdr through it.
//!
//! Lives here because it is the only crate both `flywheel` and `doorbell-cli`
//! depend on, and the workspace does not grow a crate for test support.

use std::path::PathBuf;
use std::process::Command;

/// The marker a child reads to know it is under test. Same name toron's own
/// testkit sets, so a `toron` a test spawns and a `flywheel` a test spawns
/// agree on what "under test" means.
pub const UNDER_TEST_ENV: &str = "TORON_UNDER_TEST";

/// This test process's root: one per pid, so two test binaries running at once
/// do not share a store.
///
/// `$TMPDIR` is deliberately not re-read here. A caller that wants the child to
/// agree with this process passes the same path to [`apply_test_env`], which
/// points `TMPDIR` at this root — and toron only trusts a marker that sits
/// under the child's own temp dir, so the two must be the same tree.
pub fn harness_root() -> PathBuf {
    std::env::temp_dir().join(format!("flywheel-test-env-{}", std::process::id()))
}

/// Put `cmd` under test: the marker, every root it resolves, and no ambient
/// daemon or herdr socket.
///
/// A test that needs a different root sets that variable AFTER this call; the
/// later `env` wins and the marker still keeps the child off the operator's
/// home for everything it does not pin.
pub fn apply_test_env(cmd: &mut Command) -> &mut Command {
    let root = harness_root();
    let home = root.join("home");
    for dir in [
        root.clone(),
        root.join("data"),
        home.join(".config"),
        home.join(".local").join("state"),
        home.join(".local").join("share"),
        home.join(".cache"),
    ] {
        let _ = std::fs::create_dir_all(dir);
    }
    cmd.env(UNDER_TEST_ENV, &root)
        .env("TORON_DATA_DIR", root.join("data"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local").join("state"))
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("TMPDIR", &root)
        // A path under the sandbox that was never bound: a child that dials it
        // fails here instead of reaching the operator's live multiplexer.
        .env("HERDR_SOCKET_PATH", root.join("no-herdr.sock"))
        .env_remove("TORON_MCP_URL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_pins_every_root_inside_one_tree() {
        let mut cmd = Command::new("true");
        apply_test_env(&mut cmd);
        let envs: std::collections::HashMap<_, _> = cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        let root = harness_root();
        assert_eq!(envs[UNDER_TEST_ENV], root.to_string_lossy());
        for var in [
            "TORON_DATA_DIR",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "TMPDIR",
            "HERDR_SOCKET_PATH",
        ] {
            let value = &envs[var];
            assert!(
                std::path::Path::new(value).starts_with(&root),
                "{var} must be inside the harness root, got {value}"
            );
        }
        assert!(!std::path::Path::new(&envs["HERDR_SOCKET_PATH"]).exists());
        // A removal is RECORDED, not merely absent: the child must see it gone
        // even when the parent never exported it.
        assert_eq!(
            cmd.get_envs()
                .find(|(k, _)| *k == "TORON_MCP_URL")
                .map(|(_, v)| v),
            Some(None)
        );
    }
}
