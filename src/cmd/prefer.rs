//! `flywheel prefer` (ADR-0022 D6): set, list, and forget standing
//! preferences. The store is pure; the file and the Chie lesson are edges.

use anyhow::Result;

use crate::output::print_json;
use crate::preferences::{PrefKey, PreferenceSet, Scope, preferences_path};

/// `flywheel prefer`: exactly one of an assignment, `--list`, or `--forget`.
pub(crate) fn cmd_prefer(
    assignment: Option<&str>,
    list: bool,
    forget: Option<&str>,
    json: bool,
    project: Option<&str>,
) -> Result<()> {
    cmd_prefer_at(
        &preferences_path(),
        &|msg| crate::cmd::memory::record_lesson(msg, Some("preference"), None),
        assignment,
        list,
        forget,
        json,
        project,
    )
    .map(|_| ())
}

/// The same verb against an explicit store path with the lesson edge
/// injected — the seam CLI tests use so they never touch the process env
/// or the operator's config. Returns the lesson outcome for the set path;
/// list and forget record nothing and return `None`.
fn cmd_prefer_at(
    path: &std::path::Path,
    record: &dyn Fn(&str) -> bool,
    assignment: Option<&str>,
    list: bool,
    forget: Option<&str>,
    json: bool,
    project: Option<&str>,
) -> Result<Option<bool>> {
    let modes = [assignment.is_some(), list, forget.is_some()]
        .into_iter()
        .filter(|b| *b)
        .count();
    if modes != 1 {
        anyhow::bail!("prefer takes exactly one of ASSIGNMENT, --list, --forget <role.field>");
    }
    let mut set = PreferenceSet::load(path)?;
    if let Some(key) = forget {
        let key: PrefKey = key.parse()?;
        let scope = match project {
            Some(p) => Scope::Project(p.to_string()),
            None => Scope::Always,
        };
        if set.forget(&key, project) {
            set.save(path)?;
        }
        if json {
            print_json(&serde_json::json!({
                "ok": true, "key": key.to_string(), "scope": scope.to_string(),
            }));
        } else {
            println!("forgot {key} ({scope})");
        }
        return Ok(None);
    }
    if list {
        let rows = set.list();
        if json {
            print_json(&serde_json::Value::Array(
                rows.iter()
                    .map(|p| {
                        serde_json::json!({
                            "key": p.key.to_string(),
                            "value": p.value,
                            "scope": p.scope.to_string(),
                        })
                    })
                    .collect(),
            ));
        } else {
            for p in &rows {
                println!("{}={} ({})", p.key, p.value, p.scope);
            }
        }
        return Ok(None);
    }
    let assignment = assignment.expect("assignment mode checked above");
    let (raw_key, value) = assignment.split_once('=').ok_or_else(|| {
        anyhow::anyhow!("prefer assignment must be ROLE.FIELD=VALUE (got {assignment:?})")
    })?;
    if value.is_empty() {
        anyhow::bail!("prefer assignment needs a non-empty value (got {assignment:?})");
    }
    let key: PrefKey = raw_key.parse()?;
    let scope = match project {
        Some(p) => Scope::Project(p.to_string()),
        None => Scope::Always,
    };
    let previous = set
        .lookup(&key.role, key.field, project)
        .map(str::to_string);
    set.set(key.clone(), value.to_string(), scope.clone());
    set.save(path)?;
    let recorded = record(&format!("operator prefers {key}={value} ({scope})"));
    if !recorded {
        eprintln!(
            "flywheel: memory record unavailable (chie offline?) — preference kept, lesson not recorded"
        );
    }
    if json {
        print_json(&serde_json::json!({
            "ok": true, "key": key.to_string(), "value": value, "scope": scope.to_string(),
            "recorded": recorded, "previous": previous,
        }));
    } else if let Some(old) = previous.filter(|old| old != value) {
        println!("preferred {key}={value} ({scope}, replaced {old})");
    } else {
        println!("preferred {key}={value} ({scope})");
    }
    Ok(Some(recorded))
}

