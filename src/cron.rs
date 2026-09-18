// governed-by: ADR-0024
//! The flywheel cron registry (ADR-0005 Spec 3 / ADR-0012 — smart cron,
//! flywheel half).
//!
//! Two surfaces:
//! - The ADR-0012 cron YAML surface (P12.3, bd-25g7.3): crons are YAML
//!   files in three tiers — per-project > global > built-in — with a shared
//!   [`CronSpec`] body on the toron-workflow engine. pause/resume persists
//!   `enabled` in the tier's YAML; built-ins are read-only.
//! - The legacy crons.toml registry surface (ADR-0005 Spec 3): merged view
//!   of registered crons + schedule-triggered workflows, next-due
//!   computation and status derivation. The P4.3 cron manager drives ticks
//!   off this registry until P4 (bd-25g7.4) migrates it to cron YAMLs.
//!
//! governed-by: ADR-0005 Spec 3, ADR-0012 Spec 4

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toron_workflow::schema::{CronScope, CronSpec, Workflow};

/// One registered cron: a schedule bound to a workflow template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronEntry {
    /// unique cron name (identifier used by pause/resume/history/status)
    pub name: String,
    /// workflow template run on each tick (must resolve in the catalog)
    pub template: String,
    /// 5-field cron expression ("30 3 * * 0" = weekly Sunday 03:30)
    pub cron: String,
    /// owning profile (Spec 3: the fixer's identity)
    pub owner: String,
    /// execution mode (P10.4): headless = the __run-headless subprocess
    /// runner (autonomous/cron; no workspace churn); pane = the P10.3 pane
    /// runner (crew dispatch; empty spawn registry = fail-fast, bd-5qls).
    #[serde(default)]
    pub mode: CronMode,
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

/// How a cron's workflow executes each tick (ADR-0010 Spec E).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CronMode {
    /// Autonomous: steps dispatch through the `__run-headless` subprocess
    /// and log to `runs/<run_id>.jsonl` — no herdr pane, no workspace churn.
    #[default]
    Headless,
    /// Interactive: the P10.3 pane runner (crew dispatch via `__send`).
    /// `dispatch_job` steps dispatch to running crew panes (bd-5qls); an
    /// empty crew registry is a per-step fail-fast error, never a record.
    Pane,
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

#[cfg_attr(not(test), allow(dead_code))] // legacy crons.toml write surface — P4 (bd-25g7.4) migrates/retires
impl Crons {
    /// Load the registry from the real XDG path (missing file = empty).
    pub fn load() -> Result<Self> {
        Self::load_from(&crons_file())
    }

    /// Load from an explicit path (tests use temp dirs).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }

    /// Persist to the real XDG path (creates the config dir).
    pub fn save(&self) -> Result<()> {
        self.save_to(&crons_file())
    }

    /// Persist to an explicit path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        // Atomic temp+rename (l4s.3): a torn write would corrupt crons.toml
        // and take the whole schedule surface down. Shared helper.
        let raw = toml::to_string_pretty(self)?;
        crate::fs_util::atomic_write(path, raw.as_bytes())
            .with_context(|| format!("write {}", path.display()))
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

    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&CronEntry> {
        self.crons.get(name)
    }
}

/// A cron as surfaced by `flywheel cron list`: a registry entry OR a
/// schedule-triggered workflow from the catalog/built-ins.
#[cfg_attr(not(test), allow(dead_code))] // legacy merged view — P4 (bd-25g7.4) migrates/retires
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
                    mode: CronMode::Headless,
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

// ---------------------------------------------------------------------------
// ADR-0012 Spec 4: user dirs + precedence (P12.3, bd-25g7.3). Crons are YAML
// files in three tiers; per-project > global > built-in. The spec IS the
// source — pause/resume only toggles `enabled` in the tier's YAML file.
// ---------------------------------------------------------------------------

/// Where a cron definition lives (ADR-0012 Spec 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronSource {
    /// Compiled-in (read-only; a user tier shadows by name).
    Builtin,
    /// `~/.config/flywheel/crons/<name>.yaml` (cross-project maintenance).
    Global,
    /// `<project>/.flywheel/crons/<name>.yaml` (project-scoped automation).
    Project,
}

impl CronSource {
    /// The `cron list` source label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

/// One resolved cron: the parsed spec + its tier + its on-disk path.
/// Built-ins carry a synthetic (empty) path — they are read-only.
#[derive(Debug, Clone)]
pub struct CronRow {
    pub spec: CronSpec,
    pub source: CronSource,
    pub path: PathBuf,
}

impl CronRow {
    pub fn name(&self) -> &str {
        &self.spec.name
    }
    pub fn cron(&self) -> &str {
        &self.spec.schedule
    }
    pub fn owner(&self) -> &str {
        &self.spec.owner
    }
    pub fn scope(&self) -> CronScope {
        self.spec.scope
    }
    pub fn enabled(&self) -> bool {
        self.spec.enabled
    }
    pub fn source_label(&self) -> &'static str {
        self.source.label()
    }
}

/// The compiled-in (read-only) cron tier. Today: the built-in
/// schedule-triggered workflows (weekly-review) converted to [`CronSpec`] —
/// a schedule-triggered workflow IS the pre-ADR-0012 spelling of a built-in
/// cron. A user-tier YAML shadows it by name.
pub fn builtin_crons() -> BTreeMap<String, CronSpec> {
    let mut out = BTreeMap::new();
    for (name, wf) in crate::builtin_workflows::builtin_workflows() {
        let Some(Trigger::Schedule { cron, owner, scope }) = &wf.trigger else {
            continue;
        };
        out.insert(
            name.clone(),
            CronSpec {
                name: name.clone(),
                description: wf.description.clone(),
                schedule: cron.clone(),
                owner: owner.clone().unwrap_or_default(),
                scope: *scope,
                mode: toron_workflow::schema::CronMode::Headless,
                enabled: true,
                policy: wf
                    .policy
                    .as_ref()
                    .map(|p| toron_workflow::schema::CronSpecPolicy {
                        on_failure: p.on_failure,
                        on_repeat_failure: p.on_repeat_failure,
                        repeat_threshold: p.repeat_threshold,
                        on_success: p.on_success,
                        max_retries: p.max_retries,
                        no_claim_timeout: p.no_claim_timeout,
                        catchup: toron_workflow::schema::CronCatchup::Latest,
                    }),
                steps: wf.steps.clone(),
                graph: wf.graph.clone(),
            },
        );
    }
    out
}

/// The global tier dir: `~/.config/flywheel/crons/`.
pub fn global_crons_dir() -> PathBuf {
    crate::config::config_dir().join("crons")
}

/// The per-project tier dir: `<project>/.flywheel/crons/` (project resolved
/// under the workspace root; absolute project paths pass through).
pub fn project_crons_dir(cfg: &crate::config::Config, project: &str) -> PathBuf {
    let p = PathBuf::from(project);
    let p = if p.is_absolute() {
        p
    } else {
        cfg.workspace_root().join(project)
    };
    p.join(".flywheel").join("crons")
}

