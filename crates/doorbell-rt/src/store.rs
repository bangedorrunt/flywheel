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

/// Replace a file's contents atomically: write a sibling temp file, then rename
/// over the target.
///
/// bd-xz7k: `std::fs::write` truncates FIRST, so its failure mode on a journal
/// rewrite was a destroyed file plus a discarded error. With a temp-and-rename a
/// failed rewrite is a no-op, and the original bytes are still there to fold.
fn private_replace(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = tmp_beside(path);
    if let Err(e) = private_write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The sibling temp file an atomic replace writes before renaming over its
/// target. Named beside the target so the rename stays on one filesystem.
fn tmp_beside(path: &std::path::Path) -> std::path::PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(".tmp");
    std::path::PathBuf::from(raw)
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

/// bd-oa4u (item C of bd-3uqz): the workspace a journal file belongs to, generation
/// suffix included.
///
/// Returns `None` for a file that is not a journal of ours.
///
/// The generation suffix is stripped because the LIVENESS guard in
/// `prune_unhosted_workspace_logs` has to recognise it. That guard reads
/// `doorbell-journal-<ws>.jsonl`, and for `doorbell-journal-flywheel.1.jsonl` a
/// naive prefix-strip plus `.jsonl` strip yields `flywheel.1` — a workspace that
/// is not in the hosted set even when `flywheel` is. The prune then deleted a live
/// crew's retained history on age. A comment claimed the generation file "is
/// covered by the same workspace name"; the code did not do that.
///
/// Only a trailing run of DIGITS is treated as the generation, because workspace
/// names legitimately contain dots — `flywheel-toron.dev` and `flywheel-torondev`
/// are both real rows in this state directory. Stripping the last dot-segment
/// unconditionally would merge those two distinct workspaces into one name and
/// hand a dead workspace's protection to a live one, which is the same class of
/// error pointed the other way.
pub fn journal_workspace(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("doorbell-journal-")?;
    let rest = rest.strip_suffix(".jsonl")?;
    let ws = match rest.rsplit_once('.') {
        // A generation is digits, and something must remain in front of the dot.
        Some((head, tail))
            if !head.is_empty() && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => rest,
    };
    // Never hand back a name that identifies nothing. `doorbell-journal-.jsonl`
    // and `doorbell-journal-flywheel..jsonl` are malformed files, not workspaces,
    // and a caller that receives Some("") has been handed a key no hosted set can
    // ever contain — which is the same missing-liveness-guard failure one level up.
    if ws.is_empty() || ws.ends_with('.') {
        return None;
    }
    Some(ws)
}

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

/// Failed audit-row appends process-wide, counted by the writer itself.
/// append_audit eprintln!s the cause on every failure path; it also bumps
/// this, so the failure is queryable (health, tests) and no call-site
/// `let _` can silently drop a journal write anymore — present or future.
static AUDIT_WRITE_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// How many audit-row appends have failed since process start.
#[allow(dead_code)] // consumed by tests today; health wiring is a follow-up, not this bead
pub(crate) fn audit_write_failures() -> usize {
    AUDIT_WRITE_FAILURES.load(std::sync::atomic::Ordering::SeqCst)
}

/// Bump the write-failure count. Only the writer calls this.
pub(crate) fn note_audit_write_failure() {
    AUDIT_WRITE_FAILURES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    /// rv23 #3: the sweep beat. A no-op sweep writes no journal row any more,
    /// so `/health`'s `sweep.last_age_s` reads this stamp (its mtime) first
    /// and the journal's mtime only as the fallback for an older daemon.
    pub fn sweep_stamp_file(&self, workspace: &str) -> PathBuf {
        self.state_dir
            .join("doorbell")
            .join(format!("{workspace}.sweep"))
    }

    /// Stamp the sweep beat: one small overwrite, never an append.
    pub fn touch_sweep(&self, workspace: &str, now_unix: u64) -> std::io::Result<()> {
        let path = self.sweep_stamp_file(workspace);
        if let Some(parent) = path.parent() {
            private_dir(parent)?;
        }
        std::fs::write(&path, now_unix.to_string())
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
                // bd-oa4u: parse through the shared helper so a GENERATION file
                // resolves to the workspace it belongs to. The previous inline
                // parse produced `flywheel.1`, which is in no hosted set, so every
                // retained generation of every live workspace was a prune
                // candidate the moment it aged past the window. What the comment
                // here claimed, and the code did not do.
                let Some(ws) = journal_workspace(&name) else {
                    continue;
                };
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
        // The seq range this rotation removes. Without it the drop record cannot
        // be tied to the gap it leaves, so a designed drop and a lost write look
        // identical to the fold — which is why seven workspaces read "torn" for
        // days while the daemon knew perfectly well what it had deleted.
        let mut drop_lo: Option<u64> = None;
        let mut drop_hi: Option<u64> = None;
        // bd-xz7k: every destructive step is CHECKED. This method is the one that
        // deletes rows, and it used to discard every write error (`let _ =`) and
        // then report a clean drop carrying a seq range — so a failed rewrite
        // both destroyed the journal and produced a record that excused the loss
        // (the flywheel-chiebukuro shape: four generations on disk, 89 gaps, and
        // a sum of recorded dropped_lines of zero).
        let mut failed: Vec<String> = Vec::new();
        // A plain fn, not a closure: a closure would hold the `drop_lo`/`drop_hi`
        // borrows for the whole body, and the audit row at the end reads them.
        fn note(acc: &mut Option<u64>, other: &mut Option<u64>, lo: Option<u64>, hi: Option<u64>) {
            if let Some(l) = lo {
                *acc = Some(acc.map_or(l, |x: u64| x.min(l)));
            }
            if let Some(h) = hi {
                *other = Some(other.map_or(h, |x: u64| x.max(h)));
            }
        }
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
                // Read before removing: this is the only moment the span is
                // recoverable.
                if let Ok(raw) = std::fs::read_to_string(&overflow) {
                    let lines: Vec<&str> = raw.lines().collect();
                    let (lo, hi) = seq_span(&lines);
                    note(&mut drop_lo, &mut drop_hi, lo, hi);
                }
                if let Err(e) = std::fs::remove_file(&overflow) {
                    failed.push(format!("remove {}: {e}", overflow.display()));
                }
            }
            for n in (1..RETENTION_GENS).rev() {
                let from = self.journal_gen_file(workspace, n);
                if from.exists()
                    && let Err(e) = std::fs::rename(&from, self.journal_gen_file(workspace, n + 1))
                {
                    failed.push(format!("shift {}: {e}", from.display()));
                }
            }
            // The head is committed to .1 BEFORE the active file is trimmed, on
            // purpose: if the trim fails, those rows still exist twice (in .1
            // and in the untrimmed active) — visible and recoverable. The other
            // order would lose them outright.
            if let Err(e) = private_replace(
                &self.journal_gen_file(workspace, 1),
                (head.join("\n") + "\n").as_bytes(),
            ) {
                failed.push(format!("write gen1: {e}"));
            }
            if failed.is_empty()
                && let Err(e) = private_replace(&active, (tail.join("\n") + "\n").as_bytes())
            {
                failed.push(format!("rewrite active: {e}"));
            }
        }
        // Enforce totals, oldest first. Whole-generation deletion
        // overshoots crumb excesses (it would eat a fresh .1 for one row
        // over), so trim the oldest generation partially instead — keep
        // its newest rows, drop exactly the excess.
        // A half-applied spill must not be compounded by the totals enforcer:
        // deleting more files on top of a broken chain is how a recoverable
        // inconsistency becomes an unrecoverable one.
        while failed.is_empty() {
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
                if let Ok(raw) = std::fs::read_to_string(&p) {
                    let lines: Vec<&str> = raw.lines().collect();
                    let (lo, hi) = seq_span(&lines);
                    note(&mut drop_lo, &mut drop_hi, lo, hi);
                }
                if let Err(e) = std::fs::remove_file(&p) {
                    failed.push(format!("remove {}: {e}", p.display()));
                    break;
                }
            } else if let Ok(raw) = std::fs::read_to_string(&p) {
                let rows: Vec<&str> = raw.lines().collect();
                let cut = need.min(rows.len());
                let keep = &rows[cut..];
                let (lo, hi) = seq_span(&rows[..cut]);
                note(&mut drop_lo, &mut drop_hi, lo, hi);
                dropped_lines += rows.len() - keep.len();
                dropped_bytes += gb - count_bytes(keep);
                if let Err(e) = private_replace(&p, (keep.join("\n") + "\n").as_bytes()) {
                    failed.push(format!("trim {}: {e}", p.display()));
                    break;
                }
            } else {
                break;
            }
        }
        // Loud first (GS-039): an operator watching stderr must not have to read
        // the audit to learn that the rotation failed.
        if !failed.is_empty() {
            eprintln!(
                "doorbell-rt: journal ROTATION FAILED on {workspace} — refuse silent drop \
                 (GS-039); no seq range is recorded for this rotation: {}",
                failed.join("; ")
            );
        }
        let mut row = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "workspace": workspace,
            "kind": "journal_rotated",
            "dropped_lines": dropped_lines,
            "dropped_bytes": dropped_bytes,
        });
        if let Some(obj) = row.as_object_mut() {
            if failed.is_empty() {
                // A range may only be recorded for rows we are SURE we removed.
                // A wrong range does not merely misreport: it EXCUSES a later
                // gap, because `RetentionDrop::explains` trusts it.
                obj.insert("seq_from".into(), drop_lo.into());
                obj.insert("seq_to".into(), drop_hi.into());
            } else {
                obj.insert("rotate_failed".into(), failed.join("; ").into());
            }
        }
        let _ = self.append_audit(workspace, row);
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

    /// bd-3uqz item E: sequence-number holes across the ACTIVE file and every
    /// retained generation, as one window.
    ///
    /// Generations are read oldest-first so the window is contiguous in the same
    /// order the reader folds them, and a rotation that dropped a contiguous range
    /// still reads as 0% holes rather than as a burn.
    pub fn journal_seq_holes(&self, workspace: &str) -> SeqHoles {
        // rv23 #3: borrow the lines from the file buffers. This runs on every
        // `/health` refresh, and an owned `String` per retained row was the
        // probe's second-largest share of the idle RSS climb.
        let mut raws: Vec<String> = Vec::new();
        for n in (1..=RETENTION_GENS).rev() {
            let p = self.journal_gen_file(workspace, n);
            if let Ok(raw) = std::fs::read_to_string(&p) {
                raws.push(raw);
            }
        }
        if let Ok(raw) = std::fs::read_to_string(self.journal_file(workspace)) {
            raws.push(raw);
        }
        let refs: Vec<&str> = raws.iter().flat_map(|raw| raw.lines()).collect();
        seq_holes(&refs)
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
        self.fold_chain(workspace, true)
    }

    /// rv23 #3: the same loud fold, for a caller that only wants `torn` (and
    /// `first_seq`). `rows` comes back EMPTY: every row is parsed, checked and
    /// dropped on the spot instead of being held.
    ///
    /// Root cause of the idle daemon's RSS climb: `/health` refreshes every
    /// 15 s and its torn-tail probe called `fold_workspace`, which builds a
    /// `serde_json::Value` tree for EVERY retained row and holds them all at
    /// once, only to read `torn`. The peak grew with the journal (about 1.4
    /// no-op rows/s idle), and the freed small-zone pages stayed resident.
    /// Measured A/B, 240 s idle: 9.7 KB/s with the probe, 3.4 KB/s without
    /// `torn_tails_among`, 0.6 KB/s with no health refresh at all.
    pub fn torn_probe(&self, workspace: &str) -> Fold {
        self.fold_chain(workspace, false)
    }

    fn fold_chain(&self, workspace: &str, keep_rows: bool) -> Fold {
        let mut rows = Vec::new();
        let mut first_seq: Option<u64> = None;
        let mut prev_seq: Option<u64> = None;
        let mut torn: Option<TornTail> = None;
        // A designed drop is not a tear. Retention records what it removed, so a
        // gap a record accounts for is folded straight through: calling a
        // healthy workspace degraded forever is how a signal stops meaning
        // anything (measured: seven workspaces, `journal_torn_tail`
        // permanently red, most of them for drops the daemon performed itself).
        let drops = self.retention_drops(workspace);
        let chain = self.journal_chain(workspace);
        'files: for path in &chain {
            let Ok(raw) = std::fs::read_to_string(path) else {
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
                        missing_seqs: 0,
                        missing_from: None,
                        missing_to: None,
                        rows_withheld: 0,
                        explained_by_drop: None,
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
                        let missing_from = p + 1;
                        let missing_to = s - 1;
                        let missing = s - p - 1;
                        match drops.iter().find(|d| d.explains(missing_from, missing_to)) {
                            // Retention, on the record: not a tear.
                            Some(_) => {}
                            None => {
                                torn = Some(TornTail {
                                    at_line: idx + 1,
                                    file: path.display().to_string(),
                                    reason: format!(
                                        "seq gap: {p} -> {s} ({missing} seqs missing, \
                                         {missing_from}..={missing_to}, no retention record \
                                         accounts for them)"
                                    ),
                                    missing_seqs: missing,
                                    missing_from: Some(missing_from),
                                    missing_to: Some(missing_to),
                                    rows_withheld: 0,
                                    explained_by_drop: None,
                                });
                                break 'files;
                            }
                        }
                    }
                    prev_seq = Some(s);
                }
                if keep_rows {
                    rows.push(v);
                }
            }
        }
        if let Some(t) = torn.as_mut() {
            // How much history the tear hides. The fold STOPS here, so every row
            // after it is unreachable from `doorbell why` — a cost the old
            // boolean never mentioned, and the reason a single 16-row gap looked
            // the same as a 5,175-row one.
            t.rows_withheld = rows_withheld_after(&chain, &t.file, t.at_line);
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
                let _ = self.append_audit(
                    workspace,
                    serde_json::json!({
                        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        "workspace": workspace,
                        "kind": "LoudMiss",
                        "detail": "journal_torn_tail",
                        "file": t.file,
                        "at_line": t.at_line,
                        "reason": t.reason,
                        "missing_seqs": t.missing_seqs,
                        "missing_from": t.missing_from,
                        "missing_to": t.missing_to,
                        "rows_withheld": t.rows_withheld,
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
            .any(|ws| self.torn_probe(ws).torn.is_some())
    }

    /// The SAME rule as [`Self::any_torn_tail_among`] (a tear on a workspace we
    /// abandon is history, not a delivery fault) but it hands back the numbers
    /// instead of one boolean: which workspace, how many seqs, which range, and
    /// how many rows the stopped fold cannot reach.
    ///
    /// The bare name read identically for a 16-row hole and a 5,175-row one, so
    /// `/health` could say a journal was torn and never how badly.
    #[must_use]
    pub fn torn_tails_among(&self, hosted: &[&str]) -> Vec<(String, TornTail)> {
        hosted
            .iter()
            .filter_map(|ws| self.torn_probe(ws).torn.map(|t| ((*ws).to_string(), t)))
            .collect()
    }

    /// fsync the active journal (D4.2: before Prompt-intent delivery).
    pub fn sync_journal(&self, workspace: &str) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open(self.journal_file(workspace))?;
        f.sync_data()
    }

    /// Retention drops this workspace's audit sidecar records, oldest first.
    /// A drop with no `seq_from`/`seq_to` is returned with both `None`: the
    /// daemon must not guess a range it never wrote, because a guessed range
    /// would silently excuse a real lost write.
    #[must_use]
    pub fn retention_drops(&self, workspace: &str) -> Vec<RetentionDrop> {
        let Ok(raw) = std::fs::read_to_string(self.audit_file(workspace)) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("journal_rotated"))
            .map(|v| RetentionDrop {
                seq_from: v.get("seq_from").and_then(|s| s.as_u64()),
                seq_to: v.get("seq_to").and_then(|s| s.as_u64()),
                dropped_lines: v.get("dropped_lines").and_then(|s| s.as_u64()).unwrap_or(0),
            })
            .collect()
    }

    /// Append one audit row, and refuse the silent drop on the way (bd-xz7k).
    ///
    /// The audit is the EVIDENCE channel — every tear report, every rotation and
    /// every relayed death is delivered through it — and its own write error
    /// used to be discarded twice over (the `private_dir`, and the `writeln!`
    /// inside an `if let Ok(...)`). So the channel could stop recording in total
    /// silence, which is the same defect bd-5qyt fixed one file over on the
    /// journal, where it mattered less.
    ///
    /// The error is both returned (a caller that can act may) and warned about
    /// here (so the many callers that cannot still say something out loud).
    pub fn append_audit(&self, workspace: &str, entry: serde_json::Value) -> std::io::Result<()> {
        let path = self.audit_file(workspace);
        if let Some(parent) = path.parent()
            && let Err(e) = private_dir(parent)
        {
            eprintln!(
                "doorbell-rt: AUDIT append FAILED on {workspace} (mkdir) — refuse silent \
                 drop (GS-039): {e:#}"
            );
            note_audit_write_failure();
            return Err(e);
        }
        let mut f = match private_append(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "doorbell-rt: AUDIT append FAILED on {workspace} (open) — refuse silent \
                     drop (GS-039): {e:#}"
                );
                note_audit_write_failure();
                return Err(e);
            }
        };
        use std::io::Write;
        // bd-sfvq: ONE write per row, not a `Display` stream. `O_APPEND` makes
        // each `write(2)` land at the current end, so a row serialized through
        // `Display` (which emits the document fragment by fragment) is spliced
        // by any second appender into a line that is neither row — 513 such
        // rows live in one workspace's audit. Serialize first, newline included
        // in the same buffer, so the whole row is one syscall and O_APPEND does
        // the rest.
        let mut line = match serde_json::to_string(&entry) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "doorbell-rt: AUDIT append REFUSED on {workspace} (serialize) — a row \
                     that cannot be serialized is not a row: {e:#}"
                );
                note_audit_write_failure();
                return Err(std::io::Error::other(e));
            }
        };
        line.push('\n');
        if let Err(e) = f.write_all(line.as_bytes()) {
            eprintln!(
                "doorbell-rt: AUDIT append FAILED on {workspace} (write) — refuse silent \
                 drop (GS-039): {e:#}"
            );
            note_audit_write_failure();
            return Err(e);
        }
        Ok(())
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
    /// How many seq values are missing at the tear (0 for an unparseable row).
    /// A bare boolean said only "torn"; the number is what makes it a finding —
    /// 16 missing is a lost append, 5,175 missing is retention with a paper cut.
    pub missing_seqs: u64,
    pub missing_from: Option<u64>,
    pub missing_to: Option<u64>,
    /// Rows the fold did NOT return because it stopped here. The fold stops, so
    /// everything past the tear is unreachable from `doorbell why` — that cost
    /// was previously invisible, not just unreported.
    pub rows_withheld: usize,
    /// A recorded retention drop that accounts for this gap. `Some` means rows
    /// were removed on purpose by `rotate()` and the record says so.
    pub explained_by_drop: Option<String>,
}

