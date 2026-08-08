//! The flywheel cron registry (ADR-0005 Spec 3 — smart cron, flywheel half).
//!
//! Owns the CLI/catalog surface: registration (`crons.toml` persistence),
//! pause/resume (toggle without losing entry state), the merged view of
//! registered crons + schedule-triggered workflows, next-due computation and
//! status derivation. The P4.3 cron manager (Resonate durable-sleep loop)
//! drives ticks off this same registry — the CLI never writes engine state.
//!
//! governed-by: ADR-0005 Spec 3

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use toron_workflow::schema::{CronScope, Workflow};

/// One registered cron: a schedule bound to a workflow template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CronEntry {
    /// unique cron name (identifier used by pause/resume/history/status)
    pub name: String,
    /// workflow template run on each tick (must resolve in the catalog)
    pub template: String,
    /// 5-field cron expression ("30 3 * * 0" = weekly Sunday 03:30)
    pub cron: String,
    /// owning profile (Spec 3: the fixer's identity)
    pub owner: String,
    /// signing scope: host = operator identity, project = scoped agent
    #[serde(default)]
    pub scope: CronScope,
    /// pause/resume toggle. `false` PAUSES the cron but PRESERVES the entry —
    /// pause/resume never drops registration state (the acceptance contract).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// RFC3339 creation stamp
    #[serde(default)]
    pub created_at: String,
}

fn default_enabled() -> bool {
    true
}

/// `crons.toml` — the registry persistence (mirrors the recipes.toml /
/// workflows.toml convention: one file per artifact kind under config_dir()).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Crons {
    #[serde(flatten)]
    pub crons: BTreeMap<String, CronEntry>,
}

/// The on-disk registry location (XDG config).
pub fn crons_file() -> PathBuf {
    crate::config::config_dir().join("crons.toml")
}

impl Crons {
    /// Load the registry from the real XDG path (missing file = empty).
    pub fn load() -> Result<Self> {
        Self::load_from(&crons_file())
    }

    /// Load from an explicit path (tests use temp dirs).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Crons::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }

    /// Persist to the real XDG path (creates the config dir).
    pub fn save(&self) -> Result<()> {
        self.save_to(&crons_file())
    }

    /// Persist to an explicit path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(path, raw).with_context(|| format!("write {}", path.display()))
    }

    /// Insert or replace a cron by name (rejects an unparsable cron expr).
    pub fn upsert(&mut self, entry: CronEntry) -> Result<()> {
        if next_due(&entry.cron, now_ts()).is_none() {
            bail!("invalid cron expression {:?}", entry.cron);
        }
        self.crons.insert(entry.name.clone(), entry);
        Ok(())
    }

    /// Pause (`false`) or resume (`true`) a cron without losing any state.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<()> {
        let entry = self
            .crons
            .get_mut(name)
            .ok_or_else(|| anyhow!("unknown cron {name:?}"))?;
        entry.enabled = enabled;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&CronEntry> {
        self.crons.get(name)
    }
}

/// A cron as surfaced by `flywheel cron list`: a registry entry OR a
/// schedule-triggered workflow from the catalog/built-ins.
#[derive(Debug, Clone)]
pub struct MergedCron {
    pub entry: CronEntry,
    /// provenance: "cron" (crons.toml) | "workflow" (catalog/built-in)
    pub source: &'static str,
}

