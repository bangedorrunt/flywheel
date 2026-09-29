//! Unified workflow catalog (ADR-0005 Spec 2 / P3.2).
//!
//! One vocabulary, one engine. This module is the flywheel-side catalog that
//! feeds `WorkflowEngine::invoke` (the P3.1 cargo edge):
//!
//! - **Canonical home:** `~/.config/flywheel/workflows/*.yaml` — unified
//!   toron-workflow schema (parse via `toron_workflow::parse_workflow`).
//! - **Legacy absorbed, not deprecated:** `workflows.toml`
//!   (`WorkflowStep::Send`/`Wait`) + `pipelines.toml` (a chain of workflow
//!   names) are read through the SAME catalog surface. `Send{to,stage}` →
//!   a `dispatch_job` step; `Wait{until}` → a wait step; a pipeline → a
//!   workflow of `call_workflow` steps.
//!
//! User-tier seeds prefer one file per entry: workflows remain YAML because the
//! workflow schema contains unit values, while stages use `stages/<name>.toml`.
//! Existing `stages.toml` remains a compatibility input and per-entry stages
//! win when both define the same name.
//!
//! **Honesty rule (P3.2):** if a legacy `WorkflowStep` variant does not map
//! 1:1 to a unified step, [`absorb`] records a `MappingNote` — it is NEVER
//! silently dropped. The current mapping:
//!
//! | legacy | unified step | 1:1? |
//! |---|---|---|
//! | `Send{to, stage, msg}` | `dispatch_job` (assignee=`to`, spec carries the resolved stage prompt / raw msg) | YES (herdr-direct semantics preserved via the engine's injected runner) |
//! | `Wait{until}` | `delay` (no-op shell; the engine's `spawn{await}` binding is P4) | NO — flagged, run continues |
//! | pipeline (chain) | workflow of `call_workflow` steps (closed action, engine sub-run) | YES |
//!
//! `WorkflowEngine::invoke` is the single execution door: flywheel's legacy
//! `run_workflow`/`run_pipeline` become thin callers of it (main.rs wiring is
//! coordinated with the P1 lane — see AGENT notes).
//!
//! governed-by: ADR-0005 Spec 2, P3.2

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use toron_workflow::schema::{Action, InputDecl, Step, Workflow, parse_workflow};

use crate::config::config_dir;

/// The workflow NAME that resolves at each merge site, plus its source tag
/// ("builtin" | "catalog") — the vocabulary `workflow list` reports.
pub type ResolvedWorkflow = (Workflow, &'static str);

/// One user-tier seed or drift result. The resolver remains layered; this is
/// an operator report, never a second catalog path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogEntryStatus {
    pub kind: String,
    pub name: String,
    pub status: String,
    pub source: String,
    pub compose_token: bool,
}

/// Seed copies compiled-in defaults into the user tier without replacing an
/// authored copy. Re-running is therefore safe and makes the user's override
/// surface explicit.
/// Replace a builtin copy, keeping the old bytes recoverable.
///
/// bd-3o5t follow-on: this command can rewrite every file in the operator's
/// real config dir, so a confirmation prompt over an irreversible action is not
/// a confirmation, it is a formality. The previous content goes to `<name>.bak`
/// FIRST, always, including on the overwrite path an operator did not ask for.
fn overwrite_with_backup(path: &Path, contents: &str) -> Result<()> {
    if let Ok(prev) = std::fs::read_to_string(path) {
        let bak = path.with_extension(
            path.extension()
                .map(|e| format!("{}.bak", e.to_string_lossy()))
                .unwrap_or_else(|| "bak".into()),
        );
        std::fs::write(&bak, prev).with_context(|| format!("backup {}", bak.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))
}

/// Seed the user tier. `scope` decides what may be replaced; `dry_run` reports
/// the plan without touching the disk.
///
/// "Stale" is read from [`catalog_drift`] rather than recomputed, deliberately:
/// bd-3o5t was a bug where seed and drift each had their own comparison and
/// agreed with each other while both were wrong. One source of truth, so
/// `--overwrite stale` can only ever repair exactly what `workflow drift`
/// reports.
pub fn seed_user_catalog_with(
    config_root: &Path,
    scope: OverwriteScope,
    dry_run: bool,
) -> Result<CatalogSeedReport> {
    let mut report = CatalogSeedReport {
        dry_run,
        overwrite_scope: scope,
        ..Default::default()
    };
    let drift = catalog_drift(config_root)?;
    let is_stale = |kind: &str, name: &str| {
        drift
            .iter()
            .any(|r| r.kind == kind && r.name == name && r.status == "stale")
    };
    // `All` replaces a builtin-named copy whether or not drift calls it stale.
    let may_replace = |kind: &str, name: &str| match scope {
        OverwriteScope::None => false,
        OverwriteScope::Stale => is_stale(kind, name),
        OverwriteScope::All => true,
    };

    let workflow_dir = config_root.join("workflows");
    for (name, yaml) in crate::builtin_workflows::builtin_workflows_yaml() {
        let path = workflow_dir.join(format!("{name}.yaml"));
        if !path.exists() {
            if !dry_run {
                std::fs::create_dir_all(&workflow_dir)
                    .with_context(|| format!("create {}", workflow_dir.display()))?;
                // bd-3o5t: write the compiled-in SOURCE TEXT. Going through
                // `serde_yaml::to_string(&Workflow)` is lossy — `CrewSpec` is
                // `#[serde(untagged)]` with a unit `Auto` variant, so serde
                // emits `crew: null` and the copy lost its crew. The user tier
                // wins over the builtin, so that silently made crew workflows
                // headless.
                std::fs::write(&path, yaml).with_context(|| format!("write {}", path.display()))?;
            }
            report.created_workflows.push(name.to_string());
        } else if may_replace("workflow", name) {
            if dry_run {
                report.planned_overwrites.push(name.to_string());
            } else {
                overwrite_with_backup(&path, yaml)?;
                report.overwritten_workflows.push(name.to_string());
            }
        } else {
            report.kept_workflows.push(name.to_string());
        }
    }

    let legacy_stage_path = config_root.join("stages.toml");
    let legacy = if legacy_stage_path.is_file() {
        let raw = std::fs::read_to_string(&legacy_stage_path)
            .with_context(|| format!("read {}", legacy_stage_path.display()))?;
        toml::from_str::<UserStages>(&raw)
            .with_context(|| format!("parse {}", legacy_stage_path.display()))?
    } else {
        UserStages::default()
    };
    let stage_dir = config_root.join("stages");
    let compiled = crate::stages::StageLibrary::compiled_in();
    for name in compiled.list() {
        let stage = compiled
            .get_stage(name)
            .expect("compiled stage list and stage map agree");
        let path = stage_dir.join(format!("{name}.toml"));
        if path.exists() {
            if may_replace("stage", name) {
                let rendered = crate::stages::render_stage_entry(stage)?;
                if dry_run {
                    report.planned_overwrites.push(name.to_string());
                } else {
                    overwrite_with_backup(&path, &rendered)?;
                    report.overwritten_stages.push(name.to_string());
                }
            } else {
                report.kept_stages.push(name.to_string());
            }
        } else if legacy.stages.get(name).is_some_and(|user| user == stage) {
            if !dry_run {
                crate::stages::upsert_stage_entry(&path, stage)?;
            }
            report.created_stages.push(name.to_string());
        } else if legacy.stages.contains_key(name) {
            // An authored aggregate entry remains authoritative; do not shadow
            // it with a generated per-entry copy.
            report.kept_stages.push(name.to_string());
        } else {
            if !dry_run {
                crate::stages::upsert_stage_entry(&path, stage)?;
            }
            report.created_stages.push(name.to_string());
        }
    }
    Ok(report)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CatalogSeedReport {
    pub created_workflows: Vec<String>,
    pub kept_workflows: Vec<String>,
    pub created_stages: Vec<String>,
    pub kept_stages: Vec<String>,
    /// Builtin-named copies this run replaced, and the `.bak` written first.
    pub overwritten_workflows: Vec<String>,
    pub overwritten_stages: Vec<String>,
    /// What a `--dry-run` WOULD have replaced. Populated even on a dry run,
    /// because a plan you cannot see is not a plan.
    pub planned_overwrites: Vec<String>,
    pub dry_run: bool,
    pub overwrite_scope: OverwriteScope,
}

/// How much `workflow seed` is allowed to replace in the user tier.
///
/// bd-3o5t follow-on. The default is [`None`] on purpose: seeding exists to
/// FILL GAPS, and an agent may run it on any session start, so it must never be
/// able to clobber local edits as a side effect. Replacing a file is a decision
/// a human makes once, with a `--dry-run` first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum OverwriteScope {
    /// Create what is missing; touch nothing that exists. The default.
    #[default]
    None,
    /// Replace only builtin-named copies that drift reports as `stale` — the
    /// exact repair for a user tier written by a lossy serializer.
    Stale,
    /// Replace every builtin-named copy, whether or not it is stale. Still never
    /// touches a name the binary does not ship, so an authored workflow is safe
    /// at any scope.
    All,
}

impl OverwriteScope {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "none" => Ok(Self::None),
            "stale" => Ok(Self::Stale),
            "all" => Ok(Self::All),
            other => anyhow::bail!(
                "unknown --overwrite scope {other:?} (expected: none | stale | all). \
                 `none` only fills gaps, `stale` repairs drifted builtin copies, \
                 `all` restores every builtin-named copy"
            ),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Stale => "stale",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct UserStages {
    #[serde(default)]
    stages: BTreeMap<String, crate::stages::Stage>,
}

fn workflow_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    let mut yaml = Vec::new();
    let mut toml = Vec::new();
    for entry in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
        match entry.extension().and_then(|ext| ext.to_str()) {
            Some("yaml" | "yml") => yaml.push(entry),
            Some("toml") => toml.push(entry),
            _ => {}
        }
    }
    yaml.sort();
    toml.sort();
    yaml.extend(toml);
    Ok(yaml)
}

