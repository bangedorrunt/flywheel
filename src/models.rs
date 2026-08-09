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

/// One registry entry (`[models.ID]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Provider prefix for harnesses that want `provider/model` (opencode,
    /// pi). `omp`/`grok` take the bare id.
    pub provider: String,
    /// Advisory per-mtok cost (run-cost reporting; 0 = unknown).
    #[serde(default)]
    pub cost_in_per_mtok: f64,
    #[serde(default)]
    pub cost_out_per_mtok: f64,
    /// Harness names this model supports. Non-empty = a whitelist: an
    /// unsupported harness resolves to `fallback` (or the id, passthrough).
    #[serde(default)]
    pub caps: Vec<String>,
    /// Alternative id to use when `caps` excludes the harness (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

impl ModelEntry {
    /// The injection string for one harness: `provider/ID` for opencode+pi,
    /// the bare ID for omp+grok (verified against each binary's `--help`,
    /// 2026-08-09). Unknown harness: id passthrough (never block).
    pub fn flag_for(&self, id: &str, harness: &str) -> String {
        match harness {
            "opencode" | "pi" => {
                if self.provider.is_empty() {
                    id.to_string()
                } else {
                    format!("{}/{}", self.provider, id)
                }
            }
            _ => id.to_string(),
        }
    }
}

/// The on-disk shape (`models.toml`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ModelsFile {
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
}

/// The registry: parsed `models.toml` entries keyed by id.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    pub models: BTreeMap<String, ModelEntry>,
}

impl ModelRegistry {
    /// Load from `~/.config/flywheel/models.toml`. Missing file = empty
    /// registry (zero-config; resolution then passes everything through).
    pub fn load() -> Result<Self> {
        let path = crate::config::config_dir().join("models.toml");
        Self::load_from(Some(&path))
    }

    /// Load from an explicit path (`None` = empty registry; test seam).
    pub fn load_from(path: Option<&Path>) -> Result<Self> {
        let mut models = BTreeMap::new();
        if let Some(path) = path
            && path.exists()
        {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let file: ModelsFile =
                toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
            models = file.models;
        }
        Ok(Self { models })
    }

    /// Look up one id.
    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.get(id)
    }

    /// Resolve a model id to the injection string for one harness. Known
    /// registry id → the per-harness flag form (honoring `caps`/`fallback`);
    /// unknown id → passthrough (the caller warns; we never block).
    pub fn resolve(&self, id: &str, harness: &str) -> String {
        match self.models.get(id) {
            Some(entry) => {
                if entry.caps.is_empty() || entry.caps.iter().any(|c| c == harness) {
                    entry.flag_for(id, harness)
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

    /// Save the full file to `~/.config/flywheel/models.toml` (the `add`/
    /// `remove` write path).
    pub fn save(&self) -> Result<()> {
        let path = crate::config::config_dir().join("models.toml");
        self.save_to(&path)
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
            provider: "anthropic".into(),
            cost_in_per_mtok: 3.0,
            cost_out_per_mtok: 15.0,
            caps: vec![],
            fallback: None,
        }
    }

    /// A registry roundtrips through `models.toml` byte-for-byte (the
    /// `add` write must be what `load` reads).
    #[test]
    fn save_then_load_roundtrips() {
        let _g = with_config_dir();
        let path = crate::config::config_dir().join("models.toml");
        let mut reg = ModelRegistry::default();
        reg.models.insert("claude-sonnet-4".into(), sample_entry());
        reg.save().expect("save");
        let got = ModelRegistry::load().expect("load");
        assert_eq!(got.models.get("claude-sonnet-4"), Some(&sample_entry()));
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

    /// Per-harness injection forms: opencode + pi get `provider/id`, omp +
    /// grok get the bare id.
    #[test]
    fn flag_for_injects_per_harness() {
        let e = sample_entry();
        assert_eq!(
            e.flag_for("claude-sonnet-4", "opencode"),
            "anthropic/claude-sonnet-4"
        );
        assert_eq!(
            e.flag_for("claude-sonnet-4", "pi"),
            "anthropic/claude-sonnet-4"
        );
        assert_eq!(e.flag_for("claude-sonnet-4", "omp"), "claude-sonnet-4");
        assert_eq!(e.flag_for("claude-sonnet-4", "grok"), "claude-sonnet-4");
    }

    /// Known registry id resolves to the injected form.
    #[test]
    fn resolve_known_id_injects_harness_form() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        reg.models.insert("claude-sonnet-4".into(), sample_entry());
        assert_eq!(
            reg.resolve("claude-sonnet-4", "opencode"),
            "anthropic/claude-sonnet-4"
        );
        assert_eq!(reg.resolve("claude-sonnet-4", "omp"), "claude-sonnet-4");
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
            "claude-sonnet-4".into(),
            ModelEntry {
                provider: "anthropic".into(),
                cost_in_per_mtok: 3.0,
                cost_out_per_mtok: 15.0,
                caps: vec!["opencode".into()],
                fallback: Some("claude-opus-4".into()),
            },
        );
        reg.models.insert(
            "claude-opus-4".into(),
            ModelEntry {
                provider: "anthropic".into(),
                cost_in_per_mtok: 5.0,
                cost_out_per_mtok: 25.0,
                caps: vec![],
                fallback: None,
            },
        );
        assert_eq!(
            reg.resolve("claude-sonnet-4", "opencode"),
            "anthropic/claude-sonnet-4",
            "harness inside caps uses the id"
        );
        assert_eq!(
            reg.resolve("claude-sonnet-4", "grok"),
            "claude-opus-4",
            "harness outside caps falls back (grok takes the bare id)"
        );
    }

    /// `caps` exclusion without a fallback: bare id passthrough, no panic.
    #[test]
    fn caps_exclude_harness_without_fallback_passes_through() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        reg.models.insert(
            "gpt-5.2".into(),
            ModelEntry {
                provider: "openai".into(),
                cost_in_per_mtok: 2.0,
                cost_out_per_mtok: 10.0,
                caps: vec!["omp".into()],
                fallback: None,
            },
        );
        assert_eq!(reg.resolve("gpt-5.2", "grok"), "gpt-5.2");
    }

    /// `model add` then `model list`-style roundtrip: ids + entries survive.
    #[test]
    fn added_entry_survives_reload() {
        let _g = with_config_dir();
        let mut reg = ModelRegistry::default();
        reg.models.insert("grok-4".into(), sample_entry());
        reg.save().expect("save");
        let reloaded = ModelRegistry::load().expect("load");
        let ids: Vec<&String> = reloaded.models.keys().collect();
        assert_eq!(ids, vec!["grok-4"]);
    }

    /// Cost fields are advisory and default to 0 (no requirement to set).
    #[test]
    fn cost_defaults_to_zero() {
        let _g = with_config_dir();
        let toml = r#"
[models.free-model]
provider = "local"
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
}