impl TornTail {
    /// One line for `doorbell why` — the surface whose whole job is to explain
    /// the journal, and the one an operator reads before `/health`.
    ///
    /// The numbers come from the FIELDS, never from `reason`, and stay greppable
    /// (`missing=`, `range=`, `withheld=`). Rendered out of `reason` they were
    /// prose: carried only by the seq-gap case, absent for an unparseable row,
    /// and silent about `rows_withheld` in every case — so a 16-row hole and the
    /// 5,175-row one in flywheel-chiebukuro read identically. The reason stays
    /// as the explanation, not as the carrier of the size.
    #[must_use]
    pub fn describe(&self) -> String {
        let range = match (self.missing_from, self.missing_to) {
            (Some(lo), Some(hi)) => format!("{lo}..={hi}"),
            _ => "none".to_string(),
        };
        format!(
            "fold stopped at {} line {}: missing={} range={} withheld={} — {}; nothing folds \
             past the tear, so those rows are unreachable from this command too",
            self.file, self.at_line, self.missing_seqs, range, self.rows_withheld, self.reason
        )
    }
}

/// One recorded retention drop: rows `rotate()` removed on purpose.
///
/// The audit row already existed; what it lacked was a LINK to the seq stream,
/// so a designed drop and a lost write both surfaced as "seq gap" and the fold
/// had no way to tell them apart. A drop with no range is recorded as exactly
/// that (`None`), never guessed.
#[derive(Debug, Clone)]
pub struct RetentionDrop {
    pub seq_from: Option<u64>,
    pub seq_to: Option<u64>,
    pub dropped_lines: u64,
}

