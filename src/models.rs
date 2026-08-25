//! Model registry (ADR-0010 Spec D, P10.2): `~/.config/flywheel/models.toml`
//! with `[models.ID]` entries (provider, per-mtok cost, caps, fallback).
//! Profile.model strings resolve against the registry so the per-harness
//! injection is REAL: the registry knows how each harness wants its model
//! flag (`omp --model=ID`, `opencode -m provider/ID`, `pi --model pattern`,
//! `grok -m ID`).
//!
//! Resolution is never a blocker: an unknown model id warns and passes
//! through untouched.
//!
//! Functional core / imperative shell: parsing + resolution are pure
//! (unit-tested); the `models.toml` read/write is the thin IO shell.
//!
//! governed-by: ADR-0010 (Spec D)

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::verdict::{AcShape, BeadUnit, Blast};

/// Blocked reason for the single-family degrade (ADR-0019 Spec 4): the
/// registry cannot supply a second model family, so the bead cannot be
/// independently verified. The loop marks it `blocked` with this reason,
/// mails the captain exactly once, and does not respawn the verifier.
pub const VERIFIER_FAMILY_MISSING: &str = "verifier-family-missing";

/// The loop's verifier decision (ADR-0019 Spec 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifierPlan {
    /// No independent verifier needed: the loop runs VERIFY itself (cheap
    /// band) or a WorkerReceipt is legal (non-runnable band).
    NoneRequired,
    /// Dispatch an independent verifier on this model (different family, or
    /// the captain's explicit `verifier_family_override`).
    Dispatch(String),
    /// Single-family degrade: block the bead `verifier-family-missing`,
    /// mail the captain exactly once, no respawn.
    Degrade,
}

/// Does this bead require an INDEPENDENT verifier (ADR-0019 Spec 4)? True
/// for judgment AC, priority <= 1, or blast High — the cells where neither a
/// loop-run CommandVerified nor a WorkerReceipt can legally close.
pub fn verifier_required(bead: &BeadUnit, ac: AcShape) -> bool {
    ac == AcShape::Judgment || bead.priority <= 1 || bead.blast == Blast::High
}

/// Split a registry key into `(provider, model)`. Registry keys are
/// `provider/model` composites (bd-1t3j): one model id can exist under
/// multiple providers, so the provider prefix lives IN the key.
pub fn split_key(key: &str) -> (&str, &str) {
    match key.split_once('/') {
        Some((p, m)) => (p, m),
        None => ("", key),
    }
}

/// Validate a registry key at parse time: it MUST be a non-empty
/// `provider/model` composite. A legacy bare id is a loud error naming the
/// rewrite — never silent misresolution.
fn validate_key(key: &str) -> Result<()> {
    let (p, m) = split_key(key);
    if p.is_empty() || m.is_empty() || !key.contains('/') {
        anyhow::bail!(
            "models.toml: key \"{key}\" must be provider/model — \
             rewrite as [models.\"provider/{key}\"]"
        );
    }
    Ok(())
}

/// One registry entry (`[models."provider/id"]`). The provider prefix is
/// part of the KEY, not a field (bd-1j3t composite keying).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Advisory per-mtok cost (run-cost reporting; 0 = unknown).
    #[serde(default)]
    pub cost_in_per_mtok: f64,
    #[serde(default)]
    pub cost_out_per_mtok: f64,
    /// Harness names this model supports. Non-empty = a whitelist: an
    /// unsupported harness resolves to `fallback` (or the id, passthrough).
    #[serde(default)]
    pub caps: Vec<String>,
    /// Alternative id to use when `caps` excludes the harness (optional) —
    /// ALSO the retry-on-429 fallback (bd-42fs): a rate-limited run retries
    /// with this model instead of failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// Thinking/effort level for harnesses that take one (`omp
    /// --thinking=<level>`: minimal|low|medium|high|xhigh|max). Advisory —
    /// absent = harness default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl ModelEntry {
    /// The injection string for one harness given the entry's full registry
    /// KEY (`provider/model` for opencode+pi and omp — omp's provider
    /// catalog needs the prefix, verified 2026-08-10); the bare model id
    /// for grok. Unknown harness: key passthrough (never block).
    pub fn flag_for(&self, key: &str, harness: &str) -> String {
        match harness {
            "grok" => split_key(key).1.to_string(),
            _ => key.to_string(),
        }
    }
}

/// The on-disk shape (`models.toml`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ModelsFile {
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
}

