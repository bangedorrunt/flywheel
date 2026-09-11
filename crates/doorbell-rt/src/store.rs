// governed-by: ADR-0012
//! File store — byte-identical layout to the live dispatcher so the Phase 3
//! migration copies state instead of converting it. Rotation (D4.3, bd-asr2.4):
//! active journal + `.N.jsonl` generations, 5k lines / 5MiB total retention,
//! `journal_rotated` audit rows. Torn-tail stop-and-loud (D4.4) via [`Fold`].

use std::path::PathBuf;

/// Journal/audit/wake files hold agent-mail wake metadata (pins, lines,
/// corr ids) — created owner-only (audit P2: the daemon carefully guards
/// its socket dir 0700 while the state files were umask-dependent).
/// `mode()` applies only at creation; existing files keep their perms.
#[cfg(unix)]
fn private_append(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_append(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Owner-only write (create/truncate).
#[cfg(unix)]
fn private_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

#[cfg(not(unix))]
fn private_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Create `dir` (all the way down) owner-only.
#[cfg(unix)]
fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Total retention across active + generations (D4.3: same bound as the old
/// head-trim, no silent loss within retention).
pub const RETENTION_LINES: usize = 5000;
pub const RETENTION_BYTES: u64 = 5 * 1024 * 1024;
/// Generations kept besides the active file.
pub const RETENTION_GENS: usize = 4;

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

    /// Generation file `.N` (1 = newest retained). Sorts before the active
    /// file byte-wise (`.1.jsonl` < `.jsonl`), so directory order is oldest
    /// first and every reader folds history in order.
    pub fn journal_gen_file(&self, workspace: &str, generation: usize) -> PathBuf {
        self.state_dir
            .join("journals")
            .join(format!("doorbell-journal-{workspace}.{generation}.jsonl"))
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

    /// Append one journal row with the additive `v:1` envelope (D4.1) plus a
    /// per-workspace monotonic `seq` (D4.4 gap detection; `None` for legacy
    /// writers whose rows the fold tolerates without a contiguity check).
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
        self.append_journal_seq(workspace, msg, actions, executed, pins, None, None)
    }

    // 8 args stay positional: 3 call sites, all pass-through journal
    // fields; a params struct adds indirection for zero behavior gain.
    #[allow(clippy::too_many_arguments)]
    pub fn append_journal_seq(
        &self,
        workspace: &str,
        msg: &serde_json::Value,
        actions: &serde_json::Value,
        executed: serde_json::Value,
        pins: &[String],
        seq: Option<u64>,
        subscriber: Option<&str>,
    ) -> std::io::Result<()> {
        let path = self.journal_file(workspace);
        if let Some(parent) = path.parent() {
            private_dir(parent)?;
        }
        let mut entry = serde_json::json!({
            "v": crate::JOURNAL_V,
            "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "workspace": workspace,
            "msg": msg,
            "actions": actions,
            "executed": executed,
            "pins": pins,
        });
        if let (Some(seq), Some(obj)) = (seq, entry.as_object_mut()) {
            obj.insert("seq".into(), seq.into());
        }
        // D5: subscriber tag (additive — v1 readers tolerate unknown fields;
        // full per-subscriber scoping rides journal v2 in bd-asr2.9).
        if let (Some(sub), Some(obj)) = (subscriber, entry.as_object_mut()) {
            obj.insert("subscriber".into(), sub.into());
        }
        use std::io::Write;
        let mut f = private_append(&path)?;
        writeln!(f, "{entry}")?;
        f.flush()?;
        drop(f);
        self.maybe_rotate(workspace);
        Ok(())
    }

    /// Rotation check after every append (D4.3). Cheap gate first: only
    /// journals past 512 KiB pay the full accounting; small journals stay
    /// O(1). Trigger on totals (not active alone) so steady state keeps
    /// generations instead of churning them.
    fn maybe_rotate(&self, workspace: &str) {
        let path = self.journal_file(workspace);
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if bytes <= 512 * 1024 {
            return;
        }
        let (total_lines, total_bytes, _) = self.generation_totals(workspace);
        if total_lines <= RETENTION_LINES && total_bytes <= RETENTION_BYTES {
            return;
        }
        self.rotate(workspace);
    }

    /// Move the active head into generations in chunks (D4.3): shift
    /// existing generations up, spill the oldest `active - KEEP` lines into
    /// a fresh `.1`, keep the newest KEEP lines active. Whole-file moves
    /// would let the totals enforcer delete everything at once — chunks
    /// keep the retained history dense. Every drop is an audit row.
    fn rotate(&self, workspace: &str) {
        const KEEP_LINES: usize = 4000;
        let mut dropped_lines = 0usize;
        let mut dropped_bytes = 0u64;
        let active = self.journal_file(workspace);
        let active_lines = count_lines(&active);
        // Spill the active excess into a fresh .1 (shift first, dropping
        // the oldest past the cap). Whole-file moves would let the totals
        // enforcer delete everything at once — chunks keep history dense.
        if active_lines > KEEP_LINES {
            let Ok(raw) = std::fs::read_to_string(&active) else {
                return;
            };
            let lines: Vec<&str> = raw.lines().collect();
            let (head, tail) = lines.split_at(lines.len() - KEEP_LINES);
            let overflow = self.journal_gen_file(workspace, RETENTION_GENS);
            if overflow.exists() {
                dropped_lines += count_lines(&overflow);
                dropped_bytes += file_bytes(&overflow);
                let _ = std::fs::remove_file(&overflow);
            }
            for n in (1..RETENTION_GENS).rev() {
                let from = self.journal_gen_file(workspace, n);
                if from.exists() {
                    let _ = std::fs::rename(&from, self.journal_gen_file(workspace, n + 1));
                }
            }
            let _ = std::fs::write(self.journal_gen_file(workspace, 1), head.join("\n") + "\n");
            let _ = std::fs::write(&active, tail.join("\n") + "\n");
        }
        // Enforce totals, oldest first. Whole-generation deletion
        // overshoots crumb excesses (it would eat a fresh .1 for one row
        // over), so trim the oldest generation partially instead — keep
        // its newest rows, drop exactly the excess.
        loop {
            let (total_lines, total_bytes, oldest) = self.generation_totals(workspace);
            if total_lines <= RETENTION_LINES && total_bytes <= RETENTION_BYTES {
                break;
            }
            let Some(p) = oldest else { break };
            let gl = count_lines(&p);
            let gb = file_bytes(&p);
            let excess_l = total_lines.saturating_sub(RETENTION_LINES);
            let excess_b = total_bytes.saturating_sub(RETENTION_BYTES);
            // Bytes excess converts via average row size (floor 64B).
            let avg = (total_bytes / total_lines.max(1) as u64).max(64);
            let need = excess_l.max((excess_b / avg) as usize + usize::from(excess_b > 0));
            if need == 0 || gl == 0 {
                break;
            }
            if need >= gl {
                dropped_lines += gl;
                dropped_bytes += gb;
                let _ = std::fs::remove_file(&p);
            } else if let Ok(raw) = std::fs::read_to_string(&p) {
                let rows: Vec<&str> = raw.lines().collect();
                let keep = &rows[need.min(rows.len())..];
                dropped_lines += rows.len() - keep.len();
                dropped_bytes += gb - count_bytes(keep);
                let _ = std::fs::write(&p, keep.join("\n") + "\n");
            } else {
                break;
            }
        }
        self.append_audit(
            workspace,
            serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "workspace": workspace,
                "kind": "journal_rotated",
                "dropped_lines": dropped_lines,
                "dropped_bytes": dropped_bytes,
            }),
        );
    }

    /// (lines, bytes, oldest-generation-path) across active + generations.
    fn generation_totals(&self, workspace: &str) -> (usize, u64, Option<PathBuf>) {
        let mut lines = 0usize;
        let mut bytes = 0u64;
        // Oldest = highest generation number; ascending keeps the last hit.
        let mut oldest: Option<PathBuf> = None;
        for n in 1..=RETENTION_GENS {
            let p = self.journal_gen_file(workspace, n);
            if p.exists() {
                lines += count_lines(&p);
                bytes += file_bytes(&p);
                oldest = Some(p);
            }
        }
        let active = self.journal_file(workspace);
        if active.exists() {
            lines += count_lines(&active);
            bytes += file_bytes(&active);
        }
        (lines, bytes, oldest)
    }

    /// Retained journal rows (stats denominator for journal lag).
    pub fn journal_row_count(&self, workspace: &str) -> u64 {
        self.journal_chain(workspace)
            .iter()
            .map(|p| count_lines(p))
            .sum::<usize>() as u64
    }

    /// Highest seq across active + generations (0 when none). The Brain
    /// resumes its counter from here so seq stays monotonic per workspace.
    pub fn last_seq(&self, workspace: &str) -> u64 {
        let mut max = 0u64;
        for path in self.journal_chain(workspace) {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in raw.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                    && let Some(s) = v.get("seq").and_then(|s| s.as_u64())
                {
                    max = max.max(s);
                }
            }
        }
        max
    }

    /// Ordered journal chain, oldest first: generations ascending, active last.
    pub fn journal_chain(&self, workspace: &str) -> Vec<PathBuf> {
        let mut chain = Vec::new();
        for n in (1..=RETENTION_GENS).rev() {
            let p = self.journal_gen_file(workspace, n);
            if p.exists() {
                chain.push(p);
            }
        }
        let active = self.journal_file(workspace);
        if active.exists() {
            chain.push(active);
        }
        chain
    }

    /// Fold one workspace loud (D4.4): entries in order until the first
    /// unparseable line or seq gap, then stop. Missing `seq` reads as
    /// legacy (tolerated, no contiguity check). Every torn fold appends a
    /// `LoudMiss journal_torn_tail` audit row — silence is the bug.
    pub fn fold_workspace(&self, workspace: &str) -> Fold {
        let mut rows = Vec::new();
        let mut first_seq: Option<u64> = None;
        let mut prev_seq: Option<u64> = None;
        let mut torn: Option<TornTail> = None;
        'files: for path in self.journal_chain(workspace) {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (idx, line) in raw.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    torn = Some(TornTail {
                        at_line: idx + 1,
                        file: path.display().to_string(),
                        reason: "unparseable row".into(),
                    });
                    break 'files;
                };
                if let Some(s) = v.get("seq").and_then(|s| s.as_u64()) {
                    if first_seq.is_none() {
                        first_seq = Some(s);
                    }
                    if let Some(p) = prev_seq
                        && s != p + 1
                    {
                        torn = Some(TornTail {
                            at_line: idx + 1,
                            file: path.display().to_string(),
                            reason: format!("seq gap: {p} -> {s}"),
                        });
                        break 'files;
                    }
                    prev_seq = Some(s);
                }
                rows.push(v);
            }
        }
        if let Some(t) = &torn {
            self.append_audit(
                workspace,
                serde_json::json!({
                    "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    "workspace": workspace,
                    "kind": "LoudMiss",
                    "detail": "journal_torn_tail",
                    "file": t.file,
                    "at_line": t.at_line,
                    "reason": t.reason,
                }),
            );
        }
        Fold {
            rows,
            first_seq,
            torn,
        }
    }

    /// Cheap tail probe for `/health`: true when any workspace journal ends
    /// torn (unparseable tail row or trailing seq gap).
    pub fn any_torn_tail(&self) -> bool {
        let Ok(entries) = std::fs::read_dir(self.state_dir.join("journals")) else {
            return false;
        };
        let mut seen = std::collections::HashSet::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Active files only (`doorbell-journal-{ws}.jsonl`); generation
            // files (`.N.jsonl`) fold with their workspace — probe once.
            let Some(rest) = name
                .strip_prefix("doorbell-journal-")
                .and_then(|n| n.strip_suffix(".jsonl"))
            else {
                continue;
            };
            if rest.contains('.') || rest.is_empty() {
                continue;
            }
            if !seen.insert(rest.to_string()) {
                continue;
            }
            if self.fold_workspace(rest).torn.is_some() {
                return true;
            }
        }
        false
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
            let _ = private_dir(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = private_append(&path) {
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
            private_dir(parent)?;
        }
        private_write(&path, json.as_bytes())?;
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
            let _ = private_dir(parent);
        }
        let _ = private_write(&path, (pins.join("\n") + "\n").as_bytes());
    }

    /// Envelope version; missing `v` reads as 1, unknown `v` must refuse
    /// loudly at the fold (D4.1). TODO(bd-asr2.4): torn-tail stop-and-loud.
    pub fn entry_version(entry: &serde_json::Value) -> u64 {
        entry.get("v").and_then(|v| v.as_u64()).unwrap_or(1)
    }
}
/// Loud fold result (D4.4): rows in order until the first tear, if any.
#[derive(Debug, Clone)]
pub struct Fold {
    pub rows: Vec<serde_json::Value>,
    /// First seq-bearing seq (None when the fold holds no seq rows).
    /// A query for an older seq predates retention: say the first
    /// foldable seq instead of folding a partial proof.
    pub first_seq: Option<u64>,
    pub torn: Option<TornTail>,
}