/// The merged view: crons.toml entries (source "cron") + every workflow whose
/// trigger is a `schedule` (source "workflow"), keyed by name — a registry
/// entry with the same name as a workflow wins (explicit registration
/// overrides the derived view).
pub fn merged_entries(
    crons: &Crons,
    workflows: &BTreeMap<String, (Workflow, &'static str)>,
) -> Vec<MergedCron> {
    let mut out: Vec<MergedCron> = Vec::new();
    for (name, entry) in &crons.crons {
        out.push(MergedCron {
            entry: entry.clone(),
            source: "cron",
        });
        let _ = name;
    }
    for (name, (wf, src)) in workflows {
        if crons.crons.contains_key(name) {
            continue;
        }
        if let Some(Trigger::Schedule { cron, owner, scope }) = &wf.trigger {
            out.push(MergedCron {
                entry: CronEntry {
                    name: name.clone(),
                    template: name.clone(),
                    cron: cron.clone(),
                    owner: owner.clone().unwrap_or_default(),
                    scope: *scope,
                    enabled: true,
                    created_at: String::new(),
                },
                source: src,
            });
        }
    }
    out.sort_by(|a, b| a.entry.name.cmp(&b.entry.name));
    out
}

use toron_workflow::schema::Trigger;

/// Next due tick (unix seconds) for a cron expr after `from_ts`. None for an
/// unparsable expression. Accepts the Spec 3 5-field form (`"30 3 * * 0"`,
/// min hour dom mon dow) as well as 6/7-field forms — 5-field is normalized by
/// prepending seconds=0 (the cron crate requires the seconds field).
pub fn next_due(cron_expr: &str, from_ts: i64) -> Option<i64> {
    let normalized = normalize_cron(cron_expr);
    let schedule = normalized.parse::<cron::Schedule>().ok()?;
    let from = chrono::DateTime::from_timestamp(from_ts, 0)?;
    let next = schedule.after(&from).next()?;
    Some(next.timestamp())
}

/// Normalize a Spec 3 cron expression to the form the `cron` crate parses:
/// 1. 5-field → 6-field by prepending seconds `0` (the crate requires it).
/// 2. day-of-week vixie→crate remap: vixie uses 0/7=Sunday, 1-6=Mon-Sat; the
///    crate uses 1=Sunday…7=Saturday. Per numeric component `d` the crate
///    ordinal is `(d % 7) + 1`. Names (`sun`, `mon`, …) and `*` pass through —
///    the crate maps names to the same 1=Sun convention.
/// 6/7-field and `@`-shorthand expressions pass through unchanged (their
/// day-of-week fields are already in the crate's 1-7 form). Garbage passes
/// through too — the parse fails downstream in `next_due`/`upsert`.
fn normalize_cron(expr: &str) -> String {
    let trimmed = expr.trim();
    let mut fields: Vec<String> = trimmed.split_whitespace().map(|s| s.to_string()).collect();
    if fields.len() == 5 {
        fields.insert(0, "0".to_string());
    }
    if let Some(dow) = fields.get_mut(5) {
        *dow = normalize_dow(dow);
    }
    fields.join(" ")
}

/// Remap one day-of-week token from vixie to crate ordinals.
fn normalize_dow(token: &str) -> String {
    // Names and bare `*` already match the crate's convention.
    if token.chars().any(|c| c.is_ascii_alphabetic()) || token == "*" {
        return token.to_string();
    }
    // Comma-separated list of items; each item is `d`, `a-b`, `d/step`,
    // `a-b/step` or `*/step`. Remap only the numeric leaves.
    token
        .split(',')
        .map(|item| {
            let (range, step) = match item.split_once('/') {
                Some((r, s)) => (r, Some(s)),
                None => (item, None),
            };
            let mapped = match range.split_once('-') {
                Some((a, b)) => format!("{}-{}", dow_ordinal(a), dow_ordinal(b)),
                None => dow_ordinal(range),
            };
            match step {
                Some(s) => format!("{mapped}/{s}"),
                None => mapped,
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// vixie day-of-week digit → crate ordinal (`(d % 7) + 1`; 0 and 7 → 1).
fn dow_ordinal(d: &str) -> String {
    match d.parse::<u32>() {
        Ok(n) => ((n % 7) + 1).to_string(),
        Err(_) => d.to_string(),
    }
}

/// Current wall-clock as unix seconds.
fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Derived cron status (Spec 3): the CLI maps the last state-log run for the
/// cron's template onto this. Order matters — paused wins over everything.
pub fn derive_status(enabled: bool, in_flight: bool, last_status: Option<&str>) -> &'static str {
    if !enabled {
        return "paused";
    }
    if in_flight {
        return "running";
    }
    match last_status {
        Some("failed") => "failed",
        Some("succeeded") => "succeeded",
        _ => "due",
    }
}

/// The `--json` row shape for list/status (agent-driver surface).
pub fn row_json(mc: &MergedCron, status: &str, next_due_ts: Option<i64>) -> Value {
    json!({
        "name": mc.entry.name,
        "template": mc.entry.template,
        "cron": mc.entry.cron,
        "owner": mc.entry.owner,
        "scope": match mc.entry.scope {
            CronScope::Host => "host",
            CronScope::Project => "project",
        },
        "source": mc.source,
        "enabled": mc.entry.enabled,
        "status": status,
        "next_due": next_due_ts,
    })
}

/// A cron's derived status row (Spec 3): paused | due | running | succeeded |
/// failed, plus the next-due tick for the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronStatus {
    pub name: String,
    pub status: &'static str,
    pub enabled: bool,
    pub next_due: Option<i64>,
}

/// One recorded run of a cron's workflow (state-log projection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronRun {
    pub ts: String,
    pub project: String,
    pub run_id: Option<String>,
    pub status: String,
    pub bead: Option<String>,
}

/// The cron manager contract (ADR-0005 Spec 3, P4.6). The flywheel CLI
/// compiles against THIS trait — never against a concrete manager. Two
/// implementations:
///
/// - [`RegistryCronManager`] — today's default: reads `crons.toml` + the
///   workflow catalog + the state log. Pure catalog/control surface.
/// - The P4.3 **cron manager** (BlueOak, in-progress): a Resonate
///   durable-sleep loop that DRIVES ticks (register/pause/resume/status/history
///   through the same contract). The CLI is backend-agnostic: swap the
///   manager, the command surface is unchanged.
///
/// `add` MUST fail closed on an unresolvable template / unparsable cron /
/// unknown owner (the lint-gate contract); the CLI does not re-validate.
pub trait CronManager {
    /// The merged view: registry entries + schedule-triggered workflows.
    fn list(&self) -> Result<Vec<MergedCron>>;
    /// One cron by name (merged view). None when unknown.
    fn show(&self, name: &str) -> Result<Option<MergedCron>>;
    /// Register a new cron (validation + persist). Fails closed.
    fn add(&mut self, entry: CronEntry) -> Result<()>;
    /// Pause (false) or resume (true) a cron without losing registration.
    fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<()>;
    /// Derived status for one cron.
    fn status(&self, name: &str) -> Result<Option<CronStatus>>;
    /// Derived status for every cron in the merged view.
    fn status_all(&self) -> Result<Vec<CronStatus>>;
    /// Run history for a cron (state-log scan + failure beads).
    fn history(&self, name: &str) -> Result<Vec<CronRun>>;
}

/// The default (registry-backed) cron manager. Reads `crons.toml` + the
/// workflow catalog + the state log — no tick driving. The P4.3 durable-sleep
/// manager replaces this behind the same [`CronManager`] contract.
pub struct RegistryCronManager {
    /// Workflow provider: built-ins + catalog, name → (workflow, source).
    workflows: BTreeMap<String, (Workflow, &'static str)>,
}

impl RegistryCronManager {
    pub fn new(workflows: BTreeMap<String, (Workflow, &'static str)>) -> Self {
        Self { workflows }
    }

    /// Load the manager over the real catalog (built-ins + canonical YAML).
    pub fn load() -> Result<Self> {
        let mut workflows: BTreeMap<String, (Workflow, &'static str)> = BTreeMap::new();
        for (name, wf) in crate::builtin_workflows::builtin_workflows() {
            workflows.insert(name, (wf, "builtin"));
        }
        let cat = crate::workflow_catalog::WorkflowCatalog::load()?;
        for (name, wf) in cat.workflows {
            workflows.insert(name, (wf, "catalog"));
        }
        Ok(Self::new(workflows))
    }

    /// The merged view from the current registry state.
    fn merged(&self) -> Result<Vec<MergedCron>> {
        let crons = Crons::load()?;
        Ok(merged_entries(&crons, &self.workflows))
    }
}

/// A manager whose workflows are injected (tests use hermetic catalogs).
pub fn make_registry_manager(
    workflows: BTreeMap<String, (Workflow, &'static str)>,
) -> RegistryCronManager {
    RegistryCronManager::new(workflows)
}

impl CronManager for RegistryCronManager {
    fn list(&self) -> Result<Vec<MergedCron>> {
        self.merged()
    }

    fn show(&self, name: &str) -> Result<Option<MergedCron>> {
        Ok(self
            .merged()?
            .into_iter()
            .find(|m| m.entry.name == name))
    }

    fn add(&mut self, entry: CronEntry) -> Result<()> {
        // fail-stop validation, mirroring lint_flywheel_tier: template must
        // resolve, owner profile must exist, cron must parse (upsert checks).
        if !self.workflows.contains_key(&entry.template) {
            bail!("template {:?} does not resolve (flywheel workflow list)", entry.template);
        }
        let profiles = crate::profile::ProfileCatalog::load().unwrap_or_default();
        if !profiles.get(&entry.owner).is_some() {
            bail!(
                "owner profile {:?} does not resolve (known: {})",
                entry.owner,
                profiles.names().join(", ")
            );
        }
        let mut crons = Crons::load()?;
        crons.upsert(entry)?;
        crons.save()
    }

    fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<()> {
        let mut crons = Crons::load()?;
        crons.set_enabled(name, enabled)?;
        crons.save()
    }

    fn status(&self, name: &str) -> Result<Option<CronStatus>> {
        Ok(self.show(name)?.map(|mc| cron_status(&mc)))
    }

    fn status_all(&self) -> Result<Vec<CronStatus>> {
        Ok(self.merged()?.iter().map(cron_status).collect())
    }

    fn history(&self, name: &str) -> Result<Vec<CronRun>> {
        Ok(cron_history(name))
    }
}

/// Derive a [`CronStatus`] row for a merged cron (registry state + state log).
fn cron_status(mc: &MergedCron) -> CronStatus {
    let (last, in_flight) = last_run_state(&mc.entry.name);
    let status = derive_status(mc.entry.enabled, in_flight, last.as_deref());
    let next_due = next_due(&mc.entry.cron, now_ts());
    CronStatus {
        name: mc.entry.name.clone(),
        status,
        enabled: mc.entry.enabled,
        next_due,
    }
}

/// Last terminal status + in-flight flag for a workflow from the state log.
fn last_run_state(name: &str) -> (Option<String>, bool) {
    let dir = crate::config::state_dir();
    let mut last_status: Option<String> = None;
    let mut in_flight = false;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (None, false);
    };
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.ends_with(".jsonl") {
            continue;
        }
        let project = fname.trim_end_matches(".jsonl").to_string();
        let Ok(events) = crate::state::read_log(&project) else {
            continue;
        };
        for e in events {
            if e.kind != crate::state::EventKind::Workflow {
                continue;
            }
            let owner = e.data.get("workflow").and_then(Value::as_str);
            if owner != Some(name) {
                continue;
            }
            match e.data.get("status").and_then(Value::as_str) {
                Some(s) if s == "failed" || s == "succeeded" => {
                    last_status = Some(s.to_string())
                }
                Some(s) if s == "running" => in_flight = true,
                _ => {}
            }
        }
    }
    (last_status, in_flight)
}

/// Run history for a workflow from the state log (+ failure beads).
fn cron_history(name: &str) -> Vec<CronRun> {
    let dir = crate::config::state_dir();
    let mut rows: Vec<CronRun> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return rows;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.ends_with(".jsonl") {
            continue;
        }
        let project = fname.trim_end_matches(".jsonl").to_string();
        let Ok(events) = crate::state::read_log(&project) else {
            continue;
        };
        for e in events {
            if e.kind != crate::state::EventKind::Workflow {
                continue;
            }
            let owner = e.data.get("workflow").and_then(Value::as_str);
            if owner != Some(name) {
                continue;
            }
            rows.push(CronRun {
                ts: e.ts.clone(),
                project: project.clone(),
                run_id: e
                    .data
                    .get("run_id")
                    .and_then(Value::as_i64)
                    .map(|i| i.to_string()),
                status: e
                    .data
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("done")
                    .to_string(),
                bead: e.data.get("bead").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
    rows.sort_by(|a, b| b.ts.cmp(&a.ts));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flywheel-cron-test-{tag}-{}", std::process::id()))
    }

    fn sample(name: &str) -> CronEntry {
        CronEntry {
            name: name.to_string(),
            template: format!("{name}-wf"),
            cron: "30 3 * * 0".to_string(),
            owner: "housekeeper".to_string(),
            scope: CronScope::Host,
            enabled: true,
            created_at: "2026-08-08T00:00:00Z".to_string(),
        }
    }

    /// save → load round-trips every field (nothing dropped).
    #[test]
    fn cron_roundtrip_preserves_all_fields() {
        let path = tmp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let mut crons = Crons::default();
        crons.upsert(sample("weekly-docker-upgrade")).unwrap();
        crons.save_to(&path).unwrap();

        let loaded = Crons::load_from(&path).unwrap();
        let got = loaded.get("weekly-docker-upgrade").unwrap();
        assert_eq!(got, &sample("weekly-docker-upgrade"));
        assert_eq!(got.owner, "housekeeper");
        assert_eq!(got.scope, CronScope::Host);
        assert_eq!(got.cron, "30 3 * * 0");
        assert!(got.enabled);
        let _ = std::fs::remove_file(&path);
    }

    /// THE pause/resume acceptance: toggling `enabled` must never lose the
    /// rest of the entry.
    #[test]
    fn pause_toggles_enabled_preserves_entry_state() {
        let mut crons = Crons::default();
        crons.upsert(sample("weekly-docker-upgrade")).unwrap();

        crons.set_enabled("weekly-docker-upgrade", false).unwrap();
        let paused = crons.get("weekly-docker-upgrade").unwrap();
        assert!(!paused.enabled, "pause flips enabled");
        assert_eq!(paused.template, "weekly-docker-upgrade-wf");
        assert_eq!(paused.cron, "30 3 * * 0");
        assert_eq!(paused.owner, "housekeeper");
        assert_eq!(paused.scope, CronScope::Host);

        crons.set_enabled("weekly-docker-upgrade", true).unwrap();
        let resumed = crons.get("weekly-docker-upgrade").unwrap();
        assert!(resumed.enabled, "resume flips enabled back");
        assert_eq!(resumed, &sample("weekly-docker-upgrade"), "full state intact");
    }

    /// set_enabled on an unknown name fails (fail-stop, no silent no-op).
    #[test]
    fn pause_unknown_cron_fails() {
        let mut crons = Crons::default();
        assert!(crons.set_enabled("nope", false).is_err());
    }

    /// upsert rejects an unparsable cron expression at registration time.
    #[test]
    fn invalid_cron_expr_rejected() {
        let mut bad = sample("bad");
        bad.cron = "not a cron".to_string();
        let mut crons = Crons::default();
        assert!(crons.upsert(bad).is_err());
        assert!(crons.crons.is_empty());
    }

    /// next_due finds a future tick for a valid expr; None for garbage.
    #[test]
    fn next_due_computes_future_tick() {
        // 2026-08-08T04:00:00Z — the expr is daily 03:30 UTC, so the next due
        // is the NEXT DAY 03:30 (today's 03:30 already passed).
        let from = chrono::DateTime::parse_from_rfc3339("2026-08-08T04:00:00Z")
            .unwrap()
            .timestamp();
        let due = next_due("30 3 * * *", from).expect("valid expr yields a tick");
        assert!(due > from, "next tick is in the future");
        let dt = chrono::DateTime::from_timestamp(due, 0).unwrap();
        assert_eq!(dt.hour(), 3);
        assert_eq!(dt.minute(), 30);

        assert_eq!(next_due("not a cron", from), None);
    }

    /// The Spec 3 day-of-week `0` (vixie Sunday) normalizes to `7` and parses.
    #[test]
    fn next_due_accepts_vixie_sunday_zero() {
        // 2026-08-08 is a Saturday; Sunday 03:30 UTC is the next day.
        let from = chrono::DateTime::parse_from_rfc3339("2026-08-08T04:00:00Z")
            .unwrap()
            .timestamp();
        let due = next_due("30 3 * * 0", from).expect("dow=0 parses");
        let dt = chrono::DateTime::from_timestamp(due, 0).unwrap();
        assert_eq!(dt.weekday(), chrono::Weekday::Sun);
        assert_eq!(dt.hour(), 3);
        assert_eq!(dt.minute(), 30);
    }

    /// The merged view: crons.toml entries + schedule workflows; non-schedule
    /// workflows excluded; registry beats a same-named derived workflow.
    #[test]
    fn list_merges_registry_and_schedule_workflows() {
        use toron_workflow::schema::{Step, Trigger};

        let mut crons = Crons::default();
        crons.upsert(sample("weekly-docker-upgrade")).unwrap();

        let mut workflows: BTreeMap<String, (Workflow, &'static str)> = BTreeMap::new();
        workflows.insert(
            "weekly-docker-upgrade".to_string(),
            (
                Workflow {
                    name: "weekly-docker-upgrade".into(),
                    description: None,
                    trigger: Some(Trigger::Schedule {
                        cron: "* * * * *".to_string(),
                        owner: Some("other".to_string()),
                        scope: CronScope::Project,
                    }),
                    policy: None,
                    guard: None,
                    steps: vec![Step {
                        name: None,
                        guard: None,
                        on_error: toron_workflow::schema::StepOnError::Continue,
                        action: toron_workflow::schema::Action::Delay { secs: 0 },
                    }],
                    graph: None,
                    inputs: vec![],
                    crew: None,
                },
                "catalog",
            ),
        );
        workflows.insert(
            "weekly-backup".to_string(),
            (
                Workflow {
                    name: "weekly-backup".into(),
                    description: None,
                    trigger: Some(Trigger::Schedule {
                        cron: "0 2 * * 1".to_string(),
                        owner: Some("housekeeper".to_string()),
                        scope: CronScope::Host,
                    }),
                    policy: None,
                    guard: None,
                    steps: vec![],
                    graph: None,
                    inputs: vec![],
                    crew: None,
                },
                "builtin",
            ),
        );
        workflows.insert(
            "invoke-only".to_string(),
            (
                Workflow {
                    name: "invoke-only".into(),
                    description: None,
                    trigger: None,
                    policy: None,
                    guard: None,
                    steps: vec![],
                    graph: None,
                    inputs: vec![],
                    crew: None,
                },
                "catalog",
            ),
        );

        let merged = merged_entries(&crons, &workflows);
        let names: Vec<&str> = merged.iter().map(|m| m.entry.name.as_str()).collect();
        assert_eq!(names, vec!["weekly-backup", "weekly-docker-upgrade"]);

        // registry wins: the crons.toml entry (host, housekeeper) shadows the
        // derived workflow (project, other)
        let reg = merged
            .iter()
            .find(|m| m.entry.name == "weekly-docker-upgrade")
            .unwrap();
        assert_eq!(reg.source, "cron");
        assert_eq!(reg.entry.owner, "housekeeper");
        assert_eq!(reg.entry.scope, CronScope::Host);
        assert_eq!(reg.entry.cron, "30 3 * * 0");

        // derived: built-in schedule workflow surfaces with source=builtin
        let derived = merged.iter().find(|m| m.entry.name == "weekly-backup").unwrap();
        assert_eq!(derived.source, "builtin");
        assert_eq!(derived.entry.owner, "housekeeper");
    }

    /// derive_status ordering: paused > running > terminal > due.
    #[test]
    fn status_derivation_ordering() {
        assert_eq!(derive_status(false, false, None), "paused");
        assert_eq!(derive_status(false, true, Some("failed")), "paused");
        assert_eq!(derive_status(true, true, None), "running");
        assert_eq!(derive_status(true, false, Some("succeeded")), "succeeded");
        assert_eq!(derive_status(true, false, Some("failed")), "failed");
        assert_eq!(derive_status(true, false, None), "due");
    }

    /// The JSON row shape is stable for agent drivers.
    #[test]
    fn row_json_shape_is_stable() {
        let merged = MergedCron {
            entry: sample("weekly-docker-upgrade"),
            source: "cron",
        };
        let row = row_json(&merged, "succeeded", Some(1776214200));
        assert_eq!(row["name"], "weekly-docker-upgrade");
        assert_eq!(row["template"], "weekly-docker-upgrade-wf");
        assert_eq!(row["cron"], "30 3 * * 0");
        assert_eq!(row["owner"], "housekeeper");
        assert_eq!(row["scope"], "host");
        assert_eq!(row["source"], "cron");
        assert_eq!(row["enabled"], true);
        assert_eq!(row["status"], "succeeded");
        assert_eq!(row["next_due"], 1776214200);
    }

    /// THE trait seam (bd-p4-cli-b99j, handoff): the CLI surface must compile
    /// against the [`CronManager`] CONTRACT, not a concrete impl — the P4.3
    /// durable-sleep cron manager will implement the same trait. This mock
    /// stands in for that manager and exercises the full surface: list/show/
    /// add/set_enabled/status/status_all/history.
    #[derive(Default)]
    struct MockManager {
        crons: Vec<CronEntry>,
    }

    impl CronManager for MockManager {
        fn list(&self) -> Result<Vec<MergedCron>> {
            Ok(self
                .crons
                .iter()
                .map(|e| MergedCron {
                    entry: e.clone(),
                    source: "cron",
                })
                .collect())
        }
        fn show(&self, name: &str) -> Result<Option<MergedCron>> {
            Ok(self
                .crons
                .iter()
                .find(|e| e.name == name)
                .map(|e| MergedCron {
                    entry: e.clone(),
                    source: "cron",
                }))
        }
        fn add(&mut self, entry: CronEntry) -> Result<()> {
            self.crons.push(entry);
            Ok(())
        }
        fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<()> {
            let e = self
                .crons
                .iter_mut()
                .find(|e| e.name == name)
                .ok_or_else(|| anyhow!("unknown cron {name:?}"))?;
            e.enabled = enabled;
            Ok(())
        }
        fn status(&self, name: &str) -> Result<Option<CronStatus>> {
            Ok(self.show(name)?.map(|mc| cron_status(&mc)))
        }
        fn status_all(&self) -> Result<Vec<CronStatus>> {
            Ok(self.list()?.iter().map(cron_status).collect())
        }
        fn history(&self, _name: &str) -> Result<Vec<CronRun>> {
            Ok(vec![])
        }
    }

    /// The CLI drives any manager through the trait: register → pause →
    /// resume → status — all via the [`CronManager`] surface.
    #[test]
    fn cli_surface_works_against_any_manager_impl() {
        let mut mgr = MockManager::default();
        let mut entry = sample("weekly-docker-upgrade");

        // add
        mgr.add(entry.clone()).expect("add");
        assert_eq!(mgr.list().unwrap().len(), 1);

        // show + status (due before any runs)
        let shown = mgr.show("weekly-docker-upgrade").unwrap().expect("show");
        assert_eq!(shown.entry.template, "weekly-docker-upgrade-wf");
        let st = mgr.status("weekly-docker-upgrade").unwrap().expect("status");
        assert_eq!(st.status, "due");
        assert!(st.next_due.is_some(), "next_due computed");

        // pause → status flips to paused, entry preserved
        mgr.set_enabled("weekly-docker-upgrade", false).unwrap();
        let paused = mgr.status("weekly-docker-upgrade").unwrap().unwrap();
        assert_eq!(paused.status, "paused");
        assert_eq!(
            mgr.show("weekly-docker-upgrade").unwrap().unwrap().entry.cron,
            entry.cron,
            "pause preserves cron expr"
        );

        // resume → back to due
        mgr.set_enabled("weekly-docker-upgrade", true).unwrap();
        assert_eq!(
            mgr.status("weekly-docker-upgrade").unwrap().unwrap().status,
            "due"
        );

        // status_all + history complete the surface
        let all = mgr.status_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(mgr.history("weekly-docker-upgrade").unwrap().len(), 0);

        // the same lifecycle through a trait object (the CLI's dispatch shape)
        let mut dyn_mgr: Box<dyn CronManager> = Box::new(MockManager::default());
        dyn_mgr.add(entry.clone()).unwrap();
        entry.enabled = false;
        dyn_mgr.set_enabled(&entry.name, false).unwrap();
        assert_eq!(
            dyn_mgr.status(&entry.name).unwrap().unwrap().status,
            "paused"
        );
        assert!(!dyn_mgr.list().unwrap()[0].entry.enabled);
    }
}
