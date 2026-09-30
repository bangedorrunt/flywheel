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

/// Operator-wide default model (bd-ll3k): when BOTH fields are set, every
/// slot alias (`@cheap`/`@standard`/`@premium`) resolves through this pair
/// instead of its slot. Empty/absent = legacy per-slot behavior. Explicit
/// per-profile harness and concrete (non-alias) model ids always win —
/// defaults replace the SLOT layer only, never an explicit choice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultsSpec {
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

impl DefaultsSpec {
    /// Active only when both halves are set — a half-written record is
    /// inert, never a misroute.
    pub fn active(&self) -> bool {
        !self.harness.is_empty() && !self.model.is_empty()
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

/// The harness a profile lands on when it names none, and its model is CONCRETE
/// (so no slot can supply a more specific answer).
///
/// bd-47fu, operator-chosen 2026-09-29. It lives beside the tier constants because
/// it is the same kind of fact: a compiled-in default the operator sets, not a
/// per-machine choice. Making it a `models.toml` key would be a new config surface
/// wanting its own ADR, and nothing needs it yet.
///
/// It is deliberately NOT `[defaults]`. That replaces the slot layer for every
/// alias, so `@cheap` would leave opencode and `big-pickle` — free on opencode only
/// — would stop being free. This constant is reached only when a profile names no
/// harness AND its model is concrete, which is the one case that used to bail.
pub const DEFAULT_HARNESS: &str = "pi";

/// bd-ll3k: kind → slot NAME mapping behind [`ModelRegistry::implied_slot_for_harness`].
/// Split out so consumers that resolve through [`ModelRegistry::effective_slot_model`]
/// (defaults-aware) share the exact mapping instead of re-matching kinds.
pub fn implied_slot_name_for_harness(kind: &str) -> Option<&'static str> {
    // bd-drlm: the slot table moved to `harness_registry` with every other
    // naming fact, so a harness's name, aliases, tier and slot cannot drift
    // apart across files.
    crate::harness_registry::implied_slot(kind)
}

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
    /// Operator-wide default (bd-ll3k): `[defaults] harness/model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<DefaultsSpec>,
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
    /// Operator-wide default pair (bd-ll3k): merged from user `[defaults]`
    /// by [`ModelRegistry::load_routing`]; `None` (or inactive) = legacy
    /// per-slot behavior.
    pub defaults: Option<DefaultsSpec>,
}

