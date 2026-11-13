// governed-by: ADR-0012 (doorbell), ADR-0041 (one inbox door)
//! NIP-17 mail wake for the per-workspace doorbell host.
//!
//! ADR-0041 says one inbox door. While comms is enabled the envelope host
//! (`toron next` cursor) owns rings; this poller is the door only when comms
//! is off. It reads each registered pin's unread NIP-17 inbox and prompts the
//! pane only for agent mail past the persisted high-water. System notices
//! (reservation-revoked, ack/read tags) stay readable in the inbox and never
//! ring; a fresh pin's backlog is seeded into the high-water, never replayed.
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
    pub id: String,
    pub created_ts_us: i64,
    pub sender: String,
    pub subject: String,
    /// `toron-kind` when the wire carries it; `None` for ordinary mail.
    pub kind: Option<String>,
    pub importance: String,
}

/// What one poll should do for one pin, from the row set and the persisted
/// high-water. `Seed` means "first sight": record the tip and stay quiet, so a
/// fresh pin is never rung for mail that predates its wake host.
#[derive(Debug, PartialEq, Eq)]
pub enum WakePlan<'a> {
    Seed(i64),
    Ring(&'a MailRow),
    Idle,
}

/// `toron-kind`-tagged rows are system notices (reservation-revoked, ack,
/// read, ...): readable in the inbox, never a pane wake. The surface DTO at
/// toron ca87ff5c drops `toron_kind`, so the production row is recognised by
/// its throwaway hex sender and empty subject instead.
pub fn is_system_notice(row: &MailRow) -> bool {
    row.kind.is_some() || (row.subject.trim().is_empty() && looks_like_key(&row.sender))
}

fn looks_like_key(sender: &str) -> bool {
    let s = sender.trim();
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
        || (s.starts_with("npub1") && s.len() > 20)
}

/// The newest unread row that is safe to ring for.
pub fn newest_ringable(rows: &[MailRow], high_water: i64) -> Option<&MailRow> {
    rows.iter()
        .filter(|r| r.created_ts_us > high_water && !is_system_notice(r))
        .max_by_key(|r| r.created_ts_us)
}

/// The tip to seed a fresh pin with: every row already present is
/// pre-registration backlog and must not ring.
fn seed_watermark(rows: &[MailRow]) -> i64 {
    rows.iter().map(|r| r.created_ts_us).max().unwrap_or(0)
}

/// The whole per-pin poll decision: seed on first sight, ring only for a
/// ringable row past the persisted high-water, else stay idle.
pub fn plan_wake(stored: Option<i64>, rows: &[MailRow]) -> WakePlan<'_> {
    let Some(stored) = stored else {
        return WakePlan::Seed(seed_watermark(rows));
    };
    match newest_ringable(rows, stored) {
        Some(row) => WakePlan::Ring(row),
        None => WakePlan::Idle,
    }
}

/// The one wake door (ADR-0041): the comms envelope host owns wakes while
/// comms is enabled; the NIP-17 mail wake is the door only when it is not.
pub fn mail_wake_owns(comms_enabled: bool) -> bool {
    !comms_enabled
}

fn display_sender(row: &MailRow) -> String {
    if looks_like_key(&row.sender) {
        row.kind
            .clone()
            .unwrap_or_else(|| "system notice".to_string())
    } else {
        row.sender.clone()
    }
}

fn display_subject(row: &MailRow) -> String {
    let subject = row.subject.trim();
    if subject.is_empty() {
        row.kind
            .clone()
            .unwrap_or_else(|| "(no subject)".to_string())
    } else {
        subject.chars().take(120).collect()
    }
}