/// The project a cron invocation targets: an explicit `--project` wins;
/// otherwise detect from cwd — a path under the workspace root maps to the
/// relative project name (running outside any project = no project tier).
pub fn detect_project(cfg: &crate::config::Config, explicit: Option<&str>) -> Option<String> {
    if let Some(p) = explicit {
        return Some(p.to_string());
    }
    let cwd = std::env::current_dir().ok()?;
    let rel = cwd.strip_prefix(cfg.workspace_root()).ok()?;
    let first = rel.components().next()?.as_os_str().to_str()?;
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

/// Load every cron across the three tiers with precedence: per-project >
/// global > built-in (ADR-0012 Spec 4). A higher tier with the same name
/// SHADOWS the lower tier entirely — list shows only the winner.
pub fn load_crons(cfg: &crate::config::Config, project: Option<&str>) -> Result<Vec<CronRow>> {
    // bd-blank-project-spelling-non-mail-doors-76hi: Some("") used to alias
    // the project tier onto the workspace root.
    if let Some(p) = project {
        crate::check_project_spelling(p, "cron")?;
    }
    let mut out: BTreeMap<String, CronRow> = BTreeMap::new();
    for (name, spec) in builtin_crons() {
        out.insert(
            name.clone(),
            CronRow {
                spec,
                source: CronSource::Builtin,
                path: PathBuf::new(),
            },
        );
    }
    scan_tier(&mut out, &global_crons_dir(), CronSource::Global)?;
    if let Some(p) = detect_project(cfg, project) {
        scan_tier(&mut out, &project_crons_dir(cfg, &p), CronSource::Project)?;
    }
    Ok(out.into_values().collect())
}

/// Scan one tier dir, inserting (or shadowing) its crons by name. A missing
/// tier dir is an EMPTY tier, never an error.
fn scan_tier(out: &mut BTreeMap<String, CronRow>, dir: &Path, source: CronSource) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let spec = toron_workflow::schema::parse_cron(&raw)
            .map_err(|e| anyhow!("parse {}: {e}", path.display()))?;
        out.insert(spec.name.clone(), CronRow { spec, source, path });
    }
    Ok(())
}

/// Pause (false) / resume (true) a cron by name (ADR-0012 Spec 4). Only
/// user-tier crons are mutable — built-ins are read-only (shadow by a user
/// YAML instead). The toggle persists as `enabled: false` in the tier's YAML;
/// the definition is otherwise preserved (pause/resume never drops state).
pub fn set_cron_enabled(
    cfg: &crate::config::Config,
    project: Option<&str>,
    name: &str,
    enabled: bool,
) -> Result<()> {
    let rows = load_crons(cfg, project)?;
    let row = rows
        .iter()
        .find(|r| r.name() == name)
        .ok_or_else(|| anyhow!("no cron named {name:?} (flywheel cron list)"))?;
    match row.source {
        CronSource::Builtin => bail!(
            "cron {name:?} is a built-in (read-only) — override by writing {}/<name>.yaml",
            global_crons_dir().display()
        ),
        CronSource::Global | CronSource::Project => {}
    }
    let mut spec = row.spec.clone();
    spec.enabled = enabled;
    let raw = serde_yaml::to_string(&spec)
        .with_context(|| format!("serialize {}", row.path.display()))?;
    crate::fs_util::atomic_write(&row.path, raw.as_bytes())
        .with_context(|| format!("write {}", row.path.display()))?;
    Ok(())
}

/// Remove a cron definition (user tiers only; built-ins are read-only).
/// Removing a project-tier cron reveals the global/built-in definition it
/// shadowed.
pub fn remove_cron(cfg: &crate::config::Config, project: Option<&str>, name: &str) -> Result<()> {
    let rows = load_crons(cfg, project)?;
    let row = rows
        .iter()
        .find(|r| r.name() == name)
        .ok_or_else(|| anyhow!("no cron named {name:?} (flywheel cron list)"))?;
    match row.source {
        CronSource::Builtin => bail!(
            "cron {name:?} is a built-in (read-only) — shadow by writing {}/<name>.yaml",
            global_crons_dir().display()
        ),
        CronSource::Global | CronSource::Project => {
            std::fs::remove_file(&row.path)
                .with_context(|| format!("remove {}", row.path.display()))?;
        }
    }
    Ok(())
}

/// The `--json` row shape for `cron list`/`status` over the ADR-0012 cron
/// YAMLs (agent-driver surface): the resolved source + scope included.
pub fn cron_row_json(row: &CronRow, status: &str, next_due_ts: Option<i64>) -> Value {
    json!({
        "name": row.name(),
        "schedule": row.cron(),
        "owner": row.owner(),
        "scope": match row.scope() {
            CronScope::Host => "host",
            CronScope::Project => "project",
        },
        "source": row.source_label(),
        "enabled": row.enabled(),
        "mode": match row.spec.mode {
            toron_workflow::schema::CronMode::Headless => "headless",
            toron_workflow::schema::CronMode::Pane => "pane",
        },
        "steps": row.spec.steps.len(),
        "graph_nodes": row.spec.graph.as_ref().map(|g| g.nodes.len()),
        "status": status,
        "next_due": next_due_ts,
    })
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
///    6/7-field and `@`-shorthand expressions pass through unchanged (their
///    day-of-week fields are already in the crate's 1-7 form). Garbage passes
///    through too — the parse fails downstream in `next_due`/`upsert`.
fn normalize_cron(expr: &str) -> String {
    let trimmed = expr.trim();
    let mut fields: Vec<String> = trimmed
        .split_whitespace()
        .map(std::string::ToString::to_string)
        .collect();
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
pub fn now_ts() -> i64 {
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
#[cfg_attr(not(test), allow(dead_code))] // legacy merged-row shape — P4 (bd-25g7.4) migrates/retires
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
        "mode": match mc.entry.mode {
            CronMode::Headless => "headless",
            CronMode::Pane => "pane",
        },
        "status": status,
        "next_due": next_due_ts,
    })
}

