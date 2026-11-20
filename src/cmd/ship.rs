//! `flywheel ship check` — fail-closed ship gate for one bead.
//!
//! It proves four separate facts:
//! 1. the HEAD commit names the bead;
//! 2. `.beads` has no uncommitted changes;
//! 3. the closed bead row exists in the committed `.beads/issues.jsonl`;
//! 4. `flywheel ledger check` finds a legal verdict for the current SHA.
//!
//! The third check is the one a fresh clone needs: `ledger check` reads the
//! live `br` database, which can be newer than the committed tracker snapshot.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;

#[derive(Subcommand)]
pub(crate) enum ShipCmd {
    /// Verify one bead is committed, its tracker row is committed and closed,
    /// and the current SHA has a legal verdict.
    Check {
        bead_id: String,
        /// Repo directory (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Repo to search for the citing commit when the work lives elsewhere
        /// (symmetric with `br close --repo`); default: the `--dir` repo.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Machine-readable result.
        #[arg(long)]
        json: bool,
    },
    /// Land one bead with one call: gate, close, sync, release, commit THIS
    /// bead's tracker rows, then ship check. Idempotent; stops at the first
    /// failing step and prints the exact resume command.
    Land {
        bead_id: String,
        /// The commit whose message cites the bead (40-hex).
        #[arg(long)]
        sha: String,
        /// Proof receipt (machine-readable; becomes the gate `tail=`).
        #[arg(long)]
        receipt: PathBuf,
        /// Toron project for the reservation release (default: the workspace
        /// project this repo sits in; a repo outside the workspace root must
        /// name it).
        #[arg(long)]
        project: Option<String>,
        /// Agent pin releasing the reservations (default: AGENT_NAME).
        #[arg(long = "as")]
        as_pin: Option<String>,
        /// Tracker commit message (default: `chore(beads): close <bead>`).
        #[arg(long)]
        message: Option<String>,
        /// Repo directory (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Repo holding the citing commit and the receipt's specs when the
        /// work lives elsewhere (symmetric with `br close --repo`).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Machine-readable result.
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn cmd_ship(cfg: &Config, cmd: &ShipCmd) -> Result<()> {
    match cmd {
        ShipCmd::Check {
            bead_id,
            dir,
            repo,
            json,
        } => ship_check(bead_id, dir.as_deref(), repo.as_deref(), *json),
        ShipCmd::Land {
            bead_id,
            sha,
            receipt,
            project,
            as_pin,
            message,
            dir,
            repo,
            json,
        } => ship_land(LandArgs {
            cfg,
            bead: bead_id,
            sha,
            receipt,
            project: project.as_deref(),
            as_pin: as_pin.as_deref(),
            message: message.as_deref(),
            dir: dir.as_deref(),
            repo: repo.as_deref(),
            json: *json,
        }),
    }
}

/// bd-bi0mv: typed-receipt existence predicate. A commit exists when git can
/// resolve it in the named repo; pure from the caller's perspective (no state
/// mutation), so crew_receipt validation stays effect-law clean.
pub(crate) fn sha_exists_in(root: &Path, sha: &str) -> bool {
    git(root, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
}

/// bd-bi0mv: typed-receipt ancestry predicate — does `descendant` contain
/// `ancestor` in the named repo?
pub(crate) fn is_ancestor_in(root: &Path, ancestor: &str, descendant: &str) -> bool {
    git(root, &["merge-base", "--is-ancestor", ancestor, descendant]).is_ok()
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| anyhow!("git not found: {err}"))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// bd-wcjsu: the bead is bound to the revision the GATE recorded (its leading
/// `sha=`), never to the newest commit that merely mentions the id — a later
/// unrelated commit citing a closed bead used to flip its verdict. The bound
/// revision must be an ancestor of HEAD.
///
/// bd-9zqjo (b): `search` is the repo the citing commit may live in (a flywheel
/// bead whose work landed in toron). The gate rows stay in `cwd`; only git is
/// asked about `search`, and the ancestry question is asked in that repo.
fn head_names_bead(cwd: &Path, search: &Path, bead: &str) -> Result<bool> {
    // No commit carries the id at all: not named (the check reports the
    // missing row separately), never a hard error here.
    let Ok(sha) = crate::verdict::derive_head_sha_in(cwd, search, bead) else {
        return Ok(false);
    };
    let out = Command::new("git")
        .args(["merge-base", "--is-ancestor", &sha, "HEAD"])
        .current_dir(search)
        .output()
        .map_err(|err| anyhow!("git not found: {err}"))?;
    Ok(out.status.success())
}

/// The tracker rows that belong to ONE bead, in HEAD and in the worktree.
/// git status is repo-wide, so another lane's uncommitted row must never fail
/// this bead's ship check (bd-ps6wc).
fn line_is_bead(line: &str, key: &str, bead: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| v.get(key).and_then(Value::as_str).map(|id| id == bead))
        .unwrap_or(false)
}

fn bead_rows(file: &Path, bead: &str, key: &str) -> Result<Vec<String>> {
    let body = std::fs::read_to_string(file).unwrap_or_default();
    Ok(body
        .lines()
        .filter(|line| line_is_bead(line, key, bead))
        .map(str::to_string)
        .collect())
}

fn row_is_closed(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| {
            v.get("status")
                .and_then(Value::as_str)
                .map(|s| s == "closed")
        })
        .unwrap_or(false)
}

/// True when THIS bead's tracker rows are already committed: the worktree rows
/// for it equal HEAD's rows for it, and the issue row is CLOSED. Other lanes'
/// dirty rows are ignored.
fn bead_rows_committed(dir: &Path, bead: &str) -> Result<bool> {
    for (path, key, require_closed) in [
        (".beads/issues.jsonl", "id", true),
        (".beads/gates.jsonl", "issue_id", false),
    ] {
        let head = git(dir, &["show", &format!("HEAD:{path}")]).unwrap_or_default();
        let head_rows: Vec<String> = head
            .lines()
            .filter(|line| line_is_bead(line, key, bead))
            .map(str::to_string)
            .collect();
        let work_rows = bead_rows(&dir.join(path), bead, key)?;
        if head_rows != work_rows {
            return Ok(false);
        }
        if require_closed && !work_rows.iter().any(|line| row_is_closed(line)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// HEAD's tracker files with ONLY this bead's rows swapped for the worktree
/// versions (bd-kz3sc rework). A neighbour's dirty rows are never in these
/// blobs; the commit verb stages the exact bytes into its private index.
fn tracker_blobs_for_bead(dir: &Path, bead: &str, parent: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let mut blobs = Vec::new();
    for (path, key) in [
        (".beads/issues.jsonl", "id"),
        (".beads/gates.jsonl", "issue_id"),
    ] {
        let head = git(dir, &["show", &format!("{parent}:{path}")]).unwrap_or_default();
        let work = std::fs::read_to_string(dir.join(path)).unwrap_or_default();
        let work_rows: Vec<&str> = work
            .lines()
            .filter(|line| line_is_bead(line, key, bead))
            .collect();
        let mut merged: Vec<String> = Vec::new();
        let mut inserted = false;
        for line in head.lines() {
            if line_is_bead(line, key, bead) {
                if !inserted {
                    merged.extend(work_rows.iter().map(|row| (*row).to_string()));
                    inserted = true;
                }
            } else {
                merged.push(line.to_string());
            }
        }
        if !inserted {
            merged.extend(work_rows.iter().map(|row| (*row).to_string()));
        }
        let mut body = merged.join("\n");
        if head.ends_with('\n') || !work_rows.is_empty() {
            body.push('\n');
        }
        blobs.push((path.to_string(), body.into_bytes()));
    }
    Ok(blobs)
}

/// Read the tracker snapshot from HEAD, not the live database. A bead that is
/// closed only in the working tree or in `beads.db` is not shipped.
fn head_has_closed_bead(dir: &Path, bead: &str) -> Result<(bool, bool)> {
    let body = git(dir, &["show", "HEAD:.beads/issues.jsonl"])?;
    let mut found = false;
    for line in body.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_str) == Some(bead) {
            found = true;
            if value.get("status").and_then(Value::as_str) == Some("closed") {
                return Ok((true, true));
            }
            break;
        }
    }
    Ok((found, false))
}

fn ledger_check(dir: &Path, bead: &str, repo: Option<&Path>) -> Result<(bool, String, String)> {
    let exe = std::env::current_exe().map_err(|err| anyhow!("current exe: {err}"))?;
    let mut cmd = Command::new(exe);
    cmd.args(["ledger", "check", bead, "--dir"]).arg(dir);
    if let Some(repo) = repo {
        cmd.arg("--repo").arg(repo);
    }
    let out = cmd
        .output()
        .map_err(|err| anyhow!("could not run ledger check: {err}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn ship_check(
    bead: &str,
    dir: Option<&Path>,
    repo: Option<&Path>,
    json_output: bool,
) -> Result<()> {
    if bead.trim().is_empty() {
        bail!("empty bead id");
    }
    let dir = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().map_err(|err| anyhow!("current directory: {err}"))?,
    };
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }
    git(&dir, &["rev-parse", "--git-dir"])?;

    let search = repo.unwrap_or(&dir);
    let commit_named = head_names_bead(&dir, search, bead)?;
    let beads_clean = bead_rows_committed(&dir, bead)?;
    let (head_found, head_closed) = head_has_closed_bead(&dir, bead)?;
    let (ledger_ok, ledger_stdout, ledger_stderr) = ledger_check(&dir, bead, repo)?;
    let ok = commit_named && beads_clean && head_found && head_closed && ledger_ok;

    if json_output {
        print_json(&json!({
            "schema": "flywheel-ship-check.v1",
            "bead": bead,
            "dir": dir.display().to_string(),
            "ok": ok,
            "commit_named": commit_named,
            "beads_row_committed": beads_clean,
            "head_row_found": head_found,
            "head_row_closed": head_closed,
            "ledger_legal": ledger_ok,
            "ledger_stdout": ledger_stdout,
            "ledger_stderr": ledger_stderr,
        }));
    } else {
        println!(
            "HEAD names {bead}:      {}",
            if commit_named { "yes" } else { "NO" }
        );
        println!(
            ".beads row committed:   {}",
            if beads_clean { "yes" } else { "NO" }
        );
        println!(
            "HEAD tracker row:        {}",
            if head_closed {
                "closed"
            } else if head_found {
                "found, not closed"
            } else {
                "MISSING"
            }
        );
        print!(
            "ledger check:            {}",
            if ledger_ok { "legal" } else { "NO" }
        );
        if !ledger_stdout.trim().is_empty() {
            print!("\n{}", ledger_stdout.trim_end());
        }
        println!();
        if !ledger_ok && !ledger_stderr.trim().is_empty() {
            eprintln!("{}", ledger_stderr.trim_end());
        }
    }

    if !ok {
        if !beads_clean {
            eprintln!("ship: this bead's tracker rows are not committed yet");
        }
        if !head_closed {
            eprintln!("ship: {bead} is not closed in HEAD:.beads/issues.jsonl");
        }
        if !ledger_ok {
            eprintln!("ship: no legal ledger verdict for HEAD");
        }
        bail!("ship check failed for {bead}");
    }
    Ok(())
}

/// One landing's arguments, after defaults resolve.
struct LandPlan {
    bead: String,
    sha: String,
    receipt: PathBuf,
    project: String,
    pin: String,
    message: String,
    repo: Option<PathBuf>,
}

fn run_tool(dir: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|err| anyhow!("{program} not found: {err}"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn resume_line(plan: &LandPlan) -> String {
    let mut line = format!(
        "flywheel ship land {} --sha {} --receipt {} --project {} --as {}",
        plan.bead,
        plan.sha,
        plan.receipt.display(),
        plan.project,
        plan.pin
    );
    if let Some(repo) = &plan.repo {
        line.push_str(&format!(" --repo {}", repo.display()));
    }
    line
}

fn land_step<T>(plan: &LandPlan, name: &str, step: impl FnOnce() -> Result<T>) -> Result<T> {
    step().map_err(|err| {
        anyhow!(
            "ship land: step {name} failed: {err:#}\nresume: {}",
            resume_line(plan)
        )
    })
}

/// True when a PASS gate row already binds this SHA (the gate step is done).
fn gate_bound(dir: &Path, plan: &LandPlan) -> Result<bool> {
    let out = run_tool(dir, "br", &["gate", "list", &plan.bead, "--json"]).unwrap_or_default();
    let passed = out.contains("\"passed\":true") || out.contains("\"passed\": true");
    Ok(passed && out.contains(&format!("sha={}", plan.sha)))
}

fn bead_is_closed(dir: &Path, plan: &LandPlan) -> Result<bool> {
    let out = run_tool(dir, "br", &["show", &plan.bead, "--json"])?;
    // bd-pgfqg: a SUBSTRING match said "closed" for an epic whose payload
    // embeds a closed child, so land skipped the close and check then failed.
    // Ask the field, not the text.
    Ok(status_of(&out).as_deref() == Some("closed"))
}

/// The `status` field of a `br show --json` payload, when it parses.
fn status_of(br_json: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(br_json).ok()?;
    doc.get("status")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The gate names `br` says are legal for this bead, from its own refusal text.
///
/// bd-42vhw: ship land hardcoded `unit-test-verified`, so a bead whose policy
/// names `command-verified` needed a manual gate report before a resume worked.
/// br already states the set; read it instead of keeping a second copy.
fn legal_gates(dir: &Path, bead: &str) -> Result<Vec<String>> {
    let out = Command::new("br")
        .current_dir(dir)
        .args(["close", bead, "--dry-run"])
        .output()
        .map_err(|err| anyhow!("br not found: {err}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(parse_legal_gates(&text))
}

/// Pull the `Legal gate names for this bead: a | b | c.` sentence out of a
/// `br close --dry-run` refusal.
fn parse_legal_gates(text: &str) -> Vec<String> {
    let Some((_, rest)) = text.split_once("Legal gate names for this bead: ") else {
        return Vec::new();
    };
    rest.split('.')
        .next()
        .unwrap_or("")
        .split('|')
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Which gate to record for a bead.
///
/// br prints the "Legal gate names" sentence ONLY when a policy narrows the
/// transition, so an EMPTY set means "no restriction": keep the default this
/// verb always recorded. When br does name a set, record one of those names and
/// never an invented one.
fn gate_for(legal: &[String]) -> String {
    if legal.is_empty() {
        return DEFAULT_GATE.to_string();
    }
    pick_gate(legal).unwrap_or_else(|| DEFAULT_GATE.to_string())
}

/// The gate ship land records when the bead's policy names no set.
const DEFAULT_GATE: &str = "unit-test-verified";

/// Which named gate to record: a command proof is what ship land carries, so
/// prefer `command-verified`; otherwise the unit-test name; otherwise the first
/// name the bead allows. Never a name br did not list.
fn pick_gate(legal: &[String]) -> Option<String> {
    for want in ["command-verified", "unit-test-verified"] {
        if legal.iter().any(|name| name == want) {
            return Some(want.to_string());
        }
    }
    legal.first().cloned()
}

pub(crate) struct LandArgs<'a> {
    pub cfg: &'a Config,
    pub bead: &'a str,
    pub sha: &'a str,
    pub receipt: &'a Path,
    pub project: Option<&'a str>,
    pub as_pin: Option<&'a str>,
    pub message: Option<&'a str>,
    pub dir: Option<&'a Path>,
    pub repo: Option<&'a Path>,
    pub json: bool,
}

/// The toron project `ship land` releases reservations in.
///
/// The reservation release is operator-signed and scoped to one slug, so a
/// wrong slug releases the wrong leases. This is therefore the ONE
/// project-resolution rule every other verb uses
/// ([`crate::cron::detect_project_in`]) and never the repo directory's name: an
/// explicit `--project` wins, a repo under the workspace root resolves to its
/// project, and anything else refuses naming the flag.
fn land_project(cfg: &Config, dir: &Path, explicit: Option<&str>) -> Result<String> {
    crate::cron::detect_project_in(cfg, dir, explicit).ok_or_else(|| {
        anyhow!(
            "ship land: no toron project for {} — it is not a project under the workspace \
             root {}; pass --project <slug>",
            dir.display(),
            cfg.workspace_root().display()
        )
    })
}

/// May this receipt land a bead?
///
/// A green proof that ran no mutation is not a proof of the behaviours the
/// landing touches (bd-m5afp): the receipt must carry one machine-readable row
/// per mutation that ran, every row must have produced the verdict it declared
/// (RED for a live guard, GREEN for a guard believed dead), and every row's
/// `<spec>#<id>` must still resolve in `scripts/mutations/<spec>` so a receipt
/// cannot cite a mutation the tree no longer has.
/// Does any mutation spec target this production file? Re-derived at landing
/// time from the specs themselves, so a receipt cannot claim "nothing to
/// re-prove" about a file a spec actually covers.
fn spec_targets_include(root: &Path, path: &str) -> bool {
    let harness = root.join("scripts/verify-mutations.sh");
    if !harness.is_file() {
        // No harness to re-derive with: assume the path IS covered, so a
        // missing tool can only make a landing stricter, never skip a witness.
        return true;
    }
    let Ok(entries) = std::fs::read_dir(root.join("scripts/mutations")) else {
        return false;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let spec = entry.path();
        if spec.extension().and_then(|e| e.to_str()) != Some("sh") {
            continue;
        }
        let Ok(out) = Command::new("bash")
            .arg("scripts/verify-mutations.sh")
            .arg("--list")
            .arg(&spec)
            .current_dir(root)
            .output()
        else {
            continue;
        };
        for row in String::from_utf8_lossy(&out.stdout).lines() {
            if row.rsplit('\t').next() == Some(path) {
                return true;
            }
        }
    }
    false
}

/// Is `rev` a commit in this repo?
fn base_is_a_commit(root: &Path, rev: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "-e", &format!("{rev}^{{commit}}")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The sha a green receipt proved, when it carries one.
fn receipt_proven_sha(receipt: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(receipt).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    doc.get("sha").and_then(Value::as_str).map(str::to_string)
}

/// Is `ancestor` contained in `descendant`? The one git question the
/// descendant rule asks (bd-0uzrq.22): a proof of a later tree also proves the
/// commits it contains.
fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(dir)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The gate note for one landing (bd-0uzrq.22). It LEADS with the citing sha —
/// `note_binding` reads that token, and `ledger check` keys the whole verdict on
/// it — then records the sha the receipt actually proved, so a reader can see
/// both halves of a descendant landing.
fn gate_note(sha: &str, proved: &str, cmd: &str, receipt: &Path) -> String {
    format!(
        "sha={sha} proved={proved} cmd={cmd} exit=0 tail={} receipt={}",
        receipt.display(),
        receipt.display()
    )
}

/// The cache key for one spec, recomputed from the PROVEN revision's own
/// cache script so the working tree cannot vouch for a cache entry (bd-jp9z1.2).
fn cache_key_at(root: &Path, rev: &str, spec: &str, driver: &str) -> Result<String> {
    let script = git(root, &["show", &format!("{rev}:scripts/mutation-cache.py")])
        .map_err(|e| anyhow!("cannot read scripts/mutation-cache.py at {rev}: {e}"))?;
    let mut child = Command::new("python3")
        .arg("-")
        .args([
            "key",
            "--proven",
            rev,
            "--repo",
            &root.display().to_string(),
            "--spec",
            spec,
            "--driver",
            driver,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("python3 is required to recompute a cached key: {e}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(script.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "cannot recompute the cache key for {spec} at {rev}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn receipt_is_landable(root: &Path, receipt: &Path, sha: &str) -> Result<()> {
    let raw = std::fs::read_to_string(receipt)
        .map_err(|e| anyhow!("proof receipt unreadable: {}: {e}", receipt.display()))?;
    let doc: Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow!("proof receipt is not JSON: {}: {e}", receipt.display()))?;
    if doc.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!(
            "proof receipt is not green: {} — fix the failing step ({}), re-prove, then land",
            receipt.display(),
            doc.get("failed_step")
                .and_then(Value::as_str)
                .unwrap_or("?")
        );
    }
    // The receipt proves ONE tree. A green receipt for another commit (or for a
    // dirty tree, a temporary commit nobody can check out) says nothing about
    // the sha being landed.
    let proved = doc.get("sha").and_then(Value::as_str).unwrap_or("");
    // bd-0uzrq.22: one proof lands every bead its tree contains. The citing sha
    // (the commit that names the bead, which `br close --commit-sha` validates)
    // may be an ANCESTOR of the proven sha: a later proof covers it. A receipt
    // that does not contain the citing commit still proves a different tree and
    // is refused.
    if proved != sha && !is_ancestor(root, sha, proved) {
        bail!(
            "proof receipt proves {proved}, which does not contain {sha}: {} — re-prove at or after {sha} \
             (`scripts/clean-proof.sh --sha {sha} --mutations changed`)",
            receipt.display()
        );
    }
    if doc.get("mode").and_then(Value::as_str) == Some("dirty") {
        bail!(
            "proof receipt is for a dirty tree, not a commit: {} — land only a --sha proof",
            receipt.display()
        );
    }
    let rows_opt = doc.get("mutations").and_then(Value::as_array);
    if rows_opt.is_none_or(Vec::is_empty) {
        // An empty set lands only when the diff provably touched no spec
        // target: changed mode, a base that resolves, and a changed-path list
        // re-derived here rather than trusted.
        let mode = doc.get("mutations_mode").and_then(Value::as_str);
        let base = doc
            .get("mutations_base")
            .and_then(Value::as_str)
            .filter(|b| !b.is_empty());
        let changed: Vec<&str> = doc
            .get("changed_paths")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let (Some("changed"), Some(base)) = (mode, base) else {
            bail!(
                "proof receipt carries no mutation run: {} — a landing needs witnesses \
                 (`--mutations changed` with a [mutations] base, or `--mutations full`)",
                receipt.display()
            );
        };
        if !base_is_a_commit(root, base) {
            bail!("proof receipt cites mutations_base {base}, which is not a commit here");
        }
        if let Some(path) = changed.iter().find(|p| spec_targets_include(root, p)) {
            bail!(
                "proof receipt ran no mutation but {path} is a spec target — re-prove with \
                 `--mutations changed` (its base is too new) or `--mutations full`"
            );
        }
        return Ok(());
    }
    let Some(rows) = rows_opt else {
        return Ok(());
    };
    for row in rows {
        if let Some(text) = row.as_str() {
            bail!("proof receipt has a legacy mutation row ({text}); re-prove with --mutations");
        }
        let spec = row.get("spec").and_then(Value::as_str).unwrap_or("");
        let id = row.get("id").and_then(Value::as_str).unwrap_or("");
        let verdict = row.get("verdict").and_then(Value::as_str).unwrap_or("");
        let expect_green = row
            .get("expect_green")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // A dead-guard mutation is accounted for by DEAD-CONFIRMED (its guard
        // cannot change behaviour); a live-guard mutation by RED.
        let want = if expect_green {
            "DEAD-CONFIRMED"
        } else {
            "RED"
        };
        if verdict != want {
            bail!("proof receipt mutation {spec}#{id} proved {verdict}, expected {want}");
        }
        // bd-jp9z1.2: a cached row is honest only while the key still describes
        // the proven tree and its witness is still green. Recomputation runs
        // the cache script FROM the proven revision, so an edit to the working
        // copy cannot vouch for itself.
        if row.get("cached").is_some() && spec != "ad-hoc" {
            let key = row.get("key").and_then(Value::as_str).unwrap_or("");
            if key.is_empty() {
                bail!("cached mutation {spec}#{id} carries no key");
            }
            let driver = doc.get("driver_sha").and_then(Value::as_str).unwrap_or("");
            if driver.is_empty() {
                bail!("cached mutation {spec}#{id} but the receipt names no driver_sha — re-prove");
            }
            let witness = row.get("cached").and_then(Value::as_str).unwrap_or("");
            let witness_green = std::fs::read_to_string(witness)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .is_some_and(|d| d.get("ok").and_then(Value::as_bool) == Some(true));
            if !witness_green {
                bail!(
                    "cached mutation {spec}#{id} cites witness {witness}, which is missing or not green"
                );
            }
            let recomputed = cache_key_at(root, proved, spec, driver)?;
            if recomputed != key {
                bail!(
                    "cached mutation {spec}#{id} key is stale at {proved} ({key} != {recomputed}) — re-prove this spec"
                );
            }
        }
        if spec == "ad-hoc" {
            continue;
        }
        // bd-pgzxn: the receipt witnesses the PROVEN revision, so the spec must
        // be read there. A later relabel in the worktree cannot invalidate a
        // receipt that legitimately cites the old id.
        let at = format!("{proved}:scripts/mutations/{spec}");
        let text = git(root, &["show", &at]).map_err(|e| {
            anyhow!("proof receipt cites {spec}, which does not exist at {proved}: {e}")
        })?;
        let needle = format!("mutation \"{id} ");
        if !text.contains(&needle) {
            bail!("proof receipt cites {spec}#{id}, which the spec no longer declares");
        }
    }
    Ok(())
}

/// bd-t5f0v: the post-landing nudge. Advisory only — an unanswered prompt is a
/// clean skip, never a failed land or a blocked brief close.
pub(crate) fn lesson_advisory(bead: &str) -> String {
    format!(
        "lesson recorded or clean skip? If a root cause was confirmed, record-lesson one \
         \"X breaks when Y; do Z\" line now ({bead}); a clean skip is a valid answer"
    )
}

pub(crate) fn ship_land(args: LandArgs<'_>) -> Result<()> {
    let LandArgs {
        cfg,
        bead,
        sha,
        receipt,
        project,
        as_pin,
        message,
        dir,
        repo,
        json,
    } = args;
    if bead.trim().is_empty() {
        bail!("empty bead id");
    }
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("--sha must be the bare 40-hex commit sha (got {sha:?})");
    }
    if !receipt.is_file() {
        bail!(
            "proof receipt missing: {} (it becomes the gate tail=)",
            receipt.display()
        );
    }
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().map_err(|err| anyhow!("current directory: {err}"))?,
    };
    // The proof and the citing commit belong to the WORK repo when one is
    // named; the tracker rows, the gate and the commit stay in `dir`.
    let proof_root = repo.unwrap_or(&dir);
    receipt_is_landable(proof_root, receipt, sha)?;
    git(&dir, &["rev-parse", "--git-dir"])?;
    let cited = git(proof_root, &["log", "--format=%B", "-n", "1", sha])?;
    if !cited.contains(bead) {
        bail!("commit {sha} does not cite {bead}; land only ships a commit that names the bead");
    }
    let project = land_project(cfg, &dir, project)?;
    let pin = as_pin
        .map(str::to_string)
        .or_else(|| {
            std::env::var("AGENT_NAME")
                .ok()
                .filter(|p| !p.trim().is_empty())
        })
        .ok_or_else(|| {
            anyhow!("ship land needs --as <pin> (or AGENT_NAME) to release reservations")
        })?;
    let plan = LandPlan {
        bead: bead.to_string(),
        sha: sha.to_string(),
        receipt: receipt.to_path_buf(),
        project,
        pin,
        message: message
            .map(str::to_string)
            .unwrap_or_else(|| format!("chore(beads): close {bead}")),
        repo: repo.map(Path::to_path_buf),
    };

    // 1. gate: reuse the bead's verify command as the command proof.
    if gate_bound(&dir, &plan)? {
        eprintln!("ship land: gate already bound to {sha}");
    } else {
        land_step(&plan, "gate", || {
            let verify = run_tool(&dir, "br", &["show", &plan.bead, "--json"])?;
            let cmd = verify
                .split("\"verify\"")
                .nth(1)
                .and_then(|rest| rest.split_once('"'))
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(value, _)| value)
                .unwrap_or("verify")
                .replace(' ', "_");
            // bd-42vhw: record a gate the bead's own policy allows. An EMPTY
            // set is not a refusal - br prints the sentence only when a policy
            // narrows the transition - so it keeps the default gate.
            let legal = legal_gates(&dir, &plan.bead)?;
            let gate = gate_for(&legal);
            let proved = receipt_proven_sha(&plan.receipt).unwrap_or_else(|| plan.sha.clone());
            let note = gate_note(&plan.sha, &proved, &cmd, &plan.receipt);
            run_tool(
                &dir,
                "br",
                &[
                    "gate",
                    "report",
                    &plan.bead,
                    "--gate",
                    &gate,
                    "--provider",
                    &plan.pin,
                    "--status",
                    "pass",
                    "--to",
                    "closed",
                    "--note",
                    &note,
                ],
            )?;
            Ok(())
        })?;
    }

    // 2. close.
    if bead_is_closed(&dir, &plan)? {
        eprintln!("ship land: {} already closed", plan.bead);
    } else {
        land_step(&plan, "close", || {
            let mut argv: Vec<String> = vec![
                "close".into(),
                plan.bead.clone(),
                "--commit-sha".into(),
                plan.sha.clone(),
            ];
            if let Some(repo) = repo {
                argv.push("--repo".into());
                argv.push(repo.display().to_string());
            }
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            run_tool(&dir, "br", &argv)?;
            Ok(())
        })?;
    }

    // 3. sync the tracker database to JSONL.
    land_step(&plan, "sync", || {
        run_tool(&dir, "br", &["sync", "--flush-only"])?;
        Ok(())
    })?;

    // 4. release this bead's reservations (idempotent).
    land_step(&plan, "release", || {
        run_tool(
            &dir,
            "toron",
            &[
                "reserve",
                "release-by-reason",
                &plan.bead,
                "--project",
                &plan.project,
                "--as",
                &plan.pin,
                "--json",
            ],
        )?;
        Ok(())
    })?;

    if bead_rows_committed(&dir, &plan.bead)? {
        eprintln!("ship land: tracker rows already committed");
    } else {
        land_step(&plan, "tracker-commit", || {
            crate::cmd::commit::commit_blobs(
                &dir,
                &[
                    ".beads/issues.jsonl".to_string(),
                    ".beads/gates.jsonl".to_string(),
                ],
                &plan.message,
                |parent| tracker_blobs_for_bead(&dir, &plan.bead, parent),
            )
            .map(|_| ())
        })?;
    }

    // 6. the gate, last.
    land_step(&plan, "check", || ship_check(bead, Some(&dir), repo, json))?;
    if json {
        print_json(&json!({
            "schema": "flywheel-ship-land.v1",
            "bead": bead,
            "sha": sha,
            "ok": true,
        }));
    } else {
        println!("ship land: {bead} landed at {sha}");
        // bd-t5f0v: a bug bead's confirmed root cause is a lesson candidate.
        // The prompt is advisory — no answer, no gate, no failure.
        let shown = run_tool(&dir, "br", &["show", bead, "--json"]).unwrap_or_default();
        let is_bug = serde_json::from_str::<Value>(&shown)
            .ok()
            .and_then(|v| {
                v.get("issue_type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            == Some("bug");
        if is_bug {
            println!("ship land: {}", lesson_advisory(bead));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        gate_for, head_has_closed_bead, head_names_bead, land_project, parse_legal_gates,
        pick_gate, receipt_is_landable, status_of,
    };
    use crate::config::Config;
    use std::process::Command;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(dir.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        dir
    }

    /// bd-42vhw: the gate name comes from the bead's OWN legal set, which br
    /// states in its refusal, not from a hardcoded preference.
    #[test]
    fn the_legal_gate_set_is_read_from_br_and_respected() {
        let refusal = r#"close --dry-run bd-x (open -> closed): would refuse
  missing: transition 'open' -> 'closed' is fail-closed for bd-x: no legal PASS
gate row is recorded. Legal gate names for this bead: command-verified |
live-verified. Record one BEFORE closing."#;
        assert_eq!(
            parse_legal_gates(refusal),
            vec!["command-verified".to_string(), "live-verified".to_string()],
            "the legal set is br's sentence, parsed"
        );
        assert!(parse_legal_gates("no refusal text here").is_empty());

        // A bead whose only legal gate is command-verified gets that name.
        assert_eq!(
            pick_gate(&["command-verified".to_string()]),
            Some("command-verified".to_string())
        );
        assert_eq!(
            pick_gate(&["live-verified".to_string()]),
            Some("live-verified".to_string()),
            "never a name br did not list"
        );
        assert_eq!(pick_gate(&[]), None);

        // bd-42vhw correction: br names a set only when a policy RESTRICTS the
        // transition, so an empty set is "no restriction" and keeps the
        // default gate rather than refusing the land.
        assert_eq!(gate_for(&[]), "unit-test-verified");
        assert_eq!(
            gate_for(&["command-verified".to_string()]),
            "command-verified",
            "a named set still wins"
        );
    }

    /// bd-pgfqg: an epic whose payload embeds a CLOSED CHILD is still open —
    /// the old substring match read the child's status and skipped the close.
    #[test]
    fn closed_is_the_beads_own_status_not_a_child_field() {
        let epic =
            r#"{"id":"bd-epic","status":"open","children":[{"id":"bd-kid","status":"closed"}]}"#;
        assert_eq!(status_of(epic).as_deref(), Some("open"));
        assert!(
            !(status_of(epic).as_deref() == Some("closed")),
            "a closed child must not mark its parent closed"
        );
        let closed = r#"{"id":"bd-x","status":"closed"}"#;
        assert_eq!(status_of(closed).as_deref(), Some("closed"));
        assert_eq!(status_of("not json"), None);
    }

    /// bd-ni0ek: the release project comes from the ONE resolver, never the
    /// repo directory's name. A repo whose directory name is not its toron slug
    /// refuses without `--project` (a guessed slug releases the wrong leases),
    /// and an explicit slug wins over any directory name.
    #[test]
    fn land_refuses_a_repo_whose_dir_is_not_its_toron_slug() {
        let workspace = tempfile::tempdir().expect("workspace root");
        let cfg = Config {
            workspace_root: Some(workspace.path().display().to_string()),
            ..Config::default()
        };

        let outside = tempfile::tempdir().expect("repo outside the workspace");
        let err = land_project(&cfg, outside.path(), None)
            .expect_err("a repo outside the workspace root must refuse");
        let message = format!("{err:#}");
        assert!(
            message.contains("--project"),
            "the refusal names the flag: {message}"
        );
        assert_eq!(
            land_project(&cfg, outside.path(), Some("real-slug")).expect("explicit slug wins"),
            "real-slug",
            "an explicit slug never falls back to the directory name"
        );

        let inside = workspace.path().join("some-slug").join("crates").join("x");
        std::fs::create_dir_all(&inside).expect("project subdir");
        assert_eq!(
            land_project(&cfg, &inside, None).expect("workspace child resolves"),
            "some-slug",
            "a repo under the workspace root resolves to its project"
        );
    }

    /// bd-m5afp: a receipt lands a bead only when it carries a real mutation
    /// run — every row produced its declared verdict, and every `<spec>#<id>`
    /// still resolves. A green proof with no witnesses is not a proof.
    #[test]
    fn a_receipt_without_mutation_witnesses_cannot_land() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join("scripts/mutations")).unwrap();
        std::fs::write(
            repo.path().join("scripts/mutations/demo.spec.sh"),
            "mutation \"D1  a decision\" \\\n    file=src/x.rs \\\n    find='a' \\\n    replace='b' \\\n    expect=a_test\n",
        )
        .unwrap();
        // The spec must exist at the sha the receipt proves (bd-pgzxn).
        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(repo.path())
                .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
                .args(args)
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q"]));
        assert!(git(&["add", "scripts/mutations/demo.spec.sh"]));
        assert!(git(&["commit", "-qm", "declare D1"]));
        let sha = String::from_utf8(
            Command::new("git")
                .current_dir(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let receipt = |body: &str| {
            let path = repo.path().join("receipt.json");
            std::fs::write(&path, body).unwrap();
            path
        };
        let row = |spec: &str, id: &str, verdict: &str, green: bool| {
            format!(
                r#"{{"spec":"{spec}","id":"{id}","expect_test":"a_test","expect_green":{green},"verdict":"{verdict}"}}"#
            )
        };
        let doc = |ok: bool, rows: &[String]| {
            format!(
                r#"{{"schema":"flywheel-clean-proof.v1","ok":{ok},"failed_step":null,"sha":"{sha}","mode":"sha","mutations_mode":null,"mutations_base":null,"changed_paths":[],"mutations":[{rows}]}}"#,
                rows = rows.join(",")
            )
        };

        let none = receipt(&doc(true, &[]));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &none, &sha).unwrap_err()
        );
        assert!(
            err.contains("no mutation run"),
            "an empty mutation list is refused: {err}"
        );

        let legacy = receipt(&format!(
            r#"{{"ok":true,"sha":"{sha}","mode":"sha","mutations":["src/x.rs:t:expect=RED:got=RED"]}}"#
        ));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &legacy, &sha).unwrap_err()
        );
        assert!(err.contains("legacy mutation row"), "{err}");

        let red_is_green = receipt(&doc(true, &[row("demo.spec.sh", "D1", "GREEN", false)]));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &red_is_green, &sha).unwrap_err()
        );
        assert!(
            err.contains("proved GREEN, expected RED"),
            "a mutation that stayed green fails the gate: {err}"
        );

        let dangling = receipt(&doc(true, &[row("demo.spec.sh", "D9", "RED", false)]));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &dangling, &sha).unwrap_err()
        );
        assert!(
            err.contains("no longer declares"),
            "a receipt cannot cite a mutation the spec dropped: {err}"
        );

        let not_green = receipt(&doc(false, &[row("demo.spec.sh", "D1", "RED", false)]));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &not_green, &sha).unwrap_err()
        );
        assert!(err.contains("not green"), "{err}");

        let good = receipt(&doc(true, &[row("demo.spec.sh", "D1", "RED", false)]));
        receipt_is_landable(repo.path(), &good, &sha).expect("a proving receipt lands");

        // A guard expected dead is the other way round and is not a failure.
        let dead = receipt(&doc(
            true,
            &[row("demo.spec.sh", "D1", "DEAD-CONFIRMED", true)],
        ));
        receipt_is_landable(repo.path(), &dead, &sha).expect("a dead-guard witness lands");
    }

    /// bd-m5afp review (captain re-verify): the receipt must prove the commit
    /// being landed, never a different tree, and an empty mutation set may land
    /// only when the diff provably touched no spec target.
    #[test]
    fn a_receipt_is_bound_to_the_commit_it_proved() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join("scripts/mutations")).unwrap();
        std::fs::write(
            repo.path().join("scripts/mutations/demo.spec.sh"),
            "mutation \"D1  a decision\" \\\n    file=src/x.rs \\\n    find='a' \\\n    replace='b' \\\n    expect=a_test\n",
        )
        .unwrap();
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/verify-mutations.sh"),
            repo.path().join("scripts/verify-mutations.sh"),
        )
        .expect("the land gate re-derives targets with the real harness");
        // A real commit so the mutations_base arm can resolve.
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
            vec!["commit", "-q", "--allow-empty", "-m", "base"],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(repo.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let base = String::from_utf8(
            Command::new("git")
                .current_dir(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let receipt = |body: &str| {
            let path = repo.path().join("receipt.json");
            std::fs::write(&path, body).unwrap();
            path
        };
        let sha = "a".repeat(40);
        let other = "b".repeat(40);

        let wrong_tree = receipt(&format!(
            r#"{{"ok":true,"sha":"{other}","mode":"sha","mutations":[{{"spec":"demo.spec.sh","id":"D1","expect_test":"a_test","expect_green":false,"verdict":"RED"}}]}}"#
        ));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &wrong_tree, &sha).unwrap_err()
        );
        assert!(
            err.contains("proves") && err.contains(&other[..7]),
            "a receipt for another commit is refused: {err}"
        );

        let dirty = receipt(&format!(
            r#"{{"ok":true,"sha":"{sha}","mode":"dirty","mutations":[{{"spec":"demo.spec.sh","id":"D1","expect_test":"a_test","expect_green":false,"verdict":"RED"}}]}}"#
        ));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &dirty, &sha).unwrap_err()
        );
        assert!(err.contains("dirty tree"), "{err}");

        // Empty set, changed mode, real base, and NO changed path is a spec
        // target: the "docs-only landing" case, which must land.
        let docs_only = receipt(&format!(
            r#"{{"ok":true,"sha":"{sha}","mode":"sha","mutations":[],"mutations_mode":"changed","mutations_base":"{base}","changed_paths":["docs/guide.mdx","src/uncovered.rs"]}}"#
        ));
        receipt_is_landable(repo.path(), &docs_only, &sha)
            .expect("a diff that touches no spec target may land");

        // Same shape, but one changed path IS a spec target.
        let covered = receipt(&format!(
            r#"{{"ok":true,"sha":"{sha}","mode":"sha","mutations":[],"mutations_mode":"changed","mutations_base":"{base}","changed_paths":["src/x.rs"]}}"#
        ));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &covered, &sha).unwrap_err()
        );
        assert!(
            err.contains("is a spec target"),
            "an uncovered-looking receipt cannot skip a covered file: {err}"
        );

        // No mode at all (a bare proof) still refuses.
        let bare = receipt(&format!(
            r#"{{"ok":true,"sha":"{sha}","mode":"sha","mutations":[]}}"#
        ));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &bare, &sha).unwrap_err()
        );
        assert!(err.contains("no mutation run"), "{err}");

        // A base that is not a commit cannot vouch for an empty set.
        let bogus_base = receipt(&format!(
            r#"{{"ok":true,"sha":"{sha}","mode":"sha","mutations":[],"mutations_mode":"changed","mutations_base":"not-a-commit","changed_paths":["docs/guide.mdx"]}}"#
        ));
        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &bogus_base, &sha).unwrap_err()
        );
        assert!(err.contains("not a commit here"), "{err}");
    }

    /// bd-0uzrq.22: the gate note binds the CITING sha (the token
    /// `note_binding` and `ledger check` read) and records the proven sha
    /// beside it, so a descendant landing shows both halves.
    #[test]
    fn the_gate_note_binds_the_citing_sha_and_records_the_proven_one() {
        let citing = "a".repeat(40);
        let proven = "b".repeat(40);
        let note = super::gate_note(
            &citing,
            &proven,
            "cargo_test",
            std::path::Path::new("/r.json"),
        );
        assert!(
            note.starts_with(&format!("sha={citing}")),
            "the note must LEAD with the citing sha: {note}"
        );
        assert!(note.contains(&format!("proved={proven}")), "{note}");
        assert!(note.contains("cmd=cargo_test exit=0"), "{note}");
        assert_eq!(
            crate::verdict::note_binding(&note),
            crate::verdict::NoteBinding::Bound(citing),
            "the binding is the citing sha, never the proven one"
        );
    }

    /// bd-0uzrq.22: a proof of a LATER tree lands every bead whose citing
    /// commit it contains. The citing sha is the one `br close` validates
    /// (it must name the bead); the receipt may prove any descendant of it,
    /// because that tree contains the citing commit's work. A receipt that
    /// does not contain the citing commit is still a different tree.
    #[test]
    fn a_receipt_for_a_descendant_of_the_citing_commit_lands() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join("scripts/mutations")).unwrap();
        std::fs::write(
            repo.path().join("scripts/mutations/demo.spec.sh"),
            "mutation \"D1  a decision\" \\\n    file=src/x.rs \\\n    find='a' \\\n    replace='b' \\\n    expect=a_test\n",
        )
        .unwrap();
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/verify-mutations.sh"),
            repo.path().join("scripts/verify-mutations.sh"),
        )
        .expect("the land gate re-derives targets with the real harness");
        let commit = |message: &str| {
            assert!(
                Command::new("git")
                    .current_dir(repo.path())
                    .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
                    .args(["commit", "-q", "--allow-empty", "-m", message])
                    .status()
                    .unwrap()
                    .success()
            );
            String::from_utf8(
                Command::new("git")
                    .current_dir(repo.path())
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .to_string()
        };
        assert!(
            Command::new("git")
                .current_dir(repo.path())
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(repo.path())
                .args(["add", "scripts/mutations/demo.spec.sh"])
                .status()
                .unwrap()
                .success()
        );
        let base = commit("base");
        let citing = commit("fix: the cited work (bd-0uzrq.22)");
        let proven = commit("chore: a later proof target");
        let receipt = |proved_sha: &str| {
            let path = repo
                .path()
                .join(format!("receipt-{}.json", &proved_sha[..7]));
            std::fs::write(
                &path,
                format!(
                    r#"{{"ok":true,"sha":"{proved_sha}","mode":"sha","mutations":[{{"spec":"demo.spec.sh","id":"D1","expect_test":"a_test","expect_green":false,"verdict":"RED"}}],"mutations_mode":"changed","mutations_base":"{base}","changed_paths":["src/other.rs"]}}"#
                ),
            )
            .unwrap();
            path
        };

        receipt_is_landable(repo.path(), &receipt(&proven), &citing)
            .expect("a receipt proving a descendant of the citing commit lands");

        let err = format!(
            "{:#}",
            receipt_is_landable(repo.path(), &receipt(&citing), &proven).unwrap_err()
        );
        assert!(
            err.contains("does not contain") && err.contains(&citing[..7]),
            "a receipt for an EARLIER commit does not contain the citing one: {err}"
        );
    }

    #[test]
    fn head_row_must_be_closed_not_merely_present() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".beads")).unwrap();
        std::fs::write(
            dir.path().join(".beads/issues.jsonl"),
            "{\"id\":\"bd-x\",\"status\":\"closed\"}\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["commit", "-qm", "bd-x"])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            head_has_closed_bead(dir.path(), "bd-x").unwrap(),
            (true, true)
        );
        assert_eq!(
            head_has_closed_bead(dir.path(), "bd-y").unwrap(),
            (false, false)
        );
    }

    /// bd-ps6wc: another lane's uncommitted tracker row must never fail THIS
    /// bead's ship check.
    #[test]
    fn dirty_rows_of_other_beads_do_not_block_a_clean_bead() {
        use super::bead_rows_committed;
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".beads")).unwrap();
        let issues = dir.path().join(".beads/issues.jsonl");
        std::fs::write(
            &issues,
            "{\"id\":\"bd-a\",\"status\":\"closed\"}\n{\"id\":\"bd-b\",\"status\":\"open\"}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".beads/gates.jsonl"), "").unwrap();
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["commit", "-qm", "rows"])
                .status()
                .unwrap()
                .success()
        );
        // Another lane edits bd-b's row and leaves it uncommitted.
        std::fs::write(
            &issues,
            "{\"id\":\"bd-a\",\"status\":\"closed\"}\n{\"id\":\"bd-b\",\"status\":\"in_progress\"}\n",
        )
        .unwrap();
        assert!(
            bead_rows_committed(dir.path(), "bd-a").unwrap(),
            "a clean bead ships while a peer's row is dirty"
        );
        assert!(
            !bead_rows_committed(dir.path(), "bd-b").unwrap(),
            "the dirty bead itself is not committed"
        );
    }

    /// bd-gate-ship-robustness-3pp9k: a bead cited a few commits back still
    /// ships — later landings from other lanes must not invalidate it.
    #[test]
    fn head_names_bead_survives_sequential_landings() {
        let dir = init_repo();
        let commit = |msg: &str| {
            std::fs::write(dir.path().join("file.txt"), msg).unwrap();
            assert!(
                Command::new("git")
                    .current_dir(dir.path())
                    .args(["add", "."])
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(
                Command::new("git")
                    .current_dir(dir.path())
                    .args(["commit", "-qm", msg])
                    .status()
                    .unwrap()
                    .success()
            );
        };
        commit("feat(work): the bd-abc123 work");
        commit("chore: unrelated lane landing");
        assert!(head_names_bead(dir.path(), dir.path(), "bd-abc123").unwrap());
        assert!(!head_names_bead(dir.path(), dir.path(), "bd-zzz999").unwrap());
    }

    /// bd-9zqjo (b): a bead can be a tracker row in one repo while its work
    /// landed in another (bd-9zqjo's own commits live in toron). `--repo`
    /// moves only the SEARCH; the tracker checks stay in `dir`, so the same
    /// call must still refuse when the citing commit is nowhere.
    #[test]
    fn head_names_bead_searches_the_work_repo_when_named() {
        let store = init_repo();
        let work = init_repo();
        let commit = |dir: &std::path::Path, msg: &str| {
            std::fs::write(dir.join("f.txt"), msg).unwrap();
            for args in [vec!["add", "-A"], vec!["commit", "-qm", msg]] {
                assert!(
                    Command::new("git")
                        .current_dir(dir)
                        .args(args)
                        .status()
                        .unwrap()
                        .success()
                );
            }
        };
        commit(store.path(), "chore: the tracker repo, no citation");
        commit(work.path(), "feat(work): bd-crossrepo is here");

        assert!(
            !head_names_bead(store.path(), store.path(), "bd-crossrepo").unwrap(),
            "the tracker repo does not carry the work"
        );
        assert!(
            head_names_bead(store.path(), work.path(), "bd-crossrepo").unwrap(),
            "the named work repo does"
        );
        assert!(
            !head_names_bead(store.path(), work.path(), "bd-nobody").unwrap(),
            "no repo carrying the id is still a refusal, not an error"
        );
    }
}