/// Where a rate-limited run retries: the registry `ModelEntry.fallback` rung
/// (bd-42fs, ADR-0024 W1a). The hop's identity is `model_id` (the registry
/// id); the harness and the injection string are derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackRoute {
    /// the registry `fallback` id that drove the hop
    pub model_id: String,
    /// harness that runs it — `grok` when the rung's family is grok (that
    /// harness is coupled and cannot inherit the caller's), else the caller's
    pub harness: String,
    /// the injection string [`ModelRegistry::resolve`] produced for `harness`
    pub model: String,
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
        let mut defaults = None;
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
            if let Some(d) = &file.defaults
                && d.model.starts_with('@')
            {
                anyhow::bail!(
                    "models.toml: [defaults] model must be a concrete id — defaults cannot alias slots"
                );
            }
            models = file.models;
            slots = file.slots;
            defaults = file.defaults;
        }
        Ok(Self {
            models,
            slots,
            user_keys: Default::default(),
            defaults,
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
        // bd-ll3k: user `[defaults]` replaces wholesale (one record —
        // whole-replace is correct here, unlike per-field stage merges).
        // Inactive (half-written) records stay inert downstream.
        let defaults = user.defaults;
        Ok(Self {
            models,
            slots,
            user_keys,
            defaults,
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
        default.map(std::string::ToString::to_string)
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
        let key = slot_alias(name).or({
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
            // bd-ll3k: active `[defaults]` short-circuits the slot layer —
            // every alias resolves to the default pair. An explicit profile
            // harness still wins; concrete ids never reach this arm.
            if let Some(d) = self.defaults.as_ref().filter(|d| d.active()) {
                let h = if harness.is_empty() {
                    d.harness.clone()
                } else {
                    harness.to_string()
                };
                return Ok((h, d.model.clone()));
            }
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
            // bd-47fu: pi is the DEFAULT HARNESS for a profile that names none. This
            // is the operator's stated choice (2026-09-29) and it resolves bd-ttaa,
            // which recorded the missing concept instead of inventing one.
            //
            // NOT the same as `[defaults]`, and the difference is the whole cost
            // consequence: `[defaults]` replaces the slot layer for EVERY alias, so
            // `@cheap` would leave opencode and `big-pickle` — free on opencode only —
            // would stop being free. This applies at exactly the one point where
            // nothing more specific exists, so `@cheap` keeps its slot's harness and
            // the free tier is untouched.
            let default = DEFAULT_HARNESS;
            // The gate is STRUCTURAL. A default that names an unregistered harness, or
            // a Closed-tier one that may never be handed a model, would reintroduce
            // precisely the bug bd-ziy9 refused to fix. Refuse loudly instead: a
            // misconfigured default must not become a silent misroute.
            let usable = crate::harness_registry::tier_of(default)
                .is_some_and(crate::harness_registry::HarnessTier::may_be_asked_for_a_model);
            if !usable {
                anyhow::bail!(
                    "the default harness {default:?} is not an Open-tier registered harness, so \
                     a profile may not be routed onto it. Fix \
                     `DEFAULT_HARNESS` in models.rs, or name a harness on the profile."
                );
            }
            return Ok((default.to_string(), model.to_string()));
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
        // Step 3: model_fallback ids on the same harness. The PROFILE's list first,
        // then the SLOT's.
        //
        // bd-47fu FIXED A REAL DEFECT HERE. `SlotSpec.model_fallback` was read by
        // exactly one thing — `flywheel slot show`, which DISPLAYS it — and by no
        // routing path at all: this step walked only the profile's list. So a slot
        // could declare a model fallback, an operator could see it in `slot show` and
        // reasonably believe it routed, and the ladder would never consult it. The
        // compiled-in standard slot's `zai/glm-5.3-flash` was already inert for this
        // reason, which is how it went unnoticed: nothing failed, a route just
        // happened elsewhere.
        //
        // The profile's list leads because it is the more specific statement. A
        // profile that names its own fallback means it; the slot's list is the
        // tier-wide default underneath. Letting the slot's list win would silently
        // override an explicit per-profile choice.
        let slot_fallbacks: &[String] = slot_alias(model)
            .and_then(|name| self.slots.get(name))
            .map_or(&[] as &[String], |s| s.model_fallback.as_slice());
        for id in model_fallback.iter().chain(slot_fallbacks) {
            let caps_ok = self
                .models
                .get(id.as_str())
                .map(|e| e.caps.is_empty() || e.caps.iter().any(|c| c == &h))
                .unwrap_or(false);
            if caps_ok {
                return Ok((h, id.clone(), "model_fallback"));
            }
        }
        // Step 4: the slot's fallback_harness + a routable model. The
        // grok HARNESS only accepts its own proprietary model (operator
        // note 2026-09-05 — grok MODELS are available on pi/omp/opencode,
        // but the grok harness runs no other model): a grok hop is
        // MODEL-OPAQUE — no caps check, no model pick; bare id flag form.
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
        // fallback is model-opaque (the grok harness runs only its own
        // proprietary model): the bare id rides as the flag, no caps
        // check. Other families keep the caps check.
        if let Some(fb) = self.models.get(&primary).and_then(|e| e.fallback.clone()) {
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
        // bd-ziy9 step 6: the DYNAMIC DEFAULT, and the rung the profile ladder was
        // missing. Every rung above names something DECLARED — the profile's pin,
        // the slot's model, an authored `model_fallback[]`, a slot's
        // `fallback_harness`, a registry `fallback`. None of them can answer
        // "nothing was declared for THIS harness, so what runs here?", which is
        // exactly the bare-harness profile's question: `harness: ""` means inherit
        // the crew's harness, and `@cheap` names a tier, not a model.
        //
        // The bare path has answered that question since bd-88m4
        // (`bare_model_launch_args` -> `coupled_default_model` -> the harness's
        // implied slot), and it had exactly one caller while this function had
        // none. That asymmetry is the whole bug: a bare spawn of the same intent
        // routed, and a profile carrying that intent failed on 8 of 9 harnesses.
        //
        // Order is deliberate, and each rung is a rung the bare path also uses:
        //   6a. the harness's IMPLIED slot, honoring `[defaults]` — but only when
        //       its model can actually run here, because a slot's model is a
        //       declaration about its OWN harness, not a licence anywhere.
        //   6b. a coupled harness's own family model (grok/claude/codex).
        //   6c. a portable registry pick.
        //
        // NOTHING HERE OPTIMISES COST, and that is a finding rather than an
        // omission: every registered model reports `cost_in_per_mtok = 0.0`, so the
        // registry holds NO cost data at all. A "cheapest routable model" ordering
        // would invent a cost fact, which is the same class of error as inferring an
        // empty dispatch's cause from one unrepresentative sample. So 6c prefers the
        // tier's own provider FAMILY (preserving intent), then takes the
        // lexicographically first portable id — `models` is a `BTreeMap`, so that is
        // a stable order rather than a hash order, and the returned step name says
        // which rung fired so the ledger records a pick nobody has to reverse-engineer.
        if !h.is_empty() {
            // The tier gate is checked ONCE, here, and it is what makes the rungs
            // below safe: a Closed harness is documented as "Frontier CLI with its
            // own model. Never asked for one" (harness_registry.rs:25). Handing
            // codex or claude a `zai/*` id to turn a matrix green would delete that
            // constraint rather than satisfy the requirement, so the whole dynamic
            // block is unreachable for them — structurally, not by a check someone
            // can forget in a later edit.
            //
            // `tier_of` is `None` for an unregistered name, and `None` is treated as
            // NOT open. An unknown harness must never be handed a model on the
            // strength of not being known to be closed.
            let open = crate::harness_registry::tier_of(&h)
                .is_some_and(crate::harness_registry::HarnessTier::may_be_asked_for_a_model);
            if open {
                // The harness's own implied slot, honoring `[defaults]`. `self.slots`
                // is keyed by slot name, which is why the name comes from
                // `implied_slot_name_for_harness` rather than off the `SlotSpec`.
                let implied = implied_slot_name_for_harness(&h)
                    .and_then(|name| self.slots.get(name).map(|s| (name, s)));
                if let Some((name, slot)) = implied {
                    let m = self
                        .effective_slot_model(name)
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| slot.model.clone());
                    // A slot's model is a declaration about its OWN harness, not a
                    // licence to run anywhere: pi's `@standard` model is portable, but
                    // the general rule has to survive a slot that is not.
                    if self.caps_permit(&m, &h) {
                        return Ok((h, m, "implied_slot"));
                    }
                }
                // 6b: a coupled harness's own family model. No `claude/`, `codex/` or
                // `grok/` id is registered today, so this is None in practice — it is
                // here so the day one is, the profile path picks it up rather than
                // needing the same change made twice.
                if let Some(m) = self.coupled_default_model(&h) {
                    return Ok((h, m, "coupled_default"));
                }
                // 6c: last resort. A `harness: ""` profile is the caller that lands
                // here with a real intent to preserve, so the TIER's own family
                // leads — read off the slot the profile named, not off whatever
                // model happened to fail, so `@cheap` still means "an opencode-family
                // model if one can run here".
                let want_family = slot_alias(model)
                    .and_then(|name| self.slots.get(name))
                    .map(|slot| self.family_of(&slot.model));
                if let Some(pick) = self.portable_pick(&h, want_family.as_deref()) {
                    return Ok((h, pick, "dynamic_default"));
                }
            }
        }
        // Loud failure naming every rung tried (ADR D1: no silent hop).
        anyhow::bail!(
            "no routable model: primary {primary:?} cannot run on harness {h:?} \
             (caps), model_fallback {model_fallback:?} has no caps-compatible id, \
             slot fallback_harness unroutable, and registry fallback absent/incompatible"
        )
    }

    /// Can `id` run on `harness`? A missing entry is **false**, deliberately: the
    /// dynamic-default rungs choose from registered models, and treating an unknown
    /// id as routable would let the ladder hand a caller a name nothing can serve.
    /// [`Self::resolve_profile_route_full`]'s *primary* check stays lenient for
    /// ad-hoc operator strings, which is a different question and is resolved
    /// strictly at spawn time.
    fn caps_permit(&self, id: &str, harness: &str) -> bool {
        self.models
            .get(id)
            .is_some_and(|e| e.caps.is_empty() || e.caps.iter().any(|c| c == harness))
    }

    /// The stable portable pick for step 6c: the first registered model this
    /// harness can run, preferring `want_family` so a tier's provider intent
    /// survives, then plain order.
    ///
    /// Deterministic because `self.models` is a `BTreeMap` — the order is
    /// lexicographic on the id, not a hash order that could change the resolved
    /// route between two runs of the same config. That matters: the spawn ledger
    /// records the RESOLVED route, and a route that moves run-to-row makes the
    /// ledger unreadable.
    fn portable_pick(&self, harness: &str, want_family: Option<&str>) -> Option<String> {
        let ok = |id: &str| self.caps_permit(id, harness);
        if let Some(fam) = want_family
            && let Some(id) = self
                .models
                .keys()
                .find(|id| ok(id) && self.family_of(id) == fam)
        {
            return Some(id.clone());
        }
        self.models.keys().find(|id| ok(id)).cloned()
    }

    /// Slot model honoring `[defaults]` (bd-ll3k): the role-flag surface
    /// (`resolve_role_flag` / `expand_slot_model`) funnels here so slow/
    /// plan stop burning premium the moment defaults are set. `None` = no
    /// such slot (unchanged legacy behavior).
    pub fn effective_slot_model(&self, name: &str) -> Option<String> {
        if let Some(d) = self.defaults.as_ref().filter(|d| d.active()) {
            return Some(d.model.clone());
        }
        self.slots.get(name).map(|s| s.model.clone())
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

    /// The route for a fallback id the caller already chose — a profile's
    /// `model_fallback` entry. Same grok coupling and resolution as the
    /// registry rung, minus the lookup, and infallible: a config-authored hop
    /// and a registry-authored one cannot end up routed differently.
    ///
    /// The grok flip is not a preference: the grok harness runs only its own
    /// models, so a grok-family rung cannot inherit the caller's harness.
    pub fn route_for(&self, model_id: &str, harness: &str) -> FallbackRoute {
        let harness = if self.family_of(model_id) == "grok" {
            "grok".to_string()
        } else {
            harness.to_string()
        };
        let model = self.resolve(model_id, harness.as_str());
        FallbackRoute {
            model_id: model_id.to_string(),
            harness,
            model,
        }
    }

    /// The registry rung for `model_id`: its `ModelEntry.fallback`, routed
    /// through [`Self::route_for`]. `None` = this model has no rung (unknown
    /// id, or no `fallback`), and the caller keeps whatever it was going to do
    /// — the original error, or the next rung of its own ladder.
    pub fn fallback_route(&self, model_id: &str, harness: &str) -> Option<FallbackRoute> {
        let rung = self.get(model_id)?.fallback.clone()?;
        Some(self.route_for(&rung, harness))
    }

    /// bd-88m4: coupled-harness default, the grok pattern generalized —
    /// grok, claude and codex harnesses run only their own models
    /// (model-opaque, no key needed): a registered `<harness>/...` family
    /// id (user entries first, then any compiled-in one), else `None` so
    /// the caller keeps current behavior (implied slot or harness
    /// default). Non-coupled harnesses always return `None`. Never a
    /// foreign-provider id (operator note, `resolve_profile_route_full`
    /// step 4: grok with no family id fails loud, never the premium zai id).
    pub fn coupled_default_model(&self, harness: &str) -> Option<String> {
        if !matches!(harness, "grok" | "claude" | "codex") {
            return None;
        }
        let mut user = self.models.keys().filter(|k| {
            self.user_keys.contains(k.as_str()) && self.family_of(k.as_str()) == harness
        });
        if let Some(key) = user.next() {
            return Some(key.clone());
        }
        self.models
            .keys()
            .find(|k| self.family_of(k.as_str()) == harness)
            .cloned()
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
            defaults: self.defaults.clone(),
        })?;
        std::fs::write(path, raw).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
    /// Implicit kind→slot: a bare-harness spawn with no profile uses this
    /// slot's model (ADR-0007 D1). Unknown kind → None.
    ///
    /// NOTE (bd-ovew): the returned model's provider is NOT harness-checked
    /// — notably `grok` maps here for its budget band while its model is a
    /// zai id the grok harness cannot run. Consumers launching on grok must
    /// go through [`Self::coupled_default_model`] (route) or fail loud, never
    /// imply this slot's model onto that harness.
    #[cfg(test)]
    /// The slot whose model stands in when a profile names none.
    ///
    /// bd-drlm: the tier gate lives HERE, at the one chokepoint every
    /// model-substitution path goes through. A Closed harness returns `None`
    /// even when it carries an implied slot name, because the slot's entire
    /// job is to supply the model a frontier CLI must never be handed. The
    /// slot NAME is kept in the registry for config compatibility; what is
    /// withheld is the substitution. `slots.premium.fallback_harness` is a
    /// different mechanism and is untouched — a fallback names which harness
    /// to try, it does not hand one a model.
    pub fn implied_slot_for_harness(&self, kind: &str) -> Option<&SlotSpec> {
        if !crate::harness_registry::tier_of(kind)?.may_be_asked_for_a_model() {
            return None;
        }
        let name = implied_slot_name_for_harness(kind)?;
        self.slots.get(name)
    }
}

/// Compiled-in routing defaults: the fleet's canonical models, layered
/// under user `models.toml` entries by [`ModelRegistry::load_routing`].
/// Mirrors the compiled-in profile defaults (ADR-0005): catalog profiles
/// reference these bare ids, so routing works zero-config and the registry
/// stays the SINGLE place a provider prefix lives.
///
/// `big-pickle` is capped to the `opencode` harness — it is free ONLY there,
/// and strict resolution refuses it elsewhere.
///
/// bd-ziy9 CORRECTED THE CLAUSE THAT USED TO BE HERE. This comment read "no
/// fallback by design, the scout profile pins the opencode harness", and the
/// second half was FALSE: the builtin scout profile is `harness: ""`, which
/// means it inherits whatever harness the crew supplies. The whole "no fallback
/// is by design" argument was therefore resting on a pin that did not exist, and
/// the consequence was a profile that could not crew 8 of the 9 registered
/// harnesses.
///
/// The cap itself was never the defect and is untouched — it encodes that the
/// model is free on opencode and nowhere else, which is a cost fact. What was
/// missing was not a fallback for `big-pickle` but a DEFAULT on the HARNESS
/// side, so that a profile naming a tier rather than a model has something to
/// resolve to on a harness where the tier's own model cannot run. That is
/// `resolve_profile_route_full`'s dynamic-default rung, and a bare-harness spawn
/// has had it since bd-88m4 while the profile path had no equivalent.
pub fn compiled_in_models() -> BTreeMap<String, ModelEntry> {
    let mut m = BTreeMap::new();
    m.insert(
        // bd-47fu: renamed from `opencode-go/deepseek-v4.1-flash` at the operator's
        // direction, with the cost below from the same table. The rename is a
        // rename with teeth: this id is asserted on by name in verify-0007.sh and in
        // ~50 places across 12 files, so every one of those is a live reference, not
        // documentation.
        "opencode-go/deepseek-v4.1-flash".to_string(),
        ModelEntry {
            // Operator cost table 2026-09-29: $0.30 in / $1.20 out per Mtok.
            cost_in_per_mtok: 0.30,
            cost_out_per_mtok: 1.20,
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
    // bd-47fu: the premium tier's model fallback. `caps` is left empty, which reads
    // as PORTABLE, and that is the whole point of a fallback: it has to be able to
    // run wherever the primary could not, or it is not a fallback. A capped entry
    // here would make the premium slot fail on exactly the harnesses the fallback
    // exists to rescue.
    m.insert(
        "opencode-go/gpt-6-luna".to_string(),
        ModelEntry {
            // Operator cost table 2026-09-29: $0.10 in / $0.50 out per Mtok.
            cost_in_per_mtok: 0.1,
            cost_out_per_mtok: 0.5,
            tier: Some(Tier::Premium),
            ..ModelEntry::default()
        },
    );
    // bd-47fu: from the operator cost table and previously unregistered. `caps`
    // is empty, i.e. PORTABLE, which is what lets it serve as a fallback on any
    // harness. At $1.74/$3.84 it is the most expensive entry here, so a cost-aware
    // ordering must place it LAST among known-cost models rather than assume it
    // is a mid tier.
    m.insert(
        "deepseek/deepseek-v4-pro".to_string(),
        ModelEntry {
            // Operator cost table 2026-09-29: $1.74 in / $3.84 out per Mtok.
            cost_in_per_mtok: 1.74,
            cost_out_per_mtok: 3.84,
            tier: Some(Tier::Premium),
            ..ModelEntry::default()
        },
    );
    m.insert(
        "zai/glm-5.3".to_string(),
        ModelEntry {
            tier: Some(Tier::Premium),
            ..ModelEntry::default()
        },
    );
    // Kept in the registry for explicit `--harness jcode` (bare harness).
    // Coder no longer defaults here (ADR-0007 D2: coder inherits @standard).
    m.insert(
        "opencode-go/muse-spark-1.3-contributor".to_string(),
        ModelEntry {
            // Operator cost table 2026-09-29: $0.10 in / $0.20 out per Mtok.
            cost_in_per_mtok: 0.10,
            cost_out_per_mtok: 0.20,
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
            model: "opencode-go/muse-spark-1.3-contributor".into(),
            // bd-47fu: repointed from the removed `zai/glm-5.3-flash`. This is the
            // standard tier's ONLY fallback and it became LIVE in this same change
            // (the slot list was previously read only by `slot show`), so removing
            // the model without repointing here would have left @standard with a
            // fallback id that resolves to nothing.
            //
            // COST, stated because it is a real jump: the primary is
            // `muse-spark-1.3-contributor` at $0.10/$0.20, and this fallback is
            // `deepseek-v4.1-flash` at $0.30/$1.20 — 3x the input and 6x the output.
            model_fallback: vec!["opencode-go/deepseek-v4.1-flash".into()],
            ..SlotSpec::default()
        },
    );
    m.insert(
        SLOT_PREMIUM.to_string(),
        SlotSpec {
            // bd-47fu: was "omp", which the operator removed from the harness
            // registry. Repointed to `pi` — the same harness the operator's
            // models.toml already pins premium to, so compiled-in and user config
            // now AGREE. Leaving it on a removed harness would have made every
            // zero-config install resolve `@premium` onto a harness that does not
            // exist, which is the kind of mismatch that only shows up on a machine
            // without a models.toml.
            harness: "pi".into(),
            model: "zai/glm-5.3".into(),
            // bd-47fu: the premium tier's MODEL fallback, which is a different
            // mechanism from `fallback_harness` below and from the registry-level
            // `fallback` (ADR-0013 D4 keeps all three distinct). Order matters: the
            // ladder walks `model_fallback` on the SAME harness first, so a
            // caps-blocked premium primary tries this before it swaps harness.
            model_fallback: vec!["opencode-go/gpt-6-luna".into()],
            fallback_harness: Some("grok".into()),
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
            defaults: None,
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

    /// One row per P0 (priority <= 1) verifier plan: a second family
    /// dispatches the different-family id, a single-family swarm degrades
    /// (blocked verifier-family-missing, one captain mail, no respawn loop).
    #[test]
    fn verifier_plan_p0_rows() {
        let rows = [
            (
                "second-family-dispatches",
                reg_with_keys(&["deepseek/deepseek-v4", "anthropic/opus-5"]),
                &["deepseek/deepseek-v4", "anthropic/opus-5"][..],
                VerifierPlan::Dispatch("anthropic/opus-5".into()),
            ),
            (
                "single-family-degrades",
                reg_with_keys(&["deepseek/deepseek-v4", "deepseek/deepseek-v3"]),
                &["deepseek/deepseek-v4", "deepseek/deepseek-v3"][..],
                VerifierPlan::Degrade,
            ),
        ];
        for (name, reg, candidates, expected) in rows {
            let b = bead(0, Blast::Normal, true);
            let plan = reg.verifier_plan(
                &b,
                AcShape::Checkable,
                "deepseek/deepseek-v4",
                candidates,
                None,
            );
            assert_eq!(plan, expected, "row {name}");
        }
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
            reg.resolve("opencode-go/deepseek-v4.1-flash", "omp"),
            "opencode-go/deepseek-v4.1-flash",
            "compiled-in deepseek routes zero-config (composite key)"
        );
        assert_eq!(
            reg.resolve("deepseek-v4.1-flash", "omp"),
            "opencode-go/deepseek-v4.1-flash",
            "bare id falls back to the compiled-in default provider"
        );
        assert_eq!(reg.resolve("glm-5.3", "omp"), "zai/glm-5.3");
        // big-pickle is free ONLY on the opencode harness (caps whitelist).
        assert_eq!(reg.resolve("big-pickle", "opencode"), "opencode/big-pickle");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("config dir");
        // A user entry OVERRIDES the compiled-in default for the same id.
        fs::write(&path, "[models.\"my-gateway/deepseek-v4.1-flash\"]\n")
            .expect("write user models.toml");
        let reg = ModelRegistry::load_routing().expect("reload routing");
        assert_eq!(
            reg.resolve("deepseek-v4.1-flash", "omp"),
            "my-gateway/deepseek-v4.1-flash",
            "user entry wins over compiled-in"
        );
        assert_eq!(
            reg.resolve("glm-5.3", "omp"),
            "zai/glm-5.3",
            "other default intact"
        );

        // load() (the CLI write path) must NOT see compiled-ins.
        let user = ModelRegistry::load().expect("user file");
        assert!(
            user.get("glm-5.3").is_none(),
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
            reg.resolve_strict("deepseek-v4.1-flash", "omp")
                .expect("known ok"),
            "opencode-go/deepseek-v4.1-flash"
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
            "[models.\"p/flagship\"]\ncaps = [\"omp\"]\nfallback = \"zai/glm-5.3\"\n",
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
            "zai/glm-5.3",
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
        // A DOT-FREE id on purpose. `[models.deepseek-v4.1-flash]` is a DOTTED KEY PATH
        // in TOML, not a bare key: it parses as models -> deepseek-v4 -> 1-flash, so
        // the fixture stopped being the thing this test is about. The dotted case has
        // its own test below, because it now matters and did not before bd-47fu
        // introduced the first model id containing a dot.
        fs::write(&path, "[models.legacyflash]\nprovider = \"x\"\n").expect("write legacy");
        let err = ModelRegistry::load().expect_err("bare key must fail load()");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("must be provider/model") && chain.contains("legacyflash"),
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
        assert_eq!(
            slot_alias("cheap"),
            None,
            "bare name is not a profile.model alias"
        );
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
        assert_eq!(standard.model, "opencode-go/muse-spark-1.3-contributor");
        assert_eq!(
            standard.model_fallback,
            vec!["opencode-go/deepseek-v4.1-flash"],
            "bd-47fu repointed the standard fallback; the id must be a LIVE registry entry"
        );
        let premium = reg.slot("premium").expect("premium");
        assert_eq!(
            premium.harness, "pi",
            "bd-47fu: repointed from the removed omp; agrees with the operator models.toml"
        );
        assert_eq!(premium.fallback_harness.as_deref(), Some("grok"));
        assert_eq!(
            reg.get("opencode/big-pickle").and_then(|e| e.tier),
            Some(Tier::Cheap)
        );
        assert_eq!(
            reg.get("opencode-go/deepseek-v4.1-flash")
                .and_then(|e| e.tier),
            Some(Tier::Standard)
        );
        assert_eq!(
            reg.get("zai/glm-5.3").and_then(|e| e.tier),
            Some(Tier::Premium)
        );
        assert_eq!(
            reg.get("opencode-go/deepseek-v4.1-flash")
                .and_then(|e| e.tier),
            Some(Tier::Standard)
        );
        assert_eq!(
            reg.get("opencode-go/muse-spark-1.3-contributor")
                .and_then(|e| e.tier),
            Some(Tier::Standard)
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
        assert_eq!(
            (h.as_str(), m.as_str(), route),
            ("opencode", "opencode/big-pickle", "primary")
        );
    }

    /// bd-ziy9: the dynamic-default rung, and the three properties that make it
    /// safe rather than merely convenient.
    ///
    /// 1. It RESOLVES a bare-harness profile on an OPEN-tier harness that the
    ///    declared rungs cannot serve. Before the rung, `scout` failed on 5 of 9.
    /// 2. It REFUSES a CLOSED-tier harness, because a frontier CLI is documented
    ///    as "Never asked for one". This is the half a naive fix would delete.
    /// 3. It is DETERMINISTIC, because the spawn ledger records the resolved route
    ///    and a route that moves between runs makes the ledger unreadable.
    #[test]
    fn the_dynamic_default_resolves_open_tier_refuses_closed_tier_and_is_deterministic() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load_routing().expect("routing");

        // (1) A `harness: ""` profile naming a TIER, on an open-tier harness with no
        // implied slot and no declared fallback. bd-47fu: pa is gone and
        // `fx` is the remaining Open harness whose implied slot is None —
        // exactly that cell.
        let open = reg
            .resolve_profile_route_full("fx", "@cheap", &[])
            .expect("an open-tier harness must resolve via the dynamic default");
        assert_eq!(
            open.2, "dynamic_default",
            "the rung must NAME itself so the ledger records which hop fired: {open:?}"
        );
        assert!(
            reg.caps_permit(&open.1, "fx"),
            "the pick must be a model that can actually run here, not a hopeful one: {open:?}"
        );

        // (2) A Closed-tier harness must NOT be handed a model. codex is the clean
        // case: it is Closed and has no implied slot and no registered codex/
        // family, so nothing may be substituted for it.
        let refused = reg.resolve_profile_route_full("codex", "@cheap", &[]);
        assert!(
            refused.is_err(),
            "a Closed-tier harness must never be given a foreign model, got {refused:?}"
        );
        // The refusal must NAME the reason, so it reads as a decision rather than a
        // dead end — ADR D1's "no silent hop" applied to the refusal direction.
        let msg = format!("{:#}", refused.expect_err("must fail"));
        assert!(
            msg.contains("codex") && msg.contains("no routable model"),
            "the refusal must name the harness and the reason: {msg}"
        );

        // (3) Same input, same answer. `models` is a BTreeMap so the pick is
        // lexicographic, not hash-ordered; if that ever changes this catches it.
        let a = reg
            .resolve_profile_route_full("fx", "@cheap", &[])
            .expect("route");
        let b = reg
            .resolve_profile_route_full("fx", "@cheap", &[])
            .expect("route");
        assert_eq!(a, b, "the dynamic default must be stable across calls");

        // (4) THE ONE THAT DECIDES WHETHER ANY OF THIS IS REAL. The spawn path does
        // not stop at the ladder: `resolve_profile_launch_plan` (main.rs:3931) feeds
        // the ladder's model straight into `resolve_strict`, which REQUIRES a
        // registry entry and refuses a caps-excluded harness. A dynamic pick that
        // does not survive strict resolution is a green matrix over a spawn that
        // still fails, so the two must be checked in the same breath.
        for (harness, model) in [
            ("fx", &open.1),
            (
                "fx",
                &reg.resolve_profile_route_full("fx", "@cheap", &[])
                    .expect("fx routes")
                    .1,
            ),
        ] {
            let flag = reg.resolve_strict(model, harness).unwrap_or_else(|e| {
                panic!(
                    "the dynamic pick {model:?} must survive strict resolution on {harness}: {e}"
                )
            });
            assert!(
                !flag.is_empty(),
                "strict resolution of a real pick must yield a flag, not an empty string"
            );
        }
        // (5) THE WHOLE CHAIN, ending in launch ARGS rather than a model id. Rungs
        // 6a-6c are proven in the registry above, but the operator's question is "can
        // this profile crew a pa crew", and that is decided by
        // `profile_launch_plan` -> `resolve_strict` -> argv. Testing the registry
        // alone leaves the last mile unproven, and an unproven last mile is how a
        // green matrix hides a spawn that still fails.
        // bd-47fu: omp, pa, jcode are dropped. Only OPEN-tier harnesses
        // must build a plan from an empty profile — a Closed harness is
        // REQUIRED to refuse (that is the closed-tier contract, proven in
        // `the_closed_tier_never_substitutes_a_model`).
        for h in ["opencode", "pi", "fx"] {
            let plan = crate::profile_launch_plan(
                h,
                &crate::profile::Profile {
                    harness: String::new(),
                    model: "@cheap".into(),
                    effort: "low".into(),
                    provider: String::new(),
                    model_fallback: Vec::new(),
                    ..Default::default()
                },
                None,
            )
            .unwrap_or_else(|e| panic!("the scout profile must build a launch plan on {h}: {e}"));
            assert!(
                !plan.model.is_empty(),
                "{h}: a plan with no model would launch model-less, which is not what the \
                 dynamic default decided"
            );
        }
    }

    /// bd-47fu: a profile with a CONCRETE model and no harness now lands on the
    /// default harness instead of bailing. This test used to assert the OPPOSITE —
    /// it pinned "no default harness, deliberately" — and the operator has now made
    /// that choice, so the assertion is inverted rather than deleted. Keeping the
    /// test (and its reasoning) is the point: the decision is recorded in code, not
    /// just in a bead.
    ///
    /// The contrast that carries the cost guarantee: a SLOT profile must still get
    /// its SLOT's harness, so `@cheap` keeps opencode and `big-pickle` stays free.
    /// A default that leaked into the slot path would end the free tier silently,
    /// which is the failure `[defaults]` would have caused.
    #[test]
    fn a_concrete_model_with_no_harness_lands_on_the_default_and_slots_keep_their_own() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load_routing().expect("routing");

        // (1) The new default. A concrete model, no harness -> DEFAULT_HARNESS.
        let (h, m, step) = reg
            .resolve_profile_route_full("", "opencode-go/muse-spark-1.3-contributor", &[])
            .expect("the default harness means this now resolves");
        assert_eq!(
            h, DEFAULT_HARNESS,
            "the default must be the one that was chosen"
        );
        assert_eq!(
            m, "opencode-go/muse-spark-1.3-contributor",
            "the model is unchanged"
        );
        assert_eq!(
            step, "primary",
            "the model's own route is untouched by the default"
        );

        // (2) THE cost guarantee. `@cheap` must still resolve to its SLOT's harness,
        // which is opencode, where big-pickle is free. If a slot ever picked up the
        // default instead, the free tier would end and nothing would say so.
        let (ch, cm, _) = reg
            .resolve_profile_route_full("", "@cheap", &[])
            .expect("@cheap resolves");
        assert_eq!(
            ch, "opencode",
            "a slot profile must keep its own harness, not the default — big-pickle is free \
             on opencode only, so this is the free tier"
        );
        assert_eq!(cm, "opencode/big-pickle", "and its own model");

        // (3) The default must be usable, or the guard is decorative. Asserting the
        // constant is Open-tier is what stops a future edit naming a Closed harness
        // and silently reintroducing a foreign-model substitution.
        assert!(
            crate::harness_registry::tier_of(DEFAULT_HARNESS)
                .is_some_and(crate::harness_registry::HarnessTier::may_be_asked_for_a_model),
            "the default harness must be registered AND Open tier, or it may never be \
             handed a model"
        );
    }

    /// bd-47fu: the premium tier's `model_fallback` must actually FIRE, and fire on
    /// the SAME harness before any harness swap.
    ///
    /// "It is in the config" is not "it is used". The ladder has two distinct
    /// fallback mechanisms and they are easy to confuse: `model_fallback` is tried on
    /// the current harness, `fallback_harness` swaps the harness. A premium profile
    /// whose primary is caps-blocked must reach `gpt-6-luna` on the SAME harness —
    /// swapping to grok first would be a different, more expensive decision than the
    /// operator asked for.
    #[test]
    fn the_premium_model_fallback_fires_on_the_same_harness_before_any_harness_swap() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::load_routing().expect("routing");
        // Block the premium primary on pi, which is the operator's premium harness.
        if let Some(e) = reg.models.get_mut("zai/glm-5.3") {
            e.caps = vec!["omp".into()];
        }
        let (h, m, step) = reg
            .resolve_profile_route_full("pi", "@premium", &[])
            .expect("premium must route somewhere");
        assert_eq!(
            step, "model_fallback",
            "the tier's own fallback must be reached: {h} {m}"
        );
        assert_eq!(
            h, "pi",
            "NO harness swap — the model fallback is same-harness by design"
        );
        assert_eq!(
            m, "opencode-go/gpt-6-luna",
            "the fallback must be the one the operator added"
        );
        // And the contrast that proves the two mechanisms are distinct: with the
        // fallback ALSO blocked, the ladder must fall through to the harness swap.
        // If it did not, one mechanism would be shadowing the other and this test
        // would still pass.
        if let Some(e) = reg.models.get_mut("opencode-go/gpt-6-luna") {
            e.caps = vec!["some-other-harness".into()];
        }
        let (h2, _m2, step2) = reg
            .resolve_profile_route_full("pi", "@premium", &[])
            .expect("the harness swap must still be reachable");
        assert_eq!(
            step2, "fallback_harness",
            "with the model fallback blocked, the harness swap is the next rung: {step2}"
        );
        assert_eq!(h2, "grok", "premium's fallback_harness is grok");
    }

    /// The rung must never COST the operator a choice by silently preferring a
    /// family the tier never asked for. `@cheap`'s intent is the `opencode`
    /// family, so when an `opencode/`-family model can run on the harness it must
    /// win over the alphabetically-first portable id.
    #[test]
    fn the_dynamic_default_preserves_the_tiers_own_provider_family() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load_routing().expect("routing");
        let (_, model, step) = reg
            .resolve_profile_route_full("fx", "@cheap", &[])
            .expect("route");
        assert_eq!(step, "dynamic_default");
        let wanted = reg
            .slots
            .get("cheap")
            .map(|s| reg.family_of(&s.model))
            .expect("the cheap slot is compiled in");
        // If any registered model of that family is portable, the pick MUST be one
        // of them. Asserted conditionally so the test states the rule rather than
        // the current registry contents.
        let family_available = reg
            .models
            .keys()
            .any(|id| reg.caps_permit(id, "pa") && reg.family_of(id) == wanted);
        if family_available {
            assert_eq!(
                reg.family_of(&model),
                wanted,
                "a portable model of the tier's own family exists, so the pick must be it"
            );
        }
    }

    /// The `caps_permit` helper treats a MISSING registry entry as NOT routable,
    /// while the primary rung's own check stays lenient for ad-hoc operator
    /// strings. Those are different questions and the difference is load-bearing:
    /// the dynamic rung chooses from the registry, so an unknown id must not be a
    /// candidate for it.
    #[test]
    fn an_unregistered_id_is_not_a_dynamic_default_candidate() {
        let _g = with_config_dir();
        let reg = ModelRegistry::load_routing().expect("routing");
        assert!(
            !reg.caps_permit("provider/model-that-was-never-registered", "pa"),
            "an unknown id must not be offered as routable by the default rungs"
        );
    }

    #[test]
    fn full_ladder_caps_block_detours_to_model_fallback_then_slot_harness() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::load_routing().expect("routing");
        // Block the primary: big-pickle may NOT run on opencode anymore.
        if let Some(e) = reg.models.get_mut("opencode/big-pickle") {
            e.caps = vec!["fx".into()];
        }
        // Step 3 first: a caps-compatible model_fallback id wins.
        let (h, _m, route) = reg
            .resolve_profile_route_full(
                "",
                "@cheap",
                &["opencode-go/deepseek-v4.1-flash".to_string()],
            )
            .expect("model_fallback rung");
        assert_eq!(
            route, "model_fallback",
            "ordered list wins before harness swap"
        );
        assert_eq!(h, "opencode");

        // Step 6: empty model_fallback, the primary caps-blocked, and cheap's slot
        // has no fallback_harness. bd-ziy9 CHANGED THIS ASSERTION and the reason
        // matters, so it is recorded rather than quietly rewritten.
        //
        // It used to expect a LOUD FAILURE, on the stated premise that "the registry
        // has no other opencode-capped model nor a fallback". That premise is no
        // longer true: `opencode-go/deepseek-v4.1-flash` is registered, portable, and
        // in the tier's own family, so the dynamic-default rung resolves this cell.
        //
        // What did NOT change is ADR D1's "no silent hop". The hop is REPORTED —
        // the step comes back as `dynamic_default`, so the ledger records which rung
        // fired instead of a reader having to infer a substitution. A named hop and
        // a silent one are different things, and this is the first.
        let (h, m, route) = reg
            .resolve_profile_route_full("", "@cheap", &[])
            .expect("the dynamic default resolves this cell");
        assert_eq!(
            route, "dynamic_default",
            "the default rung must NAME itself, or it is the silent hop ADR D1 forbids"
        );
        assert_eq!(h, "opencode", "the harness the slot declares is kept");
        assert!(
            reg.caps_permit(&m, "opencode"),
            "the substituted model must actually run here, not merely exist: {m}"
        );
        assert_ne!(
            m, "big-pickle",
            "the primary is caps-blocked to omp, so it must not be the pick"
        );

        // The loud-failure path is still REQUIRED, and pinning it needs a registry
        // where the default genuinely cannot help. Without this the dynamic rung
        // would make the failure path unreachable, which is how a real operator
        // error (a misconfigured model nobody is told about) turns silent.
        let mut barren = ModelRegistry::load_routing().expect("routing");
        for e in barren.models.values_mut() {
            // An empty `caps` reads as "portable", so every entry must name an
            // explicit allow-list that excludes opencode.
            e.caps = vec!["some-other-harness".into()];
        }
        let err = barren
            .resolve_profile_route_full("", "@cheap", &[])
            .expect_err("with no routable model anywhere, the ladder must still fail loud");
        assert!(
            err.to_string().contains("no routable model"),
            "names the failure: {err}"
        );

        // The premium slot's HARNESS swap is still reachable, but bd-47fu moved it one
        // rung later: premium now declares `model_fallback = ["opencode-go/gpt-6-luna"]`,
        // and once the SLOT's list is honoured (it used to be read only by `slot show`,
        // never by the ladder) the same-harness model fallback wins before any harness
        // swap. That is the better order — a model hop on the harness you are already
        // on beats moving to a different harness — so the EXPECTATION moved, not the
        // code. Both halves are pinned together in
        // `the_premium_model_fallback_fires_on_the_same_harness_before_any_harness_swap`.
        // Block BOTH models so this rung is actually reached.
        // bd-47fu: the premium slot's compiled-in harness is pi now (it was
        // omp), so caps-PIRED models are still routable and this rung would
        // never fire. Block them onto grok — the rung's own target.
        for id in ["zai/glm-5.3", "opencode-go/gpt-6-luna"] {
            if let Some(e) = reg.models.get_mut(id) {
                e.caps = vec!["grok".into()];
            }
        }
        let (h, m, route) = reg
            .resolve_profile_route_full("", "@premium", &[])
            .expect("fallback_harness rung");
        assert_eq!(route, "fallback_harness");
        assert_eq!(h, "grok", "slot fallback_harness swaps the harness");
        assert_eq!(
            m, "glm-5.3",
            "grok hop is model-opaque: bare id as flag, no registry pick"
        );
    }

    #[test]
    fn full_ladder_registry_fallback_rung_when_slot_has_no_fallback_harness() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::load_routing().expect("routing");
        // cheap slot has no fallback_harness; give big-pickle a registry
        // fallback to a caps-compatible id and block the primary caps.
        if let Some(e) = reg.models.get_mut("opencode/big-pickle") {
            e.caps = vec!["omp".into()];
            e.fallback = Some("opencode-go/deepseek-v4.1-flash".into());
        }
        let (h, m, route) = reg
            .resolve_profile_route_full("", "@cheap", &[])
            .expect("registry_fallback rung");
        assert_eq!(route, "registry_fallback");
        assert_eq!(h, "opencode", "non-grok fallback keeps the harness");
        assert_eq!(m, "opencode-go/deepseek-v4.1-flash");
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

    /// bd-ll3k: `[defaults]` overrides every slot alias while set.
    /// Precedence: explicit harness/model > concrete id > defaults > slot.
    /// Empty defaults = legacy per-slot behavior, byte-identical.
    #[test]
    fn defaults_override_all_slot_aliases() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::load_routing().expect("routing");
        // No defaults: legacy routes hold.
        // No defaults: legacy routes hold. bd-47fu: the premium slot's
        // compiled-in harness is pi, not the dropped omp.
        assert_eq!(
            reg.resolve_profile_route("", "@premium").expect("route"),
            ("pi".to_string(), "zai/glm-5.3".to_string())
        );
        reg.defaults = Some(DefaultsSpec {
            harness: "pi".to_string(),
            model: "opencode-go/muse-spark-1.3-contributor".to_string(),
            fallback: None,
        });
        // Every alias collapses onto the default pair.
        for alias in ["@premium", "@standard", "@cheap"] {
            assert_eq!(
                reg.resolve_profile_route("", alias).expect("route"),
                (
                    "pi".to_string(),
                    "opencode-go/muse-spark-1.3-contributor".to_string()
                ),
                "alias {alias} follows defaults"
            );
        }
        // Explicit profile harness wins; concrete ids never detour.
        assert_eq!(
            reg.resolve_profile_route("opencode", "@premium")
                .expect("route")
                .0,
            "opencode",
            "explicit harness survives defaults"
        );
        assert_eq!(
            reg.resolve_profile_route("pi", "zai/glm-5.3")
                .expect("route"),
            ("pi".to_string(), "zai/glm-5.3".to_string()),
            "concrete ids bypass defaults"
        );
        // Role-flag surface follows too (slow/plan burn premium today).
        assert_eq!(
            reg.effective_slot_model("premium").as_deref(),
            Some("opencode-go/muse-spark-1.3-contributor")
        );
        assert_eq!(
            reg.effective_slot_model("cheap").as_deref(),
            Some("opencode-go/muse-spark-1.3-contributor")
        );
    }

    /// bd-ll3k: user `[defaults]` merges over compiled-ins via load_routing
    /// and round-trips through save_to.
    #[test]
    fn load_routing_merges_user_defaults() {
        let _g = with_config_dir();
        let dir = std::env::temp_dir().join(format!("fw-defaults-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("models.toml");
        std::fs::write(
            &path,
            "[defaults]\nharness = \"pi\"\nmodel = \"opencode-go/muse-spark-1.3-contributor\"\n",
        )
        .expect("models.toml");
        let reg = ModelRegistry::load_from(Some(&path)).expect("load");
        let d = reg.defaults.as_ref().expect("defaults parsed");
        assert_eq!(d.harness, "pi");
        assert_eq!(d.model, "opencode-go/muse-spark-1.3-contributor");
        let out = dir.join("roundtrip.toml");
        reg.save_to(&out).expect("save");
        let back = ModelRegistry::load_from(Some(&out)).expect("reload");
        assert_eq!(
            back.defaults, reg.defaults,
            "defaults survive the round trip"
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
            reg.implied_slot_for_harness("pi")
                .map(|s| s.harness.as_str()),
            Some("pi")
        );
        // bd-drlm: grok is a CLOSED tier (a frontier CLI with its own model), so
        // the tier gate withholds the slot's MODEL even though the registry still
        // records that grok belongs to premium. Before the gate this resolved to
        // the premium slot, which meant a bare `grok` spawn was handed a model —
        // precisely what "never asked" forbids. `slots.premium.fallback_harness`
        // is a different mechanism and still names grok; assert THAT, so the
        // assertion still says something true about grok instead of going away.
        assert_eq!(
            reg.implied_slot_for_harness("grok"),
            None,
            "a closed harness must not be handed a model, even though it implies premium"
        );
        assert_eq!(
            reg.slots
                .get("premium")
                .and_then(|s| s.fallback_harness.as_deref()),
            Some("grok"),
            "premium's fallback_harness is a separate mechanism and still names grok"
        );
        // bd-47fu: omp WAS the premium harness and is gone; pi carries the
        // premium slot now, which is the zero-config guarantee that a
        // harness-less spawn still resolves a model.
        assert_eq!(
            reg.slots.get("premium").map(|s| s.harness.as_str()),
            Some("pi"),
            "premium's compiled-in harness is pi after the omp drop"
        );
    }

    /// bd-88m4: coupled no-model defaults follow the grok pattern — a
    /// registered `<harness>/...` family id wins (user entries first),
    /// absent entries and non-coupled harnesses stay `None` (current
    /// behavior: implied slot or harness default).
    #[test]
    fn tier_pick_coupled_defaults_follow_grok_pattern() {
        let mut reg = ModelRegistry::default();
        assert_eq!(reg.coupled_default_model("claude"), None);
        assert_eq!(reg.coupled_default_model("codex"), None);
        assert_eq!(
            reg.coupled_default_model("pi"),
            None,
            "open harness: never coupled"
        );
        reg.models.insert("claude/sonnet-x".into(), sample_entry());
        reg.models.insert("codex/gpt-x".into(), sample_entry());
        assert_eq!(
            reg.coupled_default_model("claude").as_deref(),
            Some("claude/sonnet-x")
        );
        assert_eq!(
            reg.coupled_default_model("codex").as_deref(),
            Some("codex/gpt-x")
        );
        // The grok case rides the same helper — no behavior change.
        reg.models.insert("grok/grok-4.5".into(), sample_entry());
        assert_eq!(
            reg.coupled_default_model("grok").as_deref(),
            Some("grok/grok-4.5")
        );
    }

    /// bd-ovew: the routed grok default prefers user entries, then any
    /// compiled grok-family id, else None (caller fails loud — never a
    /// foreign id).
    #[test]
    fn routed_grok_default_prefers_user_entries() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        assert_eq!(
            reg.coupled_default_model("grok"),
            None,
            "empty registry: no default"
        );
        reg.models.insert("aaa/glm-5.2".into(), sample_entry());
        reg.models.insert("zai/glm-5.3".into(), sample_entry());
        assert_eq!(
            reg.coupled_default_model("grok"),
            None,
            "foreign-provider ids never qualify, however many exist"
        );
        reg.models.insert("zzz/grok-4".into(), sample_entry());
        reg.models.insert("grok/grok-4.5".into(), sample_entry());
        assert_eq!(
            reg.coupled_default_model("grok").as_deref(),
            Some("grok/grok-4.5"),
            "no user keys: deterministic BTree order over qualifying ids"
        );
        reg.user_keys.insert("grok/grok-4.5".into());
        assert_eq!(
            reg.coupled_default_model("grok").as_deref(),
            Some("grok/grok-4.5"),
            "user entry wins over alphabetical fallback"
        );
    }

    #[test]
    fn resolve_profile_route_defaults_the_harness_instead_of_rejecting() {
        // bd-47fu INVERTED this test. It asserted the bail — "must pin harness or
        // slot" — which was bd-ttaa's deliberate position that no default harness
        // should be invented. The operator has now chosen pi, so the behaviour is
        // the opposite. The test is inverted rather than deleted because the
        // REASONING is the durable part: a reader who finds this should see that the
        // bail was a recorded decision, later made, not an oversight.
        let reg = ModelRegistry::default();
        let (h, m) = reg
            .resolve_profile_route("", "zai/glm-5.2")
            .expect("a concrete model with no harness now lands on the default");
        assert_eq!(h, DEFAULT_HARNESS, "the operator chose pi");
        assert_eq!(
            m, "zai/glm-5.2",
            "the model is untouched by the default harness"
        );

        // And the guard is not unconditional: with a SLOT alias the slot's own
        // harness still wins, because that is a more specific statement.
        let mut reg = ModelRegistry::default();
        reg.slots.insert(
            SLOT_CHEAP.to_string(),
            SlotSpec {
                harness: "opencode".into(),
                model: "opencode/big-pickle".into(),
                ..SlotSpec::default()
            },
        );
        let (h, _m) = reg
            .resolve_profile_route("", "@cheap")
            .expect("a slot profile resolves");
        assert_ne!(
            h, DEFAULT_HARNESS,
            "a slot must NOT be collapsed onto the default harness — @cheap's opencode is \
             where big-pickle is free"
        );
    }

    /// The registry rung a rate-limited run retries with (bd-42fs, ADR-0024
    /// W1a): its `fallback`, the harness that can run it, and the injection
    /// string for that harness. Shared by the pane hop and the headless rung,
    /// so the grok coupling is pinned here once.
    #[test]
    fn fallback_route_rows() {
        let mut reg = ModelRegistry::default();
        reg.models.insert(
            "a/current".into(),
            ModelEntry {
                fallback: Some("b/next".into()),
                ..sample_entry()
            },
        );
        reg.models.insert(
            "a/grok-hopper".into(),
            ModelEntry {
                fallback: Some("grok/grok-4".into()),
                ..sample_entry()
            },
        );
        reg.models.insert("a/no-rung".into(), sample_entry());
        // the rung targets, so `resolve` has an entry to render
        reg.models.insert("b/next".into(), sample_entry());
        reg.models.insert("grok/grok-4".into(), sample_entry());

        assert_eq!(
            reg.fallback_route("a/current", "claude"),
            Some(FallbackRoute {
                model_id: "b/next".into(),
                harness: "claude".into(),
                model: "b/next".into(),
            }),
            "a non-grok rung keeps the caller's harness"
        );
        assert_eq!(
            reg.fallback_route("a/grok-hopper", "claude"),
            Some(FallbackRoute {
                model_id: "grok/grok-4".into(),
                harness: "grok".into(),
                model: "grok-4".into(),
            }),
            "grok is coupled: the rung forces that harness and the bare id"
        );
        assert_eq!(
            reg.fallback_route("a/no-rung", "claude"),
            None,
            "an entry with no fallback has no rung"
        );
        assert_eq!(
            reg.fallback_route("a/not-registered", "claude"),
            None,
            "an id the registry never heard of has no rung"
        );
    }

    /// The routing when the fallback id comes from a profile's
    /// `model_fallback` list rather than a registry entry: the grok coupling
    /// must not depend on who named the id.
    #[test]
    fn route_for_couples_grok_whoever_named_the_id() {
        let reg = reg_with_keys(&["a/next", "grok/grok-4"]);
        assert_eq!(
            // "pi" not "omp": omp was removed from the registry, and a test that
            // names a harness which no longer exists reads as a live claim.
            reg.route_for("a/next", "pi"),
            FallbackRoute {
                model_id: "a/next".into(),
                harness: "pi".into(),
                model: "a/next".into(),
            },
            "a non-grok id keeps the caller's harness"
        );
        assert_eq!(
            reg.route_for("grok/grok-4", "omp"),
            FallbackRoute {
                model_id: "grok/grok-4".into(),
                harness: "grok".into(),
                model: "grok-4".into(),
            },
            "a grok id flips the harness whoever named it"
        );
    }
}