#[cfg(test)]
mod prefer_cli_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    /// Unique store path per test — no HOME/XDG mutation, so CLI tests
    /// never touch the operator's config and never race other suites.
    fn store(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fw-prefer-cli-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("preferences.toml")
    }

    /// Set writes the file and lookup finds it; the lesson edge reports true.
    #[test]
    fn prefer_set_persists_and_lookup_finds_it() {
        let path = store("set");
        let recorded = cmd_prefer_at(
            &path,
            &|_| true,
            Some("reviewer.harness=grok"),
            false,
            None,
            false,
            None,
        )
        .expect("set works");
        assert_eq!(recorded, Some(true));
        let back = PreferenceSet::load(&path).expect("loads");
        assert_eq!(
            back.lookup("reviewer", crate::preferences::Field::Harness, None),
            Some("grok")
        );
    }

    /// Project scope lands in the project tier and lists stably.
    #[test]
    fn prefer_set_project_scope_and_list() {
        let path = store("proj");
        let recorded = cmd_prefer_at(
            &path,
            &|_| true,
            Some("coder.model=zai/glm-5.3"),
            false,
            None,
            false,
            Some("flywheel"),
        )
        .expect("project set works");
        assert_eq!(recorded, Some(true));
        cmd_prefer_at(&path, &|_| true, None, true, None, true, None).expect("list works");
        let back = PreferenceSet::load(&path).expect("loads");
        assert_eq!(
            back.lookup("coder", crate::preferences::Field::Model, Some("flywheel")),
            Some("zai/glm-5.3")
        );
        assert_eq!(
            back.lookup("coder", crate::preferences::Field::Model, None),
            None
        );
    }

    /// Forget removes only the named scope.
    #[test]
    fn prefer_forget_removes_only_the_named_scope() {
        let path = store("forget");
        cmd_prefer_at(
            &path,
            &|_| true,
            Some("coder.model=a"),
            false,
            None,
            false,
            None,
        )
        .expect("set");
        cmd_prefer_at(
            &path,
            &|_| true,
            Some("coder.model=b"),
            false,
            None,
            false,
            Some("flywheel"),
        )
        .expect("project set");
        cmd_prefer_at(
            &path,
            &|_| true,
            None,
            false,
            Some("coder.model"),
            false,
            Some("flywheel"),
        )
        .expect("forget works");
        let back = PreferenceSet::load(&path).expect("loads");
        assert_eq!(
            back.lookup("coder", crate::preferences::Field::Model, Some("flywheel")),
            Some("a"),
            "always row shows through"
        );
        cmd_prefer_at(
            &path,
            &|_| true,
            None,
            false,
            Some("coder.model"),
            false,
            None,
        )
        .expect("forget");
        let back = PreferenceSet::load(&path).expect("loads");
        assert_eq!(
            back.lookup("coder", crate::preferences::Field::Model, Some("flywheel")),
            None
        );
    }

    /// A missing Chie warns but keeps the preference.
    #[test]
    fn prefer_set_keeps_preference_when_chie_missing() {
        let path = store("offline");
        let recorded = cmd_prefer_at(
            &path,
            &|_| false,
            Some("coder.effort=high"),
            false,
            None,
            false,
            None,
        )
        .expect("set works offline");
        assert_eq!(recorded, Some(false));
        let back = PreferenceSet::load(&path).expect("loads");
        assert_eq!(
            back.lookup("coder", crate::preferences::Field::Effort, None),
            Some("high")
        );
    }

    /// Bad assignments and empty invocations fail loud with usage.
    #[test]
    fn prefer_rejects_bad_assignment_and_empty_invocation() {
        let path = store("bad");
        cmd_prefer_at(
            &path,
            &|_| true,
            Some("reviewer.harness"),
            false,
            None,
            false,
            None,
        )
        .expect_err("missing = fails");
        cmd_prefer_at(
            &path,
            &|_| true,
            Some("reviewer.bogus=x"),
            false,
            None,
            false,
            None,
        )
        .expect_err("unknown field fails");
        cmd_prefer_at(
            &path,
            &|_| true,
            Some("reviewer.harness="),
            false,
            None,
            false,
            None,
        )
        .expect_err("empty value fails");
        cmd_prefer_at(&path, &|_| true, None, false, None, false, None)
            .expect_err("empty invocation fails");
        cmd_prefer_at(
            &path,
            &|_| true,
            None,
            true,
            Some("coder.model"),
            false,
            None,
        )
        .expect_err("two modes fail");
    }
}
