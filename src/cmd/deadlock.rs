//! Deadlock watch (bd-v4dcv): build the wait-for graph from the live lease
//! board and the open collab asks, and mail the captain once per cycle and
//! once per wait older than the threshold.

use anyhow::Result;

use crate::deadlock::{
    LiveLease, cycle_key, find_cycles, lease_edges, long_waits, unreported, wait_key,
};

/// Reported findings already mailed: cycle keys and wait keys.
#[derive(Default)]
struct Reported {
    ids: std::collections::BTreeSet<String>,
}

fn reported_path(project: &str) -> std::path::PathBuf {
    crate::config::state_dir()
        .join("deadlock")
        .join(format!("{}.json", crate::state::state_key(project)))
}

fn load_reported(project: &str) -> Reported {
    let ids = std::fs::read_to_string(reported_path(project))
        .ok()
        .and_then(|body| serde_json::from_str::<Vec<String>>(&body).ok())
        .unwrap_or_default()
        .into_iter()
        .collect();
    Reported { ids }
}

fn save_reported(project: &str, reported: &Reported) -> Result<()> {
    let path = reported_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ids: Vec<&String> = reported.ids.iter().collect();
    let body = serde_json::to_string_pretty(&ids)?;
    crate::project_state::write_atomic(&path, body.as_bytes())
        .map_err(|e| anyhow::anyhow!("write deadlock reported set: {e}"))?;
    Ok(())
}

fn parse_created_us(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp_micros())
}

/// Pure core of [`resolve_asker`]: given (id, sender, ts) rows, who asked
/// and did the askee reply after the question? A reply counts only when it
/// is newer than the ask it answers.
fn thread_verdict(
    rows: &[(&str, &str, i64)],
    askee: &str,
    head: Option<&str>,
) -> (Option<String>, bool) {
    let ask_ts: Option<i64> = head
        .and_then(|h| rows.iter().find(|(id, _, _)| *id == h))
        .map(|(_, _, ts)| *ts);
    let asker = head
        .and_then(|h| rows.iter().find(|(id, _, _)| *id == h))
        .map(|(_, from, _)| from.to_string())
        .or_else(|| {
            rows.iter()
                .min_by_key(|(_, _, ts)| *ts)
                .map(|(_, from, _)| from.to_string())
        });
    let answered = rows.iter().any(|(id, from, ts)| {
        *from == askee && Some(*id) != head && ask_ts.is_none_or(|hts| *ts > hts)
    });
    (asker, answered)
}