/// The registry: parsed `models.toml` entries keyed by `provider/model`.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    pub models: BTreeMap<String, ModelEntry>,
    /// Keys sourced from the USER file in [`ModelRegistry::load_routing`]
    /// (vs compiled-in defaults). The bare-id default-provider fallback
    /// prefers these, so a user override wins even when its provider name
    /// sorts after the compiled-in one.
    pub user_keys: std::collections::BTreeSet<String>,
}

impl ModelRegistry {
    /// Load from `~/.config/flywheel/models.toml`. Missing file = empty
    /// registry (zero-config; resolution then passes everything through).
    pub fn load() -> Result<Self> {
        let path = crate::config::config_dir().join("models.toml");
        Self::load_from(Some(&path))
    }

    /// Load from an explicit path (`None` = empty registry; test seam).
    /// Every user-file key is validated as a `provider/model` composite:
    /// a legacy bare id fails LOUDLY naming the rewrite (bd-1t3j).
    pub fn load_from(path: Option<&Path>) -> Result<Self> {
        let mut models = BTreeMap::new();
        if let Some(path) = path
            && path.exists()
        {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let file: ModelsFile =
                toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
            for key in file.models.keys() {
                validate_key(key).with_context(|| format!("parse {}", path.display()))?;
            }
            models = file.models;
        }
        Ok(Self {
            models,
            user_keys: Default::default(),
        })
    }

    /// The ROUTING registry: compiled-in fleet defaults layered under the
    /// user's `models.toml` (user entries win per id). This — not `load()` —
    /// is what profile/spawn resolution consults: catalog model ids route
    /// through the registry even on a zero-config install.
    pub fn load_routing() -> Result<Self> {
        let path = crate::config::config_dir().join("models.toml");
        let mut models = compiled_in_models();
        // A missing file is fine (zero-config); a PRESENT but unparseable
        // file is a loud error — silently skipping it would quietly drop
        // the user's routing entries (a bypass).
        let user = Self::load_from(Some(&path))?;
        let user_keys: std::collections::BTreeSet<String> = user.models.keys().cloned().collect();
        if !user.models.is_empty() {
            models.extend(user.models);
        }
        Ok(Self { models, user_keys })
    }

    /// Resolve a lookup id to its registry KEY. A full `provider/model`
    /// id must hit exactly; a bare model id falls back to the DEFAULT
    /// provider for that id — a user-sourced entry if one exists, else
    /// the alphabetically-first registry key carrying it (deterministic).
    fn key_for(&self, id: &str) -> Option<String> {
        if self.models.contains_key(id) {
            return Some(id.to_string());
        }
        if id.contains('/') {
            return None;
        }
        let mut default: Option<&str> = None;
        for k in self.models.keys() {
            if split_key(k).1 == id {
                if self.user_keys.contains(k) {
                    return Some(k.clone());
                }
                default = default.or(Some(k.as_str()));
            }
        }
        default.map(|s| s.to_string())
    }