fn parse_workflow_path(path: &Path) -> Result<Workflow> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => {
            toml::from_str(&raw).with_context(|| format!("parse workflow TOML {}", path.display()))
        }
        _ => parse_workflow(&raw).with_context(|| format!("parse workflow {}", path.display())),
    }
}

fn load_user_workflows(config_root: &Path) -> Result<BTreeMap<String, (Workflow, String, bool)>> {
    let dir = config_root.join("workflows");
    if !dir.is_dir() {
        return Ok(BTreeMap::new());
    }
    let mut out = BTreeMap::new();
    for path in workflow_paths(&dir)? {
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let is_toml = path.extension().is_some_and(|ext| ext == "toml");
        let workflow = if is_toml {
            toml::from_str(&raw)
                .with_context(|| format!("parse workflow TOML {}", path.display()))?
        } else {
            parse_workflow(&raw).with_context(|| format!("parse workflow {}", path.display()))?
        };
        out.insert(workflow.name.clone(), (workflow, raw, is_toml));
    }
    Ok(out)
}

fn load_user_stages(config_root: &Path) -> Result<BTreeMap<String, crate::stages::Stage>> {
    let mut out = BTreeMap::new();
    let aggregate = config_root.join("stages.toml");
    if aggregate.is_file() {
        let raw = std::fs::read_to_string(&aggregate)
            .with_context(|| format!("read {}", aggregate.display()))?;
        let file: UserStages =
            toml::from_str(&raw).with_context(|| format!("parse {}", aggregate.display()))?;
        out.extend(file.stages);
    }
    let entries = config_root.join("stages");
    if entries.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&entries)
            .with_context(|| format!("read {}", entries.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let stage: crate::stages::Stage = toml::from_str(&raw)
                .with_context(|| format!("parse per-entry stage {}", path.display()))?;
            out.insert(stage.name.clone(), stage);
        }
    }
    Ok(out)
}

/// Compare the user tier with the compiled-in defaults. A user copy always
/// wins resolution, while `stale` makes the intentional shadow visible.
pub fn catalog_drift(config_root: &Path) -> Result<Vec<CatalogEntryStatus>> {
    let builtins = crate::builtin_workflows::builtin_workflows();
    let user_workflows = load_user_workflows(config_root)?;
    let mut rows = Vec::new();
    for (name, builtin) in &builtins {
        // bd-3o5t: compare against the compiled-in SOURCE TEXT, the same bytes
        // seed now writes. Comparing against a re-serialization made drift and
        // seed agree with each other while both disagreed with the catalog, so
        // a crew-stripped copy read as `current`. `current` now means exactly
        // one thing: this file is the builtin's text, unmodified.
        let builtin_yaml =
            crate::builtin_workflows::builtin_workflow_yaml(name).map(str::to_string);
        let status = match user_workflows.get(name) {
            None => "missing",
            // bd-3o5t: the two tiers are compared two different ways ON PURPOSE.
            // A `.yaml` copy is a verbatim dump of the builtin's source text, so
            // byte equality is the right test and catches a hand-edit. A `.toml`
            // copy is a re-encoding, so its text can never match the YAML dump;
            // comparing it byte-wise would mark every TOML copy `stale` forever.
            // For TOML the honest test is semantic: does it parse to the same
            // Workflow the builtin parses to? That is the same notion the stage
            // branch below already used, and it is the one that actually
            // detects a dropped crew.
            Some((user, source, is_toml))
                if if *is_toml {
                    // Compared through `Debug`, NOT through serde.
                    // `CrewSpec` is `#[serde(untagged)]` with a unit `Auto`
                    // variant, and serde emits `null` for a unit variant in an
                    // untagged enum — so a serde round-trip reports a real
                    // `crew: auto` as `crew: null`, and a copy that genuinely
                    // lost its crew as identical to one that kept it. `Debug`
                    // prints the actual typed value, so the comparison sees the
                    // crew. (`Workflow` has no `PartialEq`; deriving one would
                    // mean editing toron-workflow's schema, and the YAML branch
                    // above is the one seed actually uses.)
                    format!("{user:?}") == format!("{builtin:?}")
                } else {
                    builtin_yaml
                        .as_deref()
                        .is_some_and(|yaml| yaml.trim_end() == source.trim_end())
                } =>
            {
                "current"
            }
            Some(_) => "stale",
        };
        rows.push(CatalogEntryStatus {
            kind: "workflow".into(),
            name: name.clone(),
            status: status.into(),
            source: if user_workflows.contains_key(name) {
                "user"
            } else {
                "builtin"
            }
            .into(),
            compose_token: crate::builtin_workflows::is_compose_token(name),
        });
    }
    for name in user_workflows.keys() {
        if !builtins.contains_key(name) {
            rows.push(CatalogEntryStatus {
                kind: "workflow".into(),
                name: name.clone(),
                status: "custom".into(),
                source: "user".into(),
                compose_token: false,
            });
        }
    }

    let compiled = crate::stages::StageLibrary::compiled_in();
    let user_stages = load_user_stages(config_root)?;
    for name in compiled.list() {
        let status = match user_stages.get(name) {
            None => "missing",
            Some(user) if Some(user) == compiled.get_stage(name) => "current",
            Some(_) => "stale",
        };
        rows.push(CatalogEntryStatus {
            kind: "stage".into(),
            name: name.to_string(),
            status: status.into(),
            source: if user_stages.contains_key(name) {
                "user"
            } else {
                "builtin"
            }
            .into(),
            compose_token: false,
        });
    }
    for name in user_stages.keys() {
        if !compiled.has(name) {
            rows.push(CatalogEntryStatus {
                kind: "stage".into(),
                name: name.clone(),
                status: "custom".into(),
                source: "user".into(),
                compose_token: false,
            });
        }
    }
    rows.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
    Ok(rows)
}

