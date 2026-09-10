// governed-by: ADR-0012
//! File store — byte-identical layout to the live dispatcher so the Phase 3
//! migration copies state instead of converting it. Rotation (D4.3) and
//! torn-tail guards (D4.4) land in bd-asr2.4; until then append/read match
//! today's behavior exactly, including the 5 MiB / 5k-line head-trim.

use std::path::PathBuf;

pub struct FileStore {
    state_dir: PathBuf,
}

impl FileStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    pub fn journal_file(&self, workspace: &str) -> PathBuf {
        self.state_dir
            .join("journals")
            .join(format!("doorbell-journal-{workspace}.jsonl"))
    }

    pub fn audit_file(&self, workspace: &str) -> PathBuf {
        self.state_dir
            .join("doorbell")
            .join(format!("{workspace}.audit.jsonl"))
    }

    pub fn wake_file(&self, workspace: &str) -> PathBuf {
        self.state_dir
            .join(format!("doorbell-wake-{workspace}.json"))
    }

    pub fn unanswered_file(&self, workspace: &str) -> PathBuf {
        self.state_dir
            .join("doorbell")
            .join(format!("{workspace}.unanswered"))
    }

    /// Append one journal row with the additive `v:1` envelope (D4.1).
    /// Line-flushed; call [`FileStore::sync_journal`] before Prompt delivery
    /// for the decided-before-delivery write-ahead invariant (D4.2).
    pub fn append_journal(
        &self,
        workspace: &str,
        msg: &serde_json::Value,
        actions: &serde_json::Value,
        executed: serde_json::Value,
        pins: &[String],
    ) -> std::io::Result<()> {
        let path = self.journal_file(workspace);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entry = serde_json::json!({
            "v": crate::JOURNAL_V,
            "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "workspace": workspace,
            "msg": msg,
            "actions": actions,
            "executed": executed,
            "pins": pins,
        });
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{entry}")?;
        f.flush()?;
        // Same 5 MiB / 5k line cap as today (rotation replaces this in .4).
        if f.metadata()
            .map(|m| m.len() > 5 * 1024 * 1024)
            .unwrap_or(false)
        {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                let lines: Vec<&str> = raw.lines().collect();
                if lines.len() > 5000 {
                    let keep = lines[lines.len() - 5000..].join("\n") + "\n";
                    let _ = std::fs::write(&path, keep);
                }
            }
        }
        Ok(())
    }

    /// fsync the active journal (D4.2: before Prompt-intent delivery).
    pub fn sync_journal(&self, workspace: &str) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open(self.journal_file(workspace))?;
        f.sync_data()
    }

    pub fn append_audit(&self, workspace: &str, entry: serde_json::Value) {
        let path = self.audit_file(workspace);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{entry}");
        }
    }

    /// Missing or corrupt file → DEFAULT (never a crash): at-least-once
    /// redelivery with the 24h lookback floor lives in the caller.
    pub fn load_wake(&self, workspace: &str) -> doorbell_core::WakeState {
        std::fs::read_to_string(self.wake_file(workspace))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save_wake(&self, workspace: &str, st: &doorbell_core::WakeState) -> anyhow::Result<()> {
        let json = serde_json::to_string(st)?;
        let path = self.wake_file(workspace);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load_unanswered(&self, workspace: &str) -> Vec<String> {
        std::fs::read_to_string(self.unanswered_file(workspace))
            .map(|raw| {
                raw.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn save_unanswered(&self, workspace: &str, pins: &[String]) {
        let path = self.unanswered_file(workspace);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, pins.join("\n") + "\n");
    }

    /// Envelope version; missing `v` reads as 1, unknown `v` must refuse
    /// loudly at the fold (D4.1). TODO(bd-asr2.4): torn-tail stop-and-loud.
    pub fn entry_version(entry: &serde_json::Value) -> u64 {
        entry.get("v").and_then(|v| v.as_u64()).unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip_and_missing_v_defaults() {
        let dir = std::env::temp_dir().join(format!("dbrt-{}", std::process::id()));
        let st = FileStore::new(&dir);
        st.append_journal(
            "w",
            &serde_json::json!({"rt_kind": "Sweep"}),
            &serde_json::json!([]),
            serde_json::json!({"stage": "sweep"}),
            &[],
        )
        .unwrap();
        let raw = std::fs::read_to_string(st.journal_file("w")).unwrap();
        let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(FileStore::entry_version(&v), 1);
        assert_eq!(FileStore::entry_version(&serde_json::json!({})), 1);
        assert!(st.load_wake("missing").since_by_pin.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
