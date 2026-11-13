//! Standing preferences (ADR-0022 D6): ask once, remember. A user override
//! of a Jev or default value persists here so the swarm never asks again.
//! Preferences sit above Jev in the D2 precedence; wiring them into the
//! spawn plan resolver rides a later bead.
//!
//! Pure store (parse/lookup/upsert/forget/list plus a TOML round-trip).
//! The file path and the Chie lesson are edges owned by the CLI.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};

/// A closed field set a preference may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    Harness,
    Model,
    Effort,
    Mode,
    Count,
}

impl Field {
    /// The valid set, named verbatim in parse errors.
    pub const VALID: [&'static str; 5] = ["harness", "model", "effort", "mode", "count"];

    pub fn as_str(self) -> &'static str {
        match self {
            Field::Harness => "harness",
            Field::Model => "model",
            Field::Effort => "effort",
            Field::Mode => "mode",
            Field::Count => "count",
        }
    }

    pub fn parse(s: &str) -> Result<Field> {
        match s {
            "harness" => Ok(Field::Harness),
            "model" => Ok(Field::Model),
            "effort" => Ok(Field::Effort),
            "mode" => Ok(Field::Mode),
            "count" => Ok(Field::Count),
            _ => anyhow::bail!("unknown field {s:?} (valid: {})", Self::VALID.join(" | ")),
        }
    }
}

/// A preference key: `role.field` (`reviewer.harness`, `*.effort`).
/// The `*` role means every role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefKey {
    pub role: String,
    pub field: Field,
}

impl FromStr for PrefKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<PrefKey> {
        let (role, field) = s
            .rsplit_once('.')
            .ok_or_else(|| anyhow::anyhow!("preference key must be ROLE.FIELD (got {s:?})"))?;
        if role.is_empty() {
            anyhow::bail!("preference key needs a non-empty role (got {s:?})");
        }
        Ok(PrefKey {
            role: role.to_string(),
            field: Field::parse(field)?,
        })
    }
}

impl fmt::Display for PrefKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.role, self.field.as_str())
    }
}

/// Preference scope. Role scope is the key itself (an exact role beats
/// `*` in lookup), so `Scope::Role` is not needed: `--project <p>` gives
/// `Project`, no flag gives `Always`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Always,
    Project(String),
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Always => write!(f, "always"),
            Scope::Project(p) => write!(f, "project {p}"),
        }
    }
}

/// One stored preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preference {
    pub key: PrefKey,
    pub value: String,
    pub scope: Scope,
}

/// The overlay: upsert by (scope, role, field), stable sorted list.
#[derive(Debug, Default)]
pub struct PreferenceSet {
    prefs: Vec<Preference>,
}

impl PreferenceSet {
    pub fn new() -> Self {
        Self { prefs: Vec::new() }
    }

    /// Insert or replace the row for (scope, role, field).
    pub fn set(&mut self, key: PrefKey, value: String, scope: Scope) {
        if let Some(p) = self
            .prefs
            .iter_mut()
            .find(|p| p.key == key && p.scope == scope)
        {
            p.value = value;
        } else {
            self.prefs.push(Preference { key, value, scope });
        }
    }

    /// Remove exactly the given scope; true when a row went away.
    /// `project` maps to scopes the same way as the CLI flag: `Some(p)`
    /// is `Project(p)`, `None` is `Always`.
    pub fn forget(&mut self, key: &PrefKey, project: Option<&str>) -> bool {
        let scope = match project {
            Some(p) => Scope::Project(p.to_string()),
            None => Scope::Always,
        };
        let before = self.prefs.len();
        self.prefs.retain(|p| !(p.key == *key && p.scope == scope));
        self.prefs.len() != before
    }

    /// A project-scoped value beats an always value, and an exact role
    /// beats `*`. No project means the always rows only.
    pub fn lookup(&self, role: &str, field: Field, project: Option<&str>) -> Option<&str> {
        let mut scopes = Vec::new();
        if let Some(p) = project {
            scopes.push(Scope::Project(p.to_string()));
        }
        scopes.push(Scope::Always);
        for scope in &scopes {
            for candidate in [role, "*"] {
                if let Some(p) = self
                    .prefs
                    .iter()
                    .find(|p| p.scope == *scope && p.key.role == candidate && p.key.field == field)
                {
                    return Some(p.value.as_str());
                }
            }
        }
        None
    }

