//! Mutation witnesses (bd-r7852).
//!
//! A typed crew receipt may name RED mutation witnesses. `crew done` writes
//! them to a small state-dir store keyed by (content key, sha); `ship land`
//! looks them up so a proof receipt that ran no mutations can still land when
//! every changed spec has a witness on the proven sha or an ancestor.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const STORE_FILE: &str = "mutation-witnesses.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    pub spec: String,
    pub sha: String,
    pub verdict: String,
    pub key: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Store {
    entries: BTreeMap<String, Witness>,
}

fn entry_key(key: &str, sha: &str) -> String {
    format!("{key}@{sha}")
}

impl Store {
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(entries) = serde_json::from_str::<BTreeMap<String, Witness>>(&raw) else {
            return Self::default();
        };
        Self { entries }
    }

    pub fn insert(&mut self, witness: Witness) {
        self.entries
            .insert(entry_key(&witness.key, &witness.sha), witness);
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(&self.entries).expect("serialize witnesses"),
        )
    }

    /// All witnesses recorded for one content key, regardless of which sha
    /// proved them. The caller decides ancestry against the landing sha.
    pub fn for_key(&self, key: &str) -> Vec<&Witness> {
        let prefix = format!("{key}@");
        self.entries
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, w)| w)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn witness(spec: &str, sha: &str, key: &str) -> Witness {
        Witness {
            spec: spec.to_string(),
            sha: sha.to_string(),
            verdict: "RED".to_string(),
            key: key.to_string(),
        }
    }

    #[test]
    fn a_witness_survives_a_store_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(STORE_FILE);
        let mut store = Store::load(&path);
        assert!(store.for_key("k1").is_empty(), "missing store reads empty");
        store.insert(witness("s.spec.sh", &"a".repeat(40), "k1"));
        store.save(&path).expect("save");
        let reloaded = Store::load(&path);
        assert_eq!(reloaded.for_key("k1").len(), 1);
        assert_eq!(reloaded.for_key("k1")[0].spec, "s.spec.sh");
        assert!(reloaded.for_key("other").is_empty());
    }

    #[test]
    fn a_second_sha_for_one_key_keeps_both_witnesses() {
        let mut store = Store::default();
        store.insert(witness("s.spec.sh", &"a".repeat(40), "k1"));
        store.insert(witness("s.spec.sh", &"b".repeat(40), "k1"));
        assert_eq!(store.for_key("k1").len(), 2);
    }
}
