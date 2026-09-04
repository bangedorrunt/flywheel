//! Model registry (ADR-0010 Spec D, P10.2 + ADR-0007): `~/.config/flywheel/models.toml`
//! with `[models.ID]` entries (provider, per-mtok cost, caps, fallback, tier)
//! and `[slots.<cheap|standard|premium>]` (harness + model + fallbacks).
//! Profile.model strings resolve against the registry so the per-harness
//! injection is REAL: the registry knows how each harness wants its model
//! flag (`omp --model=ID`, `opencode -m provider/ID`, `pi --model pattern`,
//! `grok -m ID`). Slot aliases (`@cheap`) inherit the slot's harness+model.
//!
//! Resolution is never a blocker: an unknown model id warns and passes
//! through untouched. Profile-sourced ids use `resolve_strict`.
//!
//! Functional core / imperative shell: parsing + resolution are pure
//! (unit-tested); the `models.toml` read/write is the thin IO shell.
//!
//! governed-by: ADR-0010 (Spec D), ADR-0007

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

/// Cost / purpose band (ADR-0007). Discovery + recommendation, not auto-dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Cheap,
    Standard,
    Premium,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
            Self::Standard => "standard",
            Self::Premium => "premium",
        }
    }
}

/// One routing slot: default harness + model for a cost band (ADR-0007 D1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotSpec {
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_harness: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_fallback: Vec<String>,
}

/// Canonical slot names. `default` is a *squad* name, never a slot.
pub const SLOT_CHEAP: &str = "cheap";
pub const SLOT_STANDARD: &str = "standard";
pub const SLOT_PREMIUM: &str = "premium";

/// Map `@cheap` / `free` / `smol` / … onto a canonical slot name.
/// Requires the `@` prefix on profile.model so a model id `cheap` is not stolen.
pub fn slot_alias(model: &str) -> Option<&'static str> {
    let rest = model.strip_prefix('@')?;
    match rest {
        "cheap" | "free" | "smol" => Some(SLOT_CHEAP),
        "standard" => Some(SLOT_STANDARD),
        "premium" | "slow" | "plan" => Some(SLOT_PREMIUM),
        // omp `default` role → standard slot. Not the `default` squad.
        "default" => Some(SLOT_STANDARD),
        _ => None,
    }
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
    /// Cost/purpose band (ADR-0007). Absent = unclassified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
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
    #[serde(default)]
    pub slots: BTreeMap<String, SlotSpec>,
}