/// bd-xmq5: THE one builtin+user(+project) merge. Previously copy-pasted at
/// ~7 sites with inconsistent tiers (some user-only `load()`, some
/// `load_layered`), so project-tier overrides applied on some paths and were
/// silently ignored on others. Precedence everywhere now: seed builtins <
/// user catalog < project tier (same Ord as StageLibrary tiers). The
/// `&'static str` source tag is the `workflow list` vocabulary.
///
/// `project_root` participates the project tier when given; callers without
/// project context pass `None` (user tier only — the old `load()` shape).
pub fn all_workflows_merged(
    project_root: Option<&Path>,
) -> Result<BTreeMap<String, ResolvedWorkflow>> {
    let mut out: BTreeMap<String, ResolvedWorkflow> = BTreeMap::new();
    for (name, wf) in crate::builtin_workflows::builtin_workflows() {
        out.insert(name, (wf, "builtin"));
    }
    let cat = match project_root {
        Some(root) => WorkflowCatalog::load_layered(Some(root))?,
        None => WorkflowCatalog::load()?,
    };
    for (name, wf) in cat.workflows {
        // Catalog (user or project tier) wins over a same-named builtin —
        // the catalog is the home; built-ins are zero-config defaults.
        out.insert(name, (wf, "catalog"));
    }
    Ok(out)
}

/// Typed catalog failures (P5). New fallible pick paths return this;
/// entrypoints map it to LoudMiss + nonzero exit instead of panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    Unknown(String),
    Ambiguous(String, Vec<String>),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(n) => write!(f, "unknown workflow '{n}'; see `workflow list`"),
            Self::Ambiguous(n, hits) => write!(f, "ambiguous workflow prefix '{n}': {hits:?}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Exact-then-unique-prefix pick over the merged catalog (P5).
/// ponytail: prefix convenience only, exact names always win.
pub fn pick_workflow(
    merged: &BTreeMap<String, ResolvedWorkflow>,
    name: &str,
) -> Result<ResolvedWorkflow, CatalogError> {
    // ADR-0016: catalog identifiers are validated at the boundary while
    // preserving prefix lookup semantics and the original error text.
    if crate::kernel::WorkflowName::parse(name.to_string()).is_err() {
        return Err(CatalogError::Unknown(name.into()));
    }
    if let Some(found) = merged.get(name) {
        return Ok(found.clone());
    }
    let mut hits: Vec<String> = merged
        .keys()
        .filter(|k| k.starts_with(name))
        .cloned()
        .collect();
    hits.sort();
    match hits.len() {
        0 => Err(CatalogError::Unknown(name.into())),
        1 => Ok(merged.get(&hits[0]).cloned().expect("prefix hit resolves")),
        _ => Err(CatalogError::Ambiguous(name.into(), hits)),
    }
}

/// [`all_workflows_merged`] without the source tag — the plain name →
/// workflow map most resolvers want.
pub fn all_workflows_plain(project_root: Option<&Path>) -> Result<BTreeMap<String, Workflow>> {
    Ok(all_workflows_merged(project_root)?
        .into_iter()
        .map(|(k, (wf, _))| (k, wf))
        .collect())
}

/// The sub-workflow resolver closure used by engine call sites: catalog
/// (user+project when `project_root` given) first, builtins as fallback.
/// Errors inside `load` degrade to builtins-only (the engine resolver must
/// never hard-fail on a missing config dir).
pub fn subworkflow_resolver(
    project_root: Option<&Path>,
) -> impl Fn(&str) -> Option<Workflow> + use<> {
    let root = project_root.map(|p| p.to_path_buf());
    move |sub: &str| {
        let cat = match &root {
            Some(r) => WorkflowCatalog::load_layered(Some(r)).ok(),
            None => WorkflowCatalog::load().ok(),
        };
        cat.and_then(|c| c.workflows.get(sub).cloned()).or_else(|| {
            crate::builtin_workflows::builtin_workflows()
                .get(sub)
                .cloned()
        })
    }
}

/// The legacy `workflows.toml` step shapes (mirrored from flywheel's
/// `config::WorkflowStep` — the catalog parses the legacy file itself so the
/// absorber is self-contained, no dependency on the live config struct).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum LegacyStep {
    Send {
        to: String,
        #[serde(default)]
        stage: Option<String>,
        #[serde(default)]
        msg: Option<String>,
    },
    Wait {
        until: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
}

/// Legacy `workflows.toml`: name → ordered steps.
#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyWorkflows {
    #[serde(flatten)]
    workflows: BTreeMap<String, Vec<LegacyStep>>,
}

/// Legacy `pipelines.toml`: name → ordered workflow names.
#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyPipelines {
    #[serde(flatten)]
    pipelines: BTreeMap<String, Vec<String>>,
}

/// A mapping caveat for a legacy step that does not map 1:1 to a unified
/// step. The catalog NEVER silently drops legacy behavior — every such step
/// produces a note surfaced to the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingNote {
    /// Legacy source: `workflows.toml:<workflow>[:<index>]` or
    /// `pipelines.toml:<pipeline>[:<index>]`.
    pub source: String,
    /// What the legacy step was.
    pub legacy: String,
    /// Why the mapping is not 1:1.
    pub reason: String,
}

/// The unified catalog view: canonical workflows + absorbed legacy, plus any
/// mapping notes.
#[derive(Debug, Clone, Default)]
pub struct WorkflowCatalog {
    /// Unified workflows (canonical YAML + absorbed legacy), name → workflow.
    pub workflows: BTreeMap<String, Workflow>,
    /// Non-1:1 mapping caveats (never silently dropped).
    pub notes: Vec<MappingNote>,
}

impl WorkflowCatalog {
    /// Load the full catalog from the default config dir (canonical
    /// `workflows/*.yaml` + absorbed legacy `workflows.toml` /
    /// `pipelines.toml`).
    pub fn load() -> Result<Self> {
        Self::load_from(config_dir())
    }

    /// [`load`] plus the PROJECT tier (ADR-0022 §3.8 / W6):
    /// `<project>/.flywheel/workflows/*.yaml` shadows the user catalog,
    /// which shadows built-ins. `project_root` is the project directory
    /// (pass None where no project context exists).
    pub fn load_layered(project_root: Option<&Path>) -> Result<Self> {
        Self::load_layered_with(config_dir(), project_root)
    }