impl RetentionDrop {
    /// True when this drop accounts for the missing range `(from ..= to)`.
    #[must_use]
    pub fn explains(&self, from: u64, to: u64) -> bool {
        match (self.seq_from, self.seq_to) {
            (Some(lo), Some(hi)) => lo <= from && hi >= to,
            _ => false,
        }
    }
}

/// Rows the fold did not return because it stopped at a tear: the rest of the
/// torn file plus every later file in the chain.
fn rows_withheld_after(chain: &[std::path::PathBuf], torn_file: &str, at_line: usize) -> usize {
    let mut withheld = 0usize;
    let mut reached = false;
    for path in chain {
        if !reached {
            if path.display().to_string() != torn_file {
                continue;
            }
            reached = true;
            // 1-based: the row AT the tear is discarded too (the fold breaks
            // before pushing it), so it counts as withheld history as well.
            withheld += count_lines(path).saturating_sub(at_line - 1);
        } else if let Ok(raw) = std::fs::read_to_string(path) {
            withheld += raw.lines().filter(|l| !l.trim().is_empty()).count();
        }
    }
    withheld
}

/// First and last `seq` in a run of raw journal lines. Used when rotation
/// removes rows: without it the drop record cannot be tied to the gap it made.
fn seq_span(lines: &[&str]) -> (Option<u64>, Option<u64>) {
    let mut lo: Option<u64> = None;
    let mut hi: Option<u64> = None;
    for line in lines {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(s) = v.get("seq").and_then(|s| s.as_u64())
        {
            lo = Some(lo.map_or(s, |x| x.min(s)));
            hi = Some(hi.map_or(s, |x| x.max(s)));
        }
    }
    (lo, hi)
}

