// governed-by: ADR-0031 (Elm core — pure decision + named effects)

/// bd-u54l: the armed host set is STATE, not argv.
///
/// The bug this exists to end: `doorbell restart` with no `--host` built its
/// expected launchd unit from its OWN empty argv, compared that against the
/// installed unit (which carried three hosts), found a mismatch, and fell back
/// to "starting detached" — with its own hostless config. Two harms at once: no
/// wakes delivered, and launchd supervision lost, so nothing restarted the
/// daemon if it died. `restart` printed "started" and exited 0 on the way there.
///
/// The invariant, and it is the whole design:
///
///     effective_hosts = persisted_hosts  merged-under  argv_hosts
///
/// Never argv alone. The plist fix alone was necessary and NOT sufficient —
/// adding `--host` to the unit still left `restart` producing standby, because
/// the comparison that decides supervised-vs-detached reads `cfg.hosts`, and
/// that came from argv.
///
/// `--unhost` is the other half and the one that matters for safety. Today the
/// only way to reach standby is to omit `--host`, which is also exactly what a
/// bug does. Making standby require an explicit verb turns "no wakes are
/// delivered" into a state someone CHOSE, persisted as an empty set, and
/// reportable by `doorbell status`.
use anyhow::Context as _;
use std::path::{Path, PathBuf};

/// One hosted loop. `Vec<(String, String)>` rather than a newtype so the rest of
/// the CLI keeps its existing shape without a wide refactor.
pub type Host = (String, String);

/// `~/.local/state/doorbell/hosts.json`, per state dir. A test with its own
/// state dir gets its own host set and cannot inherit the operator's, which is
/// what makes the fence below safe to run.
pub fn hosts_file(state_dir: &Path) -> PathBuf {
    state_dir.join("hosts.json")
}

/// Where the effective set came from. Printed on every arm, because bd-jbz1's
/// failure mode was never that health was silent — `standby_with_known_workspaces`
/// said so — it was that `restart` said "started" and exited 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSource {
    /// `--unhost`: an explicit decision to serve nothing.
    Unhosted,
    /// argv only, because nothing was persisted yet (first arm).
    Argv,
    /// persisted only, because argv was empty. This is the case that was broken.
    Persisted,
    /// persisted ∪ argv, deduplicated, persisted order first.
    Merged,
    /// Neither: no argv and nothing persisted. Genuine standby, honestly earned.
    None,
}

impl HostSource {
    /// The phrase printed after the host list, so a log line answers "why is
    /// nobody waking up" without reading the source.
    pub fn describe(self) -> &'static str {
        match self {
            HostSource::Unhosted => "explicitly unhosted",
            HostSource::Argv => "from argv",
            HostSource::Persisted => "from persisted state",
            HostSource::Merged => "merged from persisted state and argv",
            HostSource::None => "no hosts armed",
        }
    }
}

/// The whole decision, as data. Pure: no clock, no filesystem, no environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub hosts: Vec<Host>,
    pub source: HostSource,
}

/// bd-u54l: the merge. The whole design in one pure function.
///
/// `persisted ∪ argv`, persisted order first, deduplicated, so re-arming with a
/// subset is additive rather than destructive. `--unhost` wins over everything,
/// because it is the only input that is an explicit statement of intent; without
/// that precedence the flag would be advisory, which is the same class of bug.
pub fn resolve(persisted: &[Host], argv: &[Host], unhost: bool) -> Resolution {
    if unhost {
        return Resolution {
            hosts: Vec::new(),
            source: HostSource::Unhosted,
        };
    }
    let mut hosts: Vec<Host> = Vec::new();
    let mut push = |h: &Host| {
        if !hosts.contains(h) {
            hosts.push(h.clone());
        }
    };
    for h in persisted {
        push(h);
    }
    for h in argv {
        push(h);
    }
    let source = match (persisted.is_empty(), argv.is_empty()) {
        (_, true) if !persisted.is_empty() => HostSource::Persisted,
        (true, true) => HostSource::None,
        (true, false) => HostSource::Argv,
        (false, false) => HostSource::Merged,
        // argv empty and persisted empty is already covered above.
        (false, true) => HostSource::Persisted,
    };
    Resolution { hosts, source }
}