    /// [`load_layered`] against an explicit config root — the hermetic test
    /// surface (no process-global env mutation).
    pub fn load_layered_with(config_root: PathBuf, project_root: Option<&Path>) -> Result<Self> {
        let mut catalog = Self::load_from(config_root)?;
        if let Some(root) = project_root {
            let dir = root.join(".flywheel").join("workflows");
            if dir.is_dir() {
                for path in workflow_paths(&dir)? {
                    let wf = parse_workflow_path(&path)?;
                    catalog.workflows.insert(wf.name.clone(), wf);
                }
            }
        }
        Ok(catalog)
    }

    /// Load the full catalog from an explicit config root (the config dir
    /// containing `workflows/`, `workflows.toml`, `pipelines.toml`). Tests
    /// pass a hermetic temp dir — no process-global env mutation.
    pub fn load_from(config_root: PathBuf) -> Result<Self> {
        let mut catalog = Self {
            workflows: BTreeMap::new(),
            notes: Vec::new(),
        };
        catalog.load_canonical(&config_root)?;
        catalog.absorb(&config_root)?;
        Ok(catalog)
    }

    /// Load canonical `workflows/*.{toml,yaml,yml}` (unified schema).
    fn load_canonical(&mut self, config_root: &Path) -> Result<()> {
        let dir = config_root.join("workflows");
        if !dir.is_dir() {
            return Ok(());
        }
        for path in workflow_paths(&dir)? {
            let wf = parse_workflow_path(&path)?;
            self.workflows.insert(wf.name.clone(), wf);
        }
        Ok(())
    }

    /// Absorb legacy `workflows.toml` (Send/Wait) + `pipelines.toml` (chains)
    /// through the SAME catalog surface. Legacy name collisions with canonical
    /// YAML are overwritten by the canonical file (canonical is the home);
    /// a mapping note records the shadowed legacy entry.
    fn absorb(&mut self, config_root: &Path) -> Result<()> {
        // Legacy workflows.toml
        let wf_path = config_root.join("workflows.toml");
        if wf_path.exists() {
            let raw = std::fs::read_to_string(&wf_path)
                .with_context(|| format!("read {}", wf_path.display()))?;
            let legacy: LegacyWorkflows =
                toml::from_str(&raw).with_context(|| format!("parse {}", wf_path.display()))?;
            for (name, steps) in legacy.workflows {
                if self.workflows.contains_key(&name) {
                    self.notes.push(MappingNote {
                        source: "workflows.toml".into(),
                        legacy: name.clone(),
                        reason: "shadowed by canonical workflows/*.yaml entry".into(),
                    });
                    continue;
                }
                let (wf, notes) = absorb_workflow_steps(&name, &steps);
                self.workflows.insert(name, wf);
                self.notes.extend(notes);
            }
        }

        // Legacy pipelines.toml
        let pipe_path = config_root.join("pipelines.toml");
        if pipe_path.exists() {
            let raw = std::fs::read_to_string(&pipe_path)
                .with_context(|| format!("read {}", pipe_path.display()))?;
            let legacy: LegacyPipelines =
                toml::from_str(&raw).with_context(|| format!("parse {}", pipe_path.display()))?;
            for (name, chain) in legacy.pipelines {
                if self.workflows.contains_key(&name) {
                    self.notes.push(MappingNote {
                        source: "pipelines.toml".into(),
                        legacy: name.clone(),
                        reason: "shadowed by canonical workflows/*.yaml entry".into(),
                    });
                    continue;
                }
                let (wf, notes) = absorb_pipeline(&name, &chain);
                self.workflows.insert(name, wf);
                self.notes.extend(notes);
            }
        }
        Ok(())
    }
}

