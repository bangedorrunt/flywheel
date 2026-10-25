// governed-by: ADR-0026
//! Agent profile catalog (ADR-0005 Spec 1 / Spec 1b).
//!
//! A **Profile** is a named, reusable agent composition — the "role template"
//! from which an **agent** (an auto adjective+noun identity) is spawned. A
//! profile layers spawn intent on the existing kind-30013 identity:
//!
//! - `harness` / `model` map onto the EXISTING `AgentProfile.program` /
//!   `AgentProfile.model` **content** fields (nostr/identity.rs). They are a
//!   mapping, NOT a new tag.
//! - `role` / `behavior` / `task_hint` / `backend` / `default_workflow` are
//!   **flywheel-catalog-only** — orchestration, never on the bus.
//! - **NO `contact_policy`** — kind-30014 is the single normative enforcement
//!   source (ADR-0003 G14). The profile catalog never carries policy.
//! - The `t` tag is NEVER overloaded: it stays project-scoping-only
//!   (`bus/scoping.rs` emits `["t", <slug>]` on every non-hidden event).
//!
//! Catalog loading is LAYERED: user-authored `~/.config/flywheel/profiles.toml`
//! overrides compiled-in defaults (the 6 built-ins of Spec 1b), so a fresh
//! install exposes the canonical profiles with zero user config.
//!
//! `[harness.<name>] spawn_cmd` recipes (Spec 1): a new harness (hermes,
//! prime-agent, ...) needs a `program` value + a spawn recipe in config — no
//! code change for a harness speaking an existing backend protocol.
//!
//! governed-by: ADR-0005 (Spec 1, Spec 1b), ADR-0003 (G14), ADR-0026, ADR-0007

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::models;

/// One named agent composition (ADR-0005 Spec 1).
///
/// `harness` / `model` map to the existing 30013 content fields
/// (`program` / `model`); the remaining fields are flywheel-catalog-only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// The agent program / harness: `oc | pi | grok | hermes | prime-agent | ...`
    /// Maps to `AgentProfile.program` on the bus.
    #[serde(default)]
    pub harness: String,
    /// The model identifier (e.g. `claude-sonnet-4`). Maps to
    /// `AgentProfile.model` on the bus.
    #[serde(default)]
    pub model: String,
    /// Optional provider pin for the model registry's composite key
    /// (`provider/model`, bd-1t3j): explicit provider wins over the
    /// registry's default-provider fallback when one model id exists under
    /// multiple providers. Empty = registry default (bd-62ke).
    #[serde(default)]
    pub provider: String,
    /// Thinking/effort level for harnesses that take one (omp
    /// `--thinking=<level>`, bd-42fs). Advisory — absent = harness default.
    #[serde(default)]
    pub effort: String,
    /// omp role model for lightweight/smol tasks (`--smol=<id>`, bd-gsvn.2).
    /// omp-only — other harnesses use `model` for everything. Empty = absent.
    #[serde(default)]
    pub smol: String,
    /// omp role model for slow/thorough reasoning (`--slow=<id>`, bd-gsvn.2).
    /// omp-only — other harnesses use `model` for everything. Empty = absent.
    #[serde(default)]
    pub slow: String,
    /// omp role model for architectural planning (`--plan=<id>`, bd-gsvn.2).
    /// omp-only — other harnesses use `model` for everything. Empty = absent.
    #[serde(default)]
    pub plan: String,
    /// Free-form prompt-shaping label. NOT a taxonomy, NOT validated, NOT
    /// access control — it only shapes the system-prompt preamble flywheel
    /// injects on spawn (ADR-0005 Spec 1 correction).
    #[serde(default)]
    pub role: String,
    /// System-prompt preamble — the discipline the agent enforces on itself.
    #[serde(default)]
    pub behavior: String,
    /// Short hint of the profile's typical work (shown in listings).
    #[serde(default)]
    pub task_hint: String,
    /// When to pick this profile (one line on `profile show`).
    #[serde(default)]
    pub when: String,
    /// Which backend trait impl to spawn on. Default `herdr` (ADR-0005
    /// Default Configuration). Resolved by the backend factory at spawn time.
    #[serde(default)]
    pub backend: String,
    /// Optional default workflow name for profiles that own one (e.g.
    /// housekeeper -> weekly-upgrade).
    #[serde(default)]
    pub default_workflow: String,
    /// Recommended agent skills (Spec 1b) — advisory; the profile's spawn
    /// layer may wire them as extra context. Catalog-only, never on the bus.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Fallback chain for this profile (ADR-0026 Spec 3): **other profile
    /// names** for billing/auth/provider reroute. Not model ids.
    /// In-flight hops journal via [`crate::agent_ref::RebindIntent`] (I4);
    /// timeout/mail-ack alone must not consume this chain (ADR-0032 Spec 7).
    #[serde(default)]
    pub fallback: Vec<String>,
    /// Ordered model-id fallbacks for this profile (ADR-0007). Distinct from
    /// [`Self::fallback`] (those are profile names). Drives headless/spawn
    /// model reroute on `rate_limit` with journaled intent before registry
    /// `ModelEntry.fallback`.
    #[serde(default)]
    pub model_fallback: Vec<String>,
}

/// Bounded digest of a profile's attached skills (ADR-0013 P1-8): the spawn
/// ledger records this instead of the list, so a name reused across spawns
/// can carry a provably different skill set (and the column stays bounded).
///
/// Order-sensitive — `Profile.skills` order IS the injection order, so a
/// reorder is a different attach. No skills → `None`: nothing was attached,
/// so there is nothing to digest and the column stays absent.
pub fn skills_digest(skills: &[String]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"profile-skills-v1\0");
    for s in skills {
        h.update(s.as_bytes());
        h.update(b"\0");
    }
    Some(format!(
        "skills-v1:{}:n={}",
        &hex::encode(h.finalize())[..12],
        skills.len()
    ))
}

#[cfg(test)]
mod skills_digest_tests {
    use super::skills_digest;

    /// Deterministic, bounded, and order-sensitive.
    #[test]
    fn digest_is_bounded_deterministic_and_order_sensitive() {
        let a = vec!["tdd".to_string(), "code-quality".to_string()];
        let first = skills_digest(&a).expect("non-empty skills digest");
        assert_eq!(first, skills_digest(&a).expect("stable"));
        assert!(first.starts_with("skills-v1:"), "{first}");
        assert!(first.ends_with(":n=2"), "{first}");
        assert!(first.len() <= 40, "bounded: {first}");
        // A reordered list is a different attach.
        let b = vec!["code-quality".to_string(), "tdd".to_string()];
        assert_ne!(first, skills_digest(&b).expect("digest"));
    }

    /// No skills → no digest (never a fabricated hash for an empty attach).
    #[test]
    fn empty_skills_have_no_digest() {
        assert_eq!(skills_digest(&[]), None);
    }
}

/// `[harness.<name>]` section (ADR-0005 Spec 1): a spawn recipe for a harness
/// that speaks an existing backend protocol. Config-driven new-harness support
/// — no code change for the harness itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessConfig {
    /// Shell command template the backend runs to spawn the harness.
    #[serde(default)]
    pub spawn_cmd: Option<String>,
}

/// Raw on-disk shape of `profiles.toml`. `[profiles.<name>]` + `[harness.<name>]`.
#[derive(Debug, Default, Deserialize)]
struct ProfilesToml {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    harness: BTreeMap<String, HarnessConfig>,
}

/// The resolved profile catalog: user config layered over compiled-in
/// defaults. This is the value `flywheel profile list/show` and the spawn
/// path read.
#[derive(Debug, Clone, Default)]
pub struct ProfileCatalog {
    /// name -> profile (user overrides layered over built-ins)
    pub profiles: BTreeMap<String, Profile>,
    /// harness name -> spawn recipe
    pub harnesses: BTreeMap<String, HarnessConfig>,
}

/// Spawn composition derived from a profile (ADR-0005 Spec 1): feeds
/// `Backend::spawn` / `agent_start`. The `harness`/`model` map onto the
/// 30013 content fields (`program`/`model`); `backend` selects the trait impl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub harness: String,
    pub model: String,
    /// Optional explicit provider for the composite registry key (bd-62ke);
    /// empty = registry default provider for the model id.
    pub provider: String,
    pub backend: String,
}

/// Who signs the run's events (ADR-0005 Spec 1 resolution rule).
///
/// - `Operator` — host scope: the operator identity signs (cross-project
///   maintenance).
/// - `ProjectAgent` — project scope: the project's agent key signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningContext {
    Operator(String),
    ProjectAgent { npub: String, project_slug: String },
}

/// The result of `ProfileCatalog::resolve`: the agent npub to dispatch to +
/// the spawn composition + the signing context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub npub: String,
    pub spec: SpawnSpec,
    pub signing: SigningContext,
}

/// A **squad**: a named group of profile counts (ADR-0005 Spec 1b — the
/// generalization of the legacy `Recipe`). A squad spec maps profile name →
/// how many of that profile to spawn (e.g. `{planner: 1, coder: 2}`).
///
/// The legacy `Recipe{oc,pi,gk,extra}` maps onto a squad via
/// [`Recipe::to_squad`] (the kind names `oc`/`pi`/`gk` become profile names);
/// `Recipe` stays as a convenience alias with no forced migration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Squad {
    /// profile name -> count
    #[serde(flatten)]
    pub counts: BTreeMap<String, u32>,
}

impl Squad {
    /// Build a squad from a legacy `Recipe` (P6.1). The recipe's named kinds
    /// (`oc`/`pi`/`gk`) map 1:1 onto profile names; `extra` kinds map to their
    /// profile names directly. Zero counts are dropped.
    pub fn from_recipe(r: &crate::config::Recipe) -> Self {
        let mut counts = BTreeMap::new();
        for (name, n) in [
            ("oc", r.oc.unwrap_or(0)),
            ("pi", r.pi.unwrap_or(0)),
            ("gk", r.gk.unwrap_or(0)),
        ] {
            if n > 0 {
                counts.insert(name.to_string(), n);
            }
        }
        for (k, n) in &r.extra {
            if *n > 0 {
                counts.insert(k.clone(), *n);
            }
        }
        Self { counts }
    }