/// Read the persisted set.
///
/// A MISSING file is an empty set — first arm, nothing recorded yet. A CORRUPT
/// file is an ERROR, never an empty set. This split is the whole fail-closed
/// argument: treating unreadable state as "no hosts" would reproduce the exact
/// bug this module exists to end, one layer down, and it would do it silently.
/// A corrupt `hosts.json` must stop the arm and say so.
pub fn load(state_dir: &Path) -> anyhow::Result<Vec<Host>> {
    let path = hosts_file(state_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            anyhow::bail!("doorbell: cannot read {}: {e}", path.display());
        }
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!(
            "doorbell: {} is CORRUPT ({e}). Refusing to arm: treating unreadable state as \
             'no hosts' is the bug bd-u54l exists to end. Fix the file or remove it to re-arm \
             from argv.",
            path.display()
        )
    })?;
    let arr = v
        .get("hosts")
        .and_then(|h| h.as_array())
        .ok_or_else(|| anyhow::anyhow!("doorbell: {} has no `hosts` array", path.display()))?;
    let mut out = Vec::new();
    for item in arr {
        let pair = item.as_array().filter(|p| p.len() == 2).ok_or_else(|| {
            anyhow::anyhow!(
                "doorbell: {} has a malformed host entry: {item}",
                path.display()
            )
        })?;
        let project = pair[0].as_str().ok_or_else(|| {
            anyhow::anyhow!("doorbell: {} has a non-string project", path.display())
        })?;
        let workspace = pair[1].as_str().ok_or_else(|| {
            anyhow::anyhow!("doorbell: {} has a non-string workspace", path.display())
        })?;
        let h = (project.to_string(), workspace.to_string());
        if !out.contains(&h) {
            out.push(h);
        }
    }
    Ok(out)
}

/// Persist the armed set atomically: temp file beside the target, then rename
/// over it. Same discipline the journal rotation gained in 92df8f2 — a plain
/// `fs::write` truncates FIRST, so a failed write here would destroy the record
/// of what the daemon is serving, which is the one thing this file exists for.
pub fn persist(state_dir: &Path, hosts: &[Host]) -> anyhow::Result<()> {
    use std::io::Write;
    let path = hosts_file(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::json!({
        "v": 1,
        "hosts": hosts
            .iter()
            .map(|(p, w)| serde_json::json!([p, w]))
            .collect::<Vec<_>>(),
    });
    let mut bytes = serde_json::to_vec_pretty(&body)?;
    bytes.push(b'\n');

    let tmp = path.with_extension("json.tmp");
    let write_private = |p: &Path| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(p)?;
        f.write_all(&bytes)
    };
    if let Err(e) = write_private(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename onto {}", path.display()));
    }
    Ok(())
}

/// The one line an operator needs after an arm. bd-jbz1 was not silent in health
/// output; it was silent in the TRANSCRIPT, where `restart` said "started".
pub fn announce(hosts: &[Host], source: HostSource) {
    use std::fmt::Write as _;
    let mut line = String::new();
    let _ = write!(line, "doorbell: armed on ");
    if hosts.is_empty() {
        let _ = write!(line, "(nothing)");
    } else {
        for (i, (p, w)) in hosts.iter().enumerate() {
            if i > 0 {
                line.push_str(", ");
            }
            let _ = write!(line, "{p}/{w}");
        }
    }
    let _ = write!(line, " ({} {})", hosts.len(), source.describe());
    println!("{line}");
}

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(test)]
mod tests {
    use super::*;

    fn h(p: &str, w: &str) -> Host {
        (p.to_string(), w.to_string())
    }

    /// The case that is actually broken, and the one everyone means when they
    /// type `doorbell restart`. argv empty must NOT become standby.
    #[test]
    fn a_restart_with_no_host_serves_the_persisted_set() {
        let r = resolve(
            &[
                h("flywheel", "flywheel-flywheel"),
                h("toron", "flywheel-toron"),
            ],
            &[],
            false,
        );
        assert_eq!(r.hosts.len(), 2, "argv empty must not unhost: {r:?}");
        assert_eq!(r.source, HostSource::Persisted);
    }