/// The registry: parsed `models.toml` entries keyed by `provider/model`.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    pub models: BTreeMap<String, ModelEntry>,
    pub slots: BTreeMap<String, SlotSpec>,
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
        let mut slots = BTreeMap::new();
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
            slots = file.slots;
        }
        Ok(Self {
            models,
            slots,
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
        let mut slots = compiled_in_slots();
        // A missing file is fine (zero-config); a PRESENT but unparseable
        // file is a loud error — silently skipping it would quietly drop
        // the user's routing entries (a bypass).
        let user = Self::load_from(Some(&path))?;
        let user_keys: std::collections::BTreeSet<String> = user.models.keys().cloned().collect();
        if !user.models.is_empty() {
            models.extend(user.models);
        }
        for (name, spec) in user.slots {
            slots.insert(name, spec);
        }
        Ok(Self {
            models,
            slots,
            user_keys,
        })
    }

    /// Resolve a lookup id to its registry KEY. A full `provider/model`
    /// id must hit exactly; a bare model id falls back to the DEFAULT
    /// provider for that id — a user-sourced entry if one exists, else
    /// the alphabetically-first registry key carrying it (deterministic).
    pub fn key_for(&self, id: &str) -> Option<String> {
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

    /// Canonical slot spec after alias normalization (`@cheap` / `free` / …).
    pub fn slot(&self, name: &str) -> Option<&SlotSpec> {
        let key = slot_alias(name).or_else(|| {
            // slot set / [slots.cheap] use the bare name, not `@cheap`.
            match name {
                SLOT_CHEAP | "free" | "smol" => Some(SLOT_CHEAP),
                SLOT_STANDARD => Some(SLOT_STANDARD),
                SLOT_PREMIUM | "slow" | "plan" => Some(SLOT_PREMIUM),
                "default" => Some(SLOT_STANDARD),
                _ => None,
            }
        })?;
        self.slots.get(key)
    }

    /// Inherit harness + model from a slot when `model` is `@cheap` etc.
    /// Concrete model + empty harness is an error (must pin a harness or a slot).
    pub fn resolve_profile_route(&self, harness: &str, model: &str) -> Result<(String, String)> {
        if let Some(name) = slot_alias(model) {
            let slot = self.slots.get(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "slot '{name}' is not configured — `flywheel slot set {name} --harness <h> --model <id>`"
                )
            })?;
            if slot.model.is_empty() {
                anyhow::bail!(
                    "slot '{name}' has no model — `flywheel slot set {name} --model <id>`"
                );
            }
            let h = if harness.is_empty() {
                if slot.harness.is_empty() {
                    anyhow::bail!(
                        "slot '{name}' has no harness — `flywheel slot set {name} --harness <h>`"
                    );
                }
                slot.harness.clone()
            } else {
                harness.to_string()
            };
            return Ok((h, slot.model.clone()));
        }
        if harness.is_empty() {
            anyhow::bail!(
                "profile has no harness and model {model:?} is not a slot alias \
                 (@cheap/@standard/@premium)"
            );
        }
        Ok((harness.to_string(), model.to_string()))
    }

    /// ADR-0007 D1 FULL resolve ladder: profile pin → slot (harness+model)
    /// → `model_fallback[]` → slot `fallback_harness` + a caps-compatible
    /// model → registry `ModelEntry.fallback` (caps-checked). Returns
    /// `(harness, model_id)` plus the route step taken so callers can log
    /// or report the hop. The 429 retry paths keep their own step-5 hop;
    /// this helper is the pre-flight route.
    ///
    /// `model_fallback` and `fallback_harness` are consulted ONLY when the
    /// primary id cannot run on the resolved harness (caps whitelist miss)
    /// or is absent from the registry — a healthy primary route never
    /// detours through them.
    pub fn resolve_profile_route_full(
        &self,
        harness: &str,
        model: &str,
        model_fallback: &[String],
    ) -> Result<(String, String, &'static str)> {
        let (h, primary) = self.resolve_profile_route(harness, model)?;
        let primary_ok = self
            .models
            .get(&primary)
            .map(|e| e.caps.is_empty() || e.caps.iter().any(|c| c == &h))
            // Unknown ids stay lenient here (ad-hoc operator strings pass
            // through; strictness is resolve_strict's job at spawn time).
            .unwrap_or(true);
        if primary_ok {
            return Ok((h, primary, "primary"));
        }
        // Step 3: profile-ordered model_fallback ids on the same harness.
        for id in model_fallback {
            let caps_ok = self
                .models
                .get(id)
                .map(|e| e.caps.is_empty() || e.caps.iter().any(|c| c == &h))
                .unwrap_or(false);
            if caps_ok {
                return Ok((h, id.clone(), "model_fallback"));
            }
        }
        // Step 4: the slot's fallback_harness + a routable model. Grok is
        // closed-source with its own proprietary model (operator note
        // 2026-09-05): a grok hop is MODEL-OPAQUE — no caps check, no model
        // pick; the harness runs its own model (bare id flag form).
        let slot = slot_alias(model).and_then(|n| self.slots.get(n));
        if let Some(slot) = slot
            && let Some(fh) = &slot.fallback_harness
        {
            if fh == "grok" {
                let bare = slot.model.rsplit('/').next().unwrap_or(&slot.model);
                return Ok((fh.clone(), bare.to_string(), "fallback_harness"));
            }
            let slot_model_caps_ok = self
                .models
                .get(&slot.model)
                .map(|e| e.caps.is_empty() || e.caps.iter().any(|c| c == fh))
                .unwrap_or(false);
            if slot_model_caps_ok {
                return Ok((fh.clone(), slot.model.clone(), "fallback_harness"));
            }
            let candidate = self
                .models
                .iter()
                .find(|(_, e)| e.caps.iter().any(|c| c == fh))
                .map(|(id, _)| id.clone());
            if let Some(id) = candidate {
                return Ok((fh.clone(), id, "fallback_harness"));
            }
        }
        // Step 5: registry fallback of the primary id. A grok-family
        // fallback is model-opaque (closed-source proprietary model): the
        // bare id rides as the flag, no caps check. Other families keep
        // the caps check.
        if let Some(fb) = self
            .models
            .get(&primary)
            .and_then(|e| e.fallback.clone())
        {
            let fb_h = if self.family_of(&fb) == "grok" {
                "grok".to_string()
            } else {
                h.clone()
            };
            let caps_ok = fb_h != "grok"
                && self
                    .models
                    .get(&fb)
                    .map(|e| e.caps.is_empty() || e.caps.iter().any(|c| c == &fb_h))
                    .unwrap_or(false);
            if fb_h == "grok" || caps_ok {
                if fb_h == "grok" {
                    let bare = fb.rsplit('/').next().unwrap_or(&fb).to_string();
                    return Ok((fb_h, bare, "registry_fallback"));
                }
                return Ok((fb_h, fb, "registry_fallback"));
            }
        }
        // Loud failure naming every rung tried (ADR D1: no silent hop).
        anyhow::bail!(
            "no routable model: primary {primary:?} cannot run on harness {h:?} \
             (caps), model_fallback {model_fallback:?} has no caps-compatible id, \
             slot fallback_harness unroutable, and registry fallback absent/incompatible"
        )
    }

    /// Implicit kind→slot: a bare-harness spawn with no profile uses this
    /// slot's model (ADR-0007 D1). Unknown kind → None.
    pub fn implied_slot_for_harness(&self, kind: &str) -> Option<&SlotSpec> {
        let name = match kind {
            "opencode" | "oc" => SLOT_CHEAP,
            "pi" => SLOT_STANDARD,
            "grok" | "gk" => SLOT_PREMIUM,
            _ => return None,
        };
        self.slots.get(name)
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
            slots: self.slots.clone(),
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
        ModelEntry {
            tier: Some(Tier::Standard),
            ..ModelEntry::default()
        },
    );
    m.insert(
        "opencode/big-pickle".to_string(),
        ModelEntry {
            caps: vec!["opencode".into()],
            tier: Some(Tier::Cheap),
            ..ModelEntry::default()
        },
    );
    m.insert(
        "zai/glm-5.2".to_string(),
        ModelEntry {
            tier: Some(Tier::Premium),
            ..ModelEntry::default()
        },
    );
    // Kept in the registry for explicit `--harness jcode` (bare harness).
    // Coder no longer defaults here (ADR-0007 D2: coder inherits @standard/pi).
    m.insert(
        "opencode-go/muse-spark-1.2-contributor".to_string(),
        ModelEntry {
            tier: Some(Tier::Standard),
            ..ModelEntry::default()
        },
    );
    m
}

