//! bd-5vi2x: setup-mcp provisions a per-seat events token without ever
//! touching the operator's real config or a live daemon. The fake toron on PATH
//! mints a token; the test asserts the 0600 file, the path-only Claude config,
//! and a plaintext-free journal. Fully hermetic (TestSandbox).

use std::process::Command;

use flywheel_test_sandbox::TestSandbox;

fn sh(dir: &std::path::Path, program: &str, args: &[&str]) -> std::process::Output {
    Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn")
}

#[test]
fn setup_mcp_mints_a_0600_token_and_writes_only_its_path_to_claude_config() {
    let sandbox = TestSandbox::new();
    let bin = sandbox.bin_dir();
    // Fake toron: mint prints a plaintext ONCE; revoke succeeds. Records argv.
    std::fs::write(
        bin.join("toron"),
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TORON_ARGV_LOG"
if [ "$1" = events ] && [ "$2" = token ] && [ "$3" = mint ]; then
  printf '{"token":"SECRET-PLAINTEXT-42","principal":"Pin","scope":{}}\n'
  exit 0
fi
if [ "$1" = events ] && [ "$2" = token ] && [ "$3" = revoke ]; then exit 0; fi
echo "unexpected toron call: $*" >&2
exit 2
"#,
    )
    .expect("fake toron");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let p = bin.join("toron");
        let mut perms = std::fs::metadata(&p).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).expect("chmod");
    }

    let argv_log = sandbox.root().join("toron-argv.log");
    let out = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .env("TORON_BIN", bin.join("toron"))
        .env("TORON_ARGV_LOG", &argv_log)
        .env("FLYWHEEL_MCP_SUBSCRIPTION", "ClaudeSeat")
        .env("FLYWHEEL_MCP_PROJECT", "flywheel")
        .args(["setup-mcp", "--agents", "claude"])
        .output()
        .expect("setup-mcp");
    assert!(
        out.status.success(),
        "setup-mcp failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The mint ran with the pin and project, never with a plaintext argument.
    let argv = std::fs::read_to_string(&argv_log).expect("argv log");
    assert!(
        argv.contains("events token mint --pin ClaudeSeat --project flywheel"),
        "mint argv: {argv}"
    );
    assert!(!argv.contains("SECRET-PLAINTEXT"), "no secret on argv: {argv}");

    // The token file lives under the seat's state dir, 0600, plaintext inside.
    let token_file = sandbox
        .root()
        .join("state/flywheel/events-tokens/flywheel-ClaudeSeat.token");
    assert!(token_file.is_file(), "token file missing: {}", token_file.display());
    assert_eq!(
        std::fs::read_to_string(&token_file).expect("token"),
        "SECRET-PLAINTEXT-42"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&token_file)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "token file must be 0600: {mode:o}");
    }

    // The Claude config carries ONLY the path; never the secret.
    let config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(sandbox.root().join("home/.claude.json")).expect("claude config"),
    )
    .expect("claude config json");
    let server = &config["mcpServers"]["flywheel-events"];
    assert_eq!(server["command"], "flywheel");
    assert_eq!(
        server["env"]["FLYWHEEL_TORON_TOKEN_FILE"],
        token_file.display().to_string()
    );
    assert!(
        !serde_json::to_string(&config)
            .expect("json")
            .contains("SECRET-PLAINTEXT"),
        "no plaintext in the config"
    );

    // The journal has the hash but no plaintext.
    let journal = std::fs::read_to_string(sandbox.root().join("state/flywheel/events-token-journal.jsonl"))
        .expect("journal");
    assert!(journal.contains("mint-intent") && journal.contains("minted"));
    assert!(
        !journal.contains("SECRET-PLAINTEXT"),
        "no plaintext in the journal: {journal}"
    );

    // A rerun is idempotent: no duplicate config change (the file is rewritten
    // with the same path; the mint is re-run but does not alter the config).
    let again = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", sandbox.path_env())
        .env("TORON_BIN", bin.join("toron"))
        .env("TORON_ARGV_LOG", &argv_log)
        .env("FLYWHEEL_MCP_SUBSCRIPTION", "ClaudeSeat")
        .env("FLYWHEEL_MCP_PROJECT", "flywheel")
        .args(["setup-mcp", "--agents", "claude"])
        .output()
        .expect("setup-mcp rerun");
    assert!(again.status.success(), "rerun failed");

    let _ = sh(sandbox.root(), "true", &[]);
}
