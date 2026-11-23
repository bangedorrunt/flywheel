use flywheel_test_sandbox::TestSandbox;

/// bd-i4edn: an install must re-register a launchd job from its plist, since
/// launchd refuses to spawn a replaced binary after a plain kickstart.
#[test]
fn stack_restart_reregisters_jobs_from_their_plists() {
    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = TestSandbox::new();
    let status = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .arg(format!("{root}/scripts/test-stack-launchd-reregister.sh"))
        .current_dir(root)
        .status()
        .expect("run scripts/test-stack-launchd-reregister.sh");
    assert!(
        status.success(),
        "scripts/test-stack-launchd-reregister.sh failed"
    );
}