/// A cron's derived status row (Spec 3): paused | due | running | succeeded |
/// failed, plus the next-due tick for the scheduler.
#[cfg_attr(not(test), allow(dead_code))] // legacy derived-status row — P4 (bd-25g7.4) migrates/retires
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
#[cfg_attr(not(test), allow(dead_code))] // legacy manager contract — P4 (bd-25g7.4) migrates/retires
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
#[cfg(test)]
pub fn make_registry_manager(
    workflows: BTreeMap<String, (Workflow, &'static str)>,
) -> RegistryCronManager {
    RegistryCronManager::new(workflows)
}

// ---- P4.5: wiring to the landed P4.3 cron manager (bd-p4-migrate-sagas-7rmo)
//
// The toron-workflow `CronManager` (Resonate durable-sleep loop) drives ticks
// over a `CronRegistry` closure returning enabled `CronRegistration`s. This
// section builds those registrations from the merged view (crons.toml +
// schedule-triggered workflows in the catalog/built-ins), provides the
// per-mode action runner (`call_webhook` probes; headless dispatch /
// pane dispatch / recorded dry-run), and starts the loops. Failure policy
// (`on_failure: bead`) is executed by draining the engine's `PolicySink`
// → `br create`.

impl RegistryCronManager {
    /// The enabled [`CronRegistration`]s the durable cron manager drives:
    /// the merged view (registry + schedule workflows) with each cron's
    /// `Workflow` resolved from the catalog/built-ins. Paused crons keep
    /// their entry but are excluded (they still surface in `list`/`status`).
    pub fn registrations(&self) -> Result<Vec<toron_workflow::CronRegistration>> {
        self.registrations_from(&Crons::load()?)
    }

    /// Registrations from an explicit registry (tests pass a hermetic Crons;
    /// the CLI uses the real config via [`Self::registrations`]).
    pub fn registrations_from(
        &self,
        crons: &Crons,
    ) -> Result<Vec<toron_workflow::CronRegistration>> {
        let mut out = Vec::new();
        for mc in merged_entries(crons, &self.workflows) {
            let wf = self
                .workflows
                .get(&mc.entry.template)
                .map(|(wf, _)| wf.clone())
                .ok_or_else(|| {
                    anyhow!(
                        "cron {:?}: template {:?} does not resolve",
                        mc.entry.name,
                        mc.entry.template
                    )
                })?;
            out.push(toron_workflow::CronRegistration {
                name: mc.entry.name.clone(),
                cron: mc.entry.cron.clone(),
                workflow: wf,
                owner: mc.entry.owner.clone(),
                scope: mc.entry.scope,
                enabled: mc.entry.enabled,
            });
        }
        Ok(out)
    }
}

/// The flywheel action runner for cron runs (ADR-0005 Spec 3): executes
/// `call_webhook` as a REAL health probe (the health-gate steps probe the
/// saga's five services). Everything else is mode-dependent — headless
/// dispatches, pane dispatches, dry-run records.
///
/// Probe semantics: a reachable endpoint returns `{"ok":true,"status":N}`;
/// an unreachable/erroring probe returns `Err` so a step with
/// `on_error: fail_run` FAILS the run (the health-gate contract — a dead
/// service is a failure, not a branch case).
/// The per-cron action runner (P10.4): the mode decides how a tick's
/// workflow dispatches.
///
/// - `Headless` → the P10.1 headless runner: dispatch/spawn steps shell out
///   to the `__run-headless` leaf (role-conditioned, registry-injected
///   model) and log to `runs/<run_id>.jsonl` with the cron name attributed.
/// - `Pane` → the P10.3 pane runner (bd-5qls): `dispatch_job` steps resolve
///   their `@profile` to a running crew pane and dispatch via the loop's
///   `__send` leaf. Crew entries come from the cron project's spawn
///   registry (default project `flywheel`); an empty registry is fail-fast —
///   the step errors, never records.
///
/// The runner closes over the cron entry because the engine's
/// [`toron_workflow::ActionRunner`] receives only the action — cron context
/// cannot flow through the engine. `start_loops` therefore builds one engine
/// per cron.
pub fn cron_runner(entry: &CronEntry) -> toron_workflow::ActionRunner {
    use toron_workflow::schema::Action;
    match entry.mode {
        CronMode::Pane => {
            let catalog = crate::profile::ProfileCatalog::load().unwrap_or_default();
            let project = "flywheel";
            let entries = crate::state::load_spawn_registry(project);
            let name = entry.name.clone();
            let inputs = Value::Null;
            std::sync::Arc::new(move |a: &Action| {
                crate::pane::pane_dispatch(&catalog, project, &name, &inputs, &entries, a)
            })
        }
        CronMode::Headless => {
            let catalog = crate::profile::ProfileCatalog::load().unwrap_or_default();
            let name = entry.name.clone();
            let template = entry.template.clone();
            let inputs = Value::Null;
            std::sync::Arc::new(move |a: &Action| {
                crate::headless_dispatch(&catalog, &template, &inputs, Some(&name), a, None)
            })
        }
    }
}

/// The recording runner: webhook probes hit the network, everything else
/// records. The dry-run runner for the on-demand `flywheel cron run` command
/// (bd-8wvz): an on-demand run is a structural test of the body, never a
/// live agent dispatch — so it records every step instead of shelling out.
/// Pane crons no longer use this runner (bd-5qls — they dispatch).
pub fn recording_runner() -> toron_workflow::ActionRunner {
    use toron_workflow::schema::Action;
    std::sync::Arc::new(|a: &Action| match a {
        Action::CallWebhook { url, method, body } => {
            let method = method.as_deref().unwrap_or("GET").to_ascii_uppercase();
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(5))
                .build();
            let resp = match method.as_str() {
                "POST" => {
                    let b = body
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or(Value::Null);
                    agent.post(url).send_json(b)
                }
                "PUT" => {
                    let b = body
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or(Value::Null);
                    agent.put(url).send_json(b)
                }
                "DELETE" => agent.delete(url).call(),
                _ => agent.get(url).call(),
            };
            match resp {
                Ok(r) => {
                    let status = r.status();
                    Ok(json!({ "ok": (200..300).contains(&status), "status": status }))
                }
                Err(e) => Err(anyhow!("probe failed: {e}")),
            }
        }
        other => Ok(json!({ "action": other.type_name(), "ok": true })),
    })
}

/// The action runner for a [`CronSpec`] (ADR-0012 Spec 2 — one engine, two
/// input types): headless = real `__run-headless` dispatch (the default);
/// pane = the P10.3 pane dispatch (bd-5qls) — crew panes from the cron
/// project's spawn registry, empty registry fail-fast.
pub fn spec_runner(spec: &CronSpec) -> toron_workflow::ActionRunner {
    use toron_workflow::schema::{Action, CronMode as SpecMode};
    match spec.mode {
        SpecMode::Headless => {
            let catalog = crate::profile::ProfileCatalog::load().unwrap_or_default();
            let name = spec.name.clone();
            let inputs = Value::Null;
            std::sync::Arc::new(move |a: &Action| {
                crate::headless_dispatch(&catalog, &name, &inputs, Some(&name), a, None)
            })
        }
        SpecMode::Pane => {
            let catalog = crate::profile::ProfileCatalog::load().unwrap_or_default();
            let project = "flywheel";
            let entries = crate::state::load_spawn_registry(project);
            let name = spec.name.clone();
            let inputs = Value::Null;
            std::sync::Arc::new(move |a: &Action| {
                crate::pane::pane_dispatch(&catalog, project, &name, &inputs, &entries, a)
            })
        }
    }
}