/// Compiled-in routing slots (ADR-0007 D1). Layered under user `[slots.*]`.
pub fn compiled_in_slots() -> BTreeMap<String, SlotSpec> {
    let mut m = BTreeMap::new();
    m.insert(
        SLOT_CHEAP.to_string(),
        SlotSpec {
            harness: "opencode".into(),
            model: "opencode/big-pickle".into(),
            ..SlotSpec::default()
        },
    );
    m.insert(
        SLOT_STANDARD.to_string(),
        SlotSpec {
            harness: "pi".into(),
            model: "opencode-go/deepseek-v4-flash".into(),
            ..SlotSpec::default()
        },
    );
    m.insert(
        SLOT_PREMIUM.to_string(),
        SlotSpec {
            harness: "omp".into(),
            model: "zai/glm-5.2".into(),
            fallback_harness: Some("grok".into()),
            ..SlotSpec::default()
        },
    );
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
            tier: None,
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
            tier: None,
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
                tier: None,
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
                tier: None,
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
                tier: None,
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
            slots: Default::default(),
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

    // ---- ADR-0007: slots + tiers

    #[test]
    fn slot_alias_requires_at_and_maps_omp_names() {
        assert_eq!(slot_alias("@cheap"), Some(SLOT_CHEAP));
        assert_eq!(slot_alias("@free"), Some(SLOT_CHEAP));
        assert_eq!(slot_alias("@smol"), Some(SLOT_CHEAP));
        assert_eq!(slot_alias("@standard"), Some(SLOT_STANDARD));
        assert_eq!(slot_alias("@default"), Some(SLOT_STANDARD));
        assert_eq!(slot_alias("@premium"), Some(SLOT_PREMIUM));
        assert_eq!(slot_alias("@slow"), Some(SLOT_PREMIUM));
        assert_eq!(slot_alias("@plan"), Some(SLOT_PREMIUM));
        assert_eq!(slot_alias("cheap"), None, "bare name is not a profile.model alias");
        assert_eq!(slot_alias("opencode/big-pickle"), None);
    }

    #[test]
    fn compiled_in_slots_and_tiers_on_zero_config() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load_routing().expect("routing");
        let cheap = reg.slot("cheap").expect("cheap");
        assert_eq!(cheap.harness, "opencode");
        assert_eq!(cheap.model, "opencode/big-pickle");
        let standard = reg.slot("@standard").expect("standard via alias");
        assert_eq!(standard.harness, "pi");
        assert_eq!(standard.model, "opencode-go/deepseek-v4-flash");
        let premium = reg.slot("premium").expect("premium");
        assert_eq!(premium.harness, "omp");
        assert_eq!(premium.fallback_harness.as_deref(), Some("grok"));
        assert_eq!(
            reg.get("opencode/big-pickle").and_then(|e| e.tier),
            Some(Tier::Cheap)
        );
        assert_eq!(
            reg.get("opencode-go/deepseek-v4-flash")
                .and_then(|e| e.tier),
            Some(Tier::Standard)
        );
        assert_eq!(
            reg.get("zai/glm-5.2").and_then(|e| e.tier),
            Some(Tier::Premium)
        );
    }

    #[test]
    fn resolve_profile_route_inherits_slot_harness() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load_routing().expect("routing");
        let (h, m) = reg
            .resolve_profile_route("", "@cheap")
            .expect("cheap inherit");
        assert_eq!(h, "opencode");
        assert_eq!(m, "opencode/big-pickle");
    }

    #[test]
    fn full_ladder_primary_route_when_caps_compatible() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load_routing().expect("routing");
        // big-pickle caps=[opencode], routed on opencode → primary, no detour.
        let (h, m, route) = reg
            .resolve_profile_route_full("", "@cheap", &[])
            .expect("primary");
        assert_eq!((h.as_str(), m.as_str(), route), ("opencode", "opencode/big-pickle", "primary"));
    }

    #[test]
    fn full_ladder_caps_block_detours_to_model_fallback_then_slot_harness() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::load_routing().expect("routing");
        // Block the primary: big-pickle may NOT run on opencode anymore.
        if let Some(e) = reg.models.get_mut("opencode/big-pickle") {
            e.caps = vec!["omp".into()];
        }
        // Step 3 first: a caps-compatible model_fallback id wins.
        let (h, m, route) = reg
            .resolve_profile_route_full(
                "",
                "@cheap",
                &["opencode-go/deepseek-v4-flash".to_string()],
            )
            .expect("model_fallback rung");
        assert_eq!(route, "model_fallback", "ordered list wins before harness swap");
        assert_eq!(h, "opencode");

        // Step 4: empty model_fallback → premium's fallback_harness=grok…
        // but cheap's slot has NO fallback_harness; big-pickle is
        // caps-blocked and the registry has no other opencode-capped model
        // nor a fallback → loud failure naming the rungs (no silent hop).
        let err = reg
            .resolve_profile_route_full("", "@cheap", &[])
            .expect_err("loud failure");
        assert!(
            err.to_string().contains("no routable model"),
            "names the failure: {err}"
        );

        // Step 4 positive case: cap glm-5.2 to a NON-omp harness so the
        // omp slot route is caps-blocked; fallback_harness=grok is
        // model-opaque so no grok registry row is needed.
        if let Some(e) = reg.models.get_mut("zai/glm-5.2") {
            e.caps = vec!["pi".into()];
        }
        let (h, m, route) = reg
            .resolve_profile_route_full("", "@premium", &[])
            .expect("fallback_harness rung");
        assert_eq!(route, "fallback_harness");
        assert_eq!(h, "grok", "slot fallback_harness swaps the harness");
        assert_eq!(m, "glm-5.2", "grok hop is model-opaque: bare id as flag, no registry pick");
    }

    #[test]
    fn full_ladder_registry_fallback_rung_when_slot_has_no_fallback_harness() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::load_routing().expect("routing");
        // cheap slot has no fallback_harness; give big-pickle a registry
        // fallback to a caps-compatible id and block the primary caps.
        if let Some(e) = reg.models.get_mut("opencode/big-pickle") {
            e.caps = vec!["omp".into()];
            e.fallback = Some("opencode-go/deepseek-v4-flash".into());
        }
        let (h, m, route) = reg
            .resolve_profile_route_full("", "@cheap", &[])
            .expect("registry_fallback rung");
        assert_eq!(route, "registry_fallback");
        assert_eq!(h, "opencode", "non-grok fallback keeps the harness");
        assert_eq!(m, "opencode-go/deepseek-v4-flash");
    }

    #[test]
    fn user_slot_overlay_wins_and_save_does_not_bake_compiled_ins() {
        let _g = with_config_dir();
        let path = crate::config::config_dir().join("models.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
        fs::write(
            &path,
            r#"
[slots.cheap]
harness = "opencode"
model = "opencode/big-pickle"
"#,
        )
        .expect("write overlay");
        let routing = ModelRegistry::load_routing().expect("routing");
        assert_eq!(
            routing.slot("cheap").map(|s| s.model.as_str()),
            Some("opencode/big-pickle")
        );
        assert!(
            routing.slot("standard").is_some(),
            "compiled-in standard survives a cheap-only overlay"
        );
        let user = ModelRegistry::load().expect("user file");
        assert!(
            !user.slots.contains_key(SLOT_STANDARD),
            "user file must not contain baked compiled-in slots"
        );
        assert_eq!(user.slots.len(), 1);
        user.save().expect("save user-only");
        let raw = fs::read_to_string(&path).expect("reread");
        assert!(
            !raw.contains("[slots.standard]"),
            "save must not bake compiled-in standard: {raw}"
        );
    }

    #[test]
    fn implied_slot_for_bare_harness() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load_routing().expect("routing");
        assert_eq!(
            reg.implied_slot_for_harness("opencode")
                .map(|s| s.model.as_str()),
            Some("opencode/big-pickle")
        );
        assert_eq!(
            reg.implied_slot_for_harness("pi").map(|s| s.harness.as_str()),
            Some("pi")
        );
        assert_eq!(
            reg.implied_slot_for_harness("grok")
                .and_then(|s| s.fallback_harness.as_deref()),
            Some("grok"),
            "grok implies premium, whose fallback_harness is grok"
        );
        assert!(reg.implied_slot_for_harness("omp").is_none());
    }

    #[test]
    fn resolve_profile_route_rejects_empty_harness_without_slot() {
        let reg = ModelRegistry::default();
        let err = reg
            .resolve_profile_route("", "zai/glm-5.2")
            .expect_err("must pin harness or slot");
        assert!(err.to_string().contains("@cheap"));
    }
}