/// bd-3uqz item E: how many sequence numbers a journal burned without recording a
/// row for them.
///
/// The cause was fixed in ad128f1 ("a failed journal append no longer burns a
/// seq"), but the condition was discoverable only by a forensic script because
/// health had no field for it. Measured 2026-09-28: flywheel-chiebukuro carried
/// 4939 rows across a 12901 seq span — 61.7% holes — while six other workspaces
/// were at 0.0%.
///
/// This counts a hole as a number inside [lo, hi] with no row behind it, and it
/// deliberately does NOT count a designed retention drop: `rotate` discards a
/// CONTIGUOUS range, so the oldest generation simply starts later and the span
/// stays dense. That is why a tail drop reads 0% here and a burn does not.
///
/// `duplicates` is reported separately rather than folded in. A duplicate write
/// and a burned number look identical if you only count rows against a span, and
/// they are different defects with different fixes: one is a replay, the other is
/// a failed append. Reporting only a percentage would have made them the same
/// event.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SeqHoles {
    /// Rows in the window, including rows that carry no `seq`.
    pub rows: usize,
    /// Rows that carried a `seq`.
    pub seq_rows: usize,
    /// Distinct `seq` values seen.
    pub distinct: usize,
    /// Lowest `seq`, if any row had one.
    pub seq_from: Option<u64>,
    /// Highest `seq`, if any row had one.
    pub seq_to: Option<u64>,
    /// Sequence numbers inside the span with no row behind them.
    pub holes: u64,
    /// `seq_rows - distinct`: the same number recorded twice.
    pub duplicates: usize,
}

impl SeqHoles {
    /// Size of the number range the window covers. Zero when no row had a `seq`.
    pub fn span(&self) -> u64 {
        match (self.seq_from, self.seq_to) {
            (Some(lo), Some(hi)) => hi.saturating_sub(lo).saturating_add(1),
            _ => 0,
        }
    }

    /// Holes as a percentage of the span, to two decimals. `None` rather than
    /// `0.0` when there is no span: a workspace with nothing sequenced is not a
    /// healthy 0%, it is an unmeasured one, and those read identically otherwise.
    pub fn holes_pct(&self) -> Option<f64> {
        let span = self.span();
        if span == 0 {
            return None;
        }
        Some(((self.holes as f64 / span as f64) * 10_000.0).round() / 100.0)
    }
}