    /// Look up one id: an exact `provider/model` composite, or a bare
    /// model id resolved to its default provider.
    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        let key = self.key_for(id)?;
        self.models.get(key.as_str())
    }

    /// The provider half of an id's resolved key (`provider/model`).
    /// Unknown id -> empty string.
    pub fn provider_of(&self, id: &str) -> String {
        match self.key_for(id) {
            Some(key) => split_key(&key).0.to_string(),
            None => String::new(),
        }
    }

    /// Resolve a model id to the injection string for one harness. Known
    /// registry id → the per-harness flag form (honoring `caps`/`fallback`);
    /// unknown id → passthrough (the caller warns; we never block).
    pub fn resolve(&self, id: &str, harness: &str) -> String {
        match self.key_for(id) {
            Some(key) => {
                let entry = &self.models[key.as_str()];
                if entry.caps.is_empty() || entry.caps.iter().any(|c| c == harness) {
                    entry.flag_for(&key, harness)
                } else {
                    match &entry.fallback {
                        Some(fb) => self.resolve(fb, harness),
                        None => id.to_string(),
                    }
                }
            }
            None => id.to_string(),
        }
    }

    /// Strict resolution for PROFILE-sourced model ids (ADR-0010 Spec D):
    /// the id MUST be a registry entry — unknown ids and caps-excluded
    /// harnesses without a fallback are errors naming the fix, never a
    /// silent passthrough. Empty id resolves empty (the caller skips the
    /// flag). Ad-hoc CLI `--model` strings keep the lenient `resolve()`.
    pub fn resolve_strict(&self, id: &str, harness: &str) -> Result<String> {
        if id.is_empty() {
            return Ok(String::new());
        }
        match self.key_for(id) {
            Some(key) => {
                let entry = &self.models[key.as_str()];
                if entry.caps.is_empty() || entry.caps.iter().any(|c| c == harness) {
                    Ok(entry.flag_for(&key, harness))
                } else {
                    match &entry.fallback {
                        Some(fb) => self.resolve_strict(fb, harness).with_context(|| {
                            format!("model '{id}' fallback '{fb}' failed to resolve")
                        }),
                        None => anyhow::bail!(
                            "model '{id}' is not routable on harness '{harness}' (caps: {}) \
                             and has no fallback",
                            entry.caps.join(",")
                        ),
                    }
                }
            }
            None => anyhow::bail!(
                "model '{id}' is not in the registry — add it first: \
                 `flywheel model add {id} --provider <provider>` \
                 (profile models route ONLY through the registry)"
            ),
        }
    }

    /// Save the full file to `~/.config/flywheel/models.toml` (the `add`/
    /// `remove` write path).
    pub fn save(&self) -> Result<()> {
        let path = crate::config::config_dir().join("models.toml");
        self.save_to(&path)
    }

    /// The model family of an id (ADR-0019 Spec 4: "worker grok -> reviewer
    /// not grok"): the provider half of the resolved registry key. Unknown
    /// id -> the id itself (a one-model registry is one family; never guess
    /// a family).
    pub fn family_of(&self, id: &str) -> String {
        match self.key_for(id) {
            Some(key) => split_key(&key).0.to_string(),
            None => id.to_string(),
        }
    }

    /// Verifier model resolution (ADR-0019 Spec 4): pick the first
    /// `candidates` model whose family differs from the worker's. Returns
    /// `None` when the registry cannot supply a second family (single-family
    /// swarm) and no captain override applies — the caller marks the bead
    /// `blocked` with `VERIFIER_FAMILY_MISSING`, mails the captain exactly
    /// once, and does not respawn (no infinite loop).
    ///
    /// `family_override` = config `verifier_family_override`: an explicit
    /// captain allowance for one family (the single-family degrade path).
    ///
    /// Scale-to-unit (ADR-0019 Spec 4): a verifier whose entire product is
    /// rerunning one cheap loop-runnable command is ceremony — on the
    /// loop-runnable band the loop already ran it (w1-loop-run-verify). The
    /// loop only dispatches an independent verifier when the legal-close
    /// table requires it (priority <= 1, blast High, or judgment AC).
    ///
    /// Pane budget: verifier dispatches share the spawn budget with workers;
    /// one reusable verifier pane per project, headless preferred (pane only
    /// when VERIFY needs a live TUI/UI).
    pub fn verifier_for(
        &self,
        worker_model: &str,
        candidates: &[&str],
        family_override: Option<&str>,
    ) -> Option<String> {
        let worker_family = self.family_of(worker_model);
        // The worker's own model can never be the verifier (identity rule
        // survives even the family override).
        let others: Vec<&str> = candidates
            .iter()
            .copied()
            .filter(|c| *c != worker_model)
            .collect();
        // 1. Strict rule first: any candidate from a DIFFERENT family.
        if let Some(c) = others.iter().find(|c| self.family_of(c) != worker_family) {
            return Some((*c).to_string());
        }
        // 2. Single-family swarm: an explicit captain override (config
        //    verifier_family_override) may allow that family. Absent or no
        //    matching candidate -> None = the degrade signal.
        if let Some(fam) = family_override
            && let Some(c) = others.iter().find(|c| self.family_of(c) == fam)
        {
            return Some((*c).to_string());
        }
        None
    }

    /// The full verifier decision for one bead (ADR-0019 Spec 4): when
    /// `verifier_required`, resolve a different-family model; the degrade
    /// signal (`Degrade`) is what the loop turns into `blocked` +
    /// `verifier-family-missing` + exactly one captain mail + no respawn.
    pub fn verifier_plan(
        &self,
        bead: &BeadUnit,
        ac: AcShape,
        worker_model: &str,
        candidates: &[&str],
        family_override: Option<&str>,
    ) -> VerifierPlan {
        if !verifier_required(bead, ac) {
            return VerifierPlan::NoneRequired;
        }
        match self.verifier_for(worker_model, candidates, family_override) {
            Some(model) => VerifierPlan::Dispatch(model),
            None => VerifierPlan::Degrade,
        }
    }

    /// Save to an explicit path (test seam).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(&ModelsFile {
            models: self.models.clone(),
        })?;
        std::fs::write(path, raw).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