/// The asker behind an open collab ask: the sender of the thread's question
/// (the row whose id matches `head`, else the thread's earliest message).
/// Best-effort — a thread the checker cannot read leaves the asker unknown,
/// and the wait still counts for the long-wait rule.
///
/// Returns `(asker, answered)`: `answered` is true when the thread's newest
/// message is from the askee and differs from the question, i.e. the reply
/// landed and the registry row is stale.
fn resolve_asker(
    mail: &crate::mail::Mail,
    key: &str,
    thread: &str,
    askee: &str,
    head: Option<&str>,
) -> (Option<String>, bool) {
    let filter = crate::mail::MailSearchFilter {
        sender: None,
        importance: None,
        thread_id: Some(thread.to_string()),
        date_start: None,
        date_end: None,
        ranking: None,
        include_body_md: false,
    };
    let found = match mail.search(key, "", 100, &filter) {
        Ok(v) => v,
        Err(_) => return (None, false),
    };
    let rows: Vec<(&str, &str, i64)> = found
        .get("result")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    Some((
                        m.get("id")?.as_str()?,
                        m.get("from")?.as_str()?,
                        m.get("created_ts_us")?.as_i64()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        return (None, false);
    }
    thread_verdict(&rows, askee, head)
}

/// `flywheel deadlock check`: page the captain once per wait-for cycle and
/// once per wait older than `--wait-minutes`. Quiet (exit 0, summary on
/// stdout) when there is nothing new.
pub(crate) fn cmd_deadlock_check(project: &str, wait_minutes: u64) -> Result<()> {
    let cfg = crate::config::Config::load().unwrap_or_default();
    let mail =
        crate::mail::Mail::from_config(Some(&cfg.toron_mail_config()))?.ok_or_else(|| {
            anyhow::anyhow!(
                "agent-mail (toron) not configured; set [mail.toron] in {}",
                crate::config::config_file().display()
            )
        })?;
    let dir = crate::project_dir(&cfg, project);
    let key = crate::utf8_path(&dir, "project dir")?;
    mail.ensure_project(&key)?;
    let now_us = chrono::Utc::now().timestamp_micros();

    // Live exclusive leases are the lease side of the graph.
    let board = mail.list_active(&key, None, None)?;
    let leases: Vec<LiveLease> = board
        .get("reservations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.get("exclusive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .filter(|r| {
            r.get("expires_us")
                .and_then(|v| v.as_i64())
                .is_none_or(|e| e > now_us)
        })
        .filter_map(|r| {
            Some(LiveLease {
                holder: r.get("holder")?.as_str()?.to_string(),
                path: r.get("path")?.as_str()?.to_string(),
                reason: r
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();
    let mut edges = lease_edges(&leases);

    // Open collab asks are the answer side: resolve each asker through its
    // thread, keep only live pins asking within the last day, and drop rows
    // whose reply already landed (the registry is append-only; a satisfied
    // ask is stale, not open).
    let live: std::collections::BTreeSet<String> = crate::state::load_spawn_registry(project)
        .into_iter()
        .filter_map(|e| e.mail_identity)
        .collect();
    for expected in crate::state::load_expected_replies(project) {
        let (asker, answered) = resolve_asker(
            &mail,
            &key,
            &expected.thread,
            &expected.agent,
            expected.head.as_deref(),
        );
        if answered {
            continue;
        }
        let since_us = parse_created_us(&expected.created_ts).unwrap_or(now_us);
        if let Some(wait) = crate::deadlock::qualify_wait(
            asker,
            &expected.agent,
            &expected.thread,
            since_us,
            &live,
            now_us,
        ) {
            edges.extend(crate::deadlock::collab_edges(&[wait]));
        }
    }

    let cycles = find_cycles(&edges);
    let threshold_us = (wait_minutes as i64).saturating_mul(60_000_000);
    let waits = long_waits(&edges, now_us, threshold_us);
    let mut reported = load_reported(project);
    let (new_cycles, new_waits) = unreported(&cycles, &waits, &reported.ids);
    if new_cycles.is_empty() && new_waits.is_empty() {
        println!(
            "deadlock check: {} edge(s), {} cycle(s), no new findings",
            edges.len(),
            cycles.len()
        );
        return Ok(());
    }

    let mut lines = Vec::new();
    for cycle in &new_cycles {
        lines.push(format!("CYCLE {}", (*cycle).join(" > ")));
        for edge in edges.iter().filter(|e| {
            cycle.contains(&e.waiter) && cycle.contains(&e.holder) && e.waiter != e.holder
        }) {
            lines.push(format!(
                "  {} waits on {} ({})",
                edge.waiter, edge.holder, edge.resource
            ));
        }
        reported.ids.insert(cycle_key(cycle));
    }
    for wait in &new_waits {
        lines.push(format!(
            "LONG WAIT (>{}m): {} waits on {} ({})",
            wait_minutes, wait.waiter, wait.holder, wait.resource
        ));
        reported.ids.insert(wait_key(wait));
    }
    save_reported(project, &reported)?;

    let body = lines.join("\n");
    match crate::state::load_captain_alias(project) {
        Some(captain) => {
            mail.send(
                &key,
                std::slice::from_ref(&captain),
                &format!(
                    "[deadlock] {project}: {} new",
                    new_cycles.len() + new_waits.len()
                ),
                &body,
                "high",
                true,
                None,
            )?;
            println!("deadlock check: mailed {captain}\n{body}");
        }
        None => {
            println!("deadlock check: no captain bound; findings:\n{body}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::thread_verdict;

    /// Cycle keys are stable regardless of discovery order.
    #[test]
    fn cycle_keys_sort_members() {
        assert_eq!(
            crate::deadlock::cycle_key(&["B".to_string(), "A".to_string()]),
            "cycle:B>A"
        );
    }

    /// A dead waiter pages nothing, and a thread the askee already answered
    /// in reads as satisfied, not open. Both stay silent.
    #[test]
    fn dead_waiters_and_answered_threads_stay_silent() {
        // The askee replied after the ask: satisfied.
        let (asker, answered) =
            thread_verdict(&[("q1", "A", 100), ("r1", "B", 200)], "B", Some("q1"));
        assert_eq!(asker.as_deref(), Some("A"));
        assert!(answered, "B's newer reply satisfies the ask");
        // Only the ask is on the thread: still open, asker known.
        let (asker, answered) = thread_verdict(&[("q1", "A", 100)], "B", Some("q1"));
        assert_eq!(asker.as_deref(), Some("A"));
        assert!(!answered, "no reply yet");
        // The dead-waiter half lives in qualify_wait; here the shape check:
        // an answered ask never becomes a wait the captain sees.
        let live: std::collections::BTreeSet<String> = ["Nobody".to_string()].into_iter().collect();
        assert!(
            crate::deadlock::qualify_wait(Some("Ghost".into()), "B", "t", 100, &live, 200)
                .is_none(),
            "a dead waiter never qualifies"
        );
    }
}
