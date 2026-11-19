//! bd-6v6ec: a lease held by the checking process must not depend on external
//! `ps`/`sysctl` calls. Under a fully parallel suite those spawns can fail,
//! making one of two live workspaces look dead.

use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn self_lease_is_alive_when_external_liveness_tools_fail() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let root = std::env::temp_dir().join(format!("fw-self-lease-{}", std::process::id()));
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("fake bin");
    for name in ["ps", "sysctl", "hostname"] {
        let path = bin.join(name);
        std::fs::write(&path, "#!/usr/bin/env bash\nexit 1\n").expect("fake tool");
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }

    let prev_path = std::env::var_os("PATH");
    unsafe { std::env::set_var("PATH", &bin) };
    let outcome = std::panic::catch_unwind(|| {
        let state = root.join("state");
        std::fs::create_dir_all(&state).expect("state dir");
        let now = doorbell_rt::lease::unix_now();
        let lease = doorbell_rt::lease::acquire_lease(&state, "self-ws", now).expect("lease");
        assert!(
            doorbell_rt::lease::pid_alive(std::process::id()),
            "the checking process is alive without `ps`"
        );
        assert!(
            !doorbell_rt::lease::lease_stale(&lease, now),
            "a self lease must stay alive without `ps`/`sysctl`: {lease:?}"
        );
    });
    unsafe {
        match prev_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    outcome.expect("self lease hermetic under broken external tools");
}
