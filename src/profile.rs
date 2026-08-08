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
//! governed-by: ADR-0005 (Spec 1, Spec 1b), ADR-0003 (G14)

// TODO(bd-p2-cli-wn8): drop this allow once `flywheel profile {list,show,...}`
// consumes the catalog (the module is test-exercised until then).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
    /// Which backend trait impl to spawn on. Default `herdr` (ADR-0005
    /// Default Configuration). Resolved by the backend factory at spawn time.
    #[serde(default)]
    pub backend: String,
    /// Optional default workflow name for profiles that own one (e.g.
    /// housekeeper -> weekly-upgrade).
    #[serde(default)]
    pub default_workflow: String,
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

        if let Some(path) = path {
            if path.exists() {
                let raw = std::fs::read_to_string(path)
                    .with_context(|| format!("read {}", path.display()))?;
                let toml: ProfilesToml = toml::from_str(&raw)
                    .with_context(|| format!("parse {}", path.display()))?;
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
                                role: pick(user.role, &base.role),
                                behavior: pick(user.behavior, &base.behavior),
                                task_hint: pick(user.task_hint, &base.task_hint),
                                backend: pick(user.backend, &base.backend),
                                default_workflow: pick(user.default_workflow, &base.default_workflow),
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

    /// Look up a harness spawn recipe.
    pub fn harness(&self, name: &str) -> Option<&HarnessConfig> {
        self.harnesses.get(name)
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
        let spec = SpawnSpec {
            harness: profile.harness.clone(),
            model: profile.model.clone(),
            backend: self.backend(name).to_string(),
        };
        match project_scope {
            Some(slug) => {
                let npub = project_resolver(slug, name)
                    .ok_or_else(|| anyhow::anyhow!("profile '@{name}' not registered in project '{slug}'"))?;
                Ok(ResolvedProfile {
                    npub: npub.clone(),
                    spec,
                    signing: SigningContext::ProjectAgent {
                        npub: npub.clone(),
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

/// Compiled-in default profiles. The 6 canonical Spec 1b roles are the
/// zero-config baseline; the `behavior` preambles are filled verbatim from
/// the ADR by bd-p2-builtins-3zl. This seam keeps the catalog loadable and
/// testable independently of that bead.
pub fn compiled_in_defaults() -> BTreeMap<String, Profile> {
    use std::collections::BTreeMap;
    let mut m = BTreeMap::new();
    for (name, profile) in [
        (
            "oracle",
            Profile {
                harness: "pi".into(),
                model: "claude-opus-4".into(),
                role: "oracle".into(),
                task_hint: "the answer no one else considered".into(),
                ..Profile::default()
            },
        ),
        (
            "planner",
            Profile {
                harness: "pi".into(),
                model: "claude-sonnet-4".into(),
                role: "planner".into(),
                task_hint: "bite-sized, verifiable, blast-radius-aware plans".into(),
                ..Profile::default()
            },
        ),
        (
            "reviewer",
            Profile {
                harness: "opencode".into(),
                model: "claude-opus-4".into(),
                role: "reviewer".into(),
                task_hint: "code-grounded adversarial review with fix proposals".into(),
                ..Profile::default()
            },
        ),
        (
            "coder",
            Profile {
                harness: "pi".into(),
                model: "claude-sonnet-4".into(),
                role: "coder".into(),
                task_hint: "TDD, no scripts, functional core, verify before claim".into(),
                ..Profile::default()
            },
        ),
        (
            "scout",
            Profile {
                harness: "pi".into(),
                model: "claude-haiku".into(),
                role: "scout".into(),
                task_hint: "fast recon, compressed handoff, verified-vs-assumed".into(),
                ..Profile::default()
            },
        ),
        (
            "housekeeper",
            Profile {
                harness: "pi".into(),
                model: "claude-sonnet-4".into(),
                role: "housekeeper".into(),
                task_hint: "idempotent, health-gated, honest reporting".into(),
                default_workflow: "weekly-upgrade".into(),
                ..Profile::default()
            },
        ),
    ] {
        m.insert(name.to_string(), profile);
    }
    m
}

/// Compiled-in harness recipes (empty today — the default backends spawn
/// known harnesses directly). Seam for `[harness.<name>]` additions.
pub fn compiled_in_harnesses() -> BTreeMap<String, HarnessConfig> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    // Temp-dir creation without a tempfile dep. Atomic counter keeps parallel
    // tests from colliding on the same path.
    static TMP_LOCK: Mutex<()> = Mutex::new(());
    static TMP_SEQ: AtomicU32 = AtomicU32::new(0);

    fn tmp_profiles_toml(contents: &str) -> std::path::PathBuf {
        let _g = TMP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let seq = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("flywheel-profile-test-{seq}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("profiles.toml");
        fs::write(&path, contents).expect("write temp profiles.toml");
        path
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
        for name in ["oracle", "planner", "reviewer", "coder", "scout", "housekeeper"] {
            assert!(cat.get(name).is_some(), "{name} built-in missing");
        }
        assert_eq!(cat.names().len(), 6, "exactly the 6 canonical built-ins");
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
        assert_eq!(s.harness, "pi", "built-in harness preserved");
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
    fn resolve_project_scope_uses_project_30013_and_project_signing() {
        let cat = catalog();
        let project_resolver = |slug: &str, name: &str| {
            assert_eq!(slug, "demo");
            assert_eq!(name, "housekeeper");
            Some("npub-project-agent".to_string())
        };
        let r = cat
            .resolve("housekeeper", Some("demo"), "npub-operator", &project_resolver)
            .expect("resolve");
        assert_eq!(r.npub, "npub-project-agent");
        assert_eq!(r.spec.harness, "pi");
        assert_eq!(r.spec.model, "claude-sonnet-4");
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

    #[test]
    fn resolve_project_scope_unregistered_name_fails_fast() {
        let cat = catalog();
        let err = cat
            .resolve("coder", Some("demo"), "npub-operator", &no_project)
            .unwrap_err();
        assert!(err.to_string().contains("not registered"), "{err}");
    }
}