#[derive(Debug, Clone)]
pub struct TornTail {
    pub at_line: usize,
    pub file: String,
    pub reason: String,
}

fn count_lines(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|raw| raw.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

fn file_bytes(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn count_bytes(rows: &[&str]) -> u64 {
    rows.iter().map(|l| l.len() as u64 + 1).sum()
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
        assert_eq!(FileStore::entry_version(&v), 2);
        assert_eq!(FileStore::entry_version(&serde_json::json!({})), 1);
        assert!(st.load_wake("missing").since_by_pin.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn test_store(name: &str) -> (FileStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("dbrt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (FileStore::new(&dir), dir)
    }

    fn append_seq(st: &FileStore, ws: &str, seq: u64) {
        st.append_journal_seq(
            ws,
            &serde_json::json!({"rt_kind": "Sweep"}),
            &serde_json::json!([]),
            serde_json::json!({"stage": "sweep"}),
            &[],
            Some(seq),
            None,
        )
        .unwrap();
    }
    #[test]
    fn rotation_keeps_generations_and_audits_drops() {
        let (st, dir) = test_store("rot");
        // Small rows (production-shaped): the line budget binds first.
        for seq in 1..=6000u64 {
            append_seq(&st, "w", seq);
        }
        // Retention enforced: total lines bounded, generations exist, the
        // drop is an audit row (never silent).
        let (lines, _, _) = st.generation_totals("w");
        assert!(
            lines <= super::RETENTION_LINES,
            "total {lines} over retention"
        );
        assert!(st.journal_gen_file("w", 1).exists(), "generation retained");
        let audit = std::fs::read_to_string(st.audit_file("w")).unwrap();
        assert!(audit.contains("journal_rotated"), "{audit}");
        // Fold still works across generations, oldest first, seq-continuous.
        let fold = st.fold_workspace("w");
        assert!(fold.torn.is_none(), "rotation must not tear the fold");
        assert!(!fold.rows.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_files_and_dirs() {
        // Audit P2: journal/audit/wake/unanswered files are created
        // owner-only (0600, dirs 0700) — they carry wake metadata.
        let (st, dir) = test_store("perms");
        append_seq(&st, "w", 1);
        st.append_audit("w", serde_json::json!({"kind": "x"}));
        st.save_wake("w", &Default::default()).unwrap();
        st.save_unanswered("w", &["p".into()]);
        let mode = |p: &std::path::Path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode(&st.journal_file("w")), 0o600, "journal");
        assert_eq!(mode(&st.audit_file("w")), 0o600, "audit");
        assert_eq!(mode(&st.wake_file("w")), 0o600, "wake");
        assert_eq!(mode(&st.unanswered_file("w")), 0o600, "unanswered");
        assert_eq!(mode(&dir.join("journals")), 0o700, "journals dir");
        assert_eq!(mode(&dir.join("doorbell")), 0o700, "doorbell dir");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn torn_tail_stops_fold_loud() {
        let (st, dir) = test_store("torn");
        for seq in 1..=3u64 {
            append_seq(&st, "w", seq);
        }
        // Crash mid-line: torn tail.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(st.journal_file("w"))
            .unwrap();
        writeln!(f, "{{\"v\": 1, \"seq\": 4, BROKEN").unwrap();
        drop(f);
        let fold = st.fold_workspace("w");
        assert_eq!(fold.rows.len(), 3, "fold stops at the tear");
        let torn = fold.torn.expect("tear detected");
        assert_eq!(torn.at_line, 4);
        // Loud: a LoudMiss audit row names the tear.
        let audit = std::fs::read_to_string(st.audit_file("w")).unwrap();
        assert!(audit.contains("journal_torn_tail"), "{audit}");
        assert!(st.any_torn_tail(), "health probe sees the tear");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn seq_gap_stops_fold_and_legacy_rows_tolerated() {
        let (st, dir) = test_store("gap");
        append_seq(&st, "w", 1);
        // Legacy row without seq: tolerated, no contiguity check.
        st.append_journal(
            "w",
            &serde_json::json!({"rt_kind": "Legacy"}),
            &serde_json::json!([]),
            serde_json::json!({"stage": "sweep"}),
            &[],
        )
        .unwrap();
        append_seq(&st, "w", 2);
        // Gap: 2 -> 4 skips 3.
        append_seq(&st, "w", 4);
        let fold = st.fold_workspace("w");
        assert_eq!(fold.rows.len(), 3, "rows before the gap fold");
        assert_eq!(fold.first_seq, Some(1));
        assert!(fold.torn.is_some(), "gap is loud");
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn mixed_v1_v2_folds_in_order() {
        // F9: migration mix — legacy v1 rows plus v2 seq/subscriber rows fold
        // in order with no tear; unknown fields tolerated.
        let (st, dir) = test_store("mixv");
        use std::io::Write;
        let path = st.journal_file("w");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{{\"v\":1,\"workspace\":\"w\",\"msg\":{{\"rt_kind\":\"Legacy\"}},\"actions\":[],\"executed\":{{\"stage\":\"sweep\"}},\"pins\":[]}}").unwrap();
        drop(f);
        st.append_journal_seq(
            "w",
            &serde_json::json!({"rt_kind": "Sweep"}),
            &serde_json::json!([]),
            serde_json::json!({"stage": "sweep"}),
            &[],
            Some(1),
            Some("workspace:w"),
        )
        .unwrap();
        let fold = st.fold_workspace("w");
        assert!(fold.torn.is_none(), "mixed v1/v2 must not tear");
        assert_eq!(fold.rows.len(), 2);
        assert_eq!(FileStore::entry_version(&fold.rows[0]), 1);
        assert_eq!(FileStore::entry_version(&fold.rows[1]), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