    /// All rows in a stable order: always before project, then by
    /// project, role, and field.
    pub fn list(&self) -> Vec<Preference> {
        let mut out = self.prefs.clone();
        fn rank(scope: &Scope) -> (u8, &str) {
            match scope {
                Scope::Always => (0, ""),
                Scope::Project(p) => (1, p.as_str()),
            }
        }
        out.sort_by(|a, b| {
            rank(&a.scope)
                .cmp(&rank(&b.scope))
                .then(a.key.role.cmp(&b.key.role))
                .then(a.key.field.cmp(&b.key.field))
        });
        out
    }

    /// Load the store; a missing file reads as empty. Unknown keys and
    /// non-string values are loud parse errors, never silent drops.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Self::parse_toml(&raw).with_context(|| format!("parse {}", path.display()))
    }

    /// Save the store, creating parent dirs. Stable order, so a
    /// save/load round-trip is byte-stable.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut root = toml::map::Map::new();
        for p in self.list() {
            let insert = |table: &mut toml::map::Map<String, toml::Value>| {
                table
                    .entry(p.key.role.clone())
                    .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                    .as_table_mut()
                    .expect("role slot is a table")
                    .insert(
                        p.key.field.as_str().to_string(),
                        toml::Value::String(p.value.clone()),
                    );
            };
            match &p.scope {
                Scope::Always => {
                    let scope = root
                        .entry("always".to_string())
                        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
                    insert(scope.as_table_mut().expect("always slot is a table"));
                }
                Scope::Project(project) => {
                    let scope = root
                        .entry("project".to_string())
                        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
                    let named = scope
                        .as_table_mut()
                        .expect("project slot is a table")
                        .entry(project.clone())
                        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
                    insert(named.as_table_mut().expect("project name slot is a table"));
                }
            }
        }
        let text = toml::to_string(&toml::Value::Table(root))?;
        std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    fn parse_toml(raw: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(raw)?;
        let root = value
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("preferences must be a TOML table"))?;
        let mut set = Self::new();
        for (scope_name, scope_value) in root {
            let scope_table = scope_value.as_table().ok_or_else(|| {
                anyhow::anyhow!("preferences scope {scope_name:?} must be a table")
            })?;
            match scope_name.as_str() {
                "always" => {
                    for (role, fields) in scope_table {
                        set.insert_row(Scope::Always, role, fields)?;
                    }
                }
                "project" => {
                    for (project, roles) in scope_table {
                        let roles = roles.as_table().ok_or_else(|| {
                            anyhow::anyhow!("preferences project {project:?} must be a table")
                        })?;
                        for (role, fields) in roles {
                            set.insert_row(Scope::Project(project.clone()), role, fields)?;
                        }
                    }
                }
                _ => anyhow::bail!(
                    "unknown preferences scope {scope_name:?} (valid: always | project)"
                ),
            }
        }
        Ok(set)
    }

    fn insert_row(&mut self, scope: Scope, role: &str, fields: &toml::Value) -> Result<()> {
        if role.is_empty() {
            anyhow::bail!("preferences role must be non-empty");
        }
        let fields = fields.as_table().ok_or_else(|| {
            anyhow::anyhow!("preferences role {role:?} must be a table of fields")
        })?;
        for (field, value) in fields {
            let field = Field::parse(field)?;
            let value = value.as_str().ok_or_else(|| {
                anyhow::anyhow!("preference {role}.{} must be a string", field.as_str())
            })?;
            self.set(
                PrefKey {
                    role: role.to_string(),
                    field,
                },
                value.to_string(),
                scope.clone(),
            );
        }
        Ok(())
    }
}

/// The store path through the same config-home resolution the other
/// flywheel config files use: `<config home>/flywheel/preferences.toml`.
pub fn preferences_path() -> PathBuf {
    crate::config::config_dir().join("preferences.toml")
}

#[cfg(test)]
mod preferences_tests {
    use super::*;

    fn key(s: &str) -> PrefKey {
        s.parse().expect("test key parses")
    }