/// Map a legacy `WorkflowStep` list into a unified `Workflow`. Steps that map
/// 1:1 become unified steps; non-1:1 steps become flagged shell steps + a
/// [`MappingNote`].
fn absorb_workflow_steps(name: &str, steps: &[LegacyStep]) -> (Workflow, Vec<MappingNote>) {
    let mut notes = Vec::new();
    let mut unified: Vec<Step> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let source = format!("workflows.toml:{name}:{i}");
        match step {
            LegacyStep::Send { to, stage, msg } => {
                // Send{to,stage,msg} -> dispatch_job. The stage prompt / raw
                // message is carried in the spec so the injected ActionRunner
                // (the herdr-direct surface) preserves semantics. `stage` is
                // a contract field; the legacy `to`/`msg` pair rides in
                // `extra` (bd-dispatch-spec-untyped-0iud).
                let spec = toron_workflow::schema::DispatchSpec {
                    stage: stage.clone(),
                    extra: [
                        ("to".to_string(), serde_json::Value::String(to.clone())),
                        (
                            "msg".to_string(),
                            msg.clone()
                                .map(serde_json::Value::String)
                                .unwrap_or(serde_json::Value::Null),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                };
                unified.push(Step {
                    name: Some(format!("send-{i}")),
                    guard: None,
                    on_error: Default::default(),
                    build_slot: false,
                    action: Action::DispatchJob {
                        assignee: to.clone(),
                        spec,
                        channel: None,
                    },
                });
            }
            LegacyStep::Wait { until, timeout_ms } => {
                // Wait{until} -> the engine's spawn-await binding is a P4
                // subsystem. Until then a delay shell keeps the run advancing;
                // flagged, never dropped.
                notes.push(MappingNote {
                    source: source.clone(),
                    legacy: format!("wait {until}"),
                    reason: "Wait maps to the P4 spawn{await} binding; delay shell used until then"
                        .into(),
                });
                unified.push(Step {
                    name: Some(format!("wait-{i}")),
                    guard: None,
                    on_error: Default::default(),
                    build_slot: false,
                    action: Action::Delay {
                        // Round UP (is1.5): integer division truncates a
                        // sub-second wait to 0s — a silent no-op. 500ms → 1s
                        // keeps the effective wait; only a 0/absent timeout
                        // maps to 0.
                        secs: timeout_ms.map(|ms| ms.div_ceil(1000)).unwrap_or(0),
                    },
                });
            }
        }
    }
    let wf = Workflow {
        name: name.to_string(),
        description: Some("absorbed legacy workflows.toml workflow".to_string()),
        trigger: None,
        guard: None,
        steps: unified,
        graph: None,
        crew: None,
        policy: None,
        inputs: vec![InputDecl {
            name: "goal".into(),
            default: None,
        }],
        principles: vec![],
    };
    (wf, notes)
}

/// Map a legacy pipeline (chain of workflow names) into a workflow whose
/// steps are the closed `call_workflow` action, resolved as sub-runs by the
/// engine (ADR-0032 Spec 9 tail, bd-40d1). The mapping is exact — one step
/// per chained workflow — so no flag note is emitted.
fn absorb_pipeline(name: &str, chain: &[String]) -> (Workflow, Vec<MappingNote>) {
    let mut steps = Vec::new();
    for (i, wf_name) in chain.iter().enumerate() {
        steps.push(Step {
            name: Some(format!("call-{i}")),
            guard: None,
            on_error: Default::default(),
            build_slot: false,
            action: Action::CallWorkflow {
                workflow: wf_name.clone(),
                inputs: serde_json::Value::Null,
            },
        });
    }
    let wf = Workflow {
        name: name.to_string(),
        description: Some("absorbed legacy pipelines.toml chain".into()),
        trigger: None,
        guard: None,
        steps,
        graph: None,
        crew: None,
        policy: None,
        inputs: vec![InputDecl {
            name: "goal".into(),
            default: None,
        }],
        principles: vec![],
    };
    (wf, Vec::new())
}

/// Write one workflow as canonical catalog YAML — the W6 shared writer
/// (§3.7 T3 / §3.8): `workflow new` and compose `--keep` produce IDENTICAL
/// file shapes, so both paths lint identically. Overwrites a same-name entry.
pub fn write_workflow_yaml(dir: &Path, wf: &Workflow) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.yaml", wf.name));
    let raw = serde_yaml::to_string(wf).context("serialize workflow yaml")?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Parse the provenance tokens a kept workflow carries in its description
/// (`origin: composed; from: <parts>; first-run: <ts>; author: <pin>`).
/// Returns (origin, from, first_run, author) — None where absent. The
/// keep-without-green-run refusal + provenance tests ride on these tokens.
#[cfg(test)]
pub fn parse_provenance(description: &str) -> Option<(String, String, String, String)> {
    let start = description.find("[origin:")?;
    let end = description[start..].find(']')? + start;
    let body = &description[start + 1..end];
    let mut origin = None;
    let mut from = None;
    let mut first_run = None;
    let mut author = None;
    for part in body.split(';') {
        let Some((k, v)) = part.split_once(':') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim() {
            "origin" => origin = Some(v),
            "from" => from = Some(v),
            "first-run" => first_run = Some(v),
            "author" => author = Some(v),
            _ => {}
        }
    }
    Some((
        origin?,
        from.unwrap_or_default(),
        first_run.unwrap_or_default(),
        author.unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A hermetic config root (no process-global env mutation).
    fn config_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fw-cat-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &std::path::Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Canonical `workflows/*.yaml` loads into the catalog.
    #[test]
    fn canonical_yaml_loads() {
        let dir = config_root("canonical");
        write(
            &dir.join("workflows/triage.yaml"),
            "name: triage\ntrigger:\n  type: message_posted\nsteps:\n  - name: ack\n    action: send_dm\n    to: pi\n    body: received\n",
        );
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        assert!(catalog.workflows.contains_key("triage"));
        assert_eq!(catalog.workflows["triage"].name, "triage");
        let _ = fs::remove_dir_all(&dir);
    }

    /// is1.5: absorbed `Wait` timeouts round UP — a sub-second wait must not
    /// truncate to a 0s delay (silent no-op).
    #[test]
    fn absorbed_wait_timeout_rounds_up_not_truncates() {
        let dir = config_root("wait-round");
        write(
            &dir.join("workflows.toml"),
            r#"[[sub-second]]
type = "wait"
until = "ready"
timeout_ms = 500
"#,
        );
        // absorb directly (self-contained): the step shapes parse from a
        // legacy file written above.
        let raw = std::fs::read_to_string(dir.join("workflows.toml")).unwrap();
        let legacy: LegacyWorkflows = toml::from_str(&raw).unwrap();
        let steps = legacy.workflows.get("sub-second").unwrap();
        let (wf, _notes) = absorb_workflow_steps("sub-second", steps);
        match &wf.steps[0].action {
            Action::Delay { secs } => {
                assert_eq!(*secs, 1, "500ms rounds UP to 1s, never 0");
            }
            other => panic!("wait absorbed to Delay, got {other:?}"),
        }
        // exact-second and absent-timeout boundaries.
        let steps2 = vec![LegacyStep::Wait {
            until: "x".into(),
            timeout_ms: Some(1500),
        }];
        let (wf2, _) = absorb_workflow_steps("w2", &steps2);
        match &wf2.steps[0].action {
            Action::Delay { secs } => assert_eq!(*secs, 2, "1500ms -> 2s"),
            other => panic!("{other:?}"),
        }
        let steps3 = vec![LegacyStep::Wait {
            until: "x".into(),
            timeout_ms: None,
        }];
        let (wf3, _) = absorb_workflow_steps("w3", &steps3);
        match &wf3.steps[0].action {
            Action::Delay { secs } => assert_eq!(*secs, 0, "absent timeout -> 0 (no wait)"),
            other => panic!("{other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Legacy `workflows.toml` Send/Wait absorbs into unified steps.
    #[test]
    fn legacy_workflows_toml_absorbs() {
        let dir = config_root("legacy-wf");
        write(
            &dir.join("workflows.toml"),
            "[[daily]]\ntype = \"send\"\nto = \"oc\"\nstage = \"p1\"\n[[daily]]\ntype = \"wait\"\nuntil = \"done\"\n",
        );
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        let wf = catalog.workflows.get("daily").expect("absorbed");
        assert_eq!(wf.steps.len(), 2, "Send + Wait absorbed as 2 steps");
        assert!(matches!(wf.steps[0].action, Action::DispatchJob { .. }));
        assert!(matches!(wf.steps[1].action, Action::Delay { .. }));
        assert!(
            catalog
                .notes
                .iter()
                .any(|n| n.source.contains("daily") && n.legacy.contains("wait")),
            "Wait non-1:1 mapping is FLAGGED, never dropped: {:?}",
            catalog.notes
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// ADR-0032 Spec 9 tail (bd-40d1) RED: a legacy pipeline absorbs to real
    /// `call_workflow` steps — exact mapping, no DispatchJob shim, no flag.
    #[test]
    fn run_ledger_truth_absorb_pipeline_emits_call_workflow() {
        let (wf, notes) = absorb_pipeline("chain", &["a".to_string(), "b".to_string()]);
        assert_eq!(wf.steps.len(), 2, "one step per chained workflow");
        for (step, want) in wf.steps.iter().zip(["a", "b"]) {
            match &step.action {
                Action::CallWorkflow { workflow, inputs } => {
                    assert_eq!(workflow, want);
                    assert!(inputs.is_null(), "no inputs to forward: {inputs:?}");
                }
                other => panic!("pipeline absorbs to CallWorkflow, got {other:?}"),
            }
        }
        assert!(
            !notes
                .iter()
                .any(|n| n.source.starts_with("pipelines.toml:chain")),
            "exact mapping needs no flag: {notes:?}"
        );
    }

    /// Legacy `pipelines.toml` chain absorbs to exact `call_workflow` steps
    /// (bd-40d1): no DispatchJob shim, no flag note.
    #[test]
    fn legacy_pipeline_absorbs_with_flag() {
        let dir = config_root("legacy-pipe");
        write(
            &dir.join("workflows.toml"),
            "[[a]]\ntype = \"send\"\nto = \"oc\"\nmsg = \"hello\"\n",
        );
        write(&dir.join("pipelines.toml"), "chain = [\"a\"]\n");
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        let wf = catalog.workflows.get("chain").expect("pipeline absorbed");
        assert_eq!(wf.steps.len(), 1);
        assert!(
            matches!(wf.steps[0].action, Action::CallWorkflow { .. }),
            "pipeline absorbs to CallWorkflow, not the DispatchJob shim: {:?}",
            wf.steps[0].action
        );
        assert!(
            !catalog
                .notes
                .iter()
                .any(|n| n.source.starts_with("pipelines.toml:chain")),
            "exact mapping needs no flag: {:?}",
            catalog.notes
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 193.3: the legacy Send + Wait branches map to unified steps with the
    /// documented semantics — Send → dispatch_job (assignee + spec carries
    /// to/stage/msg), Wait → delay shell + a MappingNote (flagged, never
    /// dropped).
    #[test]
    fn absorb_send_and_wait_steps_map_to_unified() {
        let dir = config_root("sendwait");
        write(
            &dir.join("workflows.toml"),
            r#"[[daily]]
type = "send"
to = "oc"
stage = "fresh-eyes-explore"
msg = "hello"

[[daily]]
type = "wait"
until = "done"
timeout_ms = 500
"#,
        );
        let raw = std::fs::read_to_string(dir.join("workflows.toml")).unwrap();
        let legacy: LegacyWorkflows = toml::from_str(&raw).unwrap();
        let (wf, notes) = absorb_workflow_steps("daily", legacy.workflows.get("daily").unwrap());
        assert_eq!(wf.steps.len(), 2, "both legacy steps absorbed");
        match &wf.steps[0].action {
            Action::DispatchJob { assignee, spec, .. } => {
                assert_eq!(assignee, "oc");
                assert_eq!(
                    spec.extra.get("to").and_then(serde_json::Value::as_str),
                    Some("oc")
                );
                assert_eq!(spec.stage.as_deref(), Some("fresh-eyes-explore"));
                assert_eq!(
                    spec.extra.get("msg").and_then(serde_json::Value::as_str),
                    Some("hello")
                );
            }
            other => panic!("Send absorbed to DispatchJob, got {other:?}"),
        }
        match &wf.steps[1].action {
            Action::Delay { secs } => assert_eq!(*secs, 1, "500ms rounds up"),
            other => panic!("Wait absorbed to Delay, got {other:?}"),
        }
        assert!(
            notes.iter().any(|n| n.legacy.contains("wait done")),
            "Wait is flagged, never dropped: {notes:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 193.3: a legacy name colliding with a canonical YAML entry is
    /// SHADOWED — canonical is the home; a mapping note records the shadow.
    #[test]
    fn canonical_shadows_legacy_collision() {
        let dir = config_root("collide");
        write(
            &dir.join("workflows/triage.yaml"),
            "name: triage\ntrigger:\n  type: webhook\nsteps:\n  - name: a\n    action: send_dm\n    to: pi\n    body: x\n",
        );
        write(
            &dir.join("workflows.toml"),
            r#"[[triage]]
type = "send"
to = "oc"
msg = "legacy triage"
"#,
        );
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        assert!(catalog.workflows.contains_key("triage"));
        assert_eq!(
            catalog.workflows["triage"].steps[0].name.as_deref(),
            Some("a"),
            "canonical YAML wins the collision"
        );
        assert!(
            catalog
                .notes
                .iter()
                .any(|n| n.reason.contains("shadowed by canonical")),
            "collision recorded: {:?}",
            catalog.notes
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- ADR-0022 W6 (bd-adr22-w6-stage-registry-0jgr) ----

    /// W6 test helper: a minimal lint-clean workflow (send_dm steps).
    fn w6_wf(name: &str, description: &str) -> Workflow {
        Workflow {
            name: name.to_string(),
            description: Some(description.to_string()),
            trigger: None,
            guard: None,
            crew: None,
            policy: None,
            graph: None,
            principles: vec![],
            inputs: vec![InputDecl {
                name: "goal".into(),
                default: None,
            }],
            steps: vec![Step {
                name: Some("leaf".into()),
                guard: None,
                on_error: Default::default(),
                build_slot: false,
                action: Action::DispatchJob {
                    assignee: "x".into(),
                    spec: toron_workflow::schema::DispatchSpec {
                        profile: Some("@coder".into()),
                        task: Some("{{inputs.goal}}".into()),
                        ..Default::default()
                    },
                    channel: None,
                },
            }],
        }
    }

    /// W6 §3.7 T3: keep-after-green writes the project catalog entry the
    /// resolver finds — load_layered_with sees it, shadowing user.
    #[test]
    fn w6_keep_after_green_resolves_in_project_tier() {
        let dir = config_root("w6keep");
        // User tier already has a same-name entry (older shape)…
        write(
            &dir.join("workflows/w6-kept.yaml"),
            "name: w6-kept\ntrigger:\n  type: webhook\nsteps:\n  - name: old\n    action: send_dm\n    to: pi\n    body: x\n",
        );
        let wf = w6_wf(
            "w6-kept",
            "composed run [origin: composed; from: scout:p1; first-run: t; author: tealdune]",
        );
        let proj = dir.join("proj");
        let path = write_workflow_yaml(&proj.join(".flywheel/workflows"), &wf).unwrap();
        assert!(path.is_file(), "kept file written: {}", path.display());
        let cat = WorkflowCatalog::load_layered_with(dir.clone(), Some(&proj)).unwrap();
        let entry = cat.workflows.get("w6-kept").expect("resolver finds kept");
        assert!(
            entry.steps[0].action.type_name() == "dispatch_job",
            "project tier shadows user"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// W6: provenance tokens survive the YAML round-trip and parse back.
    #[test]
    fn w6_provenance_fields_present_after_roundtrip() {
        let dir = config_root("w6prov");
        let desc = "composed run for obj [origin: composed; from: fresh-eyes-explore,systematic-execute; first-run: 2026-08-21T12:00:00Z; author: TealDune]";
        let proj = dir.join("proj");
        write_workflow_yaml(&proj.join(".flywheel/workflows"), &w6_wf("w6-prov", desc)).unwrap();
        let cat = WorkflowCatalog::load_layered_with(dir.clone(), Some(&proj)).unwrap();
        let got = parse_provenance(cat.workflows["w6-prov"].description.as_deref().unwrap());
        let (origin, from, first_run, author) = got.expect("provenance parses");
        assert_eq!(origin, "composed");
        assert_eq!(from, "fresh-eyes-explore,systematic-execute");
        assert_eq!(first_run, "2026-08-21T12:00:00Z");
        assert_eq!(author, "TealDune");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_seed_uses_per_entry_toml_for_builtins() {
        let dir = config_root("seed-entry-toml");
        seed_user_catalog_with(&dir, OverwriteScope::None, false).unwrap();
        assert!(dir.join("workflows/goal.yaml").is_file());
        assert!(dir.join("stages/ce-execute.toml").is_file());
        assert!(!dir.join("stages.toml").exists());
        assert!(!dir.join("workflows/goal.toml").exists());

        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        assert!(catalog.workflows.contains_key("goal"));
        let stages = load_user_stages(&dir).unwrap();
        assert_eq!(
            stages.get("ce-execute").map(|stage| stage.prompt.as_str()),
            crate::stages::StageLibrary::compiled_in().resolve("ce-execute")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_catalog_seed_is_non_destructive_and_drift_is_honest() {
        let dir = config_root("seed-drift");
        let before = catalog_drift(&dir).unwrap();
        assert!(before.iter().any(|row| row.status == "missing"));
        write(
            &dir.join("stages.toml"),
            "[operator]\nkeep = \"yes\"\n\n[stages.custom]\nname = \"custom\"\nprompt = \"operator stage\"\n",
        );

        let seed = seed_user_catalog_with(&dir, OverwriteScope::None, false).unwrap();
        assert_ne!(seed.created_workflows, [] as [String; 0]);
        assert_ne!(seed.created_stages, [] as [String; 0]);
        assert_eq!(seed.kept_workflows, [] as [String; 0]);
        let after_seed = catalog_drift(&dir).unwrap();
        assert!(
            after_seed
                .iter()
                .all(|row| matches!(row.status.as_str(), "current" | "custom"))
        );
        assert!(
            after_seed
                .iter()
                .any(|row| row.name == "custom" && row.status == "custom")
        );
        let seeded_path = dir.join("stages.toml");
        let seeded_stages = fs::read_to_string(&seeded_path).unwrap();
        assert!(seeded_stages.contains("[operator]"));
        assert!(seeded_stages.contains("keep = \"yes\""));
        let parsed: toml::Table = toml::from_str(&seeded_stages).unwrap();
        assert_eq!(
            parsed["operator"]["keep"].as_str(),
            Some("yes"),
            "seeding must preserve unknown top-level user data"
        );

        let second_seed = seed_user_catalog_with(&dir, OverwriteScope::None, false).unwrap();
        assert_eq!(second_seed.created_stages, [] as [String; 0]);
        assert_eq!(
            fs::read_to_string(&seeded_path).unwrap(),
            seeded_stages,
            "a no-op seed must not rewrite user stages.toml"
        );

        let mut custom = crate::builtin_workflows::builtin_workflows()["goal"].clone();
        custom.description = Some("operator-owned copy".into());
        write_workflow_yaml(&dir.join("workflows"), &custom).unwrap();
        let mut stage = crate::stages::StageLibrary::compiled_in()
            .get_stage("ce-execute")
            .unwrap()
            .clone();
        stage.prompt.push_str("\noperator override");
        crate::stages::upsert_stage_entry(&dir.join("stages/ce-execute.toml"), &stage).unwrap();

        let after = catalog_drift(&dir).unwrap();
        assert!(
            after
                .iter()
                .any(|row| row.name == "goal" && row.status == "stale")
        );
        assert!(
            after
                .iter()
                .any(|row| row.name == "ce-execute" && row.status == "stale")
        );
        assert!(after.iter().all(|row| row.status != "missing"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn p5_pick_unknown_ambiguous_and_prefix() {
        use super::{CatalogError, pick_workflow};
        let mut merged: super::BTreeMap<String, super::ResolvedWorkflow> = super::BTreeMap::new();
        for n in ["ship", "ship-chain", "debate"] {
            let wf =
                toron_workflow::schema::parse_workflow(&format!("name: {n}\nsteps: []\n")).unwrap();
            merged.insert(n.into(), (wf, "builtin"));
        }
        assert!(pick_workflow(&merged, "ship").is_ok());
        assert_eq!(pick_workflow(&merged, "deb").unwrap().0.name, "debate");
        assert!(matches!(
            pick_workflow(&merged, "nope"),
            Err(CatalogError::Unknown(_))
        ));
        assert!(matches!(
            pick_workflow(&merged, "sh"),
            Err(CatalogError::Ambiguous(..))
        ));
    }
}

#[cfg(test)]
mod bd_3o5t_seed_fidelity {
    use super::*;

    /// bd-3o5t: a workflow seeded from the compiled-in source must still have
    /// its crew when the seeded FILE is read back. This walks the real path —
    /// verbatim text out, parse back in — because the guarantee seed depends on
    /// is about the bytes on disk, not about the in-memory struct.
    ///
    /// The bug this pins: seed used `serde_yaml::to_string(&Workflow)`, and
    /// `CrewSpec` is `#[serde(untagged)]` with a unit `Auto` variant, so serde
    /// emitted `crew: null` for EVERY crew workflow. Since the user tier wins
    /// over the builtin, seeding a machine silently turned crew workflows into
    /// headless ones, and drift reported the damage as `current`.
    #[test]
    fn a_seeded_file_reparses_with_its_crew_intact() {
        for (name, yaml) in crate::builtin_workflows::builtin_workflows_yaml() {
            let builtin = crate::builtin_workflows::builtin_workflows()
                .remove(name)
                .unwrap_or_else(|| panic!("{name} missing from the parsed map"));
            if builtin.crew.is_none() {
                continue;
            }
            // Exactly what seed writes.
            let seeded: Workflow = parse_workflow(yaml)
                .unwrap_or_else(|e| panic!("{name}: seeded text does not parse: {e}"));
            assert!(
                seeded.crew.is_some(),
                "{name}: the SEEDED FILE lost its crew. The builtin declares one, so the \
                 verbatim text must carry it. If this fails, the file on disk will run \
                 headless while the builtin runs with agents."
            );
        }
    }

    /// The guard against reintroducing the serializer as the seed path. If
    /// someone "simplifies" seed back to `to_string(&Workflow)`, the
    /// round-trip test above starts failing — this names WHY, so the failure is
    /// not mysterious.
    #[test]
    fn re_serializing_a_builtin_is_lossy_and_must_not_be_the_seed_path() {
        let builtin = crate::builtin_workflows::builtin_workflows();
        let wf = &builtin["arena"];
        assert!(wf.crew.is_some(), "arena declares a crew");
        let dumped = serde_yaml::to_string(wf).expect("serialize");
        assert!(
            !dumped.contains("crew: auto"),
            "if serde ever round-trips CrewSpec::Auto correctly this guard is stale — \
             re-check that seed still writes verbatim text before relaxing it"
        );
    }

    /// The concrete regression: `bug-fix` declares `crew: auto` in the builtin
    /// source, and that must still be true after the seed round-trip.
    #[test]
    fn bug_fix_keeps_its_crew_through_the_seed_round_trip() {
        let builtins = crate::builtin_workflows::builtin_workflows();
        let wf = builtins.get("bug-fix").expect("bug-fix is a builtin");
        assert!(
            wf.crew.is_some(),
            "the builtin bug-fix yaml declares `crew: auto`, so the parsed builtin must carry it"
        );
        let dumped = serde_yaml::to_string(wf).expect("serialize");
        assert!(
            dumped.contains("crew:"),
            "the seeded yaml must carry a crew key, got:\n{dumped}"
        );
    }
}

#[cfg(test)]
mod bd_3o5t_drift_truth {
    use super::*;

    /// bd-3o5t: drift MUST report a user copy that differs from the builtin in a
    /// behaviour-changing field as `stale`. The live proof that it does not: the
    /// operator's `~/.config/flywheel/workflows/bug-fix.yaml` declares
    /// `crew: null` while the builtin declares `crew: auto`, the user tier wins,
    /// the run is headless — and `flywheel workflow drift` printed
    /// "catalog drift clean".
    #[test]
    fn a_user_copy_that_drops_the_crew_is_reported_stale() {
        let root = std::env::temp_dir().join(format!("drift-3o5t-{}", std::process::id()));
        let wf_dir = root.join("workflows");
        std::fs::create_dir_all(&wf_dir).expect("mkdir");
        // The exact divergence: crew present on the builtin, null in the copy.
        std::fs::write(
            wf_dir.join("bug-fix.yaml"),
            "name: bug-fix\ncrew: null\nsteps: []\n",
        )
        .expect("write");

        let rows = catalog_drift(&root).expect("drift");
        let row = rows
            .iter()
            .find(|r| r.name == "bug-fix" && r.kind == "workflow")
            .expect("bug-fix row");
        assert_eq!(
            row.status, "stale",
            "a user copy that drops the crew is NOT current — reporting it current is what \
             let a headless run masquerade as a correct one"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The positive control: a byte-faithful copy IS current. Without this, a
    /// drift that always says `stale` would pass the test above while being
    /// useless in the other direction.
    #[test]
    fn a_faithful_copy_is_reported_current() {
        let root = std::env::temp_dir().join(format!("drift-ok-{}", std::process::id()));
        let wf_dir = root.join("workflows");
        std::fs::create_dir_all(&wf_dir).expect("mkdir");
        // Faithful == the builtin's own source text, byte for byte.
        let faithful = crate::builtin_workflows::builtin_workflow_yaml("bug-fix")
            .expect("bug-fix is a builtin");
        std::fs::write(wf_dir.join("bug-fix.yaml"), faithful).expect("write");

        let rows = catalog_drift(&root).expect("drift");
        let row = rows
            .iter()
            .find(|r| r.name == "bug-fix" && r.kind == "workflow")
            .expect("bug-fix row");
        assert_eq!(
            row.status, "current",
            "a faithful copy must read as current"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod bd_sync_overwrite {
    use super::*;

    fn root(tag: &str) -> std::path::PathBuf {
        let r = std::env::temp_dir().join(format!("sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(r.join("workflows")).expect("mkdir");
        r
    }

    const BUGFIX: &str = "name: bug-fix\ncrew: null\nsteps: []\n";

    /// The default must change NOTHING that already exists. An agent that runs
    /// `workflow seed` to "make sure I have the catalog" must not be able to
    /// clobber local edits by accident.
    #[test]
    fn seed_by_default_never_overwrites() {
        let r = root("default");
        std::fs::write(r.join("workflows/bug-fix.yaml"), BUGFIX).expect("write");
        let rep = seed_user_catalog_with(&r, OverwriteScope::None, false).expect("seed");
        assert_eq!(
            std::fs::read_to_string(r.join("workflows/bug-fix.yaml")).expect("read"),
            BUGFIX,
            "seed with no --overwrite rewrote an existing file"
        );
        assert_eq!(rep.overwritten_workflows, [] as [String; 0]);
        let _ = std::fs::remove_dir_all(&r);
    }

    /// `--overwrite stale` replaces exactly the builtin-named copies that drift
    /// disagrees with, and leaves everything else alone. This is the repair the
    /// operator actually needs: 40 files lost their crew.
    #[test]
    fn overwrite_stale_repairs_a_stale_copy_and_leaves_others() {
        let r = root("stale");
        std::fs::write(r.join("workflows/bug-fix.yaml"), BUGFIX).expect("write");
        // An authored file whose name is NOT a builtin: never touched, at any
        // scope, because it is not a builtin copy.
        std::fs::write(r.join("workflows/my-own.yaml"), "name: my-own\n").expect("write");

        let rep = seed_user_catalog_with(&r, OverwriteScope::Stale, false).expect("seed");
        let after = std::fs::read_to_string(r.join("workflows/bug-fix.yaml")).expect("read");
        assert_ne!(after, BUGFIX, "a stale builtin copy must be replaced");
        assert!(
            after.contains("crew:"),
            "the replacement must be the real builtin"
        );
        assert_eq!(rep.overwritten_workflows, vec!["bug-fix".to_string()]);
        assert_eq!(
            std::fs::read_to_string(r.join("workflows/my-own.yaml")).expect("read"),
            "name: my-own\n",
            "a non-builtin file must never be overwritten"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// An overwrite must be RECOVERABLE. The operator is being asked to confirm
    /// replacing 40 files in their real config dir; a copy with no way back is
    /// not something to confirm, it is a hazard.
    #[test]
    fn an_overwritten_file_is_backed_up_first() {
        let r = root("backup");
        std::fs::write(r.join("workflows/bug-fix.yaml"), BUGFIX).expect("write");
        seed_user_catalog_with(&r, OverwriteScope::Stale, false).expect("seed");
        let bak = r.join("workflows/bug-fix.yaml.bak");
        assert!(bak.exists(), "the previous content must be recoverable");
        assert_eq!(std::fs::read_to_string(&bak).expect("read"), BUGFIX);
        let _ = std::fs::remove_dir_all(&r);
    }

    /// `--dry-run` must be inert. It exists so an operator can SEE the blast
    /// radius before confirming it.
    #[test]
    fn dry_run_changes_nothing_but_reports_the_plan() {
        let r = root("dry");
        std::fs::write(r.join("workflows/bug-fix.yaml"), BUGFIX).expect("write");
        let rep = seed_user_catalog_with(&r, OverwriteScope::All, true).expect("seed");
        assert_eq!(
            std::fs::read_to_string(r.join("workflows/bug-fix.yaml")).expect("read"),
            BUGFIX,
            "--dry-run wrote to disk"
        );
        assert!(
            !r.join("workflows/bug-fix.yaml.bak").exists(),
            "--dry-run left a backup behind"
        );
        assert_eq!(
            rep.planned_overwrites,
            vec!["bug-fix".to_string()],
            "--dry-run must still name what it WOULD do, or it is useless for review"
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// Idempotence: a second identical seed is a no-op. An agent may run this
    /// on every session start.
    #[test]
    fn seeding_twice_is_a_no_op() {
        let r = root("idem");
        seed_user_catalog_with(&r, OverwriteScope::None, false).expect("first");
        let first = std::fs::read_to_string(r.join("workflows/bug-fix.yaml")).expect("read");
        let rep = seed_user_catalog_with(&r, OverwriteScope::None, false).expect("second");
        assert_eq!(
            std::fs::read_to_string(r.join("workflows/bug-fix.yaml")).expect("read"),
            first
        );
        assert!(
            rep.created_workflows.is_empty(),
            "second seed created files"
        );
        assert!(
            rep.overwritten_workflows.is_empty(),
            "a second seed at scope `none` replaced files it had just written"
        );
        let _ = std::fs::remove_dir_all(&r);
    }
}

#[cfg(test)]
mod bd_mvy4_stage_drift {
    use super::*;

    /// bd-mvy4: drift must report a user-tier STAGE whose prompt diverged from
    /// the compiled-in one as `stale`.
    ///
    /// Found while fixing bd-mvy4's own live effect: the compiled-in wave-gate
    /// prompt was corrected to pass `--project` to preflight, the user-tier copy
    /// at `~/.config/flywheel/stages/wave-gate.toml` still lacked it, and
    /// `flywheel stage show wave-gate` returned the STALE prompt — so the fix was
    /// inert in production. `workflow drift` reported that file `current`.
    ///
    /// The workflow tier does not have this problem: it compares VERBATIM SOURCE
    /// TEXT. The stage tier compares parsed structs, and something in that path
    /// treats a diverged prompt as equal.
    #[test]
    fn a_user_stage_whose_prompt_diverged_is_reported_stale() {
        let root = std::env::temp_dir().join(format!("sdrift-{}", std::process::id()));
        let dir = root.join("stages");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let compiled = crate::stages::StageLibrary::compiled_in();
        let real = compiled
            .get_stage("wave-gate")
            .expect("wave-gate is compiled in");
        // A faithful copy of the compiled-in stage must be `current`.
        std::fs::write(
            dir.join("wave-gate.toml"),
            crate::stages::render_stage_entry(real).expect("render"),
        )
        .expect("write faithful");
        let rows = catalog_drift(&root).expect("drift");
        let faithful = rows
            .iter()
            .find(|r| r.kind == "stage" && r.name == "wave-gate")
            .expect("row");
        assert_eq!(
            faithful.status, "current",
            "a faithful copy must read as current, or drift is useless in the other direction"
        );

        // Now diverge ONLY the prompt, the way a stale seed copy does.
        let mut diverged = real.clone();
        diverged.prompt = format!("{} --but-diverged", diverged.prompt);
        std::fs::write(
            dir.join("wave-gate.toml"),
            crate::stages::render_stage_entry(&diverged).expect("render"),
        )
        .expect("write diverged");
        let rows = catalog_drift(&root).expect("drift");
        let stale = rows
            .iter()
            .find(|r| r.kind == "stage" && r.name == "wave-gate")
            .expect("row");
        assert_eq!(
            stale.status, "stale",
            "a user stage whose PROMPT diverged is not current — reporting it current is what \
             let a corrected compiled-in stage read as deployed while the shadowing copy served \
             the old prompt"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