    /// First arm: nothing persisted, argv carries the set.
    #[test]
    fn a_first_arm_takes_argv_and_says_so() {
        let r = resolve(&[], &[h("flywheel", "flywheel-flywheel")], false);
        assert_eq!(r.hosts, vec![h("flywheel", "flywheel-flywheel")]);
        assert_eq!(r.source, HostSource::Argv);
    }

    /// The merge is ADDITIVE and deduplicated, so re-arming with a subset does
    /// not silently drop the others — a destructive merge would be a quieter
    /// version of the same bug.
    #[test]
    fn merging_is_additive_and_deduplicated() {
        let r = resolve(
            &[h("a", "1"), h("b", "2")],
            &[h("b", "2"), h("c", "3")],
            false,
        );
        assert_eq!(
            r.hosts,
            vec![h("a", "1"), h("b", "2"), h("c", "3")],
            "{r:?}"
        );
        assert_eq!(r.source, HostSource::Merged);
    }

    /// `--unhost` is a DECISION, so it wins over both inputs. Without that
    /// precedence the flag would be advisory.
    #[test]
    fn unhost_wins_over_persisted_and_argv_and_persists_as_empty() {
        let r = resolve(&[h("a", "1")], &[h("b", "2")], true);
        assert!(r.hosts.is_empty(), "{r:?}");
        assert_eq!(r.source, HostSource::Unhosted);
        let dir = std::env::temp_dir().join(format!("armstate-unhost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        persist(&dir, &r.hosts).expect("persist");
        assert_eq!(load(&dir).expect("load"), Vec::<Host>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Genuine standby, honestly earned: neither input present and no explicit
    /// verb. Distinguished from `Persisted` because they are different situations
    /// and an operator reading the log should be able to tell them apart.
    #[test]
    fn nothing_persisted_and_no_argv_is_none_not_persisted() {
        let r = resolve(&[], &[], false);
        assert!(r.hosts.is_empty());
        assert_eq!(r.source, HostSource::None);
    }

    /// THE FAIL-CLOSED CASE, and the reason `load` is not `unwrap_or_default`.
    /// Treating a corrupt file as "no hosts" would reproduce the very bug this
    /// module ends, one layer down, and silently.
    #[test]
    fn a_corrupt_hosts_file_is_an_error_never_an_empty_set() {
        let dir = std::env::temp_dir().join(format!("armstate-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dirs");
        std::fs::write(hosts_file(&dir), "{ not json").expect("write");
        let err = load(&dir).expect_err("a corrupt file must NOT read as zero hosts");
        let msg = err.to_string();
        assert!(
            msg.contains("CORRUPT"),
            "the error must name the condition: {msg}"
        );
        // A missing file is different: that is a first arm, not corruption.
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            load(&dir).expect("missing reads as empty"),
            Vec::<Host>::new()
        );
    }

    /// Round trip through the real file, including the atomic replace, so the
    /// persisted shape is pinned rather than assumed.
    #[test]
    fn hosts_round_trip_through_the_state_file() {
        let dir = std::env::temp_dir().join(format!("armstate-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let want = vec![
            h("flywheel", "flywheel-flywheel"),
            h("toron", "flywheel-toron"),
        ];
        persist(&dir, &want).expect("persist");
        assert_eq!(load(&dir).expect("load"), want);
        // Rewriting to a shorter set must not leave the old entries behind — a
        // failed truncate would resurrect hosts the operator just unhosted.
        persist(&dir, &want[..1]).expect("re-persist");
        assert_eq!(
            load(&dir).expect("reload"),
            vec![h("flywheel", "flywheel-flywheel")]
        );
        // No temp file left behind by the atomic replace.
        assert!(
            !path_exists(&dir.join("hosts.json.tmp")),
            "the temp file must be renamed, not left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn path_exists(p: &Path) -> bool {
        p.exists()
    }

    /// A project key can be an ABSOLUTE PATH (`/tmp/scratch/proj`), which is
    /// why `parse_host` splits on the last `/`. This pins that the persisted
    /// round trip does not mangle such a pair.
    #[test]
    fn an_absolute_path_project_survives_the_round_trip() {
        let dir = std::env::temp_dir().join(format!("armstate-abs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let want = vec![h("/tmp/scratch/proj", "ws")];
        persist(&dir, &want).expect("persist");
        assert_eq!(load(&dir).expect("load"), want);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