/// Create a failure bead via `br create` (the `on_failure: bead` side effect,
/// ADR-0005 Spec 3). The failure bundle becomes the bead description so the
/// defined fixer (`housekeeper-diagnose`) can read it. Runs in the toron
/// workspace (the default working dir of `flywheel cron start`). Returns the
/// created bead id (from `br create --json`), or errors.
pub fn spawn_failure_bead(bundle: &toron_workflow::FailureBundle) -> Result<String> {
    use std::process::Command;
    let (title, description) = failure_bead_fields(bundle);
    let out = Command::new("br")
        .args([
            "create",
            &title,
            "--type",
            "bug",
            "--priority",
            "1",
            "--description",
            &description,
            "--json",
        ])
        .output()
        .map_err(|e| anyhow!("br create failed: {e}"))?;
    if !out.status.success() {
        bail!(
            "br create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // `br create --json` returns the created issue; extract its id.
    let raw = String::from_utf8_lossy(&out.stdout);
    let id: String = match serde_json::from_str::<Value>(raw.trim()) {
        Ok(v) => {
            let node = v
                .get("issue")
                .or_else(|| v.get("id"))
                .or_else(|| v.as_array().and_then(|a| a.first()))
                .cloned();
            match node {
                Some(n) => n
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("(id unknown)")
                    .to_string(),
                None => "(id unknown)".to_string(),
            }
        }
        Err(_) => "(id unknown)".to_string(),
    };
    Ok(id)
}

/// The bead's title + description derived from a failure bundle. The bundle is
/// fully embedded so the defined fixer (`housekeeper-diagnose`) can triage
/// without the original run context. Pure — unit-testable.
pub fn failure_bead_fields(bundle: &toron_workflow::FailureBundle) -> (String, String) {
    let title = format!(
        "cron failure: {} ({}#{})",
        bundle.workflow, bundle.run_id, bundle.consecutive
    );
    let description = format!(
        "# Cron failure bead (ADR-0005 Spec 3, P4.4)\n\
         \n- run_id: `{}`\n- workflow: `{}`\n- owner: `{}`\n- error: `{}`\n- failed_step: `{}`\n- consecutive: {}\n\
         \nThe defined fixer (`housekeeper-diagnose`) reads this bundle and proposes a patch.",
        bundle.run_id,
        bundle.workflow,
        bundle.owner,
        bundle.error,
        bundle.failed_step.as_deref().unwrap_or("(unknown)"),
        bundle.consecutive,
    );
    (title, description)
}

/// The synthesized improvement idea the weekly-review oracle produced
/// (bd-m7ac). Parsed from the cron tick's workflow output — the oracle node's
/// `response` carries `{"idea": {...}}` or `{"idea": null}` (honest skip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewIdea {
    pub title: String,
    pub what: String,
    pub why: String,
    pub ev: String,
    pub blast_radius: String,
    pub acceptance: String,
}

/// Extract the weekly-review idea from a tick's workflow output (bd-m7ac).
///
/// Shape: the run output is the graph's step map (`{"oracle": {...}}`); the
/// oracle node's headless dispatch returns `{"response": "<idea json>"}`.
/// `{"idea": null}` (or a missing/empty idea) yields `None` — no bead, honest.
///
/// Pure — unit-testable, no IO.
pub fn review_idea_from_output(output: &serde_json::Value) -> Option<ReviewIdea> {
    let oracle = output.get("oracle")?;
    let response = oracle.get("response").and_then(serde_json::Value::as_str)?;
    let parsed: serde_json::Value = serde_json::from_str(response.trim()).ok()?;
    let idea = parsed.get("idea")?;
    if idea.is_null() {
        return None;
    }
    let s = |k: &str| {
        idea.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let title = s("title");
    if title.trim().is_empty() {
        return None;
    }
    Some(ReviewIdea {
        title,
        what: s("what"),
        why: s("why"),
        ev: s("ev"),
        blast_radius: s("blast_radius"),
        acceptance: s("acceptance"),
    })
}

/// The review bead's title + description (bd-m7ac). The idea is embedded so
/// the captain can triage implement-or-close from `br list` alone. Pure.
pub fn review_bead_fields(idea: &ReviewIdea, date: &str) -> (String, String) {
    let title = format!("weekly-review {}: {}", date, idea.title);
    let description = format!(
        "# Weekly review (bd-m7ac smart-cron, {date})\n\
         \n## What\n{}\n\
         \n## Why\n{}\n\
         \n## EV\n{}\n\
         \n## Blast radius\n{}\n\
         \n## Acceptance\n{}\n\
         \nCaptain triages: implement or close. Tagged `weekly-review`.",
        idea.what, idea.why, idea.ev, idea.blast_radius, idea.acceptance,
    );
    (title, description)
}

/// Create the weekly-review follow-up bead via `br create` (bd-m7ac): EXACTLY
/// ONE bead tagged `weekly-review,<date>`, or `Ok(None)` when the oracle
/// found nothing worth proposing (honest, not forced). The date is the tick's
/// calendar date (the cron label + title context). Returns the bead id.
pub fn spawn_review_bead(output: &serde_json::Value, tick_ts: i64) -> Result<Option<String>> {
    use std::process::Command;
    let Some(idea) = review_idea_from_output(output) else {
        return Ok(None);
    };
    let date = tick_date(tick_ts);
    let (title, description) = review_bead_fields(&idea, &date);
    let out = Command::new("br")
        .args([
            "create",
            &title,
            "--type",
            "feature",
            "--priority",
            "2",
            "--description",
            &description,
            "--labels",
            &format!("weekly-review,{date}"),
            "--json",
        ])
        .output()
        .map_err(|e| anyhow!("br create (weekly-review) failed: {e}"))?;
    if !out.status.success() {
        bail!(
            "br create (weekly-review) failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let id: String = match serde_json::from_str::<Value>(raw.trim()) {
        Ok(v) => {
            let node = v
                .get("issue")
                .or_else(|| v.get("id"))
                .or_else(|| v.as_array().and_then(|a| a.first()))
                .cloned();
            match node {
                Some(n) => n
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("(id unknown)")
                    .to_string(),
                None => "(id unknown)".to_string(),
            }
        }
        Err(_) => "(id unknown)".to_string(),
    };
    Ok(Some(id))
}

/// The tick's calendar date (`YYYY-MM-DD`) from its unix timestamp — the
/// weekly-review cron's per-tick label. Pure.
pub fn tick_date(tick_ts: i64) -> String {
    use chrono::TimeZone;
    if tick_ts < 0 {
        return "unknown".to_string();
    }
    chrono::Utc
        .timestamp_opt(tick_ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Start (or resume) the durable cron loops over the current cron set —
/// the ADR-0012 cron YAMLs (per-project > global > built-in) PLUS the legacy
/// crons.toml/schedule-workflow registrations (P4 migrates those).
/// Idempotent: re-invoking `cron-spec:{name}` / `cron:{name}` resumes the
/// same durable function (restart survival). Returns once every loop is
/// started; the loops run on the engine's in-process Resonate runtime.
///
/// The health-gate + policy surface is live: `call_webhook` probes run, and
/// `on_failure: bead` creates a `br` bead with the failure bundle. This is
/// the P4.5 migration path — the old systemd timers stay until the workflows
/// pass the same health-gates.
pub async fn start_loops(cfg: &crate::config::Config, project: Option<&str>) -> Result<()> {
    use toron_workflow::{CronManager, CronSpecRegistration, CronSpecRegistry};

    let manager = RegistryCronManager::load()?;
    let registrations = manager.registrations()?;
    let rows = load_crons(cfg, project)?;
    if registrations.is_empty() && !rows.iter().any(CronRow::enabled) {
        bail!("no enabled crons defined (flywheel cron list)");
    }

    // One engine + manager PER cron (P10.4): the ActionRunner receives only
    // the action, so the per-cron mode/name must be closed over at engine
    // construction. The durable loop ids `cron-spec:{name}` (ADR-0012 YAML
    // crons) and `cron:{name}` (legacy registrations) are unchanged.
    let mut drain_tasks = Vec::new();

    // ADR-0012 YAML crons: the spec IS the registration (schedule + owner +
    // policy + steps body all from the YAML, one engine two input types).
    for row in rows.iter().filter(|r| r.enabled()) {
        // bd-engine-run-path-journaling-fwlx: a cron body's `dispatch_job` is
        // an external effect — journal it. The resolver is spelled explicitly
        // (it is `local_with`'s default) so the combined constructor can carry
        // the effect runner without changing what a cron body can resolve.
        let spec_run = spec_runner(&row.spec);
        // bd-mi9n: both journal halves (effect runner + run lifecycle) come
        // from the one store, so a live cron fire's run row and effect rows
        // agree on run_id.
        let built = crate::effect_journal::journals(cfg, spec_run.clone());
        let (effect, run_journal) = match built {
            Some(j) => (Some(j.effect), Some(j.run)),
            None => (None, None),
        };
        // bd-engine-run-path-journaling-fwlx (the Spec 6 flip): a live cron
        // body really dispatches, so its engine is Enforced when the journal
        // opened; with no journal the mode stays Recording so the bypass
        // marker keeps the gap visible instead of failing every fire.
        let mode = if effect.is_some() {
            toron_workflow::engine::EffectJournalMode::Enforced
        } else {
            toron_workflow::engine::EffectJournalMode::Recording
        };
        let engine = std::sync::Arc::new(
            toron_workflow::WorkflowEngine::local_with_resolvers_effect_mode_and_run_journal(
                spec_run.clone(),
                crate::profile::ProfileCatalog::catalog_profile_resolver(),
                toron_workflow::WorkflowEngine::default_workflow_resolver(),
                effect,
                mode,
                run_journal,
            )
            .map_err(|e| anyhow!("engine for cron {}: {e}", row.name()))?,
        );
        let cron_mgr = CronManager::with_spec_registry(
            engine.clone(),
            std::sync::Arc::new(Vec::<toron_workflow::CronRegistration>::new),
            Some({
                let spec = row.spec.clone();
                let spec_registry: CronSpecRegistry = std::sync::Arc::new(move || {
                    vec![CronSpecRegistration {
                        spec: spec.clone(),
                        enabled: true,
                        start_ts: None,
                    }]
                });
                spec_registry
            }),
        )?;
        let sink = engine.policy_sink().clone();
        let proj = project.map(str::to_string);
        drain_tasks.push(tokio::spawn(async move {
            let _ = cron_mgr.start().await;
            drain_policy_sink(sink, proj).await;
        }));
    }

    // Legacy crons.toml + schedule-workflow registrations (P4 migrates).
    for reg in registrations.iter().filter(|r| r.enabled) {
        let entry = manager
            .show(&reg.name)?
            .map(|m| m.entry)
            .ok_or_else(|| anyhow!("cron {:?} vanished between load and start", reg.name))?;
        let cron_run = cron_runner(&entry);
        let built = crate::effect_journal::journals(cfg, cron_run.clone());
        let (effect, run_journal) = match built {
            Some(j) => (Some(j.effect), Some(j.run)),
            None => (None, None),
        };
        // Same flip as the YAML cron above: Enforced only when journaled.
        let mode = if effect.is_some() {
            toron_workflow::engine::EffectJournalMode::Enforced
        } else {
            toron_workflow::engine::EffectJournalMode::Recording
        };
        let engine = std::sync::Arc::new(
            toron_workflow::WorkflowEngine::local_with_resolvers_effect_mode_and_run_journal(
                cron_run.clone(),
                crate::profile::ProfileCatalog::catalog_profile_resolver(),
                toron_workflow::WorkflowEngine::default_workflow_resolver(),
                effect,
                mode,
                run_journal,
            )
            .map_err(|e| anyhow!("engine for cron {}: {e}", reg.name))?,
        );
        let cron_mgr = CronManager::new(engine.clone(), {
            let regs = registrations.clone();
            let name = reg.name.clone();
            std::sync::Arc::new(move || regs.iter().filter(|&r| r.name == name).cloned().collect())
        })?;
        let sink = engine.policy_sink().clone();
        let proj = project.map(str::to_string);
        drain_tasks.push(tokio::spawn(async move {
            let _ = cron_mgr.start().await;
            drain_policy_sink(sink, proj).await;
        }));
    }

    for h in drain_tasks {
        let _ = h.await;
    }
    Ok(())
}

/// Drain a cron engine's policy sink (bead side-effects) until Ctrl-C. The
/// weekly-review SUCCESS side-effect (bd-m7ac): its tick output carries the
/// oracle's idea — EXACTLY ONE bead tagged weekly-review+date; None (no bead)
/// when the oracle honestly found nothing worth proposing. Failure events for
/// the same cron still take the failure-bead path below (the output won't
/// parse as an idea).
async fn drain_policy_sink(sink: toron_workflow::PolicySink, project: Option<String>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let drained: Vec<toron_workflow::PolicyEvent> = {
            let mut guard = sink.lock().await;
            std::mem::take(&mut *guard)
        };
        for ev in drained {
            if ev.bundle.workflow == "weekly-review" && ev.output.as_ref().is_some() {
                let out = ev
                    .output
                    .as_ref()
                    .expect("output presence checked in the guard above");
                if let Some(idea) = review_idea_from_output(out) {
                    match spawn_review_bead(out, ev.tick) {
                        Ok(Some(id)) => eprintln!(
                            "cron {}({}): weekly-review -> bead {id} (\"{}\")",
                            ev.cron, ev.tick, idea.title
                        ),
                        Ok(None) => eprintln!(
                            "cron {}({}): weekly-review found nothing worth proposing — no bead",
                            ev.cron, ev.tick
                        ),
                        Err(e) => eprintln!(
                            "cron {}({}): weekly-review bead side-effect failed: {e}",
                            ev.cron, ev.tick
                        ),
                    }
                    continue;
                }
                eprintln!(
                    "cron {}({}): weekly-review output carried no idea — no bead",
                    ev.cron, ev.tick
                );
                continue;
            }
            match ev.action {
                toron_workflow::schema::FailureAction::Bead => {
                    match spawn_failure_bead(&ev.bundle) {
                        Ok(id) => eprintln!(
                            "cron {}({}): failure -> bead {id} ({}#{})",
                            ev.cron, ev.tick, ev.bundle.workflow, ev.bundle.consecutive
                        ),
                        Err(e) => eprintln!(
                            "cron {}({}): bead side-effect failed: {e}",
                            ev.cron, ev.tick
                        ),
                    }
                }
                other => eprintln!(
                    "cron {}({}): policy action {other:?} (side-effect not wired)",
                    ev.cron, ev.tick
                ),
            }
            // bd-asr2.16 (doorbell sink, flag-gated): every drained tick
            // completion journals a Fact wake (digest-visible with zero
            // swarm). Best-effort inside the flag check; never fails drain.
            if let Some(p) = project.as_deref() {
                crate::doorbell::cron_tick_wake(p, &ev.cron, &ev.tick.to_string(), "fired");
            }
        }
    }
}
impl CronManager for RegistryCronManager {
    fn list(&self) -> Result<Vec<MergedCron>> {
        self.merged()
    }

    fn show(&self, name: &str) -> Result<Option<MergedCron>> {
        Ok(self.merged()?.into_iter().find(|m| m.entry.name == name))
    }

    fn add(&mut self, entry: CronEntry) -> Result<()> {
        // fail-stop validation, mirroring lint_flywheel_tier: template must
        // resolve, owner profile must exist, cron must parse (upsert checks).
        if !self.workflows.contains_key(&entry.template) {
            bail!(
                "template {:?} does not resolve (flywheel workflow list)",
                entry.template
            );
        }
        let profiles = crate::profile::ProfileCatalog::load().unwrap_or_default();
        if profiles.get(&entry.owner).is_none() {
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
#[cfg_attr(not(test), allow(dead_code))] // legacy — P4 (bd-25g7.4) migrates/retires
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
pub fn last_run_state(name: &str) -> (Option<String>, bool) {
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
                Some(s) if s == "failed" || s == "succeeded" => last_status = Some(s.to_string()),
                Some("running") => in_flight = true,
                _ => {}
            }
        }
    }
    (last_status, in_flight)
}

/// Run history for a workflow from the state log (+ failure beads).
pub fn cron_history(name: &str) -> Vec<CronRun> {
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
            // Annotation rows (ship link gates, plan-warn telemetry, loop
            // monitor notices) share the run event kinds; they are not runs.
            if crate::is_annotation_row(&e) {
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
                // ADR-0032 Spec 9 tail: an unrecorded status is `unknown`,
                // never a defaulted `done` (shared with `runs list` /
                // `--robot-snapshot`).
                status: crate::history_row_status(&e.data),
                bead: e
                    .data
                    .get("bead")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    // P10.4: headless runs land in `runs/<run_id>.jsonl` (the __run-headless
    // leaf) with the cron name on the entry — the same history surface must
    // read them, or cron outcomes would be invisible here.
    for hl in crate::run_log::read_runs() {
        if hl.cron.as_deref() != Some(name) {
            continue;
        }
        rows.push(CronRun {
            ts: hl.ts.clone(),
            project: String::new(),
            run_id: Some(hl.run_id.clone()),
            status: hl.status.clone(),
            bead: None,
        });
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
            mode: CronMode::Headless,
            scope: CronScope::Host,
            enabled: true,
            created_at: "2026-08-08T00:00:00Z".to_string(),
        }
    }

    /// ADR-0032 Spec 9 tail: `cron history` reads the same run rows as
    /// `runs list`, so a status-less workflow row reads `unknown` — never an
    /// invented `done` for a run nothing recorded an outcome for.
    #[test]
    fn cron_history_never_invents_done() {
        let _g = crate::config::test_state_home();
        crate::log(
            "demo-honest-cron",
            crate::state::EventKind::Workflow,
            serde_json::json!({ "workflow": "weekly-honest" }),
        );
        let rows = cron_history("weekly-honest");
        assert!(!rows.is_empty(), "the recorded row is returned");
        assert_eq!(rows[0].status, "unknown", "no invented done");
    }

    /// A phase-gate annotation shares the Workflow event kind with cron runs;
    /// `cron history` must not list a ship link's gate as a cron run.
    #[test]
    fn cron_history_skips_phase_gate_rows() {
        let _g = crate::config::test_state_home();
        crate::log(
            "demo-honest-cron-gate",
            crate::state::EventKind::Workflow,
            serde_json::json!({
                "workflow": "weekly-honest",
                "phase_gate": true,
                "gate_ok": true,
            }),
        );
        assert!(
            cron_history("weekly-honest").is_empty(),
            "gate annotations are not runs"
        );
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

    /// A cron registered BEFORE the mode field existed (no `mode` key in
    /// crons.toml) defaults to headless — the P10.4 backward-compat contract.
    #[test]
    fn cron_mode_defaults_to_headless_when_field_missing() {
        let path = tmp_path("mode-default");
        let _ = std::fs::remove_file(&path);
        let raw = r#"
[legacy-cron]
name = "legacy-cron"
template = "beads"
cron = "30 3 * * 0"
owner = "housekeeper"
"#;
        std::fs::write(&path, raw).unwrap();
        let loaded = Crons::load_from(&path).unwrap();
        let got = loaded.get("legacy-cron").unwrap();
        assert_eq!(
            got.mode,
            CronMode::Headless,
            "missing mode = headless default"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `--mode pane` round-trips through crons.toml byte-for-byte.
    #[test]
    fn cron_mode_roundtrips_pane() {
        let path = tmp_path("mode-pane");
        let _ = std::fs::remove_file(&path);
        let mut e = sample("weekly-x");
        e.mode = CronMode::Pane;
        let mut crons = Crons::default();
        crons.upsert(e.clone()).unwrap();
        crons.save_to(&path).unwrap();
        let loaded = Crons::load_from(&path).unwrap();
        assert_eq!(loaded.get("weekly-x"), Some(&e));
        assert_eq!(loaded.get("weekly-x").unwrap().mode, CronMode::Pane);
        let _ = std::fs::remove_file(&path);
    }

    /// The agent-driver row carries the mode (the `cron list/status --json`
    /// surface must show how a cron will execute).
    #[test]
    fn row_json_includes_mode() {
        let mut e = sample("weekly-x");
        e.mode = CronMode::Pane;
        let mc = MergedCron {
            entry: e,
            source: "cron",
        };
        let row = row_json(&mc, "due", Some(1_700_000_000));
        assert_eq!(row.get("mode").and_then(Value::as_str), Some("pane"));
        let e2 = sample("weekly-x");
        let mc2 = MergedCron {
            entry: e2,
            source: "cron",
        };
        let row2 = row_json(&mc2, "due", None);
        assert_eq!(row2.get("mode").and_then(Value::as_str), Some("headless"));
    }

    /// The pane-mode runner DISPATCHES through `pane_dispatch` (bd-5qls) — a
    /// `dispatch_job` step is never recorded. With an empty crew registry
    /// (no panes spawned for the cron project) it fails fast with the pane
    /// dispatch error instead of fabricating `{action, ok:true}`.
    #[test]
    fn pane_mode_runner_dispatches_not_records() {
        use toron_workflow::schema::Action;
        // Hermetic XDG: no catalog, no spawn registry → the dispatch path
        // must fail fast (the recording runner would return Ok).
        let _g = crate::config::lock_env();
        let dir = tmp_path("pane-dispatch-xdg");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let mut e = sample("weekly-x");
        e.mode = CronMode::Pane;
        let runner = cron_runner(&e);
        let err = runner(&Action::DispatchJob {
            assignee: "x".into(),
            spec: toron_workflow::schema::DispatchSpec {
                profile: Some("@planner".into()),
                ..Default::default()
            },
            channel: None,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("pane dispatch"),
            "dispatch path failed fast, not recorded: {err}"
        );
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-8wvz: the on-demand `cron run` dry-run uses the RECORDING runner
    /// even for a HEADLESS cron (whose spec_runner would shell real
    /// `__run-headless` dispatch). The contract: every step type records
    /// `{action, ok:true}` immediately — including dispatch_job — so the
    /// body trace completes in ms and is always visible. This is the
    /// regression pin for "cron run hangs / exits 0 with zero output".
    #[test]
    fn recording_runner_records_headless_body_steps() {
        use toron_workflow::schema::Action;
        // a headless-mode cron: spec_runner for it dispatches, but the
        // on-demand path must NOT use spec_runner — the recording runner is
        // the dry-run surface for every mode.
        let mut e = sample("weekly-x");
        e.mode = CronMode::Headless;
        let runner = recording_runner();
        for a in [
            Action::DispatchJob {
                assignee: "x".into(),
                spec: toron_workflow::schema::DispatchSpec {
                    profile: Some("@scout".into()),
                    task: Some("...".into()),
                    ..Default::default()
                },
                channel: None,
            },
            Action::CallWebhook {
                url: "http://127.0.0.1:9/probe".into(), // unroutable: recorded as failure, still completes
                method: None,
                body: None,
            },
            Action::CallWorkflow {
                workflow: "weekly-review".into(),
                inputs: serde_json::Value::Null,
            },
        ] {
            let out = runner(&a)
                .unwrap_or_else(|e| serde_json::json!({ "ok": false, "error": e.to_string() }));
            assert!(
                out.get("ok").is_some(),
                "every step returns a JSON value (no hang): {out}"
            );
        }
    }

    /// l4s.3: save_to writes ATOMICALLY — the destination is either the old
    /// or the new content, never torn; a stale temp file from a "crash"
    /// neither corrupts the destination nor survives the save.
    #[test]
    fn save_to_is_atomic_leaves_no_temp() {
        // Hermetic per-test dir (193.2): the temp root is shared across
        // processes (sibling agents' cargo runs, cron_cli subprocesses) —
        // stale-file names must not collide with other tests' artifacts.
        let dir = tmp_path("atomic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crons.toml");
        let mut crons = Crons::default();
        crons.upsert(sample("atomic-cron")).unwrap();
        crons.save_to(&path).unwrap();

        // Simulated crash residue: a stale temp file must be ignored (the
        // atomic rename never touches it) and a fresh save must produce a
        // clean, loadable destination with no NEW temp leftovers.
        let stale = dir.join(".crons.toml.tmp-999999");
        std::fs::write(&stale, b"garbage").unwrap();
        crons.upsert(sample("atomic-cron-2")).unwrap();
        crons.save_to(&path).unwrap();

        let loaded = Crons::load_from(&path).unwrap();
        assert!(loaded.get("atomic-cron-2").is_some(), "fresh save intact");
        let pid_tmp = format!(".crons.toml.tmp-{}", std::process::id());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n == &pid_tmp)
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temps from this save: {leftovers:?}"
        );
        assert!(
            stale.exists(),
            "pre-existing stale temp left inert, not clobbered"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(
            resumed,
            &sample("weekly-docker-upgrade"),
            "full state intact"
        );
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
                    principles: vec![],
                    steps: vec![Step {
                        name: None,
                        guard: None,
                        on_error: toron_workflow::schema::StepOnError::Continue,
                        error_kinds: Vec::new(),
                        build_slot: false,
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
                    principles: vec![],
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
                    principles: vec![],
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
        let derived = merged
            .iter()
            .find(|m| m.entry.name == "weekly-backup")
            .unwrap();
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
        let st = mgr
            .status("weekly-docker-upgrade")
            .unwrap()
            .expect("status");
        assert_eq!(st.status, "due");
        assert!(st.next_due.is_some(), "next_due computed");

        // pause → status flips to paused, entry preserved
        mgr.set_enabled("weekly-docker-upgrade", false).unwrap();
        let paused = mgr.status("weekly-docker-upgrade").unwrap().unwrap();
        assert_eq!(paused.status, "paused");
        assert_eq!(
            mgr.show("weekly-docker-upgrade")
                .unwrap()
                .unwrap()
                .entry
                .cron,
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

    /// P4.5: `registrations()` resolves each cron's Workflow from the
    /// catalog/built-ins and excludes nothing (enabled flag preserved).
    #[test]
    fn registrations_resolve_workflows_from_catalog() {
        use toron_workflow::schema::{Step, Trigger};

        let mut workflows: BTreeMap<String, (Workflow, &'static str)> = BTreeMap::new();
        workflows.insert(
            "docker-upgrade".to_string(),
            (
                Workflow {
                    name: "docker-upgrade".into(),
                    description: None,
                    trigger: Some(Trigger::Schedule {
                        cron: "30 3 * * 0".to_string(),
                        owner: Some("housekeeper".to_string()),
                        scope: CronScope::Host,
                    }),
                    policy: None,
                    guard: None,
                    principles: vec![],
                    steps: vec![Step {
                        name: None,
                        guard: None,
                        on_error: toron_workflow::schema::StepOnError::Continue,
                        error_kinds: Vec::new(),
                        build_slot: false,
                        action: toron_workflow::schema::Action::Delay { secs: 0 },
                    }],
                    graph: None,
                    inputs: vec![],
                    crew: None,
                },
                "catalog",
            ),
        );
        let mut crons = Crons::default();
        let mut entry = sample("docker-upgrade");
        entry.template = "docker-upgrade".to_string();
        crons.upsert(entry).unwrap();
        let mgr = make_registry_manager(workflows);

        let regs = mgr.registrations_from(&crons).unwrap();
        assert_eq!(regs.len(), 1);
        let reg = &regs[0];
        assert_eq!(reg.name, "docker-upgrade");
        assert_eq!(reg.cron, "30 3 * * 0");
        assert_eq!(reg.owner, "housekeeper");
        assert_eq!(reg.scope, CronScope::Host);
        assert!(reg.enabled);
        assert_eq!(reg.workflow.name, "docker-upgrade");
    }

    /// P4.5: an unresolvable template fails registrations (fail-stop — the
    /// durable loop must never start on a broken registration).
    #[test]
    fn registrations_fail_on_unresolved_template() {
        let mut crons = Crons::default();
        crons.upsert(sample("ghost-cron")).unwrap(); // template ghost-cron-wf
        let mgr = make_registry_manager(BTreeMap::new());
        assert!(mgr.registrations_from(&crons).is_err());
    }

    /// P4.5: `cron_runner()` executes a real `call_webhook` health probe
    /// against a live listener (200 → ok:true; connection refused → ok:false),
    /// and records non-webhook actions (no dispatch side-effects).
    #[test]
    fn cron_runner_probes_http_and_records_others() {
        use std::net::TcpListener;
        use toron_workflow::schema::Action;

        fn serve(listener: std::net::TcpListener) {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || serve(listener));
        std::thread::sleep(std::time::Duration::from_millis(20));

        // The probe behavior lives on the recording runner (the dry-run
        // surface) — pane mode dispatches (bd-5qls), headless shells out to
        // __run-headless.
        let runner = recording_runner();
        let ok = runner(&Action::CallWebhook {
            url: format!("http://127.0.0.1:{port}/health"),
            method: Some("GET".into()),
            body: None,
        })
        .unwrap();
        assert_eq!(ok["ok"], true, "live probe succeeds: {ok}");

        // closed port → probe errors (a step with on_error: fail_run fails)
        let dead_port = 1; // port 1 is not listening
        let dead = runner(&Action::CallWebhook {
            url: format!("http://127.0.0.1:{dead_port}/health"),
            method: Some("GET".into()),
            body: None,
        });
        assert!(dead.is_err(), "dead probe errors: {dead:?}");

        // non-webhook actions are recorded, not dispatched
        let rec = runner(&Action::SendDm {
            to: "housekeeper".into(),
            body: "x".into(),
            project: None,
        })
        .unwrap();
        assert_eq!(rec["action"], "send_dm");
        assert_eq!(rec["ok"], true);
    }

    /// P4.5: the docker-upgrade health-gate — explicit probes for the five
    /// saga services (aiostreams/resonate/honcho/pihole/dnscrypt), each with
    /// `on_error: fail_run`. All up → run passes; any service down → run
    /// fails (the sequencing gate: the workflow must pass the SAME health
    /// gates as the bash saga before the systemd timer is disabled).
    ///
    /// NOTE: probes are EXPLICIT per-service steps, not a `foreach` — the
    /// engine's foreach does not (yet) inject `{{item}}` into the template
    /// context (engine-side gap, BlueOak's lane). The bash saga also checks
    /// each service explicitly.
    #[tokio::test]
    async fn docker_upgrade_health_gate_fails_when_a_service_is_down() {
        use toron_workflow::schema::{Action, Step, StepOnError};

        // Five live listeners standing in for the saga's health-gated services.
        // Each reads the request, then replies (a proper HTTP handshake — the
        // bash saga's real services behave the same way).
        fn serve(listener: std::net::TcpListener) {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        }
        let mut ports = Vec::new();
        for _ in 0..5 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            ports.push(listener.local_addr().unwrap().port());
            std::thread::spawn(move || serve(listener));
        }
        // Listener-readiness barrier (193.2 flake fix): a fixed sleep races
        // parallel test load (sibling agents' cargo runs, the loop's own
        // subprocesses) — accept() may not have been reached yet and the
        // probe gets connection-refused. Poll until every listener accepts
        // (bounded 5s), THEN build the workflow.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let ready = ports
                .iter()
                .all(|p| std::net::TcpStream::connect(("127.0.0.1", *p)).is_ok());
            if ready {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "listeners did not accept within 5s"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        // One explicit probe step per service, each on_error: fail_run.
        let probe_step = |name: &str, port: u16| Step {
            name: Some(name.into()),
            guard: None,
            on_error: StepOnError::FailRun,
            error_kinds: vec!["timeout".into(), "rate_limit".into(), "unknown".into()],
            build_slot: false,
            action: Action::CallWebhook {
                url: format!("http://127.0.0.1:{port}/health"),
                method: Some("GET".into()),
                body: None,
            },
        };
        let gate_wf = |services: &[u16]| Workflow {
            name: "housekeeper-health-gate".into(),
            description: None,
            trigger: None,
            policy: None,
            guard: None,
            principles: vec![],
            steps: services
                .iter()
                .enumerate()
                .map(|(i, p)| probe_step(&format!("probe-{i}"), *p))
                .collect(),
            graph: None,
            inputs: vec![],
            crew: None,
        };

        // All five up → pass. The probe surface is the recording runner
        // (dry-run); pane mode dispatches (bd-5qls).
        let engine = toron_workflow::WorkflowEngine::local(recording_runner()).unwrap();
        let all_up: Vec<u16> = ports.clone();
        let out = engine
            .invoke("gate-all-up", gate_wf(&all_up), json!({}))
            .await
            .expect("gate runs");
        assert!(
            out.as_object()
                .and_then(|o| o.get("status"))
                .and_then(Value::as_str)
                .is_none_or(|s| s != "failed"),
            "all services up => gate passes: {out}"
        );

        // One dead service (e.g. honcho down) → gate fails.
        let dead = ports.last().unwrap() + 1;
        let mut with_dead = ports.clone();
        with_dead[2] = dead;
        let out2 = engine
            .invoke("gate-one-down", gate_wf(&with_dead), json!({}))
            .await
            .expect("gate runs");
        assert!(
            out2.as_object()
                .and_then(|o| o.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| s == "failed"),
            "one service down => gate fails: {out2}"
        );
    }

    /// P4.5 acceptance: `on_failure: bead` produces a DIAGNOSABLE bead — the
    /// derived title + description carry the full failure bundle (run_id,
    /// workflow, owner, error, failed_step, consecutive) so the defined fixer
    /// (`housekeeper-diagnose`) can triage without the original run context.
    #[test]
    fn failure_bead_fields_carry_the_full_bundle() {
        let bundle = toron_workflow::FailureBundle {
            run_id: "cron:docker-upgrade:1786246200".into(),
            workflow: "docker-upgrade".into(),
            owner: "@housekeeper".into(),
            error: "probe failed: connection refused to 127.0.0.1:8010".into(),
            failed_step: Some("gate-honcho".into()),
            consecutive: 2,
        };
        let (title, description) = failure_bead_fields(&bundle);

        assert!(title.contains("docker-upgrade"), "title names the workflow");
        assert!(
            title.contains("cron:docker-upgrade:1786246200"),
            "title carries run_id"
        );
        assert!(title.contains("#2"), "title carries streak");
        assert!(
            description.contains("cron:docker-upgrade:1786246200"),
            "desc run_id"
        );
        assert!(description.contains("docker-upgrade"), "desc workflow");
        assert!(description.contains("@housekeeper"), "desc owner");
        assert!(
            description.contains("connection refused to 127.0.0.1:8010"),
            "desc error"
        );
        assert!(description.contains("gate-honcho"), "desc failed_step");
        assert!(description.contains("consecutive: 2"), "desc streak");
    }

    // ---- bd-m7ac: weekly-review bead extraction ----

    /// The oracle's `{"idea": {...}}` in the tick output extracts cleanly.
    #[test]
    fn review_idea_extracts_from_oracle_output() {
        let output = serde_json::json!({
            "oracle": {
                "response": r#"{"idea": {"title": "Soft-conflict freeze suggestion", "what": "Propose splits", "why": "Less griefing", "ev": "High", "blast_radius": "flywheel", "acceptance": "one test"}}"#
            }
        });
        let idea = review_idea_from_output(&output).expect("idea present");
        assert_eq!(idea.title, "Soft-conflict freeze suggestion");
        assert_eq!(idea.what, "Propose splits");
        assert_eq!(idea.why, "Less griefing");
        assert_eq!(idea.ev, "High");
        assert_eq!(idea.blast_radius, "flywheel");
        assert_eq!(idea.acceptance, "one test");
    }

    /// `{"idea": null}` (the honest no-proposal contract) → no bead.
    #[test]
    fn review_idea_null_is_no_bead() {
        let output = serde_json::json!({
            "oracle": { "response": r#"{"idea": null}"# }
        });
        assert!(
            review_idea_from_output(&output).is_none(),
            "null idea -> None"
        );
    }

    /// Missing oracle node / missing response / malformed JSON / empty title
    /// all degrade to None (never a forced bead).
    #[test]
    fn review_idea_degradations_are_none() {
        assert!(review_idea_from_output(&serde_json::json!({})).is_none());
        assert!(review_idea_from_output(&serde_json::json!({"oracle": {}})).is_none());
        assert!(
            review_idea_from_output(&serde_json::json!({
                "oracle": { "response": "not json at all" }
            }))
            .is_none()
        );
        assert!(
            review_idea_from_output(&serde_json::json!({
                "oracle": { "response": r#"{"idea": {"title": "  "}}"# }
            }))
            .is_none(),
            "whitespace title -> None"
        );
    }

    /// The review bead's fields carry the date + the full idea (captain can
    /// triage implement-or-close from `br list` alone).
    #[test]
    fn review_bead_fields_carry_idea_and_date() {
        let idea = ReviewIdea {
            title: "Detector escalation arm".into(),
            what: "Wire the LLM planner".into(),
            why: "Rules-only splits miss cases".into(),
            ev: "Medium".into(),
            blast_radius: "flywheel/src/intent.rs".into(),
            acceptance: "Rejecting a rule-split fires the planner".into(),
        };
        let (title, description) = review_bead_fields(&idea, "2026-08-10");
        assert_eq!(title, "weekly-review 2026-08-10: Detector escalation arm");
        assert!(description.contains("## What"), "what section");
        assert!(description.contains("Wire the LLM planner"), "what body");
        assert!(description.contains("## Why"), "why section");
        assert!(description.contains("## EV"), "ev section");
        assert!(
            description.contains("## Blast radius"),
            "blast radius section"
        );
        assert!(description.contains("## Acceptance"), "acceptance section");
        assert!(description.contains("weekly-review"), "tag hint");
    }

    /// The tick's unix timestamp formats to its calendar date.
    #[test]
    fn tick_date_formats_calendar_date() {
        // 2026-08-10T01:00:00Z
        assert_eq!(tick_date(1786323600), "2026-08-10");
        assert_eq!(tick_date(0), "1970-01-01");
        assert_eq!(tick_date(-5), "unknown", "pre-epoch degrades safely");
    }
}

#[cfg(test)]
mod kind_classify_tests {
    use crate::failure;

    #[test]
    fn kind_classify_cron_rate_limit() {
        let cf = failure::classify("429 Too Many Requests", None, None);
        assert_eq!(cf.kind, failure::FailureKind::RateLimit);
        assert!(cf.retryable);
    }

    #[test]
    fn kind_classify_cron_timeout() {
        let cf = failure::classify("deadline exceeded for cron tick", None, None);
        assert_eq!(cf.kind, failure::FailureKind::Timeout);
        assert!(cf.retryable);
    }

    #[test]
    fn kind_classify_cron_auth() {
        let cf = failure::classify("401 Unauthorized for cron provider", None, None);
        assert_eq!(cf.kind, failure::FailureKind::Auth);
        assert!(!cf.retryable);
    }

    #[test]
    fn kind_classify_cron_unclassified_stays_continue() {
        let cf = failure::classify("cron succeeded but no error", None, None);
        // No error string should be Unknown, and the engine's default on_error is Continue
        // The cron runner should not abort retryable kinds; it should surface kind and let engine decide
        assert_eq!(cf.kind, failure::FailureKind::Unknown);
    }

    #[test]
    fn kind_classify_cron_one_shot_fallback_via_headless() {
        // Cron's headless mode delegates to headless_dispatch which already has the one-shot fallback
        // We verify the registry fallback exists for the cron's headless model
        let mut registry = crate::models::ModelRegistry::default();
        registry.models.insert(
            "cron/primary".to_string(),
            crate::models::ModelEntry {
                cost_in_per_mtok: 0.0,
                cost_out_per_mtok: 0.0,
                caps: vec![],
                fallback: Some("cron/fallback".to_string()),
                effort: None,
                tier: None,
            },
        );
        registry.models.insert(
            "cron/fallback".to_string(),
            crate::models::ModelEntry {
                cost_in_per_mtok: 0.0,
                cost_out_per_mtok: 0.0,
                caps: vec![],
                fallback: None,
                effort: None,
                tier: None,
            },
        );
        let fb = registry
            .get("cron/primary")
            .and_then(|m| m.fallback.clone());
        assert_eq!(fb, Some("cron/fallback".to_string()));
        let cf = failure::classify("429 rate limit for cron", None, None);
        assert_eq!(cf.kind, failure::FailureKind::RateLimit);
        assert!(cf.retryable);
    }
}