    /// Valid keys parse; unknown fields fail naming the valid set.
    #[test]
    fn preferences_parse_valid_and_invalid_keys() {
        let k: PrefKey = "reviewer.harness".parse().expect("parses");
        assert_eq!(k.role, "reviewer");
        assert_eq!(k.field, Field::Harness);
        let star: PrefKey = "*.effort".parse().expect("wildcard parses");
        assert_eq!(star.role, "*");
        let err = "reviewer.bogus"
            .parse::<PrefKey>()
            .expect_err("unknown field fails");
        for valid in Field::VALID {
            assert!(
                err.to_string().contains(valid),
                "names the valid set: {err}"
            );
        }
        "nodot".parse::<PrefKey>().expect_err("missing field fails");
        ".harness".parse::<PrefKey>().expect_err("empty role fails");
        "reviewer."
            .parse::<PrefKey>()
            .expect_err("empty field fails");
    }

    /// A project-scoped value beats an always value.
    #[test]
    fn preferences_project_beats_always() {
        let mut set = PreferenceSet::new();
        set.set(key("coder.model"), "default-model".into(), Scope::Always);
        set.set(
            key("coder.model"),
            "project-model".into(),
            Scope::Project("flywheel".into()),
        );
        assert_eq!(
            set.lookup("coder", Field::Model, Some("flywheel")),
            Some("project-model")
        );
        assert_eq!(
            set.lookup("coder", Field::Model, Some("other")),
            Some("default-model")
        );
        assert_eq!(
            set.lookup("coder", Field::Model, None),
            Some("default-model")
        );
    }

    /// An exact role beats `*`, in both scopes.
    #[test]
    fn preferences_exact_role_beats_wildcard() {
        let mut set = PreferenceSet::new();
        set.set(key("*.effort"), "low".into(), Scope::Always);
        set.set(key("coder.effort"), "high".into(), Scope::Always);
        assert_eq!(set.lookup("coder", Field::Effort, None), Some("high"));
        assert_eq!(set.lookup("reviewer", Field::Effort, None), Some("low"));
        set.set(key("*.mode"), "remote".into(), Scope::Project("p".into()));
        set.set(
            key("coder.mode"),
            "local".into(),
            Scope::Project("p".into()),
        );
        assert_eq!(set.lookup("coder", Field::Mode, Some("p")), Some("local"));
        assert_eq!(
            set.lookup("reviewer", Field::Mode, Some("p")),
            Some("remote")
        );
    }

    /// Forget removes only the given scope.
    #[test]
    fn preferences_forget_removes_only_the_given_scope() {
        let mut set = PreferenceSet::new();
        set.set(key("coder.model"), "always-model".into(), Scope::Always);
        set.set(
            key("coder.model"),
            "project-model".into(),
            Scope::Project("flywheel".into()),
        );
        assert!(set.forget(&key("coder.model"), None));
        assert_eq!(
            set.lookup("coder", Field::Model, Some("flywheel")),
            Some("project-model"),
            "project row survives"
        );
        assert_eq!(set.lookup("coder", Field::Model, None), None);
        assert!(
            !set.forget(&key("coder.model"), None),
            "second forget reports false"
        );
        assert!(set.forget(&key("coder.model"), Some("flywheel")));
        assert_eq!(set.lookup("coder", Field::Model, Some("flywheel")), None);
    }

    /// Save then load preserves every row; list order is stable.
    #[test]
    fn preferences_toml_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "fw-prefer-rt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let path = dir.join("preferences.toml");
        let mut set = PreferenceSet::new();
        set.set(key("*.effort"), "low".into(), Scope::Always);
        set.set(key("reviewer.harness"), "grok".into(), Scope::Always);
        set.set(
            key("coder.model"),
            "zai/glm-5.3".into(),
            Scope::Project("flywheel".into()),
        );
        set.save(&path).expect("saves");
        let back = PreferenceSet::load(&path).expect("loads");
        assert_eq!(back.list(), set.list());
        assert_eq!(back.list(), back.list(), "list order is stable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unknown fields and non-string values are loud, never silent drops.
    #[test]
    fn preferences_unknown_keys_are_loud() {
        let dir = std::env::temp_dir().join(format!(
            "fw-prefer-loud-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("preferences.toml");
        std::fs::write(&path, "[always.reviewer]\ntelepathy = \"yes\"\n").expect("writes");
        let err = PreferenceSet::load(&path).expect_err("unknown field fails");
        assert!(
            format!("{err:#}").contains("harness"),
            "names the valid set: {err:#}"
        );
        std::fs::write(&path, "[mystery.reviewer]\nharness = \"x\"\n").expect("writes");
        PreferenceSet::load(&path).expect_err("unknown scope fails");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
