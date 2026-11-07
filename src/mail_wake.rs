// governed-by: ADR-0012 (doorbell), ADR-0041 (one inbox door)
//! NIP-17 mail wake for the per-workspace doorbell host.
//!
//! The comms `inbox_host` rings the envelope log (`toron next`). A NIP-17
//! `toron mail` DM used to wake only through the agent's peer handler, which
//! is a one-turn prompt: once the pane finished (`done`), new mail sat unread
//! until a human sent a pane prompt. This host is the missing door: on a
//! short poll it reads each registered pin's unread NIP-17 inbox, and when a
//! row is newer than the persisted high-water it prompts the pin's pane with
//! a short mail notice. The agent then runs `toron mail inbox` and acts.
//!
//! Effects: the poll is a std thread (never a Tokio runtime), the mail call
//! is the one `MailSurface` (no CLI shell-out), and the pane prompt is the one
//! plane backend. The high-water is persisted per project/workspace so a
//! restart does not re-ring the same mail, and a failed prompt does not
//! advance it (the next poll retries).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::backend::{InboxQuery, MailItem};

/// Fallback poll interval when the env override is absent.
const DEFAULT_INTERVAL_MS: u64 = 5_000;
/// Do not retry a failed prompt for the same pin inside this window.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// The row the wake decision needs, independent of the wire type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailRow {
    pub created_ts_us: i64,
    pub sender: String,
    pub subject: String,
    pub importance: String,
}

/// The newest unread row strictly newer than `high_water`.
pub fn newest_unseen(rows: &[MailRow], high_water: i64) -> Option<&MailRow> {
    rows.iter()
        .filter(|r| r.created_ts_us > high_water)
        .max_by_key(|r| r.created_ts_us)
}

/// The one pane prompt for a new mail. It names the pin and the exact door so
/// the agent never has to infer the mailbox.
pub fn wake_text(project: &str, pin: &str, row: &MailRow) -> String {
    let urgent = matches!(row.importance.as_str(), "high" | "urgent");
    let subject: String = row.subject.chars().take(120).collect();
    format!(
        "{}Doorbell: mail from {} — {}. Run `toron mail inbox --project {} --as {} --limit 5` and act.",
        if urgent { "URGENT " } else { "" },
        row.sender,
        subject,
        project,
        pin
    )
}

/// Per project+workspace high-water, by pin. `created_ts_us` is the mail time,
/// not the read time: the agent's read does not change it, so a prompt is
/// sent once per message even if the agent has not marked it read.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct HighWater(BTreeMap<String, i64>);

fn high_water_path(state_dir: &Path, project: &str, workspace: &str) -> PathBuf {
    state_dir.join("doorbell").join(format!(
        "mail-wake-{}-{}.json",
        crate::state::state_key(project),
        crate::state::state_key(workspace)
    ))
}