/// bd-3uqz item E: the pure arithmetic behind `journal.holes_pct`. Counts DISTINCT
/// seq values rather than rows, so a replayed write cannot inflate the denominator
/// and hide real holes behind it.
pub fn seq_holes(lines: &[&str]) -> SeqHoles {
    let mut out = SeqHoles {
        rows: lines.iter().filter(|l| !l.trim().is_empty()).count(),
        ..SeqHoles::default()
    };
    let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(s) = v.get("seq").and_then(|s| s.as_u64()) else {
            continue;
        };
        out.seq_rows += 1;
        out.seq_from = Some(out.seq_from.map_or(s, |x: u64| x.min(s)));
        out.seq_to = Some(out.seq_to.map_or(s, |x: u64| x.max(s)));
        seen.insert(s);
    }
    out.distinct = seen.len();
    out.duplicates = out.seq_rows - out.distinct;
    out.holes = out.span().saturating_sub(out.distinct as u64);
    out
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

    /// bd-swad: the `why` line renders from the FIELDS, so BOTH tear shapes
    /// carry their numbers.
    ///
    /// The seq gap is the shape with a size; an unparseable row is the shape
    /// without one, and it is the case the old prose line said nothing numeric
    /// about at all. `missing=0 range=none` is the honest reading there — no
    /// SEQ is missing, the row is — and `reason` is what names the fault.
    #[test]
    fn a_tear_line_names_its_size_range_and_withheld_rows() {
        let (st, _dir) = test_store("tear-describe");
        for seq in 1..=3u64 {
            append_seq(&st, "w", seq);
        }
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(st.journal_file("w"))
                .unwrap();
            writeln!(f, "{{not json").unwrap();
        }
        for seq in 4..=5u64 {
            append_seq(&st, "w", seq);
        }

        let t = st
            .fold_workspace("w")
            .torn
            .expect("a corrupt row is a tear, and silence is the bug");
        let line = t.describe();
        assert!(
            line.contains("missing=0") && line.contains("range=none"),
            "an unparseable row has no missing seqs and must say so rather than \
             borrow a number from somewhere: {line}"
        );
        assert_eq!(t.rows_withheld, 3, "the corrupt row and the two after it");
        assert!(
            line.contains("withheld=3"),
            "the count that the old line never carried: {line}"
        );
        assert!(
            line.contains("unparseable row"),
            "and the reason still names the fault: {line}"
        );
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

    /// bd-xz7k: a rotation that FAILS must hide no rows and must not claim the
    /// drop it did not perform.
    ///
    /// The failure is real, not mocked: a directory sits where the rewrite's
    /// sibling temp file goes, so the replace fails with EISDIR. Under the old
    /// code the active journal was rewritten IN PLACE with `let _ = ...`, so the
    /// error vanished, the rows were gone, and the audit row that followed still
    /// reported a clean drop carrying a seq range — a record that reads as
    /// "designed" and would excuse a real gap forever. That is the
    /// flywheel-chiebukuro shape: four generations on disk, 89 gaps, and a sum
    /// of recorded dropped_lines of zero.
    #[test]
    fn a_failed_rotation_hides_no_rows_and_claims_no_range() {
        let (st, dir) = test_store("rot-fail");
        // A production-sized active journal, written directly so no append can
        // trigger maybe_rotate first and rotate it out from under the test.
        let mut body = String::new();
        for seq in 1..=5000u64 {
            body.push_str(&format!(
                "{{\"v\":2,\"seq\":{seq},\"actions\":[],\"executed\":{{}}}}\n"
            ));
        }
        let active = st.journal_file("w");
        std::fs::create_dir_all(active.parent().expect("journals dir")).expect("mkdir journals");
        std::fs::write(&active, &body).expect("oversized active journal");
        let blocked = super::tmp_beside(&active);
        std::fs::create_dir_all(&blocked).expect("block the rewrite");

        st.rotate("w");

        assert_eq!(
            super::count_lines(&active),
            5000,
            "a rotation that could not rewrite the active journal must leave every row in it"
        );
        let audit = std::fs::read_to_string(st.audit_file("w")).expect("audit sidecar");
        assert!(
            audit.contains("journal_rotated"),
            "the attempt is still reported — silence is the bug: {audit}"
        );
        assert!(
            audit.contains("rotate_failed"),
            "and the failure NAMES itself: {audit}"
        );
        assert!(
            !audit.contains("seq_from"),
            "a range may only be recorded for rows we are SURE were removed, or a later \
             gap gets excused by a drop that never happened: {audit}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// bd-xz7k: the audit is the EVIDENCE channel — every tear report is
    /// delivered through it — so its own write failure cannot be discarded.
    /// The old shape swallowed both the mkdir and the writeln inside an
    /// `if let Ok(...)`.
    #[test]
    fn a_failed_audit_append_is_reported_not_swallowed() {
        let (st, dir) = test_store("audit-fail");
        // Block the audit path itself: a directory where the file belongs.
        std::fs::create_dir_all(st.audit_file("w")).expect("block the audit file");
        let result = st.append_audit("w", serde_json::json!({"kind": "probe"}));
        assert!(
            result.is_err(),
            "an audit row that did not land must be an error the caller can see, \
             not a discarded Result: {result:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_files_and_dirs() {
        // Audit P2: journal/audit/wake/unanswered files are created
        // owner-only (0600, dirs 0700) — they carry wake metadata.
        let (st, dir) = test_store("perms");
        append_seq(&st, "w", 1);
        let _ = st.append_audit("w", serde_json::json!({"kind": "x"}));
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

    /// rv23 #3: the `/health` probe reads the tear WITHOUT holding the rows.
    /// Same verdict as the full fold (tear and seq gap), but `rows` is empty,
    /// so a 15 s probe no longer builds one `Value` tree per retained row.
    #[test]
    fn the_torn_probe_agrees_with_the_fold_and_holds_no_rows() {
        let (st, _dir) = test_store("rv23-probe");
        for seq in 1..=50u64 {
            append_seq(&st, "w", seq);
        }
        let clean = st.torn_probe("w");
        assert!(clean.torn.is_none());
        assert_eq!(clean.first_seq, Some(1));
        assert!(clean.rows.is_empty(), "the probe holds no rows");
        assert_eq!(st.fold_workspace("w").rows.len(), 50, "the fold still does");
        append_seq(&st, "w", 60);
        let (probe, fold) = (st.torn_probe("w"), st.fold_workspace("w"));
        let (p, f) = (probe.torn.expect("gap"), fold.torn.expect("gap"));
        assert_eq!((p.at_line, p.missing_seqs), (f.at_line, f.missing_seqs));
        assert!(probe.rows.is_empty());
    }

    /// rv23 #3: and the health entry points use the probe, not the fold.
    #[test]
    fn the_health_tear_probes_do_not_fold_the_rows() {
        let src = include_str!("store.rs");
        for name in ["pub fn torn_tails_among", "pub fn any_torn_tail_among"] {
            let at = src.find(name).expect(name);
            let body = &src[at..at + src[at..].find("\n    }\n").expect("body end")];
            assert!(body.contains("torn_probe("), "{name} must use torn_probe: {body}");
            assert!(!body.contains("fold_workspace("), "{name} folds the rows: {body}");
        }
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

    /// The tear signal said ONE thing: torn. It never said how big, where, or
    /// how much history that cost — so the 16-seq hole in opencode and the
    /// 5,175-seq hole in chiebukuro read identically, and `/health` had one
    /// permanently red boolean with no way to triage it.
    #[test]
    fn a_seq_gap_reports_its_size_its_range_and_what_it_hid() {
        let (st, dir) = test_store("gap-detail");
        for seq in 1..=3u64 {
            append_seq(&st, "w", seq);
        }
        // 4..=19 gone, exactly the shape of the live opencode hole.
        for seq in 20..=21u64 {
            append_seq(&st, "w", seq);
        }
        let fold = st.fold_workspace("w");
        let torn = fold.torn.expect("an unexplained gap is still a tear");
        assert_eq!(torn.missing_seqs, 16, "the size is the finding: {torn:?}");
        assert_eq!(torn.missing_from, Some(4));
        assert_eq!(torn.missing_to, Some(19));
        assert_eq!(
            torn.rows_withheld, 2,
            "the fold stops at the tear, so the 2 rows after it are unreachable — that \
             cost was invisible before: {torn:?}"
        );
        assert!(
            torn.reason.contains("16 seqs missing") && torn.reason.contains("no retention record"),
            "the reason must name the size and say nothing accounts for it: {}",
            torn.reason
        );
        let audit = std::fs::read_to_string(st.audit_file("w")).unwrap();
        assert!(
            audit.contains("\"missing_seqs\":16") && audit.contains("\"rows_withheld\":2"),
            "the report must carry the numbers, not just the word torn: {audit}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// And the other half: a drop the daemon RECORDED is not a tear. Retention
    /// removes rows on purpose; if the record cannot excuse the gap it made,
    /// then every workspace that rotates reports `journal_torn_tail` forever
    /// and the flag stops meaning anything. The fold must also keep going:
    /// stopping threw away the readable tail of the file.
    #[test]
    fn a_recorded_retention_drop_is_not_a_tear() {
        let (st, dir) = test_store("drop-explains");
        for seq in 1..=3u64 {
            append_seq(&st, "w", seq);
        }
        for seq in 20..=21u64 {
            append_seq(&st, "w", seq);
        }
        let _ = st.append_audit(
            "w",
            serde_json::json!({
                "kind": "journal_rotated",
                "dropped_lines": 16,
                "seq_from": 4,
                "seq_to": 19,
            }),
        );
        let fold = st.fold_workspace("w");
        assert!(
            fold.torn.is_none(),
            "a recorded retention drop is not a tear: {:?}",
            fold.torn
        );
        assert_eq!(
            fold.rows.len(),
            5,
            "the fold must carry on past a designed drop instead of throwing the tail away"
        );
        // A record that does NOT cover the range must not excuse it: a guessed
        // range would hide a real lost write.
        let _ = st.append_audit(
            "w",
            serde_json::json!({"kind": "journal_rotated", "dropped_lines": 2, "seq_from": 4, "seq_to": 5}),
        );
        assert!(
            st.retention_drops("w").iter().any(|d| d.explains(4, 19)),
            "the first, covering record still explains it"
        );
        assert!(
            !st.retention_drops("w").iter().any(|d| d.explains(4, 900)),
            "a short record must not excuse a longer gap"
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
#[cfg(test)]
mod audit_append_tests {
    use super::*;

    /// bd-sfvq: an audit row must land WHOLE or not at all.
    ///
    /// The writer opened `O_APPEND` and then wrote the row with
    /// `writeln!(f, "{entry}")`, where `entry` is a `serde_json::Value`. A
    /// `Value`'s `Display` emits the document in fragments (brace, key, value,
    /// comma...), and `write_fmt` turns each fragment into its own `write(2)`.
    /// `O_APPEND` makes each of those small writes land at the current end, so
    /// two appenders interleave FRAGMENTS and produce a line that is neither
    /// row. Measured live on 2026-09-28: 513 such rows in one workspace's
    /// audit, each one a splice of two or three writers' bytes.
    ///
    /// The fix is to serialize to a buffer and hand the file ONE write, which
    /// `O_APPEND` makes atomic against other appenders. Threads stand in for
    /// processes here: each holds its own descriptor, which is the same race.
    /// The rows are fat (4 KiB) on purpose — a thin row fragments into a
    /// handful of writes and the race stops being observable, so a green test
    /// would prove nothing.
    #[test]
    fn concurrent_audit_appends_never_splice_a_row() {
        const WRITERS: usize = 8;
        const PER_WRITER: usize = 12;
        let dir = std::env::temp_dir().join(format!("dbrt-auditappend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileStore::new(&dir);
        // Pre-build the payloads: a Value built per row would put the race
        // behind the allocator as well as the writer.
        let payloads: Vec<Vec<serde_json::Value>> = (0..WRITERS)
            .map(|w| {
                (0..PER_WRITER)
                    .map(|i| {
                        serde_json::json!({
                            "kind": "probe",
                            "writer": w,
                            "seq": i,
                            "filler": "x".repeat(4096),
                        })
                    })
                    .collect()
            })
            .collect();
        let mut handles = Vec::new();
        let mut seen: std::collections::BTreeSet<(Option<u64>, Option<u64>)> =
            std::collections::BTreeSet::new();
        for (w, rows) in payloads.into_iter().enumerate() {
            let dir = dir.clone();
            handles.push(std::thread::spawn(move || {
                let store = FileStore::new(&dir);
                for row in rows {
                    store.append_audit("ws", row).expect("append");
                }
                w
            }));
        }
        for h in handles {
            h.join().expect("writer thread");
        }
        let body = std::fs::read_to_string(store.audit_file("ws")).expect("audit body");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines.len(),
            WRITERS * PER_WRITER,
            "every row is its own line: a short count means rows were lost"
        );
        for (n, line) in lines.iter().enumerate() {
            let parsed: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("row {n} is not one row ({e}): {line:.200}"));
            let obj = parsed.as_object().expect("an audit row is an object");
            assert_eq!(
                obj.len(),
                4,
                "row {n} carries exactly this row's fields, so no other writer's \
                 fragment rode in: {line:.200}"
            );
            // Completeness, NOT order. Which writer's row lands on which line
            // is a race with no contract, so an ordering assertion here would
            // be a test of the scheduler. What must hold is that every
            // (writer, seq) pair appears exactly once: nothing lost, nothing
            // duplicated.
            let pair = (obj["writer"].as_u64(), obj["seq"].as_u64());
            assert!(
                pair.0.is_some() && pair.1.is_some(),
                "row {n} is identifiable: {line:.120}"
            );
            assert!(
                seen.insert(pair),
                "row {n} duplicates an earlier one, so a row was written twice: {line:.120}"
            );
        }
        assert_eq!(
            seen.len(),
            WRITERS * PER_WRITER,
            "every row that was written is in the file exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    fn ws_gen_file(dir: &Path, ws: &str, generation: usize) -> PathBuf {
        dir.join("journals")
            .join(format!("doorbell-journal-{ws}.{generation}.jsonl"))
    }

    /// bd-oa4u: the name parser, on its own, because the DOT cases are the ones
    /// that would regress silently. Only a trailing run of digits is a generation:
    /// `flywheel-toron.dev` and `flywheel-torondev` are both real workspaces in this
    /// state directory, so an unconditional last-dot-strip would merge two distinct
    /// workspaces and hand a dead one's protection to a live one — the same class
    /// of error pointed the other way.
    #[test]
    fn a_journal_name_resolves_to_its_workspace_generation_or_not() {
        for (name, want) in [
            ("doorbell-journal-flywheel.jsonl", Some("flywheel")),
            ("doorbell-journal-flywheel.1.jsonl", Some("flywheel")),
            ("doorbell-journal-flywheel.4.jsonl", Some("flywheel")),
            (
                "doorbell-journal-flywheel-toron.dev.jsonl",
                Some("flywheel-toron.dev"),
            ),
            (
                "doorbell-journal-flywheel-toron.dev.2.jsonl",
                Some("flywheel-toron.dev"),
            ),
            ("doorbell-journal-flywheel.abc.jsonl", Some("flywheel.abc")),
            ("doorbell-journal-flywheel.1x.jsonl", Some("flywheel.1x")),
            // Nothing in front of the dot, or nothing behind it: not a generation,
            // and not a workspace either.
            ("doorbell-journal-.jsonl", None),
            ("doorbell-journal-flywheel..jsonl", None),
            // Not ours.
            ("flywheel.audit.jsonl", None),
            ("doorbell-journal-flywheel.jsonl.bak", None),
        ] {
            assert_eq!(journal_workspace(name), want, "for {name}");
        }
    }

    /// bd-3uqz item D: the `.unanswered` file is what gate P2b reads to decide
    /// whether a crew is INVISIBLE, and it had no test at all. The bead asked for a
    /// row-retention policy; the measurement says retention is already structural,
    /// and this is the fence that proves it rather than a comment that claims it.
    ///
    /// `Hear::heard` and the late-pubkey path both `unanswered.remove(pin)`, and
    /// `persist_hear` REWRITES the whole file from the live set on every sweep. So a
    /// row exists if and only if the pin is unanswered right now. An unanswered pin
    /// from three weeks ago is not "stale data needing a window" — if it were still
    /// unanswered it would be re-written every sweep, and if it is not in the file
    /// it is already answered. There is nothing for an age policy to reclaim, and
    /// building one would add a SECOND mechanism for a job the daemon already does
    /// correctly, in the one file whose loss disarms a gate.
    #[test]
    fn unanswered_retention_is_structural_a_rewrite_drops_the_answered_pins() {
        let dir = std::env::temp_dir().join(format!("unans-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = FileStore::new(&dir);

        // The daemon's first sweep finds three pins it could not wake.
        st.save_unanswered(
            "w",
            &[
                "EmberOtter".into(),
                "TealMarlin".into(),
                "IvoryRidge".into(),
            ],
        );
        // Compared as a SET, not a sequence. `load_unanswered` returns file order
        // and the order is not a contract: `save_unanswered` writes whatever the
        // caller passed, and `Hear::seed_unanswered` feeds a BTreeSet, which
        // normalizes. Asserting an order here would pin a property nothing relies
        // on and would fail on a harmless reordering.
        let mut round_trip = st.load_unanswered("w");
        round_trip.sort();
        assert_eq!(
            round_trip,
            vec!["EmberOtter", "IvoryRidge", "TealMarlin"],
            "a round trip must preserve every pin: this file IS the P2b signal"
        );

        // EmberOtter is answered, so `heard` removed it and the next persist_hear
        // rewrites the file without it. No window, no clock, no policy.
        st.save_unanswered("w", &["TealMarlin".into(), "IvoryRidge".into()]);
        let mut after = st.load_unanswered("w");
        after.sort();
        assert_eq!(
            after,
            vec!["IvoryRidge", "TealMarlin"],
            "a pin that stopped being unanswered must LEAVE, with no age policy involved"
        );

        // And the file keeps existing even with nothing in it. P2b reads this path,
        // so a future "skip writing when the list is empty" optimisation would turn
        // "no invisible pins" into "the file is gone" — indistinguishable from a
        // daemon that never looked. The file must stay, and stay empty.
        st.save_unanswered("w", &[]);
        assert!(
            st.unanswered_file("w").exists(),
            "the file must EXIST even with zero pins: P2b reads it by path, and a \
             missing file is indistinguishable from one that was never written"
        );
        assert!(
            st.load_unanswered("w").is_empty(),
            "and must read as no pins rather than as a phantom row"
        );

        // A workspace never seen reads as empty rather than failing.
        assert!(
            st.load_unanswered("never-seen").is_empty(),
            "an absent file is an empty signal, not an error"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-3uqz item E: `seq_holes` on its own, because every case here encodes a
    /// different real event and a percentage alone is easy to agree with by
    /// accident.
    fn row(seq: Option<u64>) -> String {
        serde_json::json!({ "v": 1, "msg": "m", "seq": seq }).to_string()
    }

    #[test]
    fn a_dense_journal_has_no_holes_and_says_zero_percent() {
        let lines: Vec<String> = (1..=100).map(|s| row(Some(s))).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let h = seq_holes(&refs);
        assert_eq!((h.rows, h.distinct, h.holes), (100, 100, 0), "{h:?}");
        assert_eq!(h.holes_pct(), Some(0.0));
        assert_eq!(h.duplicates, 0);
    }

    /// The measured flywheel-chiebukuro shape was 4939 rows across a 12901 span.
    /// A percentage is only meaningful if the ARITHMETIC is pinned, so this uses
    /// a small exact case and asserts the numbers, not just the shape.
    #[test]
    fn a_burned_range_is_counted_against_the_span() {
        // seq 1..=6 present, 7..=15 burned, 16..=20 present: a 20-number span
        // carrying 11 rows, so 9 holes — 45.0%.
        let mut lines: Vec<String> = (1..=6).map(|s| row(Some(s))).collect();
        lines.extend((16..=20).map(|s| row(Some(s))));
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let h = seq_holes(&refs);
        assert_eq!(h.rows, 11, "{h:?}");
        assert_eq!(h.span(), 20);
        assert_eq!(h.holes, 9);
        assert_eq!(h.holes_pct(), Some(45.0));
    }

    /// A DESIGNED retention drop must not read as a burn. `rotate` discards a
    /// CONTIGUOUS range, so the oldest generation simply starts later and the span
    /// stays dense. If this reported holes, every rotated workspace would look like
    /// flywheel-chiebukuro and the field would be pure noise.
    #[test]
    fn a_contiguous_tail_drop_is_a_drop_not_a_hole() {
        // Rows 1001..=1100 retained after a rotation that dropped 1..=1000.
        let lines: Vec<String> = (1001..=1100).map(|s| row(Some(s))).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let h = seq_holes(&refs);
        assert_eq!(h.holes, 0, "a contiguous drop is not a hole: {h:?}");
        assert_eq!(h.holes_pct(), Some(0.0));
    }

    /// A replayed write and a burned number are DIFFERENT defects. Counting rows
    /// against a span lets a duplicate inflate the denominator and hide real
    /// holes, so distinct values are counted and the repeats are named.
    #[test]
    fn a_duplicated_seq_is_named_and_does_not_hide_holes() {
        let mut lines: Vec<String> = (1..=5).map(|s| row(Some(s))).collect();
        lines.push(row(Some(3))); // replay of 3
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let h = seq_holes(&refs);
        assert_eq!(h.rows, 6);
        assert_eq!(h.seq_rows, 6);
        assert_eq!(h.distinct, 5, "6 rows carry 5 distinct numbers");
        assert_eq!(h.duplicates, 1, "the repeat is named, not absorbed");
        assert_eq!(h.holes, 0, "a replay must not invent a hole");
    }

    /// Rows with no `seq` are excluded from BOTH sides. Counting them in the
    /// denominator would report a hole where the only fault is a missing field.
    #[test]
    fn rows_without_a_seq_do_not_appear_in_either_side_of_the_ratio() {
        let mut lines: Vec<String> = (1..=10).map(|s| row(Some(s))).collect();
        lines.push(row(None));
        lines.push(row(None));
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let h = seq_holes(&refs);
        assert_eq!(h.rows, 12, "both unsequenced rows are still rows");
        assert_eq!(h.seq_rows, 10);
        assert_eq!(h.span(), 10);
        assert_eq!(h.holes, 0);
    }

    /// A window with nothing sequenced is UNMEASURED, not healthy. Returning 0.0
    /// would put a clean bill of health on a journal the scan never read.
    #[test]
    fn an_unsequenced_window_reports_absent_rather_than_zero_percent() {
        let lines: Vec<String> = vec![row(None), row(None)];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let h = seq_holes(&refs);
        assert_eq!(h.holes_pct(), None, "absent, not 0.0: {h:?}");
        assert_eq!(h.span(), 0);
        assert_eq!(seq_holes(&[]).holes_pct(), None);
    }

    /// bd-oa4u: the age prune's LIVENESS GUARD does not see a generation file.
    ///
    /// The guard parses the workspace name with
    /// `strip_prefix("doorbell-journal-")` then `strip_suffix(".jsonl")`. For
    /// `doorbell-journal-flywheel.1.jsonl` that yields the name `flywheel.1`, not
    /// `flywheel`, so `live.contains("flywheel.1")` is false for a workspace that
    /// is hosted RIGHT NOW. A comment above the guard asserts the opposite —
    /// "A generation file is covered by the same workspace name" — and the code
    /// does not do what the comment says.
    ///
    /// The consequence is silent and it eats the thing retention exists to keep:
    /// the daemon passes 7 days in production, so any generation file older than
    /// a week is deleted even for a live crew. `rotate` later shifts `.1 -> .2`
    /// over a hole, which is exactly the shape that made seven workspaces read
    /// "torn" for days while the daemon believed it had kept its history.
    ///
    /// The sibling test above only ever wrote the ACTIVE file, so this
    /// intersection — hosted workspace, generation file, aged out — was never
    /// exercised. A fence in a comment is not a fence.
    #[test]
    fn a_hosted_workspaces_rotation_generations_are_never_age_pruned() {
        let dir = std::env::temp_dir().join(format!("prune-gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("journals")).expect("dirs");
        let store = FileStore::new(&dir);
        let now = 2_000_000_000u64;
        const OLD: u64 = 30 * 24 * 60 * 60;

        // One hosted workspace, its active file and all four retained generations.
        for generation in 0..=super::RETENTION_GENS {
            let path = if generation == 0 {
                ws_file(&dir, "live")
            } else {
                ws_gen_file(&dir, "live", generation)
            };
            std::fs::write(&path, "row\n").expect("write");
            let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(now - OLD);
            std::fs::File::open(&path)
                .and_then(|f| f.set_modified(old))
                .unwrap_or_else(|e| panic!("age {}: {e}", path.display()));
        }
        // A genuinely abandoned workspace, generation file included, as the control.
        std::fs::write(ws_gen_file(&dir, "gone", 1), "row\n").expect("write");
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(now - OLD);
        std::fs::File::open(ws_gen_file(&dir, "gone", 1))
            .and_then(|f| f.set_modified(old))
            .expect("age gone");

        let live = BTreeSet::from(["live".to_string()]);
        let report = store.prune_unhosted_workspace_logs(&live, 7 * 24 * 60 * 60, now);

        for generation in 0..=super::RETENTION_GENS {
            let path = if generation == 0 {
                ws_file(&dir, "live")
            } else {
                ws_gen_file(&dir, "live", generation)
            };
            assert!(
                path.exists(),
                "generation {generation} of a HOSTED workspace was pruned: {report:?}"
            );
        }
        assert!(
            !ws_gen_file(&dir, "gone", 1).exists(),
            "the control must still prune: an unhosted workspace's generation is the thing \
             this policy exists to reclaim"
        );
        assert_eq!(report.journals_removed, 1, "only the control: {report:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