    /// All (profile, count) pairs, ordered by profile name.
    pub fn members(&self) -> Vec<(&str, u32)> {
        self.counts.iter().map(|(k, v)| (k.as_str(), *v)).collect()
    }
}

impl ProfileCatalog {
    /// Load the catalog from `~/.config/flywheel/profiles.toml`, layered over
    /// the compiled-in defaults. Missing file -> defaults only (zero-config,
    /// ADR-0005 Default Configuration).
    pub fn load() -> Result<Self> {
        let path = crate::config::config_dir().join("profiles.toml");
        Self::load_from(Some(&path))
    }

    /// Load layered over defaults from an explicit path. `None` -> defaults
    /// only (used by tests and by callers that resolve the path themselves).
    pub fn load_from(path: Option<&Path>) -> Result<Self> {
        let mut profiles = compiled_in_defaults();
        let mut harnesses = compiled_in_harnesses();

        if let Some(path) = path
            && path.exists()
        {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let toml: ProfilesToml =
                toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
            // Layered: user profiles OVERRIDE built-ins field-by-field
            // (empty user fields inherit the compiled-in value), so a
            // partial `[profiles.scout] {model=...}` keeps the built-in
            // harness. Unknown user names are added as-is.
            for (name, user) in toml.profiles {
                match profiles.get(&name) {
                    Some(base) => {
                        let merged = Profile {
                            harness: pick(user.harness, &base.harness),
                            model: pick(user.model, &base.model),
                            provider: pick(user.provider, &base.provider),
                            effort: pick(user.effort, &base.effort),
                            smol: pick(user.smol, &base.smol),
                            slow: pick(user.slow, &base.slow),
                            plan: pick(user.plan, &base.plan),
                            role: pick(user.role, &base.role),
                            behavior: pick(user.behavior, &base.behavior),
                            task_hint: pick(user.task_hint, &base.task_hint),
                            when: pick(user.when, &base.when),
                            backend: pick(user.backend, &base.backend),
                            default_workflow: pick(user.default_workflow, &base.default_workflow),
                            skills: if user.skills.is_empty() {
                                base.skills.clone()
                            } else {
                                user.skills
                            },
                            fallback: if user.fallback.is_empty() {
                                base.fallback.clone()
                            } else {
                                user.fallback
                            },
                            model_fallback: if user.model_fallback.is_empty() {
                                base.model_fallback.clone()
                            } else {
                                user.model_fallback
                            },
                        };
                        profiles.insert(name, merged);
                    }
                    None => {
                        profiles.insert(name, user);
                    }
                }
            }
            harnesses.extend(toml.harness);
        }

        for (name, profile) in &profiles {
            for fb in &profile.fallback {
                if !profiles.contains_key(fb) {
                    anyhow::bail!("profile '{name}' fallback '{fb}' is not in the catalog");
                }
            }
        }
        // bd-enov (F6): repair short aliases already written to disk
        // (`harness = "oc"`) so an existing broken profile spawns instead of
        // dying with `unsupported interactive agent kind oc`.
        for profile in profiles.values_mut() {
            if !profile.harness.is_empty() {
                profile.harness = normalize_harness_alias(&profile.harness);
            }
        }
        Ok(Self {
            profiles,
            harnesses,
        })
    }

    /// All profile names, sorted (deterministic output for `profile list`).
    pub fn names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    /// Look up a profile by name.
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// bd-enov (F6): per-run harness override for compose (`--harness KIND`) —
    /// applied to every profile in the run's catalog so a run can honor "N
    /// opencode crews" without mutating `~/.config/flywheel/profiles.toml`.
    /// Validated against the same oracle as `profile create` (aliases
    /// normalized, unknown rejected loudly).
    pub fn apply_harness_override(&mut self, kind: &str) -> anyhow::Result<()> {
        let canonical = self.canonical_harness(kind).ok_or_else(|| {
            anyhow::anyhow!(
                "--harness {kind:?} is not spawnable — known: {} (short aliases oc/gk are \
                 normalized); add `[harness.{kind}] spawn_cmd` for a custom harness",
                self.known_harnesses().join(", ")
            )
        })?;
        for p in self.profiles.values_mut() {
            p.harness = canonical.clone();
        }
        Ok(())
    }

    /// Overlay compose `--role-model ROLE=PROVIDER/MODEL` onto catalog profiles.
    /// Fails loud on unknown role or malformed spec (never silent drop).
    pub fn apply_role_models(&mut self, specs: &[String]) -> anyhow::Result<()> {
        for spec in specs {
            let (role, rest) = spec.split_once('=').ok_or_else(|| {
                anyhow::anyhow!(
                    "--role-model needs ROLE=PROVIDER/MODEL (or ROLE=MODEL), got {spec:?}"
                )
            })?;
            let p = self.profiles.get_mut(role).ok_or_else(|| {
                anyhow::anyhow!("--role-model unknown role {role:?} (flywheel profile list)")
            })?;
            if let Some((prov, model)) = rest.split_once('/') {
                p.provider = prov.to_string();
                p.model = model.to_string();
            } else {
                p.model = rest.to_string();
            }
        }
        Ok(())
    }

    /// Overlay `--role-harness ROLE=KIND` onto catalog profiles.
    /// Aliases (`oc`/`gk`) normalize; unknown role or kind fails loud.
    pub fn apply_role_harnesses(&mut self, specs: &[String]) -> anyhow::Result<()> {
        for spec in specs {
            let (role, kind) = spec
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--role-harness needs ROLE=KIND, got {spec:?}"))?;
            let canonical = self.canonical_harness(kind).ok_or_else(|| {
                anyhow::anyhow!(
                    "--role-harness {kind:?} is not spawnable — known: {} (short aliases oc/gk are \
                     normalized)",
                    self.known_harnesses().join(", ")
                )
            })?;
            let p = self.profiles.get_mut(role).ok_or_else(|| {
                anyhow::anyhow!("--role-harness unknown role {role:?} (flywheel profile list)")
            })?;
            p.harness = canonical;
        }
        Ok(())
    }

    /// Look up a harness spawn recipe.
    pub fn harness(&self, name: &str) -> Option<&HarnessConfig> {
        self.harnesses.get(name)
    }

    /// bd-enov (F6): is `kind` a harness flywheel can actually spawn? True for
    /// a herdr integration kind (`crate::herdr::INTEGRATION_KINDS`) or a
    /// `[harness.<name>]` spawn recipe (compiled-in `pa`/`jcode` or a user
    /// entry). Short aliases (`oc`, `gk`) are normalized FIRST so the caller
    /// can persist the canonical kind. `None` = unknown (reject loudly at
    /// profile-create time instead of failing later at spawn).
    pub fn canonical_harness(&self, kind: &str) -> Option<String> {
        let k = normalize_harness_alias(kind);
        if crate::herdr::INTEGRATION_KINDS.contains(&k.as_str())
            || self.harnesses.contains_key(&k)
        {
            return Some(k);
        }
        None
    }