/// Compiled-in routing defaults: the fleet's canonical models, layered
/// under user `models.toml` entries by [`ModelRegistry::load_routing`].
/// Mirrors the compiled-in profile defaults (ADR-0005): catalog profiles
/// reference these bare ids, so routing works zero-config and the registry
/// stays the SINGLE place a provider prefix lives.
///
/// `big-pickle` is capped to the `opencode` harness — it is free ONLY
/// there; strict resolution refuses it elsewhere (no fallback by design,
/// the scout profile pins the opencode harness).
pub fn compiled_in_models() -> BTreeMap<String, ModelEntry> {
    let mut m = BTreeMap::new();
    m.insert(
        "opencode-go/deepseek-v4-flash".to_string(),
        ModelEntry::default(),
    );
    m.insert(
        "opencode/big-pickle".to_string(),
        ModelEntry {
            caps: vec!["opencode".into()],
            ..ModelEntry::default()
        },
    );
    m.insert("zai/glm-5.2".to_string(), ModelEntry::default());
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Hermetic config dir for a test: the crate-wide env lock + a fresh
    /// temp `XDG_CONFIG_HOME` for the guard's lifetime.
    fn with_config_dir() -> std::sync::MutexGuard<'static, ()> {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-models-{}", uuid::Uuid::new_v4()));
        // env mutation is unsafe in edition 2024 (matches run_log.rs tests)
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        _g
    }

    fn sample_entry() -> ModelEntry {
        ModelEntry {
            cost_in_per_mtok: 3.0,
            cost_out_per_mtok: 15.0,
            caps: vec![],
            fallback: None,
            effort: None,
        }
    }

    /// A registry roundtrips through `models.toml` byte-for-byte (the
    /// `add` write must be what `load` reads).
    #[test]
    fn save_then_load_roundtrips() {
        let _g = with_config_dir();
        let path = crate::config::config_dir().join("models.toml");
        let mut reg = ModelRegistry::default();
        reg.models
            .insert("anthropic/claude-sonnet-4".into(), sample_entry());
        reg.save().expect("save");
        let got = ModelRegistry::load().expect("load");
        assert_eq!(
            got.models.get("anthropic/claude-sonnet-4"),
            Some(&sample_entry())
        );
        assert!(path.exists(), "models.toml written");
    }

    /// Missing models.toml is an empty registry, not an error (zero-config).
    #[test]
    fn missing_file_is_empty_registry() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load().expect("load with no file");
        assert!(reg.models.is_empty());
        assert_eq!(reg.resolve("gpt-5.2", "omp"), "gpt-5.2");
    }

    /// Per-harness injection forms (bd-42fs): opencode + pi + omp get
    /// the full `provider/model` key (omp's provider catalog needs the
    /// prefix — verified 2026-08-10); grok gets the bare model id.
    #[test]
    fn flag_for_injects_per_harness() {
        let e = sample_entry();
        let key = "anthropic/claude-sonnet-4";
        assert_eq!(e.flag_for(key, "opencode"), key);
        assert_eq!(e.flag_for(key, "pi"), key);
        assert_eq!(e.flag_for(key, "omp"), key, "omp needs the provider prefix");
        assert_eq!(e.flag_for(key, "grok"), "claude-sonnet-4");
    }

    /// bd-42fs: an unknown model passes through for omp (no provider entry)
    /// — never blocks, and the effort field is advisory-optional.
    #[test]
    fn effort_field_is_optional_and_resolve_unknown_passes_through() {
        let _g = with_config_dir();
        let reg = ModelRegistry::default();
        assert_eq!(reg.resolve("no-such-model", "omp"), "no-such-model");
        let e = ModelEntry {
            cost_in_per_mtok: 0.0,
            cost_out_per_mtok: 0.0,
            caps: vec![],
            fallback: Some("grok/grok-4.5".into()),
            effort: Some("high".into()),
        };
        assert_eq!(e.effort.as_deref(), Some("high"));
        assert_eq!(e.fallback.as_deref(), Some("grok/grok-4.5"));
        assert_eq!(e.flag_for("zai/glm-5.2", "omp"), "zai/glm-5.2");
    }

    /// Known registry id resolves to the injected form — via its full
    /// composite key OR a bare id falling back to the default provider.
    #[test]
    fn resolve_known_id_injects_harness_form() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        reg.models
            .insert("anthropic/claude-sonnet-4".into(), sample_entry());
        assert_eq!(
            reg.resolve("anthropic/claude-sonnet-4", "opencode"),
            "anthropic/claude-sonnet-4"
        );
        assert_eq!(
            reg.resolve("claude-sonnet-4", "omp"),
            "anthropic/claude-sonnet-4",
            "bare id falls back to the default provider (bd-1t3j)"
        );
    }

    /// Unknown id passes through untouched (never blocks).
    #[test]
    fn resolve_unknown_id_passes_through() {
        let _g = with_config_dir();
        let reg = ModelRegistry::default();
        assert_eq!(reg.resolve("unknown-model", "opencode"), "unknown-model");
    }

    /// `caps` whitelist: a harness outside the caps falls back to the
    /// fallback id (itself resolved), or the bare id when no fallback.
    #[test]
    fn caps_exclude_harness_uses_fallback() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        reg.models.insert(
            "anthropic/claude-sonnet-4".into(),
            ModelEntry {
                cost_in_per_mtok: 3.0,
                cost_out_per_mtok: 15.0,
                caps: vec!["opencode".into()],
                fallback: Some("anthropic/claude-opus-4".into()),
                effort: None,
            },
        );
        reg.models.insert(
            "anthropic/claude-opus-4".into(),
            ModelEntry {
                cost_in_per_mtok: 5.0,
                cost_out_per_mtok: 25.0,
                caps: vec![],
                fallback: None,
                effort: None,
            },
        );
        assert_eq!(
            reg.resolve("anthropic/claude-sonnet-4", "opencode"),
            "anthropic/claude-sonnet-4",
            "harness inside caps uses the id"
        );
        assert_eq!(
            reg.resolve("claude-sonnet-4", "grok"),
            "claude-opus-4",
            "harness outside caps falls back (grok takes the bare model id)"
        );
    }

    /// `caps` exclusion without a fallback: bare id passthrough, no panic.
    #[test]
    fn caps_exclude_harness_without_fallback_passes_through() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        reg.models.insert(
            "openai/gpt-5.2".into(),
            ModelEntry {
                cost_in_per_mtok: 2.0,
                cost_out_per_mtok: 10.0,
                caps: vec!["omp".into()],
                fallback: None,
                effort: None,
            },
        );
        assert_eq!(
            reg.resolve("openai/gpt-5.2", "grok"),
            "openai/gpt-5.2",
            "caps-excluded without fallback passes the id through untouched"
        );
    }

    /// `model add` then `model list`-style roundtrip: ids + entries survive.
    #[test]
    fn added_entry_survives_reload() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        reg.models.insert("anthropic/grok-4".into(), sample_entry());
        reg.save().expect("save");
        let reloaded = ModelRegistry::load().expect("load");
        let ids: Vec<&String> = reloaded.models.keys().collect();
        assert_eq!(ids, vec!["anthropic/grok-4"]);
    }

    /// Cost fields are advisory and default to 0 (no requirement to set).
    #[test]
    fn cost_defaults_to_zero() {
        let _g = with_config_dir();
        let toml = r#"
[models."local/free-model"]
"#;
        let path = crate::config::config_dir().join("models.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, toml).unwrap();
        let reg = ModelRegistry::load().expect("load");
        let e = reg.get("free-model").expect("entry");
        assert_eq!(e.cost_in_per_mtok, 0.0);
        assert_eq!(e.cost_out_per_mtok, 0.0);
        assert!(e.caps.is_empty());
        assert!(e.fallback.is_none());
    }

    // ---- ADR-0019 Spec 4: verifier family resolution + single-family degrade

    /// Build a registry from full composite keys (provider = key prefix).
    fn reg_with_keys(keys: &[&str]) -> ModelRegistry {
        ModelRegistry {
            models: keys
                .iter()
                .map(|k| ((*k).to_string(), ModelEntry::default()))
                .collect(),
            user_keys: Default::default(),
        }
    }

    /// Family = the provider half of the resolved key; unknown ids are
    /// their own family. A bare id resolves through the default-provider
    /// fallback before the family is read.
    #[test]
    fn verifier_family_of_uses_registry_provider() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "anthropic/opus-5"]);
        assert_eq!(reg.family_of("deepseek/deepseek-v4"), "deepseek");
        assert_eq!(
            reg.family_of("deepseek-v4"),
            "deepseek",
            "bare id -> default provider's family"
        );
        assert_eq!(reg.family_of("anthropic/opus-5"), "anthropic");
        // Unknown id: the id itself is the family (never guess).
        assert_eq!(reg.family_of("unregistered-model"), "unregistered-model");
    }

    /// A different-family candidate wins (worker deepseek -> anthropic).
    #[test]
    fn verifier_resolution_prefers_different_family() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "anthropic/opus-5", "google/flash-x"]);
        let v = reg
            .verifier_for(
                "deepseek/deepseek-v4",
                &["deepseek/deepseek-v4", "anthropic/opus-5", "google/flash-x"],
                None,
            )
            .expect("different family exists");
        assert_eq!(v, "anthropic/opus-5");
        // Candidate order matters: first different-family entry wins.
        let v = reg
            .verifier_for(
                "deepseek/deepseek-v4",
                &["deepseek/deepseek-v4", "google/flash-x"],
                None,
            )
            .expect("different family exists");
        assert_eq!(v, "google/flash-x");
    }

    /// Single-family swarm: no second family -> None (the degrade signal).
    #[test]
    fn verifier_resolution_degrades_single_family() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "deepseek/deepseek-v3"]);
        assert_eq!(
            reg.verifier_for(
                "deepseek/deepseek-v4",
                &["deepseek/deepseek-v4", "deepseek/deepseek-v3"],
                None
            ),
            None
        );
        // Empty candidate list degrades too.
        let reg = reg_with_keys(&["deepseek/deepseek-v4"]);
        assert_eq!(reg.verifier_for("deepseek/deepseek-v4", &[], None), None);
    }

    /// An explicit captain override (config verifier_family_override) allows
    /// the same family when no second family exists.
    #[test]
    fn verifier_resolution_honors_family_override() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "deepseek/deepseek-v3"]);
        let v = reg
            .verifier_for(
                "deepseek/deepseek-v4",
                &["deepseek/deepseek-v4", "deepseek/deepseek-v3"],
                Some("deepseek"),
            )
            .expect("override allows the family");
        assert_eq!(v, "deepseek/deepseek-v3");
        // Override for a family with no candidate still degrades.
        assert_eq!(
            reg.verifier_for(
                "deepseek/deepseek-v4",
                &["deepseek/deepseek-v4"],
                Some("anthropic")
            ),
            None
        );
        // Override never kicks in when a different family already exists.
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "anthropic/opus-5"]);
        let v = reg
            .verifier_for(
                "deepseek/deepseek-v4",
                &["deepseek/deepseek-v4", "anthropic/opus-5"],
                Some("deepseek"),
            )
            .expect("different family wins before override");
        assert_eq!(v, "anthropic/opus-5");
    }

    /// The degrade constant is the documented blocked reason.
    #[test]
    fn verifier_family_missing_reason_constant() {
        assert_eq!(VERIFIER_FAMILY_MISSING, "verifier-family-missing");
    }

    fn bead(priority: u8, blast: Blast, runnable: bool) -> BeadUnit {
        BeadUnit {
            id: "bd-v-1".into(),
            title: "verifier fixture".into(),
            priority,
            verify: if runnable {
                "cargo test -p flywheel verifier".into()
            } else {
                "cargo test -p flywheel verifier && sleep 3600".into()
            },
            blast,
            owner: None,
            head_sha: None,
        }
    }

    /// Cheap band (p>=2, Normal, loop-runnable): the loop verifies itself —
    /// no independent verifier, even in a single-family swarm.
    #[test]
    fn verifier_plan_none_required_on_cheap_band() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4"]);
        let b = bead(3, Blast::Normal, true);
        let plan = reg.verifier_plan(
            &b,
            AcShape::Checkable,
            "deepseek/deepseek-v4",
            &["deepseek/deepseek-v4"],
            None,
        );
        assert_eq!(plan, VerifierPlan::NoneRequired);
        // Non-runnable band (WorkerReceipt legal) also needs no verifier.
        let b = bead(3, Blast::Normal, false);
        let plan = reg.verifier_plan(
            &b,
            AcShape::Checkable,
            "deepseek/deepseek-v4",
            &["deepseek/deepseek-v4"],
            None,
        );
        assert_eq!(plan, VerifierPlan::NoneRequired);
    }

    /// P0 (priority <= 1) with a second family: dispatch the different-family
    /// verifier.
    #[test]
    fn verifier_plan_dispatches_different_family_for_p0() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "anthropic/opus-5"]);
        let b = bead(0, Blast::Normal, true);
        let plan = reg.verifier_plan(
            &b,
            AcShape::Checkable,
            "deepseek/deepseek-v4",
            &["deepseek/deepseek-v4", "anthropic/opus-5"],
            None,
        );
        assert_eq!(plan, VerifierPlan::Dispatch("anthropic/opus-5".into()));
    }

    /// Single-family swarm + P0: Degrade (blocked verifier-family-missing,
    /// one captain mail, no respawn loop).
    #[test]
    fn verifier_plan_degrades_single_family_for_p0() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "deepseek/deepseek-v3"]);
        let b = bead(0, Blast::Normal, true);
        let plan = reg.verifier_plan(
            &b,
            AcShape::Checkable,
            "deepseek/deepseek-v4",
            &["deepseek/deepseek-v4", "deepseek/deepseek-v3"],
            None,
        );
        assert_eq!(plan, VerifierPlan::Degrade);
    }

    /// Judgment AC always needs a verifier (ReviewerSigned); single-family +
    /// override makes it dispatchable.
    #[test]
    fn verifier_plan_judgment_ac_requires_reviewer() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "deepseek/deepseek-v3"]);
        let b = bead(2, Blast::Normal, true);
        // Judgment + no override: degrade (no second family).
        let plan = reg.verifier_plan(
            &b,
            AcShape::Judgment,
            "deepseek/deepseek-v4",
            &["deepseek/deepseek-v4", "deepseek/deepseek-v3"],
            None,
        );
        assert_eq!(plan, VerifierPlan::Degrade);
        // Override: dispatch the same-family reviewer.
        let plan = reg.verifier_plan(
            &b,
            AcShape::Judgment,
            "deepseek/deepseek-v4",
            &["deepseek/deepseek-v4", "deepseek/deepseek-v3"],
            Some("deepseek"),
        );
        assert_eq!(plan, VerifierPlan::Dispatch("deepseek/deepseek-v3".into()));
    }

    /// High blast (overlap) needs an independent verifier too.
    #[test]
    fn verifier_plan_required_for_high_blast() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4", "anthropic/opus-5"]);
        let b = bead(3, Blast::High, true);
        let plan = reg.verifier_plan(
            &b,
            AcShape::Checkable,
            "deepseek/deepseek-v4",
            &["deepseek/deepseek-v4", "anthropic/opus-5"],
            None,
        );

        assert_eq!(plan, VerifierPlan::Dispatch("anthropic/opus-5".into()));
    }
    /// Routing registry layers compiled-in fleet defaults under user
    /// entries (user wins per id); `load()` stays the USER FILE ONLY so
    /// `save()` never bakes defaults into models.toml.
    #[test]
    fn load_routing_layers_compiled_in_under_user_entries() {
        let _g = with_config_dir();
        let path = crate::config::config_dir().join("models.toml");
        let _ = fs::remove_file(&path);

        let reg = ModelRegistry::load_routing().expect("routing registry");
        assert_eq!(
            reg.resolve("opencode-go/deepseek-v4-flash", "omp"),
            "opencode-go/deepseek-v4-flash",
            "compiled-in deepseek routes zero-config (composite key)"
        );
        assert_eq!(
            reg.resolve("deepseek-v4-flash", "omp"),
            "opencode-go/deepseek-v4-flash",
            "bare id falls back to the compiled-in default provider"
        );
        assert_eq!(reg.resolve("glm-5.2", "omp"), "zai/glm-5.2");
        // big-pickle is free ONLY on the opencode harness (caps whitelist).
        assert_eq!(reg.resolve("big-pickle", "opencode"), "opencode/big-pickle");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
        // A user entry OVERRIDES the compiled-in default for the same id.
        fs::write(&path, "[models.\"my-gateway/deepseek-v4-flash\"]\n")
            .expect("write user models.toml");
        let reg = ModelRegistry::load_routing().expect("reload routing");
        assert_eq!(
            reg.resolve("deepseek-v4-flash", "omp"),
            "my-gateway/deepseek-v4-flash",
            "user entry wins over compiled-in"
        );
        assert_eq!(
            reg.resolve("glm-5.2", "omp"),
            "zai/glm-5.2",
            "other default intact"
        );

        // load() (the CLI write path) must NOT see compiled-ins.
        let user = ModelRegistry::load().expect("user file");
        assert!(
            user.get("glm-5.2").is_none(),
            "load() reads the user file only — save() must not persist defaults"
        );
    }

    /// Strict resolution: profile-sourced models MUST be registry ids.
    /// Unknown id, or a caps-excluded harness with no fallback, is an
    /// error naming the fix — never a silent passthrough.
    #[test]
    fn resolve_strict_fails_unknown_and_caps_excluded() {
        let _g = with_config_dir();
        let path = crate::config::config_dir().join("models.toml");
        let _ = fs::remove_file(&path);
        let reg = ModelRegistry::load_routing().expect("reg");

        assert_eq!(reg.resolve_strict("", "omp").expect("empty ok"), "");
        assert_eq!(
            reg.resolve_strict("deepseek-v4-flash", "omp")
                .expect("known ok"),
            "opencode-go/deepseek-v4-flash"
        );
        let err = reg
            .resolve_strict("no-such-model", "omp")
            .expect_err("unknown id must fail");
        assert!(
            err.to_string().contains("flywheel model add"),
            "error names the fix: {err}"
        );
        // big-pickle caps = opencode-only: strict refuses omp outright.
        let err = reg
            .resolve_strict("big-pickle", "omp")
            .expect_err("caps-excluded must fail");
        assert!(
            err.to_string().contains("not routable"),
            "error says why: {err}"
        );
    }

    /// Strict resolution honors the caps->fallback chain: a caps-excluded
    /// harness with a fallback resolves the fallback (strictly).
    #[test]
    fn resolve_strict_routes_fallback_chain() {
        let _g = with_config_dir();
        let path = crate::config::config_dir().join("models.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
        fs::write(
            &path,
            "[models.\"p/flagship\"]\ncaps = [\"omp\"]\nfallback = \"zai/glm-5.2\"\n",
        )
        .expect("write");
        let reg = ModelRegistry::load_routing().expect("reg");
        assert_eq!(
            reg.resolve_strict("p/flagship", "omp").expect("caps allow"),
            "p/flagship"
        );
        assert_eq!(
            reg.resolve_strict("flagship", "opencode")
                .expect("fallback"),
            "zai/glm-5.2",
            "caps-excluded -> compiled-in fallback routes strictly (bare id too)"
        );
        // A fallback pointing at an unknown id stays an error (no bypass).
        fs::write(
            &path,
            "[models.\"p/flagship\"]\ncaps = [\"omp\"]\nfallback = \"ghost\"\n",
        )
        .expect("rewrite");
        let reg = ModelRegistry::load_routing().expect("reg");
        assert!(reg.resolve_strict("flagship", "opencode").is_err());
    }

    // ---- bd-1t3j: provider/model composite keys

    /// Composite lookup hits exactly; a bare model id falls back to the
    /// DEFAULT provider for that id (alphabetically-first registry key
    /// carrying it) so one model id can exist under multiple providers.
    #[test]
    fn composite_lookup_and_bare_id_default_provider_fallback() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        reg.models
            .insert("alpha/flash".into(), ModelEntry::default());
        reg.models.insert(
            "zeta/flash".into(),
            ModelEntry {
                caps: vec!["opencode".into()],
                ..ModelEntry::default()
            },
        );
        // Exact composite hit wins over the bare-id fallback.
        assert_eq!(
            reg.get("zeta/flash").map(|e| e.caps.join(",")),
            Some("opencode".into())
        );
        // Bare id resolves to the default (first sorted = alpha).
        assert_eq!(
            reg.resolve("flash", "omp"),
            "alpha/flash",
            "bare id -> default provider, injected form"
        );
        assert_eq!(reg.resolve("alpha/flash", "grok"), "flash");
    }

    /// Strict resolution fails on an unknown provider: a well-formed but
    /// unregistered composite id is an error naming the fix.
    #[test]
    fn resolve_strict_fails_unknown_provider() {
        let reg = reg_with_keys(&["deepseek/deepseek-v4"]);
        let err = reg
            .resolve_strict("ghost/deepseek-v4", "omp")
            .expect_err("unknown provider must fail");
        assert!(
            err.to_string().contains("flywheel model add"),
            "error names the fix: {err}"
        );
    }

    /// A legacy bare key in user models.toml is a LOUD parse error naming
    /// the rewrite — never silent misresolution. Both load() and
    /// load_routing() refuse it.
    #[test]
    fn bare_toml_key_is_loud_parse_error() {
        let _g = with_config_dir();
        let path = crate::config::config_dir().join("models.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
        fs::write(&path, "[models.deepseek-v4-flash]\nprovider = \"x\"\n").expect("write legacy");
        let err = ModelRegistry::load().expect_err("bare key must fail load()");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("must be provider/model") && chain.contains("deepseek-v4-flash"),
            "error names the rewrite: {chain}"
        );
        let err = ModelRegistry::load_routing().expect_err("bare key must fail load_routing()");
        assert!(
            format!("{err:#}").contains("must be provider/model"),
            "routing layer is loud too: {err:#}"
        );
    }
}
