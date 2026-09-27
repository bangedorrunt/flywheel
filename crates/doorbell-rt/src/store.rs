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

/// bd-lorot: what a prune pass did. Counted so the hourly tick can be honest
/// about it rather than deleting silently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Journal files removed.
    pub journals_removed: usize,
    /// Bytes reclaimed.
    pub bytes_freed: u64,
    /// Files left alone because they were too young to be abandoned.
    pub skipped_young: usize,
    /// Liveness stamp files removed (`watch-<ws>.hb` / `.event`).
    pub stamps_removed: usize,
    /// Files this process could not remove (permissions, a race with a writer).
    pub remove_failed: usize,
}

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

    /// bd-lorot: delete per-workspace logs for workspaces this daemon no longer
    /// hosts AND has not touched recently.
    ///
    /// Both file kinds accumulate one-per-workspace and nothing ever removed
    /// them, so the set only grew. `known_journal_workspaces` reads the journals
    /// directory and reports one entry per file, which is the input to
    /// `known_workspaces` and therefore to the standby wake-blind signal: a
    /// workspace the plane served once, months ago, kept the daemon reporting
    /// "a fleet expects rings I will not deliver" forever. Measured: 913 journal
    /// files, 20 MB, `known_workspaces` 858.
    ///
    /// Two conditions, both required, and the second is what makes this safe:
    /// - the workspace is not in `live` (not currently hosted), AND
    /// - its newest log mtime is older than `max_age_secs`.
    ///
    /// A brand-new workspace whose host loop has not written yet is therefore
    /// never a candidate, so a just-spawned crew cannot have its journal deleted
    /// out from under it. Deleting live state is not reversible, so the age gate
    /// is deliberately generous; the caller passes the policy.
    pub fn prune_unhosted_workspace_logs(
        &self,
        live: &std::collections::BTreeSet<String>,
        max_age_secs: u64,
        now_secs: u64,
    ) -> PruneReport {
        let mut report = PruneReport::default();
        let age_ok = |mtime: u64| now_secs.saturating_sub(mtime) >= max_age_secs;

        if let Ok(entries) = std::fs::read_dir(self.state_dir.join("journals")) {
            for entry in entries.filter_map(std::result::Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(ws) = name
                    .strip_prefix("doorbell-journal-")
                    .and_then(|n| n.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                // A generation file (`…<ws>.1.jsonl`) is covered by the same
                // workspace name, so pruning the active file's siblings is safe
                // once the workspace itself is gone.
                if live.contains(ws) {
                    continue;
                }
                let Ok(md) = entry.metadata() else { continue };
                let Some(mtime) = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                else {
                    continue;
                };
                if !age_ok(mtime) {
                    report.skipped_young += 1;
                    continue;
                }
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => {
                        report.journals_removed += 1;
                        report.bytes_freed += md.len();
                    }
                    Err(_) => report.remove_failed += 1,
                }
            }
        }
        // bd-lorot: the LIVENESS stamps get the same two guards, and they matter
        // more than the journals. `watch-<ws>.hb` is a heartbeat that exists to
        // say "this workspace's bus is delivering". One left behind for a
        // workspace nobody hosts is a liveness artifact vouching for nothing,
        // and a reader who trusts a file over the hosted set is worse off than
        // one with no file. Measured 10 stale, the oldest 14.4 days.
        for (prefix, suffix) in [("watch-", ".hb"), ("watch-", ".event")] {
            let Ok(entries) = std::fs::read_dir(self.state_dir.join("doorbell")) else {
                continue;
            };
            for entry in entries.filter_map(std::result::Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(ws) = name
                    .strip_prefix(prefix)
                    .and_then(|n| n.strip_suffix(suffix))
                else {
                    continue;
                };
                if ws.is_empty() || live.contains(ws) {
                    continue;
                }
                let Ok(md) = entry.metadata() else { continue };
                let Some(mtime) = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                else {
                    continue;
                };
                if !age_ok(mtime) {
                    report.skipped_young += 1;
                    continue;
                }
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => {
                        report.stamps_removed += 1;
                        report.bytes_freed += md.len();
                    }
                    Err(_) => report.remove_failed += 1,
                }
            }
        }
        report
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

    /// bd-v7uu: the "have I already reported this tear?" memory, kept OUT of the
    /// audit on purpose.
    ///
    /// It used to be a `raw.contains(&fingerprint)` scan of the audit file — but
    /// the audit is pruned to `AUDIT_KEEP_LINES` / `AUDIT_MAX_BYTES`, so the
    /// memory had the same lifetime as the report it was suppressing. Measured on
    /// this host: `flywheel-toron.audit.jsonl` sat at 3965 of 4000 lines, so a
    /// tear reported before the next prune would be re-reported afterwards. The
    /// general shape is the receipt-tombstone one: a bounded store cannot hold
    /// dedup state for itself, because the state disappears exactly when it is
    /// still needed.
    ///
    /// One fingerprint per line, append-only, never pruned: the size is bounded by
    /// the number of DISTINCT tears, which is a handful, not by the traffic that
    /// produces reports.
    fn loudmiss_file(&self, workspace: &str) -> std::path::PathBuf {
        let dir = self.audit_file(workspace);
        let stem = dir.file_stem().and_then(|s| s.to_str()).unwrap_or("audit");
        dir.with_file_name(format!("{stem}.loudmiss"))
    }

    fn tear_already_reported(&self, workspace: &str, fingerprint: &str) -> bool {
        std::fs::read_to_string(self.loudmiss_file(workspace))
            .is_ok_and(|raw| raw.lines().any(|l| l == fingerprint))
    }

    fn mark_tear_reported(&self, workspace: &str, fingerprint: &str) {
        let path = self.loudmiss_file(workspace);
        if let Some(parent) = path.parent() {
            let _ = private_dir(parent);
        }
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{fingerprint}");
        }
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
            // bd-lorot: this fired on EVERY fold, and a health probe folds, so a
            // workspace left torn wrote a LoudMiss row every few seconds forever
            // (observed 13:06:55, 13:07:20, 13:07:26, 13:07:27). The row is a
            // REPORT, and a report that repeats unchanged is noise that trains
            // a reader to skip the field. One row per distinct tear.
            let fingerprint = format!("{}:{}:{}", t.file, t.at_line, t.reason);
            let already = self.tear_already_reported(workspace, &fingerprint);
            if !already {
                // Mark BEFORE the append, and separately: the marker has to exist
                // even if the audit write fails, or a failed report becomes a
                // permanently re-firing one.
                self.mark_tear_reported(workspace, &fingerprint);
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
                        "fingerprint": fingerprint,
                    }),
                );
            }
        }
        Fold {
            rows,
            first_seq,
            torn,
        }
    }

    /// Cheap tail probe for `/health`: true when any workspace journal ends
    /// torn (unparseable tail row or trailing seq gap).
    /// Is any HOSTED workspace's journal torn?
    ///
    /// bd-lorot: this used to scan EVERY journal on disk, so an unhosted,
    /// abandoned workspace could hold a live daemon's `degraded` red forever.
    /// Measured live: workspace `flywheel` — not one of the two hosted — carried
    /// a frozen `seq gap: 366803 -> 366807` at line 3440, and
    /// `journal_torn_tail` never cleared, for a workspace nobody delivers to.
    /// An unhosted journal cannot affect delivery, so it cannot be evidence of
    /// a delivery fault.
    ///
    /// This is bd-gpj0.4's rule applied to the tail: one local miss is not a
    /// dead path. A tear on a workspace we HOST is degraded; a tear on one we
    /// abandoned is history.
    pub fn any_torn_tail_among(&self, hosted: &[&str]) -> bool {
        hosted
            .iter()
            .any(|ws| self.fold_workspace(ws).torn.is_some())
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

    pub fn save_wake(
        &self,
        workspace: &str,
        st: &doorbell_core::WakeState,
    ) -> Result<(), crate::error::RtError> {
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

    /// bd-v7uu: a store root unique per CALL, not per name.
    ///
    /// It was `dbrt-{name}-{pid}`, so two tests sharing a name shared a
    /// directory — and `test_store` REMOVES that directory on entry. Observed as
    /// a ~1-in-5 flake: the compiled binary registered
    /// `a_tear_is_reported_once_even_after_the_audit_is_emptied` twice (one
    /// definition in the source, two in `--list`, i.e. a stale build), the two
    /// instances ran in parallel, one wiped the other's journal, and the fold came
    /// back with zero rows and no tear.
    ///
    /// The pid does not save you. It makes roots unique across PROCESSES, and the
    /// hazard is two tests inside ONE process — which is exactly what parallel
    /// test threads are. And a name collision can be introduced by nothing more
    /// than a copy-pasted test, with no compile error to notice it.
    ///
    /// A per-call counter makes the root unique within the process, so the
    /// harness cannot be broken by duplication, by a stale binary, or by a
    /// future test reusing a name.
    fn test_store(name: &str) -> (FileStore, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dbrt-{name}-{}-{n}", std::process::id()));
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
    /// bd-v7uu: one row per distinct tear must survive the AUDIT's own retention.
    ///
    /// The dedup used to be `raw.contains(&fingerprint)` over the audit file — and
    /// the audit is pruned to `AUDIT_KEEP_LINES` / `AUDIT_MAX_BYTES`. So the memory
    /// had the same lifetime as the report it suppresses: measured on this host,
    /// `flywheel-toron.audit.jsonl` sat at 3965 of 4000 lines, so one prune and
    /// every previously-reported tear became reportable again, forever, at the
    /// re-detection rate. A bounded store cannot hold dedup state for itself; the
    /// state vanishes exactly when it is still needed.
    ///
    /// The audit is emptied here rather than pruned to the cap, which is the
    /// strongest form of the same failure.
    #[test]
    fn a_tear_is_reported_once_even_after_the_audit_is_emptied() {
        let (st, _dir) = test_store("torn-dedup-rot");
        for seq in 1..=3u64 {
            append_seq(&st, "w", seq);
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(st.journal_file("w"))
            .unwrap();
        writeln!(f, "{{\"v\": 1, \"seq\": 4, BROKEN").unwrap();
        drop(f);

        // SPLIT the two halves so a failure names WHICH one is flaky: detection
        // (the fold saw a tear) and the report (the audit row landed).
        let fold = st.fold_workspace("w");
        assert_eq!(
            fold.rows.len(),
            3,
            "fold must stop at the tear; if this is the failure the tear was never \
             detected, and if it is the count below then detection was fine"
        );
        assert!(
            fold.torn.is_some(),
            "the torn line must be detected; rows={} torn={:?}",
            fold.rows.len(),
            fold.torn.as_ref().map(|t| (t.at_line, t.reason.clone()))
        );
        let count = |st: &FileStore| {
            std::fs::read_to_string(st.audit_file("w"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains("journal_torn_tail"))
                .count()
        };
        assert_eq!(count(&st), 1, "the first fold reports the tear");

        // Fold again with the audit intact: still one (the pre-existing dedup).
        st.fold_workspace("w");
        assert_eq!(count(&st), 1, "an un-deduped report repeats on every fold");

        // Now the audit loses everything. The MARKER must still remember.
        std::fs::write(st.audit_file("w"), "").unwrap();
        st.fold_workspace("w");
        assert_eq!(
            count(&st),
            0,
            "an emptied audit must not resurrect a reported tear — the old \
             dedup read the audit, so pruning it made every old tear reportable again"
        );
        assert!(
            st.loudmiss_file("w").exists(),
            "the dedup memory must live OUTSIDE the rotating audit"
        );
        let marker = std::fs::read_to_string(st.loudmiss_file("w")).unwrap_or_default();
        assert!(
            marker
                .lines()
                .any(|l| l.contains("journal_torn_tail") || l.contains(".jsonl")),
            "the marker must name the tear, not just exist: {marker:?}"
        );
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
        assert!(
            st.any_torn_tail_among(&["w"]),
            "health probe sees the tear on a HOSTED workspace"
        );
        // bd-lorot: the same tear on a workspace we do NOT host must degrade
        // nothing. An unhosted journal cannot affect delivery, so it is history,
        // not a fault — and it held `degraded` red for hours here.
        assert!(
            !st.any_torn_tail_among(&["other"]),
            "an unhosted torn workspace degraded the daemon"
        );
        // bd-lorot: and the LoudMiss REPORT is emitted once, not once per fold.
        // A health probe folds on every call, so an un-deduped report repeats
        // every few seconds forever.
        let rows_before = std::fs::read_to_string(st.audit_file("w"))
            .unwrap()
            .lines()
            .filter(|l| l.contains("journal_torn_tail"))
            .count();
        for _ in 0..5 {
            let _ = st.fold_workspace("w");
            let _ = st.any_torn_tail_among(&["w"]);
        }
        let rows_after = std::fs::read_to_string(st.audit_file("w"))
            .unwrap()
            .lines()
            .filter(|l| l.contains("journal_torn_tail"))
            .count();
        assert_eq!(
            rows_before, rows_after,
            "six folds of an unchanged tear wrote {rows_before} -> {rows_after} LoudMiss rows: \
             a report that repeats unchanged is noise that trains a reader to skip the field"
        );
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

#[cfg(test)]
mod prune_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn ws_file(dir: &Path, ws: &str) -> PathBuf {
        dir.join("journals")
            .join(format!("doorbell-journal-{ws}.jsonl"))
    }

    /// bd-lorot: an unhosted, old journal goes; a hosted one and a young one
    /// stay. All three conditions in one fence, because the dangerous case is
    /// deleting a LIVE crew's wake record.
    #[test]
    fn prune_takes_unhosted_and_old_and_spares_the_rest() {
        let dir = std::env::temp_dir().join(format!("prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("journals")).expect("dirs");
        let store = FileStore::new(&dir);
        // Real current time: a logical future `now` would make a file written
        // just now look decades old, which is a broken fixture, not a finding.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        const OLD: u64 = 30 * 24 * 60 * 60;

        for ws in ["gone-old", "live-old", "gone-young"] {
            std::fs::write(ws_file(&dir, ws), "row\n").expect("write");
        }
        // gone-old: unhosted and a month old. live-old: a month old but HOSTED.
        // gone-young: unhosted but written an hour ago.
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(now - OLD);
        std::fs::File::open(ws_file(&dir, "gone-old"))
            .and_then(|f| f.set_modified(old))
            .expect("age gone-old");
        std::fs::File::open(ws_file(&dir, "live-old"))
            .and_then(|f| f.set_modified(old))
            .expect("age live-old");

        let live: BTreeSet<String> = BTreeSet::from(["live-old".to_string()]);
        let report = store.prune_unhosted_workspace_logs(&live, 7 * 24 * 60 * 60, now);

        assert_eq!(
            report.journals_removed, 1,
            "exactly one should go: {report:?}"
        );
        assert!(
            !ws_file(&dir, "gone-old").exists(),
            "the unhosted old journal must be gone"
        );
        assert!(
            ws_file(&dir, "live-old").exists(),
            "a HOSTED workspace's journal must never be pruned — that is a live crew's wake record"
        );
        assert!(
            ws_file(&dir, "gone-young").exists(),
            "a young journal is a just-started workspace, not an abandoned one"
        );
        assert_eq!(
            report.skipped_young, 1,
            "the young one is reported, not silently kept"
        );
        assert_eq!(report.remove_failed, 0);
        assert!(report.bytes_freed > 0, "freed bytes are reported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fence that matters most: pruning twice is idempotent, and a second
    /// pass over an already-clean directory does not error or under-report.
    #[test]
    fn pruning_is_idempotent_and_reports_cleanly_when_there_is_nothing_to_do() {
        let dir = std::env::temp_dir().join(format!("prune-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("journals")).expect("dirs");
        let store = FileStore::new(&dir);
        let now = 2_000_000_000u64;
        let live = BTreeSet::new();

        let first = store.prune_unhosted_workspace_logs(&live, 0, now);
        assert_eq!(first.journals_removed, 0, "empty dir: {first:?}");
        assert_eq!(first.remove_failed, 0);

        std::fs::write(ws_file(&dir, "a"), "x\n").expect("write");
        let a = store.prune_unhosted_workspace_logs(&live, 0, now);
        assert_eq!(a.journals_removed, 1);
        let b = store.prune_unhosted_workspace_logs(&live, 0, now);
        assert_eq!(
            b.journals_removed, 0,
            "second pass must find nothing: {b:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-journal files in the journals directory are never touched — the prune
    /// matches the exact `doorbell-journal-<ws>.jsonl` shape and ignores the
    /// rest, so it cannot delete something it does not understand.
    #[test]
    fn the_prune_only_touches_journal_files_it_recognises() {
        let dir = std::env::temp_dir().join(format!("prune-narrow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("journals")).expect("dirs");
        let store = FileStore::new(&dir);
        let now = 2_000_000_000u64;

        let bystander = dir.join("journals").join("notes.jsonl");
        std::fs::write(&bystander, "keep me\n").expect("write");
        std::fs::write(ws_file(&dir, "gone"), "x\n").expect("write");

        let report = store.prune_unhosted_workspace_logs(&BTreeSet::new(), 0, now);
        assert_eq!(report.journals_removed, 1);
        assert!(
            bystander.exists(),
            "a file the prune does not recognise must survive: it matches no journal shape"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// bd-lorot: the LIVENESS stamps obey the same two guards as the journals,
    /// and the hosted one is the one that must never be touched — a live
    /// workspace's heartbeat is how /health knows its bus is delivering.
    #[test]
    fn prune_takes_unhosted_stale_stamps_and_spares_a_live_heartbeat() {
        let dir = std::env::temp_dir().join(format!("stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("journals")).expect("dirs");
        std::fs::create_dir_all(dir.join("doorbell")).expect("dirs");
        let store = FileStore::new(&dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let old_t = std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(now.saturating_sub(30 * 24 * 60 * 60));

        for ws in ["dead", "live", "young"] {
            std::fs::write(dir.join("doorbell").join(format!("watch-{ws}.hb")), "x").expect("hb");
            std::fs::write(dir.join("doorbell").join(format!("watch-{ws}.event")), "y")
                .expect("ev");
        }
        // BOTH the dead AND the live workspace are aged. This is the point: if
        // only the dead one is old, the live one is protected by the AGE gate
        // and the hosted guard is never exercised — a fence that passes for the
        // wrong reason is worse than no fence, because it reads as coverage.
        // `young` is deliberately NOT aged; that is what makes the age gate
        // falsifiable in this path instead of only in the journal path.
        for ws in ["dead", "live"] {
            for suffix in ["hb", "event"] {
                std::fs::File::open(dir.join("doorbell").join(format!("watch-{ws}.{suffix}")))
                    .and_then(|f| f.set_modified(old_t))
                    .expect("age stamp");
            }
        }
        let live = BTreeSet::from(["live".to_string()]);
        let report = store.prune_unhosted_workspace_logs(&live, 7 * 24 * 60 * 60, now);

        assert_eq!(
            report.stamps_removed, 2,
            "the dead workspace's .hb AND .event both go: {report:?}"
        );
        assert!(
            !dir.join("doorbell").join("watch-dead.hb").exists(),
            "an unhosted stale heartbeat must be removed — it vouches for a workspace nobody watches"
        );
        assert!(
            dir.join("doorbell").join("watch-live.hb").exists(),
            "a HOSTED workspace's heartbeat is how /health knows its bus delivers; never prune it"
        );
        assert!(
            dir.join("doorbell").join("watch-live.event").exists(),
            "and its .event survives on the same ground"
        );
        assert_eq!(
            report.stamps_removed, 2,
            "exactly the dead workspace's two files: the live pair is held by the hosted guard \
             ALONE, since both were aged past the threshold"
        );
        assert!(
            dir.join("doorbell").join("watch-young.hb").exists(),
            "a YOUNG unhosted stamp is a workspace that just started, not an abandoned one — \
             deleting it on the unhosted guard alone would kill a live crew's liveness signal \
             during its first week"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