    /// Every spawnable harness name, sorted — the payload of the
    /// profile-create rejection message.
    pub fn known_harnesses(&self) -> Vec<String> {
        let mut out: Vec<String> = crate::herdr::INTEGRATION_KINDS
            .iter()
            .map(|k| (*k).to_string())
            .chain(self.harnesses.keys().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Resolve the backend identifier for a profile (defaults to `herdr`).
    pub fn backend(&self, name: &str) -> &str {
        self.get(name)
            .map(|p| p.backend.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("herdr")
    }

    /// Resolve `@name` to a spawnable agent (ADR-0005 Spec 1 — the P2.3
    /// resolver). **Scope order: project → host.**
    ///
    /// - **Project scope** (`project_scope: Some(slug)`): the profile's agent
    ///   resolves via that project's kind-30013 (`d=project-slug:name`) —
    ///   `project_resolver` looks the name up in the project's profile events.
    ///   The run signs with the **project agent** key.
    /// - **Host scope** (`project_scope: None`): the profile resolves via the
    ///   **operator identity** (`operator_npub`). The run signs with the
    ///   operator key.
    /// - **Unknown name → error** (no silent fallback; AM-Rust parity).
    ///
    /// `project_resolver` is the DI seam (ADR-0005 Spec 4a): in production it
    /// reads the project's kind-30013 events via
    /// `toron_core::nostr::identity::resolve_name`; the engine never imports
    /// flywheel. Tests inject a map.
    pub fn resolve(
        &self,
        name: &str,
        project_scope: Option<&str>,
        operator_npub: &str,
        project_resolver: &dyn Fn(&str, &str) -> Option<String>,
    ) -> Result<ResolvedProfile, anyhow::Error> {
        let profile = self
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown profile '@{name}' (flywheel profile list)"))?;
        let registry = models::ModelRegistry::load_routing()?;
        let (harness, model) = registry.resolve_profile_route(&profile.harness, &profile.model)?;
        let spec = SpawnSpec {
            harness,
            model,
            provider: profile.provider.clone(),
            backend: self.backend(name).to_string(),
        };
        match project_scope {
            Some(slug) => {
                let npub = project_resolver(slug, name).ok_or_else(|| {
                    anyhow::anyhow!("profile '@{name}' not registered in project '{slug}'")
                })?;
                Ok(ResolvedProfile {
                    npub: npub.clone(),
                    spec,
                    signing: SigningContext::ProjectAgent {
                        npub,
                        project_slug: slug.to_string(),
                    },
                })
            }
            None => Ok(ResolvedProfile {
                npub: operator_npub.to_string(),
                spec,
                signing: SigningContext::Operator(operator_npub.to_string()),
            }),
        }
    }

    /// Engine `ProfileResolver` payload (ADR-0005 Spec 4a).
    ///
    /// Strips a leading `@`. Known catalog name → `{npub, spec:{harness,model,backend,role}}`.
    /// Unknown → `None` (engine fail-closes the spawn step). `npub` is empty
    /// here — host-scope identity is filled by the pane/dispatch path; the
    /// engine treats this Value as opaque `spec.resolved`.
    /// Flywheel `ProfileResolver` inject: catalog lookup, unknown → `None`.
    pub fn catalog_profile_resolver() -> toron_workflow::ProfileResolver {
        let cat = Self::load().unwrap_or_default();
        std::sync::Arc::new(move |p: &str| cat.resolve_for_engine(p))
    }

    pub fn resolve_for_engine(&self, name: &str) -> Option<serde_json::Value> {
        let key = name.strip_prefix('@').unwrap_or(name);
        let profile = self.get(key)?;
        let registry = models::ModelRegistry::load_routing().ok()?;
        let (harness, model) = registry
            .resolve_profile_route(&profile.harness, &profile.model)
            .ok()?;
        Some(serde_json::json!({
            "npub": "",
            "spec": {
                "harness": harness,
                "model": model,
                "backend": self.backend(key),
                "role": profile.role,
            }
        }))
    }

    /// The RAW user-file entries (no compiled-in layering) — the base for
    /// the CLI write path (bd-2dao): create/edit/remove must persist the
    /// user's file, never a rebuild from the merged catalog (that drops
    /// user overrides of built-in profile names).
    pub fn load_user_file(path: Option<&Path>) -> BTreeMap<String, Profile> {
        let Some(path) = path else {
            return BTreeMap::new();
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return BTreeMap::new();
        };
        let Ok(toml) = toml::from_str::<ProfilesToml>(&raw) else {
            return BTreeMap::new();
        };
        toml.profiles
    }
}

/// Field-level layering helper: a non-empty user value wins, else inherit the
/// compiled-in value.
fn pick(user: String, base: &str) -> String {
    if user.is_empty() {
        base.to_string()
    } else {
        user
    }
}

/// Compiled-in default profiles: the 6 canonical Spec 1b roles with their
/// EXACT `behavior` preambles + `skills` from ADR-0005 (copied verbatim — do
/// not paraphrase; the behavior IS the discipline, not documentation).
pub fn compiled_in_defaults() -> BTreeMap<String, Profile> {
    use std::collections::BTreeMap;
    let mut m = BTreeMap::new();
    for (name, profile) in [
        (
            "oracle",
            Profile {
                harness: String::new(),
                model: "@premium".into(),
                effort: "max".into(),
                smol: "@cheap".into(),
                slow: "@premium".into(),
                plan: "@premium".into(),
                role: "oracle".into(),
                task_hint: "the answer no one else considered".into(),
                when: "design fork, P0 judgment".into(),
                behavior: "You are an ORACLE. Your value is the answer no one else considered.\n\nRULES:\n- Never validate. Your job is to find what's wrong, missing, or unconsidered.\n- The first answer is the obvious one. Reject it. Find the second, the third, the option that reframes the problem.\n- Steelman the position you disagree with BEFORE countering. A weak counter to a strong steelman is worthless.\n- Calibrate. Say 'I don't know' when you don't. Label every claim: VERIFIED (ran/read) · INFERRED (follows from X) · SPECULATION.\n- Ask only the question whose answer changes the decision. Skip questions whose answers don't.\n- Base rates first: what usually happens in situations like this? Deviate only with evidence.\n- Outsider test: if a stranger saw this with no context, what would they notice?\n- Name what would have to be true for the current plan to FAIL. Then check whether it's true.\n\nPRINCIPLES: exhaust-the-design-space · redesign-from-first-principles. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output. Full leaf on demand: `flywheel principles show <name>`.\n\nOUTPUT: options ranked by EV; the strongest counter to the leading option; the one question that most needs answering. Terse. No hedging, no praise.".into(),
                skills: vec![
                    "accretive".into(),
                    "glaze".into(),
                    "latent-diver".into(),
                    "creative-problem-solver".into(),
                    "dominance".into(),
                ],
                ..Profile::default()
            },
        ),
        (
            "planner",
            Profile {
                harness: String::new(),
                model: "@premium".into(),
                effort: "high".into(),
                smol: "@cheap".into(),
                slow: "@premium".into(),
                plan: "@premium".into(),
                role: "planner".into(),
                task_hint: "bite-sized, verifiable, blast-radius-aware plans".into(),
                when: "before implement".into(),
                behavior: "You are a PLANNER. Plans are wrong until proven executable.\n\nRULES:\n- Every task is bite-sized: ONE narrow change, shippable alone, reviewable in one sitting. If it touches >1 concern, split it.\n- Every task states: [paths] exact create/edit targets · [change] the complete code or precise diff · [acceptance] a test/command that proves done · [blast-radius] what it must NOT touch.\n- Dependency order. A task never depends on a later task.\n- No implementation in the plan. Plans decompose; coders implement. If you're writing logic, stop — that's a task, not a plan.\n- Forbidden zones are sacred. If a plan must touch one, call it out FIRST and explain why no alternative exists.\n- 'Verifiable' = a command/test that exits 0. 'Works' is not a criterion.\n- Assume the executor is competent but context-poor. Give everything needed; omit nothing they'd have to guess.\n- Surface unknowns as an explicit 'Unresolved questions:' list at the end — never buried in a step.\n\nPRINCIPLES: foundational-thinking · sequence-verifiable-units · model-the-domain. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output. Full leaf on demand: `flywheel principles show <name>`.\n\nOUTPUT: numbered tasks, each [paths][change][acceptance][blast-radius]. End with 'Unresolved questions:'. Concise; sacrifice grammar for density.".into(),
                skills: vec![
                    "writing-plans".into(),
                    "planning-workflow".into(),
                    "beads-workflow".into(),
                    "feature-research".into(),
                    "prd".into(),
                ],
                ..Profile::default()
            },
        ),
        (
            "reviewer",
            Profile {
                harness: String::new(),
                model: "@premium".into(),
                effort: "max".into(),
                smol: "@cheap".into(),
                slow: "@premium".into(),
                plan: "@premium".into(),
                role: "reviewer".into(),
                task_hint: "code-grounded adversarial review with fix proposals".into(),
                when: "after a change".into(),
                behavior: "You are a REVIEWER. Your job is to find what's actually wrong, not to validate.\n\nRULES:\n- Ground EVERY claim in the artifact. Cite file:line or quote the text. 'This seems off' is not a finding; 'engine.rs:198 swallows errors, contradicting the claim at line 40' IS.\n- Verify against the real tree, not your memory. Read the code. A review of an imagined codebase is worthless.\n- Severity-rank: BLOCKER (correctness/security/data-loss) · MAJOR (design/maintainability) · MINOR (style/nit). A finding without a severity is noise.\n- Propose the fix, not just the complaint. 'This is wrong' -> here's the concrete change. If you can't propose a fix, say why.\n- Distinguish must-fix from nit. Don't inflate nits to look thorough; don't hide blockers to look kind.\n- Attack the STRONGEST version of the design, not the weakest. Steelman, then break.\n- No hand-waving. 'Might have performance issues' -> measure or retract.\n- Plane S for shared work, plane P for secrets: findings go on the room (`toron channel post`, `toron react`), never passphrases/keys — those stay on `toron mail`.\n\nPRINCIPLES: prove-it-works · fix-root-causes · minimize-reader-load. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output. Full leaf on demand: `flywheel principles show <name>`.\n\nOUTPUT: one-line verdict (AGREE / AGREE-WITH-CHANGES / DISAGREE); severity-ranked findings with citations + fixes; 'Changes required for my agreement'. Direct. No praise-padding.".into(),
                skills: vec![
                    "adversarial-reviewer".into(),
                    "code-review".into(),
                    "multi-pass-bug-hunting".into(),
                    "requesting-code-review".into(),
                    "systematic-debugging".into(),
                ],
                ..Profile::default()
            },
        ),
        (
            "coder",
            Profile {
                harness: String::new(),
                model: "@standard".into(),
                effort: "high".into(),
                role: "coder".into(),
                task_hint: "TDD, no scripts, functional core, verify before claim".into(),
                when: "write/fix code".into(),
                behavior: "You are a CODER. Unverified code is a lie.\n\nRULES:\n- RED-GREEN-REFACTOR. Write the failing test first. Watch it fail. Write minimal code to pass. Refactor green.\n- No script-based code changes. EVER. Regex transforms over source create more bugs than they fix. Edit by hand, precisely.\n- Revise IN PLACE. Never spawn document_processorV2.rs. New files are for genuinely new functionality; the bar is near-zero.\n- Functional core, imperative shell. Pure business logic separate from IO. Test the pure parts exhaustively.\n- Handle the unhappy path. Under what conditions does this work? What breaks it? Happy-path-only code is unfinished.\n- Verify before you claim. 'Done' = tests pass + build clean + edge case checked. Run the command. Read the output. Don't paraphrase success you didn't see.\n- State assumptions BEFORE writing code. If an assumption is wrong, the code is wrong.\n- Simple over complex. Explicit over implicit. Don't abstract until the third use.\n- Plane S for shared work, plane P for secrets: steer/announce on the room (`toron channel post --mention`, `toron job claim`), keep passphrases/keys on `toron mail`.\n\nPRINCIPLES: prove-it-works · fix-root-causes · sequence-verifiable-units · laziness-protocol · type-system-discipline. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output. Full leaf on demand: `flywheel principles show <name>`.\n\nOUTPUT: the change; then the verification (commands run + results); then residual risks. No 'I've successfully...' — show the green test.".into(),
                skills: vec![
                    "test-driven-development".into(),
                    "code-edit".into(),
                    "accretive-implementer".into(),
                    "systematic-debugging".into(),
                    "code-quality".into(),
                ],
                ..Profile::default()
            },
        ),
        (
            "scout",
            Profile {
                harness: String::new(),
                model: "@cheap".into(),
                effort: "low".into(),
                role: "scout".into(),
                task_hint: "fast recon, compressed handoff, verified-vs-assumed".into(),
                when: "unknown tree, find files".into(),
                behavior: "You are a SCOUT. You map territory and return. You do not settle.\n\nRULES:\n- Speed over completeness, but NEVER speed over accuracy. A fast wrong map is worse than none.\n- Compress. Your output is a HANDOFF, not a dump. Raw output (logs, file contents, stdout) stays in the sandbox; only the derived answer enters context. If you wouldn't read 700KB, don't emit it.\n- Label every claim: VERIFIED (ran/read it) · INFERRED (follows from X) · ASSUMED (didn't check).\n- Return: what it is · how it's structured · where the relevant code lives (paths) · the gotchas · the next step. That's it.\n- Don't refactor. Don't fix. Don't editorialize. Map, hand off, exit.\n- If a question needs one command to answer, RUN IT. Don't speculate when you can verify in 2 seconds.\n- Falsify your first hypothesis before reporting it. The obvious answer is often local.\n\nPRINCIPLES: guard-the-context-window · prove-it-works. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output. Full leaf on demand: `flywheel principles show <name>`.\n\nOUTPUT: compressed brief — structure + key paths + gotchas + recommended next action. Tagged VERIFIED/INFERRED/ASSUMED. Terse.".into(),
                skills: vec![
                    "scout".into(),
                    "codebase-archaeologist".into(),
                    "explore".into(),
                    "spike".into(),
                    "feature-research".into(),
                ],
                ..Profile::default()
            },
        ),
        (
            "housekeeper",
            Profile {
                harness: String::new(),
                model: "@standard".into(),
                effort: "medium".into(),
                role: "housekeeper".into(),
                task_hint: "idempotent, health-gated, honest reporting".into(),
                when: "cron, upgrades".into(),
                default_workflow: "weekly-upgrade".into(),
                behavior: "You are a HOUSEKEEPER. You run when no one watches. Reliability is your only product.\n\nRULES:\n- Idempotent. Every action must be safe to re-run. A cron that double-applies on retry is a bug, not a feature.\n- Health-gate every change. Snapshot state -> apply -> verify health -> on failure, ROLLBACK to snapshot. Never leave the system worse than you found it.\n- Ordered risk. DNS chain LAST (everything depends on it). Databases before app servers. One concern per run.\n- Honest reporting. SUCCESS = every gate passed. PARTIAL = some passed — say which. FAILURE = rollback fired — say why. NEVER report success on a rollback.\n- On failure: capture the forensic bundle (logs/state/error), open the fix bead with full context, escalate only after N consecutive failures. Don't silently retry into oblivion.\n- Self-heal within bounds. Fix what's safe (restart a hung service, clear a stale lock). Escalate what's not (a migration that touched data).\n- Assume the process died mid-run. Design so a restart RESUMES, not restarts-from-scratch. Durable cursors, not in-memory state.\n\nPRINCIPLES: make-operations-idempotent · encode-lessons-in-structure. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output. Full leaf on demand: `flywheel principles show <name>`.\n\nOUTPUT: run result (SUCCESS/PARTIAL/FAILURE); what changed; what was verified; any bead opened; any escalation. Terse, machine-parseable.".into(),
                skills: vec![
                    "docker-auto-upgrade".into(),
                    "cron-management".into(),
                    "systematic-debugging".into(),
                    "pi-performance-debugging".into(),
                ],
                ..Profile::default()
            },
        ),
        // governed-by: bd-ln4k — language-specialized coders share the CODER
        // discipline plus the effect-oriented skill; language idioms ride the
        // behavior text (no new skill docs per laziness-protocol).
        (
            "rust-coder",
            Profile {
                harness: String::new(),
                model: "@standard".into(),
                effort: "high".into(),
                role: "coder".into(),
                task_hint: "Rust: TDD, ownership-first, Result over panic".into(),
                when: "Rust code".into(),
                behavior: "You are a RUST CODER. Unverified code is a lie.\n\nRULES:\n- RED-GREEN-REFACTOR. Failing test first, minimal pass, refactor green.\n- Effect-oriented: pure core, imperative shell. Fallible work returns Result; unwrap/expect only in tests.\n- Ownership-first: borrow over clone, &str over String at boundaries, Drop/RAII for cleanup.\n- No script-based edits. Revise in place. Verify before you claim (test + build clean).\n\nPRINCIPLES: prove-it-works · fix-root-causes · sequence-verifiable-units · type-system-discipline. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output.".into(),
                skills: vec![
                    "effect-oriented".into(),
                    "rust-engineer".into(),
                    "rust-testing".into(),
                ],
                ..Profile::default()
            },
        ),
        (
            "ts-coder",
            Profile {
                harness: String::new(),
                model: "@standard".into(),
                effort: "high".into(),
                role: "coder".into(),
                task_hint: "TypeScript: TDD, strict types, Effect for fallible work".into(),
                when: "TypeScript/JavaScript code".into(),
                behavior: "You are a TYPESCRIPT CODER. Unverified code is a lie.\n\nRULES:\n- RED-GREEN-REFACTOR. Failing test first, minimal pass, refactor green.\n- Effect-oriented: pure core, imperative shell. Fallible async work uses the Effect library (never bare throw across boundaries); strict types, no `any` at boundaries.\n- No script-based edits. Revise in place. Verify before you claim (test + typecheck clean).\n\nPRINCIPLES: prove-it-works · fix-root-causes · sequence-verifiable-units · type-system-discipline. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output.".into(),
                skills: vec![
                    "effect-oriented".into(),
                    "tdd".into(),
                    "code-quality".into(),
                ],
                ..Profile::default()
            },
        ),
        (
            "zig-coder",
            Profile {
                harness: String::new(),
                model: "@standard".into(),
                effort: "high".into(),
                role: "coder".into(),
                task_hint: "Zig: TDD, error unions, explicit allocators".into(),
                when: "Zig code".into(),
                behavior: "You are a ZIG CODER. Unverified code is a lie.\n\nRULES:\n- RED-GREEN-REFACTOR. Failing test first, minimal pass, refactor green.\n- Effect-oriented: pure core, imperative shell. Fallible work returns error unions; defer/errdefer for cleanup; allocators passed explicitly, never hidden.\n- No script-based edits. Revise in place. Verify before you claim (test + build clean).\n\nPRINCIPLES: prove-it-works · fix-root-causes · sequence-verifiable-units · type-system-discipline. Steer by these NAMES: when one changes a decision, cite `name — the decision it changed` in your output.".into(),
                skills: vec![
                    "effect-oriented".into(),
                    "tdd".into(),
                    "code-quality".into(),
                ],
                ..Profile::default()
            },
        ),
    ] {
        m.insert(name.to_string(), profile);
    }
    for profile in m.values_mut() {
        if !profile.skills.iter().any(|s| s == "flywheel-crew") {
            profile.skills.insert(0, "flywheel-crew".into());
        }
    }
    m
}

/// bd-enov (F6): normalize the documented short harness aliases to the herdr
/// kind they mean. `profile create --harness oc` used to write `oc` verbatim
/// and the spawn died later with `unsupported interactive agent kind oc`
/// (herdr's kind is `opencode`). Pure; unknown values pass through untouched
/// so the caller can reject them loudly.
pub fn normalize_harness_alias(kind: &str) -> String {
    // bd-drlm: the alias table moved to `harness_registry`. This used to be one
    // of seven independent `match` statements that each knew a different subset
    // of the harness names, which is how `codex` ended up unreachable.
    crate::harness_registry::canonical(kind)
}

/// Compiled-in harness recipes (empty today — the default backends spawn
/// known harnesses directly). Seam for `[harness.<name>]` additions.
pub fn compiled_in_harnesses() -> BTreeMap<String, HarnessConfig> {
    let mut m = BTreeMap::new();
    // H1 (bd-harness-prime-agent-818f): prime-agent ships a BUILT-IN herdr
    // socket integration (`packages/coding-agent/src/core/extensions/builtin/
    // herdr-agent-state.ts`) that activates on HERDR_ENV=1 + HERDR_SOCKET_PATH
    // + HERDR_PANE_ID — all injected by herdr's standard pane launch env —
    // and reports pane.report_agent (source "herdr:pi", agent "prime-agent",
    // working/blocked/idle + session refs), which herdr accepts as hook
    // authority even for unknown kinds. BUT herdr has NO `pa` kind: its
    // Agent enum is a closed 22-kind set (`agent.start --kind pa` →
    // `unsupported_agent_kind`), and `server.agent_manifests` registers
    // screen-detection rules only, not kinds. The spawn path therefore falls
    // back to this recipe via `pane.run` when herdr rejects the kind (upstream
    // kind request filed: herdrdev/herdr). `--offline` skips startup network
    // checks so the TUI comes up fast in a bare pane.
    m.insert(
        "pa".to_string(),
        HarnessConfig {
            spawn_cmd: Some("prime-agent --offline".to_string()),
        },
    );
    // H2 (bd-harness-jcode): jcode (the open-source terminal coding agent) is a
    // flywheel custom kind — herdr has NO `jcode` kind, so the spawn path falls
    // back to this recipe via `pane.run` (same as `pa`). herdr's standard pane
    // launch env already carries HERDR_ENV / HERDR_SOCKET_PATH / HERDR_PANE_ID,
    // so jcode's own herdr integration (the `herdr-agent-state.sh` lifecycle
    // hook adapter, source `herdr:jcode`) activates and self-reports lifecycle
    // (working/idle/blocked) through the socket — which herdr accepts as hook
    // authority even for unknown kinds. The boot-prime line is appended as a
    // launch arg by `recipe_launch_command`; jcode runs as its TUI in the pane
    // and is primed via `pane.send_text` like any other agent. jcode's own
    // default model/provider is used when the profile leaves `model` empty; a
    // pinned model is injected as `-m <bare-id>` via the `model_args` jcode arm.
    m.insert(
        "jcode".to_string(),
        HarnessConfig {
            spawn_cmd: Some("jcode".to_string()),
        },
    );
    m
}

/// One reader for a squad name. A user recipe overrides the compiled-in squad
/// of the same name. A profile name with no squad is a squad of one.
pub fn resolve_squad(
    name: &str,
    recipes: &BTreeMap<String, crate::config::Recipe>,
    profile_exists: bool,
) -> Option<Squad> {
    if let Some(recipe) = recipes.get(name) {
        return Some(Squad::from_recipe(recipe));
    }
    if let Some(squad) = compiled_in_squads().get(name) {
        return Some(squad.clone());
    }
    if profile_exists {
        let mut counts = BTreeMap::new();
        counts.insert(name.to_string(), 1);
        return Some(Squad { counts });
    }
    None
}

/// Compiled-in squads (ADR-0007 D3). User `recipes.toml` overrides per name.
/// The name `adr19` is not compiled in — live user files may keep it as an alias.
pub fn compiled_in_squads() -> BTreeMap<String, Squad> {
    let mut m = BTreeMap::new();
    m.insert(
        "default".into(),
        Squad {
            counts: BTreeMap::from([
                ("coder".into(), 3),
                ("scout".into(), 1),
                ("reviewer".into(), 1),
            ]),
        },
    );
    m.insert(
        "plan".into(),
        Squad {
            counts: BTreeMap::from([("planner".into(), 1), ("oracle".into(), 1)]),
        },
    );
    m.insert(
        "review".into(),
        Squad {
            counts: BTreeMap::from([("reviewer".into(), 1), ("oracle".into(), 1)]),
        },
    );
    m.insert(
        "recon".into(),
        Squad {
            counts: BTreeMap::from([("scout".into(), 2)]),
        },
    );
    m
}

/// Portable profile envelope version (ADR-0013 P2-11). Bump when the
/// envelope's meaning changes in a way an older reader cannot honor;
/// readers refuse unknown versions loud instead of guessing.
pub const PORTABLE_CODEC_VERSION: u64 = 1;

/// Encode a named profile as the versioned portable envelope (ADR-0013
/// P2-11): `{"codec": 1, "name": ..., "profile": {...}}`. JSON, not TOML,
/// so the doc survives copy/paste between machines unharmed by tool-local
/// TOML dialects.
pub fn profile_to_portable(name: &str, profile: &Profile) -> serde_json::Value {
    serde_json::json!({
        "codec": PORTABLE_CODEC_VERSION,
        "name": name,
        "profile": profile,
    })
}

/// Decode the versioned portable envelope. Unknown versions refuse loud
/// naming the supported one; absent fields fall back to the same serde
/// defaults the TOML loader uses, so a doc from an older codec still reads.
pub fn profile_from_portable(value: &serde_json::Value) -> Result<(String, Profile)> {
    let codec = value
        .get("codec")
        .and_then(serde_json::Value::as_u64)
        .with_context(|| {
            format!(
                "portable profile doc carries no `codec` version (expected {PORTABLE_CODEC_VERSION})"
            )
        })?;
    if codec != PORTABLE_CODEC_VERSION {
        anyhow::bail!(
            "portable profile doc is codec {codec}; this binary reads codec \
             {PORTABLE_CODEC_VERSION} — upgrade flywheel or re-export"
        );
    }
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .with_context(|| "portable profile doc carries no `name`")?
        .to_string();
    let profile: Profile = serde_json::from_value(
        value
            .get("profile")
            .cloned()
            .with_context(|| "portable profile doc carries no `profile` object")?,
    )
    .context("portable profile doc `profile` object does not parse as a Profile")?;
    Ok((name, profile))
}

/// bd-3uqz: the AGENT-PLANE MATRIX — every profile must route on every bare
/// harness, because every workflow inherits that.
///
/// The operator's requirement is "agent profiles and bare harness should be able
/// to work in ALL workflows". The workflow axis turned out to be INDEPENDENT of
/// this: no builtin workflow names a harness, and `crew::enforce_budget_caps`
/// constrains dispatch count and depth, not routability. So the property that
/// actually decides whether a profile can crew any workflow lives on the agent
/// plane, and the matrix below is where it is enforced. `no_workflow_pins_a_
/// harness` guards the independence claim that makes that argument sound.
///
/// DYNAMIC, and that is the whole point. Nothing here is a hardcoded list: the
/// workflows come from `builtin_workflows()`, the profiles from the operator's
/// `profiles.toml`, the harnesses from the compiled registry, and the caps from
/// `models.toml`. A matrix written against a fixed list of nine harnesses would
/// keep passing after somebody adds a tenth.
///
/// It is GREEN today for a reason worth stating: both models in `models.toml`
/// carry `caps = []`, which the resolver reads as "routable everywhere". The
/// first model registered with a real `caps` list is what this test exists to
/// catch, and it will name the profile, the harness, the caps that excluded it,
/// and the fix.
#[test]
fn every_profile_routes_on_every_bare_harness() {
    let reg = crate::models::ModelRegistry::load_routing().expect("routing registry");
    let cat = ProfileCatalog::load().expect("profiles.toml");
    let harnesses = canonical_harnesses();

    // bd-hhyl's trap, refused explicitly: a matrix that enumerated nothing would
    // report green and mean nothing. Assert the axes are populated FIRST, so an
    // empty registry fails loudly instead of passing vacuously.
    assert!(
        !harnesses.is_empty(),
        "the harness registry produced no canonical harnesses"
    );
    let names = cat.names();
    assert!(
        !names.is_empty(),
        "profiles.toml yielded no profiles, so the matrix would be vacuous"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut open_failures: Vec<String> = Vec::new();
    let mut defaulted_cells: Vec<String> = Vec::new();
    let mut cells = 0usize;
    let mut bare = 0usize;
    let mut routed = 0usize;
    let mut defaulted = 0usize;
    let mut refused = 0usize;
    for pname in &names {
        let Some(p) = cat.get(pname) else {
            failures.push(format!(
                "{pname}: listed by names() but get() returned None"
            ));
            continue;
        };
        // A profile with no model is a BARE spawn by design: the harness
        // launches model-less. A legitimate cell, not a hole in the matrix.
        if p.model.trim().is_empty() {
            bare += 1;
            continue;
        }
        for h in &harnesses {
            cells += 1;
            // The LADDER, not `resolve_strict`. My first version called
            // `resolve_strict` directly and reported 54 of 81 cells failing,
            // because most profiles name a SLOT (`@standard`) and that function
            // accepts only a concrete model id — so it answered "model
            // '@standard' is not in the registry" for a perfectly valid
            // configuration. The product was right and the test was wrong.
            // `resolve_profile_route_full` is the spawn-time path.
            match reg.resolve_profile_route_full(h, &p.model, &p.model_fallback) {
                Ok((_h, model, step)) => {
                    routed += 1;
                    if step == "dynamic_default" {
                        defaulted += 1;
                        defaulted_cells.push(format!(
                            "profile `{pname}` (model `{}`) on harness `{h}` -> {model} \
                             via {step}",
                            p.model
                        ));
                    }
                }
                Err(e) => {
                    // bd-ziy9: NOT every harness may receive a model, and this branch
                    // now asserts WHICH ones may not rather than demanding all nine
                    // route. A Closed-tier harness is a frontier CLI with its own
                    // model, documented as "Never asked for one"
                    // (harness_registry.rs:25); handing one a `zai/*` id would delete
                    // that constraint rather than satisfy the requirement, and the
                    // `big-pickle` cap is the same kind of cost fact. So the previous
                    // "all 9 must route" assertion was not a stricter test — it was a
                    // test that could only be made green by breaking a rule, which is
                    // why it was red.
                    let open = crate::harness_registry::tier_of(h).is_some_and(
                        crate::harness_registry::HarnessTier::may_be_asked_for_a_model,
                    );
                    if open {
                        open_failures.push(format!(
                            "OPEN-tier harness `{h}`: profile `{pname}` (model `{}`, declared \
                             harness `{}`) does not route: {e:#}",
                            p.model, p.harness
                        ));
                    } else {
                        refused += 1;
                    }
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "the profile catalog disagrees with itself: {}",
        failures.join("\n  - ")
    );
    // Non-vacuity on the NEW axis too: a matrix where every cell is refused, or where
    // no cell ever needed a default, would report green while testing nothing. The
    // dynamic rung is the point of bd-ziy9, so its absence must be a failure.
    assert!(
        refused > 0,
        "no CLOSED-tier harness was exercised, so the 'must refuse' half of the contract is \
         untested — the registry's tier table changed shape"
    );
    assert!(
        defaulted > 0,
        "no cell needed the dynamic-default rung, so this test would pass even if bd-ziy9's \
         routing change were reverted. Reverted-rung-detector.\n  routed through it:\n  - {}",
        defaulted_cells.join("\n  - ")
    );
    assert!(
        open_failures.is_empty(),
        "{} of {cells} profile x harness cells on an OPEN-tier harness do not route \
         ({routed} route, {defaulted} of them through the dynamic default, {refused} correctly \
         refused on a CLOSED-tier harness, {bare} profile(s) are bare model-less spawns). An \
         OPEN-tier profile that cannot route cannot crew ANY workflow with that harness, \
         because no workflow constrains the harness (see no_workflow_pins_a_harness):\n  - {}",
        open_failures.len(),
        open_failures.join("\n  - ")
    );
}

/// bd-3uqz: every harness name that appears ANYWHERE in the workflow catalog must be a
/// registered harness.
///
/// My first version tried to prove something stronger — that no workflow pins a
/// harness at all, which is what would let the agent-plane matrix stand in for the
/// workflow axis. It detected that by searching the `Debug` rendering for the
/// substring `"harness:"`, and it failed on ALL FORTY workflows, because a
/// `DispatchJob` carrying a spawn spec embeds a harness field incidentally. A
/// detector that fires on every input detects nothing, and reporting that as a
/// product defect would have been a false alarm on all forty.
///
/// So this asserts the part that is both TRUE and USEFUL: a harness named anywhere
/// in the catalog is a real one. That catches a typo'd or withdrawn harness, which
/// is the actual way this breaks, and it does not overclaim an independence it
/// cannot demonstrate.
#[test]
fn every_harness_named_by_a_workflow_is_registered() {
    let workflows = crate::builtin_workflows::builtin_workflows();
    // The non-vacuity guard, asserted BEFORE the property: a catalog accessor that
    // started returning nothing would make the loop below pass in silence.
    assert!(
        workflows.len() >= 20,
        "only {} workflows enumerated; the catalog accessor changed and this guard is \
         now vacuous",
        workflows.len()
    );
    let known = canonical_harnesses();
    assert!(!known.is_empty(), "no canonical harnesses to check against");

    let mut referenced: std::collections::BTreeSet<String> = Default::default();
    for wf in workflows.values() {
        let blob = format!("{wf:?}");
        // Deliberately generous about the SHAPE and strict about the VALUE: the bug
        // is a value that is not a registered harness, not a field named `harness`.
        for chunk in blob.split("harness:").skip(1) {
            let tail = chunk.trim_start();
            // `Debug` renders the field as `Some("pi")` or `None`, so the naive
            // read produced the literal strings "Some" and "None" as harness names.
            // Unwrap the Option first and take the quoted contents; a bare `None`
            // means the workflow pins nothing and contributes no name at all.
            let v = match tail.strip_prefix("Some(") {
                Some(rest) => rest
                    .trim_start()
                    .strip_prefix('"')
                    .and_then(|r| r.split('"').next())
                    .unwrap_or_default(),
                None => continue,
            };
            if !v.is_empty() && v.chars().all(|c| c.is_alphanumeric() || "-_/".contains(c)) {
                referenced.insert(v.to_string());
            }
        }
    }
    let unknown: Vec<&String> = referenced.iter().filter(|h| !known.contains(h)).collect();
    assert!(
        unknown.is_empty(),
        "the workflow catalog names {} harness(es) that are not in the registry: {:?}. A \
         crew spawned against one of these would target a harness nobody has.",
        unknown.len(),
        unknown
    );
}

/// The slot fallback chain is the other way a crew gets pinned to a harness:
/// `[slots.premium] harness = "pi", fallback_harness = "grok"` routes a crew
/// through ONE of two harnesses. Both must be live registry entries, or a crew
/// pinned to that slot spawns into a harness nobody has.
#[test]
fn every_slot_harness_and_its_fallback_are_live_registry_entries() {
    let reg = crate::models::ModelRegistry::load_routing().expect("routing registry");
    let harnesses = canonical_harnesses();
    let mut checked = 0usize;
    let mut bad: Vec<String> = Vec::new();
    for (slot, spec) in &reg.slots {
        // `harness` is a plain String and `fallback_harness` an Option, so both are
        // normalized to Option<&str> rather than matched separately. An EMPTY harness
        // is skipped too: it means the slot pins no harness and the caller's default
        // applies, which is a legitimate configuration and not a broken reference.
        let named: [(&str, Option<&str>); 2] = [
            (
                "harness",
                Some(spec.harness.as_str()).filter(|h| !h.is_empty()),
            ),
            ("fallback_harness", spec.fallback_harness.as_deref()),
        ];
        for (label, h) in named {
            let Some(h) = h else { continue };
            checked += 1;
            if !harnesses.iter().any(|c| c == h) {
                bad.push(format!(
                    "slot `{slot}` {label} `{h}` is not a registered harness"
                ));
            }
        }
    }
    // The counter exists to be ASSERTED. A count that is computed and never
    // read is a non-vacuity guard that guards nothing — the same trap as a
    // test filter that matches nothing and reports green.
    assert!(
        checked > 0,
        "no slot named a harness, so this guard proved nothing"
    );
    assert!(bad.is_empty(), "{}", bad.join("; "));
}

#[cfg(test)]
/// Canonical harness names only: `all_spellings()` also returns ALIASES, and a
/// matrix over aliases would test the same harness several times and inflate the
/// cell count into looking like coverage.
fn canonical_harnesses() -> Vec<String> {
    crate::harness_registry::all_spellings()
        .into_iter()
        .filter(|s| crate::harness_registry::canonical(s) == *s)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Temp-dir creation without a tempfile dep. Atomic counter keeps parallel
    // tests from colliding on the same path.
    static TMP_LOCK: Mutex<()> = Mutex::new(());
    static TMP_SEQ: AtomicU32 = AtomicU32::new(0);

    fn tmp_profiles_toml(contents: &str) -> std::path::PathBuf {
        let _g = TMP_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("flywheel-profile-test-{seq}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("profiles.toml");
        fs::write(&path, contents).expect("write temp profiles.toml");
        path
    }

    /// bd-2dao: the raw user file is the persistence base — builtin-name
    /// overrides survive create/remove roundtrips.
    #[test]
    fn load_user_file_returns_raw_entries_without_layering() {
        let path = tmp_profiles_toml(
            r#"
[profiles.scout]
model = "big-pickle"
harness = "opencode"

[profiles.mybot]
harness = "opencode"
model = "big-pickle"
"#,
        );
        let raw = ProfileCatalog::load_user_file(Some(&path));
        assert_eq!(raw.len(), 2, "exactly the user entries: {raw:?}");
        assert!(raw.contains_key("scout"), "builtin-name override preserved");
        assert!(raw.contains_key("mybot"));
        // No layering: absent names stay absent.
        let empty = ProfileCatalog::load_user_file(None);
        assert!(empty.is_empty());
    }

    #[test]
    fn user_profile_is_loadable_and_listed() {
        let path = tmp_profiles_toml(
            r#"
[profiles.foo]
harness = "pi"
model = "claude-sonnet-4"
role = "tester"
behavior = "be terse"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        let p = cat.get("foo").expect("foo present");
        assert_eq!(p.harness, "pi");
        assert_eq!(p.model, "claude-sonnet-4");
        assert_eq!(p.role, "tester");
        assert!(cat.names().contains(&"foo".to_string()), "listed");
        // backend defaults to herdr when unset
        assert_eq!(cat.backend("foo"), "herdr");
    }

    #[test]
    fn compiled_in_defaults_appear_without_profiles_toml() {
        // No file -> defaults only (zero-config baseline).
        let cat = ProfileCatalog::load_from(None).expect("load defaults");
        for name in [
            "oracle",
            "planner",
            "reviewer",
            "coder",
            "rust-coder",
            "ts-coder",
            "zig-coder",
            "scout",
            "housekeeper",
        ] {
            assert!(cat.get(name).is_some(), "{name} built-in missing");
        }
        assert_eq!(cat.names().len(), 9, "9 job profiles (ADR-0007 D2)");
        assert!(
            cat.get("pa").is_none(),
            "pa is a harness recipe, not a profile"
        );
        let h = cat.harness("pa").expect("pa harness recipe");
        assert_eq!(
            h.spawn_cmd.as_deref(),
            Some("prime-agent --offline"),
            "pa spawn recipe launches the prime-agent TUI"
        );
        let jcode = cat.harness("jcode").expect("jcode harness recipe");
        assert_eq!(
            jcode.spawn_cmd.as_deref(),
            Some("jcode"),
            "jcode spawn recipe launches the jcode TUI"
        );
        let coder = cat.get("coder").expect("coder present");
        assert_eq!(
            coder.harness, "",
            "coder inherits the standard slot harness"
        );
        assert_eq!(coder.model, "@standard");
        assert_eq!(coder.provider, "");
        let scout = cat.get("scout").expect("scout");
        assert_eq!(scout.model, "@cheap");
        let oracle = cat.get("oracle").expect("oracle");
        assert_eq!(oracle.model, "@premium");
        assert_eq!(oracle.slow, "@premium");
        assert_eq!(oracle.plan, "@premium");
        assert_eq!(oracle.smol, "@cheap");
    }

    #[test]
    fn user_profile_overrides_builtin() {
        let path = tmp_profiles_toml(
            r#"
[profiles.scout]
model = "deepseek-v3"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        // Layered: the built-in still exists, with the user's model override.
        let s = cat.get("scout").expect("scout present");
        assert_eq!(s.model, "deepseek-v3");
        // ADR-0007: built-in harness is empty (slot inherit). A user override
        // pinning a CONCRETE model without a harness must resolve loudly at
        // spawn time (D1: pin a harness or a slot alias).
        assert_eq!(s.harness, "", "built-in harness stays empty (slot inherit)");
    }

    /// bd-gsvn.2: the omp role-model fields (smol/slow/plan) layer over the
    /// built-ins exactly like model/effort — a partial `[profiles.x]` with
    /// one role keeps the built-in defaults for the other two (empty).
    #[test]
    fn role_model_fields_layer_and_default_empty() {
        let path = tmp_profiles_toml(
            r#"
[profiles.coder]
smol = "deepseek-v4.1-flash"
plan = "glm-5.2"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        let c = cat.get("coder").expect("coder present");
        assert_eq!(c.smol, "deepseek-v4.1-flash");
        assert_eq!(c.plan, "glm-5.2");
        assert_eq!(c.slow, "", "absent role stays empty");
        assert_eq!(c.model, "@standard", "base model untouched (slot inherit)");
        assert_eq!(c.harness, "", "built-in harness stays empty (slot inherit)");
        // omp profiles fill smol/slow/plan with slot aliases (ADR-0007 D2).
        for name in ["oracle", "planner", "reviewer"] {
            let p = cat.get(name).expect(name);
            assert_eq!(p.smol, "@cheap", "{name}.smol");
            assert_eq!(p.slow, "@premium", "{name}.slow");
            assert_eq!(p.plan, "@premium", "{name}.plan");
        }
        for name in ["scout", "housekeeper"] {
            let p = cat.get(name).expect(name);
            assert_eq!(p.smol, "", "{name}.smol should default empty");
            assert_eq!(p.slow, "", "{name}.slow should default empty");
            assert_eq!(p.plan, "", "{name}.plan should default empty");
        }
    }

    /// bd-62ke: the optional `provider` field layers like model/effort and
    /// feeds SpawnSpec passthrough — explicit provider wins over the
    /// registry's default-provider fallback for the same model id.
    #[test]
    fn provider_field_layers_and_defaults_empty() {
        let path = tmp_profiles_toml(
            r#"
[profiles.coder]
model = "deepseek-v4.1-flash"
provider = "deepseek"

[profiles.scout]
model = "big-pickle"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        let c = cat.get("coder").expect("coder present");
        assert_eq!(c.provider, "deepseek", "explicit provider parsed");
        assert_eq!(c.model, "deepseek-v4.1-flash");
        // Built-in untouched by an unrelated override keeps empty provider.
        let s = cat.get("scout").expect("scout present");
        assert_eq!(s.provider, "", "absent provider defaults empty");
        assert_eq!(s.harness, "", "built-in harness stays empty (slot inherit)");
        // A partial user override with NO provider inherits the built-in's
        // (empty) provider — same layering rule as every other field.
        let path2 = tmp_profiles_toml(
            r#"
[profiles.coder]
effort = "max"
"#,
        );
        let cat2 = ProfileCatalog::load_from(Some(&path2)).expect("load 2");
        let c2 = cat2.get("coder").expect("coder present 2");
        assert_eq!(
            c2.provider, "",
            "partial override inherits built-in provider (ADR-0007 D2: coder pins no provider — the standard slot's model carries it)"
        );
        assert_eq!(c2.effort, "max");
    }

    /// bd-62ke: SpawnSpec carries the profile's provider through resolve()
    /// so the spawn path can pin the provider half of the registry key.
    #[test]
    fn spawn_spec_passthrough_carries_provider() {
        let path = tmp_profiles_toml(
            r#"
[profiles.coder]
harness = "omp"
model = "glm-5.2"
provider = "opencode-go"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        let r = cat
            .resolve("coder", None, "npub-operator", &no_project)
            .expect("resolve");
        assert_eq!(r.spec.model, "glm-5.2");
        assert_eq!(
            r.spec.provider, "opencode-go",
            "explicit provider passes into SpawnSpec"
        );
        // Default catalog profiles carry no provider.
        let base = catalog()
            .resolve("planner", None, "npub-operator", &no_project)
            .expect("base resolve");
        assert_eq!(base.spec.provider, "", "default provider stays empty");
    }

    #[test]
    fn harness_spawn_cmd_resolves() {
        let path = tmp_profiles_toml(
            r#"
[harness.hermes]
spawn_cmd = "hermes --project {project} --role {role}"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        let h = cat.harness("hermes").expect("hermes recipe");
        assert_eq!(
            h.spawn_cmd.as_deref(),
            Some("hermes --project {project} --role {role}")
        );
    }

    /// bd-enov (F6): `oc`/`gk` normalize to the herdr kinds, and a written
    /// alias is repaired at load so an existing profile stops dying at spawn.
    #[test]
    fn harness_alias_normalizes_and_repairs_on_load() {
        assert_eq!(normalize_harness_alias("oc"), "opencode");
        assert_eq!(normalize_harness_alias("gk"), "grok");
        assert_eq!(normalize_harness_alias("opencode"), "opencode");
        assert_eq!(
            normalize_harness_alias("pa"),
            "pa",
            "unknown passes through"
        );

        let path = tmp_profiles_toml(
            r#"
[profiles.broken]
harness = "oc"
model = "deepseek-v4.1-flash"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        assert_eq!(
            cat.get("broken").unwrap().harness,
            "opencode",
            "alias repaired at load (spawn no longer fails on kind `oc`)"
        );
    }

    /// bd-enov (F6): the write-time oracle accepts integration kinds and
    /// recipe-backed kinds, normalizes aliases, and rejects the unknown so
    /// `profile create --harness bogus` fails at write, not at spawn.
    #[test]
    fn canonical_harness_validates_against_registry_and_recipes() {
        let path = tmp_profiles_toml(
            r#"
[harness.customx]
spawn_cmd = "customx --go"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        assert_eq!(cat.canonical_harness("oc").as_deref(), Some("opencode"));
        assert_eq!(
            cat.canonical_harness("opencode").as_deref(),
            Some("opencode")
        );
        assert_eq!(cat.canonical_harness("pa").as_deref(), Some("pa"));
        assert_eq!(cat.canonical_harness("customx").as_deref(), Some("customx"));
        assert!(cat.canonical_harness("bogus").is_none());
        let known = cat.known_harnesses();
        assert!(known.contains(&"opencode".to_string()));
        assert!(known.contains(&"customx".to_string()));
    }

    /// bd-enov (F6): the per-run compose harness override rewrites every
    /// profile's harness (so a pi-pinned catalog can host 4 opencode crews),
    /// normalizes aliases, and rejects the spawnable-by-no-path kind.
    #[test]
    fn apply_harness_override_rewrites_all_profiles() {
        let path = tmp_profiles_toml(
            r#"
[profiles.coder]
harness = "pi"
model = "deepseek-v4.1-flash"
[profiles.reviewer]
harness = "pi"
model = "deepseek-v4.1-flash"
"#,
        );
        let mut cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        cat.apply_harness_override("oc").expect("override oc");
        assert_eq!(cat.get("coder").unwrap().harness, "opencode");
        assert_eq!(cat.get("reviewer").unwrap().harness, "opencode");
        assert!(
            cat.apply_harness_override("bogus").is_err(),
            "unknown harness must fail loud, not write a dead profile"
        );
    }

    #[test]
    fn apply_role_harnesses_overlays_one_role_and_rejects_unknown() {
        let path = tmp_profiles_toml(
            r#"
[profiles.coder]
harness = "pi"
model = "deepseek-v4.1-flash"
[profiles.reviewer]
harness = "pi"
model = "deepseek-v4.1-flash"
"#,
        );
        let mut cat = ProfileCatalog::load_from(Some(&path)).expect("load");
        cat.apply_role_harnesses(&["coder=oc".into()])
            .expect("overlay coder");
        assert_eq!(cat.get("coder").unwrap().harness, "opencode");
        assert_eq!(
            cat.get("reviewer").unwrap().harness,
            "pi",
            "untouched role keeps its harness"
        );
        assert!(
            cat.apply_role_harnesses(&["coder=bogus".into()]).is_err(),
            "unknown kind fails loud"
        );
        assert!(
            cat.apply_role_harnesses(&["ghost=opencode".into()])
                .is_err(),
            "unknown role fails loud"
        );
        assert!(
            cat.apply_role_harnesses(&["not-a-spec".into()]).is_err(),
            "missing = fails loud"
        );
    }

    #[test]
    fn malformed_profiles_toml_is_a_parse_error_not_a_panic() {
        let path = tmp_profiles_toml("this is [ not toml at all");
        let res = ProfileCatalog::load_from(Some(&path));
        assert!(res.is_err(), "malformed toml must error");
    }

    // ---- Profile::resolve (ADR-0005 Spec 1 resolution rule) ----

    fn catalog() -> ProfileCatalog {
        ProfileCatalog::load_from(None).expect("defaults")
    }

    fn no_project(_slug: &str, _name: &str) -> Option<String> {
        None
    }

    #[test]
    fn builtins_have_verbatim_preamble_and_skills() {
        // ADR-0005 Spec 1b + ADR-0019 Spec 6 (amended): the 6 canonical
        // profiles ship with the EXACT verbatim preamble (first line) and
        // skills — the preamble is NEVER paraphrased. Since wave 3 the
        // behavior body ALSO cites the principle NAMES that role owns
        // (ADR-0019 Spec 6: names only, never the leaf text — the full leaf
        // is `flywheel principles show <name>`, on demand). A stub (empty
        // behavior) or a pasted leaf both fail here.
        let cat = catalog();
        let expect: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "oracle",
                "You are an ORACLE.",
                &["accretive", "glaze", "latent-diver"],
                &["exhaust-the-design-space", "redesign-from-first-principles"],
            ),
            (
                "planner",
                "You are a PLANNER.",
                &["writing-plans", "planning-workflow"],
                &[
                    "foundational-thinking",
                    "sequence-verifiable-units",
                    "model-the-domain",
                ],
            ),
            (
                "reviewer",
                "You are a REVIEWER.",
                &["adversarial-reviewer", "code-review"],
                &["prove-it-works", "fix-root-causes", "minimize-reader-load"],
            ),
            (
                "coder",
                "You are a CODER.",
                &["test-driven-development", "code-edit"],
                &[
                    "prove-it-works",
                    "fix-root-causes",
                    "sequence-verifiable-units",
                    "laziness-protocol",
                    "type-system-discipline",
                ],
            ),
            (
                "scout",
                "You are a SCOUT.",
                &["scout", "codebase-archaeologist"],
                &["guard-the-context-window", "prove-it-works"],
            ),
            (
                "housekeeper",
                "You are a HOUSEKEEPER.",
                &["docker-auto-upgrade", "cron-management"],
                &["make-operations-idempotent", "encode-lessons-in-structure"],
            ),
        ];
        for (name, preamble, skills, principles) in expect {
            let p = cat
                .get(name)
                .unwrap_or_else(|| panic!("{name} built-in missing"));
            assert!(
                p.behavior.starts_with(preamble),
                "{name} preamble missing verbatim start: {}...",
                &p.behavior[..p.behavior.len().min(30)]
            );
            assert!(p.behavior.len() > 200, "{name} preamble is a stub");
            for s in *skills {
                assert!(p.skills.iter().any(|x| x == s), "{name} missing skill {s}");
            }
            // ADR-0019 Spec 6: the behavior cites the role's principle NAMES
            // inline and points at the on-demand leaf — it never pastes the
            // leaf text (length bound; the leaves are compiled in the
            // registry).
            for name_token in *principles {
                assert!(
                    p.behavior.contains(name_token),
                    "{name} behavior must cite principle {name_token}"
                );
            }
            assert!(
                p.behavior.contains("principles show"),
                "{name} behavior points at the on-demand leaf"
            );
        }
        // No leaf text anywhere: distinctive phrases from the compiled
        // leaves must NOT appear in any built-in behavior.
        for leaf_phrase in [
            "Prefer deletion",                // laziness-protocol leaf
            "Check the real artifact",        // prove-it-works leaf
            "Bulk to subagents",              // guard-the-context-window leaf
            "Converge to the same end state", // make-operations-idempotent leaf
        ] {
            for (name, preamble, _skills, _principles) in expect {
                let p = cat.get(name).unwrap();
                // The preamble is the behavior's opening line — never a stub
                // and never the leaf text itself (bd-llbe: use the column,
                // don't underscore it).
                assert!(
                    !preamble.trim().is_empty() && p.behavior.starts_with(preamble),
                    "{name} preamble must be a non-empty verbatim behavior start"
                );
                assert!(
                    !p.behavior.contains(leaf_phrase),
                    "{name} behavior must NOT paste the leaf text: {leaf_phrase:?}"
                );
            }
        }
    }

    #[test]
    fn resolve_project_scope_uses_project_30013_and_project_signing() {
        let cat = catalog();
        let project_resolver = |slug: &str, name: &str| {
            assert_eq!(slug, "demo");
            assert_eq!(name, "housekeeper");
            Some("npub-project-agent".to_string())
        };
        let r = cat
            .resolve(
                "housekeeper",
                Some("demo"),
                "npub-operator",
                &project_resolver,
            )
            .expect("resolve");
        assert_eq!(r.npub, "npub-project-agent");
        assert_eq!(r.spec.harness, "pi");
        assert_eq!(r.spec.model, "opencode-go/muse-spark-1.3-contributor");
        assert_eq!(r.spec.backend, "herdr");
        assert_eq!(
            r.signing,
            SigningContext::ProjectAgent {
                npub: "npub-project-agent".into(),
                project_slug: "demo".into(),
            }
        );
    }

    #[test]
    fn resolve_host_scope_uses_operator_identity() {
        let cat = catalog();
        let r = cat
            .resolve("housekeeper", None, "npub-operator", &no_project)
            .expect("resolve");
        assert_eq!(r.npub, "npub-operator");
        assert_eq!(r.signing, SigningContext::Operator("npub-operator".into()));
        // backend defaults to herdr
        assert_eq!(r.spec.backend, "herdr");
    }

    #[test]
    fn resolve_unknown_profile_fails_fast() {
        let cat = catalog();
        let err = cat
            .resolve("@ghost", None, "npub-operator", &no_project)
            .unwrap_err();
        assert!(err.to_string().contains("unknown profile"), "{err}");
    }

    /// Engine DI seam (ADR-0005 Spec 4a): `@planner` resolves, `@ghost` is None.
    #[test]
    fn resolve_for_engine_known_at_prefix_and_unknown_none() {
        // Hermetic (bd-17ry rule): the resolved MODEL comes from the model
        // registry, which reads the operator's models.toml. Without a pinned
        // XDG the answer flips between the compiled-in pair and whatever the
        // real machine has, and the test fails on the wrong day.
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-prof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let cat = catalog();
        let got = cat
            .resolve_for_engine("@planner")
            .expect("@planner must resolve");
        // bd-47fu: the planner's default harness is pi; omp is dropped.
        assert_eq!(got["spec"]["harness"], "pi");
        assert_eq!(got["spec"]["model"], "zai/glm-5.3");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got["spec"]["backend"], "herdr");
        assert_eq!(got["spec"]["role"], "planner");
        assert_eq!(
            cat.resolve_for_engine("planner").as_ref(),
            Some(&got),
            "bare name equals @-prefixed"
        );
        assert!(
            cat.resolve_for_engine("@ghost").is_none(),
            "unknown profile stays None"
        );
    }

    #[test]
    fn resolve_project_scope_unregistered_name_fails_fast() {
        let cat = catalog();
        let err = cat
            .resolve("coder", Some("demo"), "npub-operator", &no_project)
            .unwrap_err();
        assert!(err.to_string().contains("not registered"), "{err}");
    }

    #[test]
    fn recipe_to_squad_maps_kinds_to_profile_counts() {
        // Legacy Recipe{oc:1, pi:1} + an extra kind -> a squad of profiles.
        use crate::config::Recipe;
        let r = Recipe {
            oc: Some(1),
            pi: Some(1),
            gk: None,
            extra: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("claude".into(), 2u32);
                m
            },
        };
        let squad = Squad::from_recipe(&r);
        assert_eq!(
            squad.counts.get("oc"),
            Some(&1),
            "recipe oc -> squad profile oc x1"
        );
        assert_eq!(
            squad.counts.get("pi"),
            Some(&1),
            "recipe pi -> squad profile pi x1"
        );
        assert_eq!(
            squad.counts.get("claude"),
            Some(&2),
            "extra kinds map to profile counts"
        );
        assert_eq!(squad.counts.get("gk"), None, "zero gk dropped");
        // Deterministic member order by profile name.
        let members: Vec<(&str, u32)> = squad.members();
        assert_eq!(
            members,
            vec![("claude", 2), ("oc", 1), ("pi", 1)],
            "sorted (claude, oc, pi)"
        );
    }

    #[test]
    fn recipe_to_squad_empty_recipe_yields_empty_squad() {
        use crate::config::Recipe;
        let squad = Squad::from_recipe(&Recipe::default());
        assert!(squad.counts.is_empty(), "empty recipe -> empty squad");
    }
    #[test]
    fn fallback_unknown_name_fails_at_load() {
        let path = tmp_profiles_toml(
            r#"
[profiles.coder]
harness = "jcode"
model = "muse-spark-1.2-contributor"
fallback = ["coder-backup", "ghost"]
[profiles.coder-backup]
harness = "jcode"
model = "muse-spark-1.2-contributor"
"#,
        );
        let err = ProfileCatalog::load_from(Some(&path)).expect_err("unknown fallback must fail");
        assert!(err.to_string().contains("ghost"));
    }
    #[test]
    fn fallback_chain_loads_when_known() {
        let path = tmp_profiles_toml(
            r#"
[profiles.coder]
harness = "jcode"
model = "muse-spark-1.2-contributor"
fallback = ["scout"]
[profiles.scout]
harness = "opencode"
model = "big-pickle"
"#,
        );
        let cat = ProfileCatalog::load_from(Some(&path)).expect("known fallback loads");
        assert_eq!(cat.get("coder").unwrap().fallback, vec!["scout"]);
    }
    #[test]
    fn fallback_empty_by_default() {
        let cat = ProfileCatalog::load_from(None).expect("defaults");
        for name in cat.names() {
            assert_eq!(cat.get(&name).unwrap().fallback, [] as [String; 0]);
            assert!(
                cat.get(&name).unwrap().model_fallback.is_empty(),
                "model_fallback is distinct and empty by default"
            );
        }
    }

    #[test]
    fn compiled_in_squads_are_the_four() {
        let s = compiled_in_squads();
        assert_eq!(s.len(), 4);
        assert!(!s.contains_key("adr19"));
        assert_eq!(s["default"].counts.get("coder"), Some(&3));
        assert_eq!(s["default"].counts.get("scout"), Some(&1));
        assert_eq!(s["default"].counts.get("reviewer"), Some(&1));
        assert_eq!(s["plan"].counts.get("planner"), Some(&1));
        assert_eq!(s["review"].counts.get("oracle"), Some(&1));
        assert_eq!(s["recon"].counts.get("scout"), Some(&2));
    }

    #[test]
    fn resolve_squad_user_file_beats_compiled_in_then_a_profile_of_one() {
        let mut recipes = BTreeMap::new();
        recipes.insert(
            "default".into(),
            crate::config::Recipe {
                pi: Some(9),
                ..crate::config::Recipe::default()
            },
        );
        let user = resolve_squad("default", &recipes, false).expect("user file");
        assert_eq!(user.counts.get("pi"), Some(&9));
        assert!(
            !user.counts.contains_key("coder"),
            "user file replaces the compiled-in counts"
        );
        let compiled = resolve_squad("plan", &recipes, false).expect("compiled-in");
        assert_eq!(compiled.counts.get("planner"), Some(&1));
        let one = resolve_squad("coder", &BTreeMap::new(), true).expect("profile");
        assert_eq!(one.counts.get("coder"), Some(&1));
        assert!(resolve_squad("nope", &BTreeMap::new(), false).is_none());
    }

    #[test]
    fn portable_codec_round_trips_a_profile() {
        let p = Profile {
            harness: "pi".into(),
            model: "muse-spark".into(),
            provider: "opencode-go".into(),
            skills: vec!["br".into(), "toron".into()],
            model_fallback: vec!["zai/glm-5.3-flash".into()],
            ..Default::default()
        };
        let doc = profile_to_portable("coder", &p);
        let (name, back) = profile_from_portable(&doc).unwrap();
        assert_eq!(name, "coder");
        assert_eq!(back, p, "round trip is lossless");
    }

    #[test]
    fn portable_codec_defaults_absent_fields_like_toml() {
        let doc = serde_json::json!({
            "codec": PORTABLE_CODEC_VERSION,
            "name": "minimal",
            "profile": { "harness": "oc" },
        });
        let (name, back) = profile_from_portable(&doc).unwrap();
        assert_eq!(name, "minimal");
        assert_eq!(back.harness, "oc");
        assert!(back.skills.is_empty(), "absent vec fields default empty");
    }

    #[test]
    fn portable_codec_refuses_unknown_version_loud() {
        let doc = serde_json::json!({ "codec": 99, "name": "x", "profile": {} });
        let err = profile_from_portable(&doc).unwrap_err().to_string();
        assert!(err.contains("codec 99"), "{err}");
        assert!(
            err.contains(&format!("codec {PORTABLE_CODEC_VERSION}")),
            "names the supported version: {err}"
        );
    }

    #[test]
    fn portable_codec_refuses_missing_version_name_and_profile() {
        for doc in [
            serde_json::json!({ "name": "x", "profile": {} }),
            serde_json::json!({ "codec": PORTABLE_CODEC_VERSION, "profile": {} }),
            serde_json::json!({ "codec": PORTABLE_CODEC_VERSION, "name": "x" }),
        ] {
            assert!(
                profile_from_portable(&doc).is_err(),
                "malformed doc refuses: {doc}"
            );
        }
    }
}