fn load_high_water(path: &Path) -> HighWater {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_high_water(path: &Path, hw: &HighWater) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(body) = serde_json::to_vec_pretty(hw) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The running mail-wake host. Dropping it stops the thread (process exit also
/// stops it).
pub struct MailWakeHost {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl MailWakeHost {
    /// Start the mail-wake thread for one workspace.
    pub fn spawn(cfg: crate::config::Config, project: String, workspace: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let st = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("flywheel-mail-wake".into())
            .spawn(move || run(cfg, project, workspace, st))
            .ok();
        Self { stop, join }
    }
}

impl Drop for MailWakeHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn interval() -> Duration {
    let ms = std::env::var("FLYWHEEL_MAIL_WAKE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_INTERVAL_MS);
    Duration::from_millis(ms)
}

fn run(cfg: crate::config::Config, project: String, workspace: String, stop: Arc<AtomicBool>) {
    let path = high_water_path(&crate::config::state_dir(), &project, &workspace);
    let mut high_water = load_high_water(&path);
    let mut last_try: BTreeMap<String, Instant> = BTreeMap::new();
    // Let the panes finish their boot before the first poll.
    std::thread::sleep(Duration::from_secs(2));
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        poll_once(
            &cfg,
            &project,
            &workspace,
            &path,
            &mut high_water,
            &mut last_try,
        );
        std::thread::sleep(interval());
    }
}

fn poll_once(
    cfg: &crate::config::Config,
    project: &str,
    workspace: &str,
    path: &Path,
    high_water: &mut HighWater,
    last_try: &mut BTreeMap<String, Instant>,
) {
    let registry = crate::state::load_spawn_registry(project);
    if registry.is_empty() {
        return;
    }
    let captain = crate::state::load_captain_alias(project);
    let Ok(backend) = crate::backend::multiplexer_backend(cfg) else {
        return;
    };
    let pane_ids: std::collections::BTreeSet<String> = backend
        .pane_list(Some(workspace))
        .map(|panes| panes.into_iter().map(|p| p.pane_id).collect())
        .unwrap_or_default();
    let Ok(mail) = crate::backend::mail_backend(cfg) else {
        return;
    };
    let project_key = crate::project_dir_str(cfg, project).unwrap_or_else(|_| project.to_string());
    let mut changed = false;
    for entry in &registry {
        if stop_pin(entry, captain.as_deref(), &pane_ids) {
            continue;
        }
        let pin = entry.mail_identity.clone().unwrap_or_default();
        if pin.is_empty() || entry.pane_id.is_empty() {
            continue;
        }
        let high = high_water.0.get(&pin).copied().unwrap_or(0);
        if let Some(at) = last_try.get(&pin)
            && at.elapsed() < RETRY_AFTER
        {
            continue;
        }
        let query = InboxQuery {
            project: project_key.clone(),
            agent: pin.clone(),
            unread_only: true,
            limit: Some(10),
            include_bodies: false,
            urgent_only: false,
            since_ts: None,
            thread_id: None,
        };
        let Ok(items) = crate::backend::run_mail(mail.inbox(&query)) else {
            continue;
        };
        let rows: Vec<MailRow> = items.into_iter().map(row_of).collect();
        let Some(row) = newest_unseen(&rows, high) else {
            continue;
        };
        let text = wake_text(project, &pin, row);
        match backend.prompt(&entry.pane_id, &text, None) {
            Ok(_) => {
                high_water.0.insert(pin.clone(), row.created_ts_us);
                last_try.remove(&pin);
                changed = true;
            }
            Err(err) => {
                last_try.insert(pin.clone(), Instant::now());
                eprintln!(
                    "flywheel mail-wake: {} ({}) not promptable ({err}) — retry later",
                    pin, entry.pane_id
                );
            }
        }
    }
    if changed {
        save_high_water(path, high_water);
    }
}

fn stop_pin(
    entry: &crate::state::SpawnEntry,
    captain: Option<&str>,
    pane_ids: &std::collections::BTreeSet<String>,
) -> bool {
    let Some(pin) = entry.mail_identity.as_deref() else {
        return true;
    };
    if pin.is_empty() || entry.pane_id.is_empty() {
        return true;
    }
    // The operator/captain pane is never woken by the mail host.
    if captain == Some(pin) {
        return true;
    }
    // A registry may retain panes from an earlier session; only wake the
    // panes that belong to THIS workspace when the plane answered.
    if !pane_ids.is_empty() && !pane_ids.contains(&entry.pane_id) {
        return true;
    }
    false
}

fn row_of(item: MailItem) -> MailRow {
    MailRow {
        created_ts_us: item.created_ts_us,
        sender: item.sender,
        subject: item.subject,
        importance: item.importance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(created_ts_us: i64, sender: &str, subject: &str, importance: &str) -> MailRow {
        MailRow {
            created_ts_us,
            sender: sender.into(),
            subject: subject.into(),
            importance: importance.into(),
        }
    }

    #[test]
    fn newest_unseen_is_strictly_above_the_high_water() {
        let rows = vec![
            row(10, "A", "old", "normal"),
            row(20, "B", "new", "normal"),
            row(15, "C", "middle", "normal"),
        ];
        assert_eq!(newest_unseen(&rows, 0).map(|r| r.created_ts_us), Some(20));
        assert_eq!(newest_unseen(&rows, 20).map(|r| r.created_ts_us), None);
        assert_eq!(newest_unseen(&rows, 16).map(|r| r.created_ts_us), Some(20));
    }

    #[test]
    fn urgent_text_leads_and_names_the_exact_door() {
        let text = wake_text(
            "toron",
            "TopazKnoll",
            &row(1, "CloudySparrow", "hi", "urgent"),
        );
        assert!(text.starts_with("URGENT "), "{text}");
        assert!(text.contains("toron mail inbox --project toron --as TopazKnoll"));
        assert!(text.contains("CloudySparrow"));
    }

    #[test]
    fn normal_text_is_not_urgent() {
        let text = wake_text("toron", "TopazKnoll", &row(1, "a", "b", "normal"));
        assert!(!text.starts_with("URGENT "), "{text}");
    }

    /// The per-workspace host must start this poller; a future refactor that
    /// removes the spawn would silently restore the old "mail waits for a
    /// human prompt" behavior.
    #[test]
    fn the_embedded_host_starts_the_mail_wake() {
        let src = include_str!("doorbell_host.rs");
        assert!(
            src.contains("MailWakeHost::spawn"),
            "doorbell_host must start the NIP-17 mail wake"
        );
    }
}