/// The one pane prompt for a new mail. It names the pin and the exact door so
/// the agent never has to infer the mailbox.
pub fn wake_text(project: &str, pin: &str, row: &MailRow) -> String {
    let urgent = matches!(row.importance.as_str(), "high" | "urgent");
    format!(
        "{}Doorbell: mail from {} — {}. Run `toron mail inbox --project {} --as {} --limit 5` and act.",
        if urgent { "URGENT " } else { "" },
        display_sender(row),
        display_subject(row),
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
    let captain_alias = crate::state::load_captain_alias(project);
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
    // The flywheel mirror may be absent; toron's own `captain` alias is the
    // authoritative one. Both are read so the operator's pane is never typed
    // into (and never retried as a busy pane).
    let captain = captain_alias
        .or_else(|| toron_captain(project))
        .or_else(|| {
            crate::backend::run_mail(mail.whois(&project_key, "captain"))
                .ok()
                .map(|w| w.name)
        });
    let mut changed = false;
    for entry in &registry {
        if stop_pin(entry, captain.as_deref(), &pane_ids) {
            continue;
        }
        let pin = entry.mail_identity.clone().unwrap_or_default();
        if pin.is_empty() || entry.pane_id.is_empty() {
            continue;
        }
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
        let row = match plan_wake(high_water.0.get(&pin).copied(), &rows) {
            WakePlan::Seed(tip) => {
                high_water.0.insert(pin.clone(), tip);
                changed = true;
                continue;
            }
            WakePlan::Idle => continue,
            WakePlan::Ring(row) => row,
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

/// The authoritative captain alias lives in toron's contact table; the
/// flywheel mirror is only written by some doors (not by `toron contact
/// captain`). Read the one CLI door so the operator pane is always skipped.
fn toron_captain(project: &str) -> Option<String> {
    let out = std::process::Command::new("toron")
        .args(["contact", "captain", "--project", project])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("captain")
        .and_then(|c| c.as_str())
        .map(str::to_string)
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
        id: item.id.to_string(),
        created_ts_us: item.created_ts_us,
        sender: item.sender,
        subject: item.subject,
        // The surface DTO at toron ca87ff5c carries no `toron_kind`; the
        // notice classifier falls back to the throwaway-sender signature.
        kind: None,
        importance: item.importance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(created_ts_us: i64, sender: &str, subject: &str, importance: &str) -> MailRow {
        row_full("m", created_ts_us, sender, subject, None, importance)
    }

    fn row_full(
        id: &str,
        created_ts_us: i64,
        sender: &str,
        subject: &str,
        kind: Option<&str>,
        importance: &str,
    ) -> MailRow {
        MailRow {
            id: id.into(),
            created_ts_us,
            sender: sender.into(),
            subject: subject.into(),
            kind: kind.map(str::to_string),
            importance: importance.into(),
        }
    }

    const HASH: &str = "66f8ee6b23714f24db7682b2d741e7ae0c0f8ff609dbafb2f7e706d3b1869f16";

    /// bd-ohxou: a reservation-revoked notice never rings a pane.
    #[test]
    fn reservation_revoked_notice_never_rings() {
        let r = row_full("n1", 10, HASH, "", Some("reservation-revoked"), "normal");
        assert!(is_system_notice(&r));
        assert!(plan_wake(None, std::slice::from_ref(&r)) == WakePlan::Seed(10));
        assert!(plan_wake(Some(0), std::slice::from_ref(&r)) == WakePlan::Idle);
        assert!(newest_ringable(std::slice::from_ref(&r), 0).is_none());
    }

    /// The production wire carries no toron-kind, so the same notice is
    /// recognised by its throwaway hex sender and empty subject.
    #[test]
    fn raw_key_notice_with_empty_subject_never_rings() {
        let r = row_full("n2", 10, HASH, "", None, "normal");
        assert!(is_system_notice(&r));
        assert!(plan_wake(Some(0), std::slice::from_ref(&r)) == WakePlan::Idle);
    }

    /// Agent mail rings, and the wake names the sender pin and subject.
    #[test]
    fn agent_mail_rings_with_pin_and_subject() {
        let r = row_full("m1", 20, "SlateHarbor", "Brief: slice 2", None, "normal");
        assert!(!is_system_notice(&r));
        assert!(matches!(
            plan_wake(Some(10), std::slice::from_ref(&r)),
            WakePlan::Ring(row) if row.id == "m1"
        ));
        let text = wake_text("flywheel", "PurpleGorge", &r);
        assert!(text.contains("SlateHarbor"), "{text}");
        assert!(text.contains("Brief: slice 2"), "{text}");
    }

    /// A ringing row with no subject renders its kind, never an empty dash.
    #[test]
    fn empty_subject_renders_the_kind() {
        let r = row_full("m2", 30, "SlateHarbor", "", Some("notice"), "normal");
        let text = wake_text("flywheel", "PurpleGorge", &r);
        assert!(text.contains("notice"), "{text}");
        assert!(!text.contains("— ."), "{text}");
    }

    /// A raw hex sender never leaks into the pane prompt.
    #[test]
    fn wake_text_never_prints_a_raw_hex_sender() {
        let r = row_full("n3", 40, HASH, "ping", Some("ack"), "normal");
        let text = wake_text("flywheel", "PurpleGorge", &r);
        assert!(!text.contains(HASH), "{text}");
    }

    /// bd-4xthm: exactly one door owns a wake; the envelope host and the mail
    /// host never both ring.
    #[test]
    fn exactly_one_wake_door_owns() {
        assert!(mail_wake_owns(false));
        assert!(!mail_wake_owns(true));
    }

    /// A fresh pin with a backlog is seeded to the tip, not rung.
    #[test]
    fn fresh_pin_backlog_is_seeded_not_rung() {
        let rows = vec![
            row_full("old1", 10, "Someone", "old", None, "normal"),
            row_full("old2", 20, "Someone", "older", None, "normal"),
        ];
        assert_eq!(plan_wake(None, &rows), WakePlan::Seed(20));
        let fresh = vec![
            row_full("old1", 10, "Someone", "old", None, "normal"),
            row_full("new1", 30, "Someone", "new", None, "normal"),
        ];
        assert!(matches!(
            plan_wake(Some(20), &fresh),
            WakePlan::Ring(row) if row.id == "new1"
        ));
        assert_eq!(plan_wake(Some(30), &fresh), WakePlan::Idle);
    }

    #[test]
    fn newest_ringable_is_strictly_above_the_high_water() {
        let rows = vec![
            row(10, "A", "old", "normal"),
            row(20, "B", "new", "normal"),
            row(15, "C", "middle", "normal"),
        ];
        assert_eq!(newest_ringable(&rows, 0).map(|r| r.created_ts_us), Some(20));
        assert_eq!(newest_ringable(&rows, 20).map(|r| r.created_ts_us), None);
        assert_eq!(
            newest_ringable(&rows, 16).map(|r| r.created_ts_us),
            Some(20)
        );
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

    #[test]
    fn the_operator_captain_is_never_woken() {
        use std::collections::BTreeSet;
        let entry = crate::state::SpawnEntry::with_mail_identity(
            "p1",
            "flywheel-toron-oc",
            "CloudySparrow",
        );
        assert!(stop_pin(&entry, Some("CloudySparrow"), &BTreeSet::new()));
        assert!(!stop_pin(&entry, Some("OtherPin"), &BTreeSet::new()));
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
