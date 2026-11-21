//! `flywheel commit <paths>` — commit exactly the named paths (bd-aberm).
//!
//! A private `GIT_INDEX_FILE` is seeded from HEAD, so another lane's staged
//! entries in the shared index can never be swept into this commit. The repo's
//! own pre-commit hook fences the private tree before HEAD moves; HEAD advances
//! only by compare-and-swap, retried if another lane lands first.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};

const MAX_CAS_RETRIES: usize = 5;
/// A shared-index stage is stale (not a neighbour's intent) when its blob was
/// any first-parent HEAD within this many commits (bd-j8x81 ruling 2).
const STALE_PARENT_WINDOW: usize = 50;
static INDEX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One git invocation. With `index`, every git command sees only that private
/// index — the shared index stays untouched until the final path refresh.
struct Git<'a> {
    repo: &'a Path,
    index: Option<&'a Path>,
}

impl<'a> Git<'a> {
    fn shared(repo: &'a Path) -> Self {
        Self { repo, index: None }
    }

    fn private(repo: &'a Path, index: &'a Path) -> Self {
        Self {
            repo,
            index: Some(index),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.repo);
        if let Some(index) = self.index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.args(args);
        cmd
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        self.command(args)
            .output()
            .with_context(|| format!("git {args:?}"))
    }

    fn out(&self, args: &[&str]) -> Result<String> {
        let output = self.run(args)?;
        if !output.status.success() {
            bail!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    fn ok(&self, args: &[&str]) -> Result<()> {
        self.out(args).map(|_| ())
    }

    fn out_stdin(&self, args: &[&str], input: &[u8]) -> Result<String> {
        use std::io::Write as _;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("git {args:?}"))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("git {args:?} stdin"))?
            .write_all(input)
            .map_err(|e| anyhow!("git {args:?} stdin write: {e}"))?;
        let out = child
            .wait_with_output()
            .with_context(|| format!("git {args:?} wait"))?;
        if !out.status.success() {
            bail!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    }
}

struct IndexGuard(PathBuf);
impl Drop for IndexGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn validate_paths(paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        bail!("flywheel commit needs at least one path");
    }
    for path in paths {
        let p = Path::new(path);
        if p.as_os_str().is_empty() {
            bail!("empty path");
        }
        if p.is_absolute()
            || p.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("path must be repo-relative without '..': {path}");
        }
    }
    Ok(())
}

fn add_args<'a>(prefix: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut args = prefix.to_vec();
    for path in paths {
        args.push(path);
    }
    args
}

fn ensure_only_requested(staged: &str, paths: &[String]) -> Result<()> {
    for line in staged.lines().filter(|line| !line.is_empty()) {
        if !paths
            .iter()
            .any(|p| line == p || line.starts_with(format!("{p}/").as_str()))
        {
            bail!("private index staged an unrequested path: {line}");
        }
    }
    Ok(())
}

/// Run the repo's own commit fence against the private tree. A repo with no
/// pre-commit hook has no fence; every repo with one gets it, whatever language
/// its hook is written in.
fn fence_private_tree(repo: &Path, private: &Git<'_>, nonce: &str) -> Result<()> {
    let hook = private.out(&["rev-parse", "--git-path", "hooks/pre-commit"])?;
    let hook_path = if Path::new(&hook).is_absolute() {
        PathBuf::from(&hook)
    } else {
        repo.join(&hook)
    };
    if !hook_path.is_file() {
        return Ok(());
    }
    // The per-invocation nonce is what distinguishes a fenced `flywheel
    // commit` from a plain `git commit`; the installed hook refuses the
    // latter (bd-j1g64). Never forward an ambient token from the caller.
    let output = private
        .command(&["hook", "run", "pre-commit"])
        .env_remove("FLYWHEEL_COMMIT_OPERATOR")
        .env("FLYWHEEL_COMMIT_TOKEN", nonce)
        .output()
        .with_context(|| "git hook run pre-commit")?;
    if !output.status.success() {
        bail!(
            "commit fence failed, HEAD untouched: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Build the private tree on top of `parent`. Called again after a CAS race so
/// a tree built on the old HEAD can never revert what the other lane landed.
fn build_tree(private: &Git<'_>, paths: &[String], parent: &str) -> Result<String> {
    private.ok(&["read-tree", parent])?;
    private.ok(&add_args(&["add", "--"], paths))?;
    let staged = private.out(&["diff", "--cached", "--name-only"])?;
    ensure_only_requested(&staged, paths)?;
    private.out(&["write-tree"])
}

/// Build the private tree from exact blob contents: HEAD's tree, with each
/// named path replaced by the caller-supplied bytes (bd-kz3sc). The caller
/// decides which rows those bytes contain — this verb only stages them.
fn build_blobs_tree(
    private: &Git<'_>,
    blobs: &[(String, Vec<u8>)],
    parent: &str,
) -> Result<String> {
    private.ok(&["read-tree", parent])?;
    let paths: Vec<String> = blobs.iter().map(|(p, _)| p.clone()).collect();
    for (path, content) in blobs {
        let sha = private.out_stdin(&["hash-object", "-w", "--stdin"], content)?;
        private.ok(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{sha},{path}"),
        ])?;
    }
    let staged = private.out(&["diff", "--cached", "--name-only"])?;
    ensure_only_requested(&staged, &paths)?;
    private.out(&["write-tree"])
}

/// True when `path` is one of the named paths (or below a named directory).
fn is_named(path: &str, paths: &[String]) -> bool {
    paths
        .iter()
        .any(|p| path == p.as_str() || path.starts_with(format!("{p}/").as_str()))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct StagedClass {
    /// Named paths carrying unique content: this commit refuses.
    conflict: Vec<String>,
    /// Every stale entry in the whole index (named or not): heal after CAS.
    stale: Vec<String>,
    /// Unnamed entries with unique content: leave alone and mail the holder.
    unclassifiable: Vec<String>,
}

/// Classify EVERY staged entry in the shared index, not just the paths being
/// committed. A stage is stale when its blob is one a first-parent HEAD held in
/// the window, or a staged deletion whose file is unchanged in HEAD and on
/// disk; neither carries unique work (bd-himh8).
fn classify_staged(shared: &Git<'_>, paths: &[String]) -> Result<StagedClass> {
    let staged = shared.out(&["diff", "--cached", "--name-status"])?;
    let ancestors = shared
        .out(&[
            "rev-list",
            "--first-parent",
            &format!("-{STALE_PARENT_WINDOW}"),
            "HEAD",
        ])
        .unwrap_or_default();
    let mut class = StagedClass::default();
    for line in staged.lines() {
        let mut fields = line.split('\t');
        let status = fields.next().unwrap_or("");
        for path in fields {
            let named = is_named(path, paths);
            let head_blob = shared.out(&["rev-parse", &format!("HEAD:{path}")]).ok();
            let stage_blob = shared
                .out(&["rev-parse", &format!(":{path}")])
                .unwrap_or_default();
            // A named path being re-created is the existing commit contract;
            // an unnamed one is only stale when the disk copy still equals HEAD.
            let stale_delete = status.starts_with('D')
                && head_blob.is_some()
                && (named
                    || std::fs::read(shared.repo.join(path))
                        .ok()
                        .is_some_and(|raw| {
                            shared
                                .out_stdin(&["hash-object", "--stdin"], &raw)
                                .unwrap_or_default()
                                == head_blob.clone().unwrap_or_default()
                        }));
            let stale_blob = !stage_blob.is_empty()
                && ancestors.lines().any(|rev| {
                    shared
                        .out(&["rev-parse", &format!("{rev}:{path}")])
                        .unwrap_or_default()
                        == stage_blob
                });
            if stale_delete || stale_blob {
                if !class.stale.iter().any(|p: &String| p == path) {
                    class.stale.push(path.to_string());
                }
            } else if named {
                class.conflict.push(path.to_string());
            } else {
                class.unclassifiable.push(path.to_string());
            }
        }
    }
    Ok(class)
}

/// bd-ahkw1: the live/dead split toron already computes for us.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReservationVerdict {
    live: Vec<(String, String)>,
    dead: Vec<String>,
}

fn parse_reservations(raw: &str) -> Result<ReservationVerdict> {
    let doc: serde_json::Value =
        serde_json::from_str(raw).context("toron reserve conflicts returned no JSON")?;
    let rows = |key: &str| -> Result<Vec<serde_json::Value>> {
        doc.get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .with_context(|| format!("toron reserve conflicts has no {key} array"))
    };
    let live = rows("conflicts")?
        .iter()
        .filter_map(|row| {
            Some((
                row.get("holders")?
                    .as_array()?
                    .first()?
                    .as_str()?
                    .to_string(),
                row.get("reservation_ids")?
                    .as_array()?
                    .first()?
                    .as_str()?
                    .to_string(),
            ))
        })
        .collect();
    let dead = rows("soft_conflicts")?
        .iter()
        .filter_map(|row| row.get("path")?.as_str().map(str::to_string))
        .collect();
    Ok(ReservationVerdict { live, dead })
}

/// Everything needed to turn a coordination warning into one mail to the
/// holder plus one copy to the captain, deduplicated inside a window
/// (bd-v7ucf). Built at the CLI edge; tests pass a sandbox-owned fake mailer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoordinationCtx {
    requester: String,
    project: String,
    captain: String,
    notice_store: PathBuf,
    mailer_bin: PathBuf,
    now_secs: u64,
}

impl CoordinationCtx {
    fn from_env() -> Self {
        let cfg = crate::config::Config::default();
        let project = std::env::var("FLYWHEEL_PROJECT")
            .ok()
            .or_else(|| crate::cron::detect_project(&cfg, None))
            .unwrap_or_default();
        // bd-s3hzg: the captain is a project fact, not a hardcoded pin. An
        // env override remains for tests and for an operator's one-off run.
        let project_key =
            crate::project_dir_str(&cfg, &project).unwrap_or_else(|_| project.clone());
        let captain = std::env::var("FLYWHEEL_CAPTAIN_PIN")
            .ok()
            .or_else(|| crate::state::captain_pin(&project, &project_key))
            .unwrap_or_default();
        Self {
            project,
            requester: std::env::var("AGENT_NAME").unwrap_or_else(|_| "operator".into()),
            captain,
            notice_store: crate::config::state_dir().join("coordination-notices.jsonl"),
            mailer_bin: std::env::var("FLYWHEEL_MAIL_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("flywheel")),
            now_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

const NOTICE_WINDOW_SECS: u64 = 600;

/// Send at most one mail per (holder, requester, paths) inside the window.
/// The holder gets a plain-English notice; the captain gets a copy. A failed
/// send is a warning, never a reason to let a foreign stage be stripped.
/// bd-s3hzg F13: the same paths in another project are a different notice.
fn notice_key(coord: &CoordinationCtx, holder: &str, paths: &[String]) -> String {
    format!(
        "{}|{}|{}|{}",
        coord.project,
        holder,
        coord.requester,
        paths.join(",")
    )
}

fn notify_holder(coord: &CoordinationCtx, holder: &str, paths: &[String]) {
    if holder.is_empty() || holder == coord.requester {
        return;
    }
    let key = notice_key(coord, holder, paths);
    let seen: Vec<(String, u64)> = fs::read_to_string(&coord.notice_store)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if seen
        .iter()
        .any(|(k, ts)| k == &key && coord.now_secs.saturating_sub(*ts) < NOTICE_WINDOW_SECS)
    {
        return;
    }
    let body = doorbell_core::agent_text::coordination_notice(
        holder,
        &coord.requester,
        paths,
        &coord.project,
    );
    let subject = format!(
        "flywheel: a file you reserved is needed by {}",
        coord.requester
    );
    let mut sent = true;
    for to in [holder, coord.captain.as_str()] {
        let status = Command::new(&coord.mailer_bin)
            .args([
                "mail",
                "send",
                "--project",
                &coord.project,
                "--as",
                &coord.requester,
                "--to",
                to,
                "--subject",
                &subject,
                "--body",
                &body,
                "--wake",
                "tasked",
            ])
            .status();
        if !status.map(|s| s.success()).unwrap_or(false) {
            sent = false;
            eprintln!("flywheel commit: could not send coordination mail to {to}");
        }
    }
    if !sent {
        return;
    }
    let mut kept: Vec<(String, u64)> = seen
        .into_iter()
        .filter(|(_, ts)| coord.now_secs.saturating_sub(*ts) < NOTICE_WINDOW_SECS)
        .collect();
    kept.push((key, coord.now_secs));
    if let Some(parent) = coord.notice_store.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        &coord.notice_store,
        serde_json::to_string(&kept).unwrap_or_default(),
    );
}

/// Everything the reservation guard needs, gathered ONCE at the CLI edge so
/// nothing inside the commit path reads or mutates ambient process env. Tests
/// pass an explicit ctx with a sandbox-owned fake toron (bd-ahkw1 race fix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReservationCtx {
    pin: Option<String>,
    project: String,
    toron_bin: PathBuf,
    coord: Option<CoordinationCtx>,
}

impl ReservationCtx {
    fn from_env() -> Self {
        let pin = std::env::var("AGENT_NAME").ok().filter(|p| !p.is_empty());
        let project = std::env::var("FLYWHEEL_PROJECT")
            .ok()
            .or_else(|| crate::cron::detect_project(&crate::config::Config::default(), None))
            .unwrap_or_default();
        let toron_bin = std::env::var("FLYWHEEL_TORON_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("toron"));
        Self {
            coord: Some(CoordinationCtx::from_env()),
            pin,
            project,
            toron_bin,
        }
    }

    /// Programmatic commits (tests, ship land) carry no seat identity.
    fn skip() -> Self {
        Self {
            pin: None,
            project: String::new(),
            toron_bin: PathBuf::from("toron"),
            coord: None,
        }
    }
}

/// Mail the holder of a unique staged entry we refused to touch (bd-v7ucf).
/// Best effort: a missing identity, an unreachable toron or a failed send
/// degrades to a loud warning, never to stripping the neighbour's stage.
fn report_unclassifiable(paths: &[String], ctx: &ReservationCtx) {
    let Some((pin, project)) = ctx.pin.as_deref().map(|p| (p, ctx.project.as_str())) else {
        for path in paths {
            eprintln!(
                "flywheel commit: UNIQUE staged content for {path} was left alone; mail the reserving holder"
            );
        }
        return;
    };
    for path in paths {
        let found = Command::new(&ctx.toron_bin)
            .args([
                "reserve",
                "conflicts",
                "--project",
                project,
                "--as",
                pin,
                "--paths",
                path,
                "--json",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
            .and_then(|v| {
                v.get("conflicts")?
                    .as_array()?
                    .iter()
                    .filter_map(|c| c.get("holders")?.as_array())
                    .flatten()
                    .filter_map(|h| h.as_str().map(str::to_string))
                    .next()
            });
        match found {
            Some(holder) => {
                if let Some(coord) = &ctx.coord {
                    notify_holder(coord, &holder, std::slice::from_ref(path));
                } else {
                    eprintln!("flywheel commit: mail {holder} about the unique stage on {path}");
                }
            }
            None => eprintln!(
                "flywheel commit: UNIQUE staged content for {path} has no live reserving holder; mail the last editor"
            ),
        }
    }
}

/// bd-ahkw1: refuse a path exclusively held by another live pin, naming the
/// holder and the exact request-release call. Dead leases are revoked first.
const RESERVATION_ESCAPE: &str =
    "pass --no-reservation-check to commit anyway (recorded in the commit message)";

fn guard_commit_reservations(paths: &[String], ctx: &ReservationCtx) -> Result<()> {
    // bd-ahkw1: no identity is NOT consent. Refuse and say how to identify or
    // escape, instead of silently skipping every lease.
    let pin = ctx.pin.as_deref().ok_or_else(|| {
        anyhow!("flywheel commit: no AGENT_NAME/--as identity; set it or {RESERVATION_ESCAPE}")
    })?;
    let project = &ctx.project;
    let out = Command::new(&ctx.toron_bin)
        .args([
            "reserve",
            "conflicts",
            "--project",
            project,
            "--as",
            pin,
            "--paths",
            &paths.join(","),
            "--json",
        ])
        .output()
        .with_context(|| {
            format!("flywheel commit: cannot reach toron reserve conflicts; {RESERVATION_ESCAPE}")
        })?;
    // bd-ahkw1: a failed or unreadable answer is NEVER treated as no conflicts.
    if !out.status.success() {
        bail!(
            "flywheel commit: toron reserve conflicts failed ({}); {RESERVATION_ESCAPE}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let verdict = parse_reservations(&raw).map_err(|e| {
        anyhow!("flywheel commit: cannot read reservation answer ({e:#}); {RESERVATION_ESCAPE}")
    })?;
    for path in &verdict.dead {
        let _ = Command::new(&ctx.toron_bin)
            .args([
                "reserve",
                "force-release",
                "--project",
                project,
                "--as",
                pin,
                "--path",
                path,
                "--note",
                "dead reservation holder",
            ])
            .status();
    }
    if let Some((holder, id)) = verdict.live.first() {
        if let Some(coord) = &ctx.coord {
            notify_holder(coord, holder, paths);
        }
        bail!(
            "flywheel commit: path reserved exclusively by {holder} (reservation {id}); ask them with `toron reserve request-release --project {project} --as {pin} --target {holder} --reservation-id {id}`"
        );
    }
    Ok(())
}

/// bd-ahkw1: a successful commit frees the committer's own leases on the paths
/// it landed; a refused or failed commit keeps them.
fn release_own_reservations(paths: &[String], ctx: &ReservationCtx) {
    let Some(pin) = ctx.pin.as_deref() else {
        return;
    };
    let project = &ctx.project;
    let _ = Command::new(&ctx.toron_bin)
        .args([
            "reserve",
            "release",
            "--project",
            project,
            "--as",
            pin,
            "--paths",
            &paths.join(","),
        ])
        .status();
}

/// After a successful CAS, point every stale shared-index entry (any path,
/// not just the committed ones) at the blob HEAD now holds. Unique stages are
/// never in this list, so a neighbour's intent survives (bd-himh8).
fn sync_stale_stage(shared: &Git<'_>, stale: &[String], commit: &str) -> Result<()> {
    for path in stale {
        let entry = shared.out(&["ls-tree", commit, "--", path])?;
        let mut parts = entry.split_whitespace();
        let mode = parts.next().unwrap_or("100644");
        let _kind = parts.next();
        let Some(blob) = parts.next() else {
            shared.ok(&["update-index", "--force-remove", "--", path])?;
            continue;
        };
        shared.ok(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{blob},{path}"),
        ])?;
    }
    Ok(())
}

/// Warn when the SHARED index holds a staged entry for a path outside this
/// commit whose blob came from an ancestor of HEAD, not from HEAD: a stale
/// stage that a later plain commit would revert (bd-j1g64).
#[cfg(test)]
fn stale_stage_warnings(shared: &Git<'_>, paths: &[String]) -> Result<Vec<String>> {
    let staged = shared.out(&["diff", "--cached", "--name-only"])?;
    let mut warnings = Vec::new();
    let ancestors = shared
        .out(&["rev-list", "--max-count=32", "HEAD^"])
        .unwrap_or_default(); // a one-commit repo has no ancestor to be stale from
    for line in staged.lines() {
        if paths
            .iter()
            .any(|p| line == p.as_str() || line.starts_with(format!("{p}/").as_str()))
        {
            continue; // already refused by refuse_foreign_stage
        }
        let Ok(staged_blob) = shared.out(&["rev-parse", &format!(":{line}")]) else {
            continue;
        };
        let Ok(head_blob) = shared.out(&["rev-parse", &format!("HEAD:{line}")]) else {
            continue;
        };
        if staged_blob == head_blob {
            continue;
        }
        for rev in ancestors.lines() {
            if let Ok(blob) = shared.out(&["rev-parse", &format!("{rev}:{line}")])
                && blob == staged_blob
            {
                warnings.push(format!(
                    "flywheel commit: WARNING: shared index has a stale stage for {line} \
                     (blob from {rev}, not HEAD) — a later plain commit would revert it; \
                     mail the holder"
                ));
                break;
            }
        }
    }
    Ok(warnings)
}

/// The one private-index fence + CAS path. `build` produces a tree on top of
/// the named parent; it is called again after a CAS race so a tree built on an
/// old HEAD can never revert what another lane landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationMode {
    /// Programmatic commits (tests, ship land) that do not carry a seat pin.
    Skip,
    /// CLI commit: enforce, unless the operator took the recorded escape.
    Enforce { escape: bool },
}

fn guard_nonempty(
    shared: &Git<'_>,
    tree: &str,
    parent: &str,
    paths: &[String],
    allow_empty: bool,
) -> Result<()> {
    if allow_empty {
        return Ok(());
    }
    let head_tree = shared.out(&["rev-parse", &format!("{parent}^{{tree}}")])?;
    if tree == head_tree {
        bail!(
            "no change in named path(s): {} — refusing an empty commit (use --allow-empty)",
            paths.join(", ")
        );
    }
    Ok(())
}

fn commit_tree<F>(
    repo: &Path,
    paths: &[String],
    message: &str,
    ctx: &ReservationCtx,
    mode: ReservationMode,
    allow_empty: bool,
    mut build: F,
) -> Result<String>
where
    F: for<'a> FnMut(&Git<'a>, &str) -> Result<String>,
{
    let shared = Git::shared(repo);
    let old_head = shared.out(&["rev-parse", "HEAD"])?;
    let message = match mode {
        ReservationMode::Skip => message.to_string(),
        ReservationMode::Enforce { escape: false } => {
            guard_commit_reservations(paths, ctx)?;
            message.to_string()
        }
        ReservationMode::Enforce { escape: true } => {
            // The escape is never silent: the commit itself says it was used.
            format!("{message}\n\nflywheel-reservation-check: skipped")
        }
    };
    let class = classify_staged(&shared, paths)?;
    if !class.conflict.is_empty() {
        bail!(
            "named path(s) already staged in the shared index (another lane's hunks?): {} — mail the holder, do not strip them",
            class.conflict.join(", ")
        );
    }
    report_unclassifiable(&class.unclassifiable, ctx);

    let unique = format!(
        "{}-{}",
        std::process::id(),
        INDEX_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let nonce = format!("fwc-{unique}-{nanos}");
    let index = std::env::temp_dir().join(format!("fw-commit-{unique}"));
    let _guard = IndexGuard(index.clone());
    let private = Git::private(repo, &index);
    let mut tree = build(&private, &old_head)?;
    guard_nonempty(&shared, &tree, &old_head, paths, allow_empty)?;
    fence_private_tree(repo, &private, &nonce)?;

    let mut parent = old_head;
    for _ in 0..MAX_CAS_RETRIES {
        let commit = shared.out(&["commit-tree", &tree, "-p", &parent, "-m", &message])?;
        let update = shared.run(&["update-ref", "HEAD", &commit, &parent])?;
        if update.status.success() {
            sync_stale_stage(&shared, &class.stale, &commit)?;
            if matches!(mode, ReservationMode::Enforce { .. }) {
                release_own_reservations(paths, ctx);
            }
            return Ok(commit);
        }
        let moved = shared.out(&["rev-parse", "HEAD"])?;
        if moved == parent {
            bail!(
                "update-ref failed without HEAD moving: {}",
                String::from_utf8_lossy(&update.stderr).trim()
            );
        }
        parent = moved;
        tree = build(&private, &parent)?;
        guard_nonempty(&shared, &tree, &parent, paths, allow_empty)?;
        fence_private_tree(repo, &private, &nonce)?;
    }
    bail!("HEAD moved {MAX_CAS_RETRIES} times; refusing to commit")
}

/// Commit exactly `paths` from the working tree, returning the new commit sha.
fn commit_paths_mode(
    repo: &Path,
    paths: &[String],
    message: &str,
    ctx: &ReservationCtx,
    mode: ReservationMode,
    allow_empty: bool,
) -> Result<String> {
    validate_paths(paths)?;
    let sha = commit_tree(
        repo,
        paths,
        message,
        ctx,
        mode,
        allow_empty,
        |private, parent| build_tree(private, paths, parent),
    )?;
    Git::shared(repo).ok(&add_args(&["add", "--"], paths))?;
    Ok(sha)
}

/// Programmatic commit without a seat pin (tests only).
#[cfg(test)]
fn commit_paths(repo: &Path, paths: &[String], message: &str) -> Result<String> {
    commit_paths_mode(
        repo,
        paths,
        message,
        &ReservationCtx::skip(),
        ReservationMode::Skip,
        true,
    )
}

/// CLI commit: enforce reservations unless the operator took the recorded
/// escape (bd-ahkw1).
pub(crate) fn commit_paths_checked(
    repo: &Path,
    paths: &[String],
    message: &str,
    no_reservation_check: bool,
    allow_empty: bool,
    ctx: &ReservationCtx,
) -> Result<String> {
    commit_paths_mode(
        repo,
        paths,
        message,
        ctx,
        ReservationMode::Enforce {
            escape: no_reservation_check,
        },
        allow_empty,
    )
}

/// Commit exact `path -> content` blobs produced by `splice(parent)` (bd-kz3sc
/// rework 2). The splice is recomputed on EVERY CAS retry against the moved
/// parent, so a neighbour that landed in between is never reverted. The shared
/// index is refreshed from the final HEAD's splice.
pub(crate) fn commit_blobs<F>(
    repo: &Path,
    paths: &[String],
    message: &str,
    mut splice: F,
) -> Result<String>
where
    F: FnMut(&str) -> Result<Vec<(String, Vec<u8>)>>,
{
    validate_paths(paths)?;
    let sha = commit_tree(
        repo,
        paths,
        message,
        &ReservationCtx::skip(),
        ReservationMode::Skip,
        true,
        |private, parent| {
            let blobs = splice(parent)?;
            build_blobs_tree(private, &blobs, parent)
        },
    )?;
    let shared = Git::shared(repo);
    let head = shared.out(&["rev-parse", "HEAD"])?;
    for (path, content) in splice(&head)? {
        let blob = shared.out_stdin(&["hash-object", "-w", "--stdin"], &content)?;
        shared.ok(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{path}"),
        ])?;
    }
    Ok(sha)
}

pub(crate) fn cmd_commit(
    paths: &[String],
    message: &str,
    repo: Option<&str>,
    no_reservation_check: bool,
    allow_empty: bool,
) -> Result<()> {
    let repo_arg = repo.unwrap_or(".");
    let repo = fs::canonicalize(repo_arg).with_context(|| format!("repo dir {repo_arg}"))?;
    let ctx = ReservationCtx::from_env();
    let sha = commit_paths_checked(
        &repo,
        paths,
        message,
        no_reservation_check,
        allow_empty,
        &ctx,
    )?;
    println!("commit {}", &sha[..sha.len().min(12)]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn hook(dir: &Path, body: &str) {
        let path = dir.join("hooks/pre-commit");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
    }

    fn scratch_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fw-commit-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("hooks")).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.name", "Test Agent"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "core.hooksPath", "hooks"]);
        fs::write(dir.join("base.txt"), "base\n").unwrap();
        git(&dir, &["add", "base.txt"]);
        git(&dir, &["commit", "-qm", "base"]);
        hook(&dir, "exit 0");
        dir
    }

    fn tracked_in_head(dir: &Path, sha: &str) -> Vec<String> {
        git(dir, &["ls-tree", "-r", "--name-only", sha])
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn shared_index_staged_files_are_never_swept() {
        let dir = scratch_repo("sweep");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        fs::write(dir.join("theirs.txt"), "theirs\n").unwrap();
        git(&dir, &["add", "theirs.txt"]);

        let sha = commit_paths(&dir, &["ours.txt".into()], "ours").unwrap();

        let files = tracked_in_head(&dir, &sha);
        assert!(files.contains(&"ours.txt".to_string()), "{files:?}");
        assert!(!files.contains(&"theirs.txt".to_string()), "{files:?}");
        let still_staged = git(&dir, &["diff", "--cached", "--name-only"]);
        assert!(
            still_staged.lines().any(|l| l == "theirs.txt"),
            "{still_staged}"
        );
        assert!(
            !still_staged.lines().any(|l| l == "ours.txt"),
            "{still_staged}"
        );
    }

    #[test]
    fn named_paths_are_committed_byte_equal() {
        let dir = scratch_repo("bytes");
        let body = "line one\nline two\n";
        fs::write(dir.join("ours.txt"), body).unwrap();
        let sha = commit_paths(&dir, &["ours.txt".into()], "bytes").unwrap();
        let committed = git(&dir, &["show", &format!("{sha}:ours.txt")]);
        assert_eq!(
            committed.replace('\r', ""),
            body.trim_end().replace('\r', "")
        );
    }

    #[test]
    fn fence_sees_only_the_private_tree() {
        let dir = scratch_repo("fence-tree");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        fs::write(dir.join("theirs.txt"), "theirs\n").unwrap();
        git(&dir, &["add", "theirs.txt"]);
        hook(&dir, "git diff --cached --name-only > fence-saw.txt");

        commit_paths(&dir, &["ours.txt".into()], "ok").unwrap();

        let saw = fs::read_to_string(dir.join("fence-saw.txt")).unwrap();
        assert!(saw.lines().any(|l| l == "ours.txt"), "{saw}");
        assert!(!saw.lines().any(|l| l == "theirs.txt"), "{saw}");
    }

    #[test]
    fn a_red_fence_leaves_head_and_shared_index_untouched() {
        let dir = scratch_repo("red");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        fs::write(dir.join("theirs.txt"), "theirs\n").unwrap();
        git(&dir, &["add", "theirs.txt"]);
        hook(&dir, "exit 1");
        let head = git(&dir, &["rev-parse", "HEAD"]);
        let staged = git(&dir, &["write-tree"]);

        let err = commit_paths(&dir, &["ours.txt".into()], "never").unwrap_err();
        assert!(format!("{err:#}").contains("fence failed"), "{err:#}");
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&dir, &["write-tree"]), staged);
    }

    #[test]
    fn concurrent_head_move_is_retried_not_overwritten() {
        let dir = scratch_repo("race");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        hook(
            &dir,
            "if [ ! -f race-moved ]; then \
               : > race-moved; \
               unset GIT_INDEX_FILE; \
               printf interloper > racer.txt; \
               git add racer.txt; \
               t=$(git write-tree); \
               o=$(git rev-parse HEAD); \
               n=$(printf race | git -c user.name=r -c user.email=r@x commit-tree \"$t\" -p \"$o\"); \
               git update-ref HEAD \"$n\" \"$o\"; \
             fi; exit 0",
        );
        let before = git(&dir, &["rev-parse", "HEAD"]);

        let sha = commit_paths(&dir, &["ours.txt".into()], "ours").unwrap();

        let parent = git(&dir, &["rev-parse", &format!("{sha}^")]);
        assert_ne!(
            parent, before,
            "the commit was re-parented onto the moved HEAD"
        );
        assert_eq!(git(&dir, &["log", "-1", "--format=%s", &parent]), "race");
        let files = tracked_in_head(&dir, &sha);
        assert!(files.contains(&"ours.txt".to_string()), "{files:?}");
        assert!(
            files.contains(&"racer.txt".to_string()),
            "the other lane's file was reverted by a stale tree: {files:?}"
        );
    }

    #[test]
    fn a_named_path_already_staged_is_refused_not_stripped() {
        let dir = scratch_repo("staged");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        git(&dir, &["add", "ours.txt"]);

        let err = commit_paths(&dir, &["ours.txt".into()], "never").unwrap_err();
        assert!(
            format!("{err:#}").contains("ours.txt"),
            "the error must name the path: {err:#}"
        );
        assert!(
            git(&dir, &["diff", "--cached", "--name-only"])
                .lines()
                .any(|line| line == "ours.txt"),
            "the neighbour's staged hunk must be untouched"
        );
    }

    #[test]
    fn a_stage_three_commits_old_is_tolerated_and_rewritten() {
        let dir = scratch_repo("stale-three");
        fs::write(dir.join("ours.txt"), "v1\n").unwrap();
        commit_paths(&dir, &["ours.txt".into()], "v1").unwrap();
        let old = git(&dir, &["rev-parse", "HEAD:ours.txt"]);
        for v in ["v2", "v3", "v4"] {
            fs::write(dir.join("ours.txt"), format!("{v}\n")).unwrap();
            commit_paths(&dir, &["ours.txt".into()], v).unwrap();
        }
        // The shared index still holds v1 — three commits behind HEAD.
        git(
            &dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{old},ours.txt"),
            ],
        );
        fs::write(dir.join("ours.txt"), "v5\n").unwrap();
        let sha = commit_paths(&dir, &["ours.txt".into()], "v5").unwrap();
        let new_blob = git(&dir, &["rev-parse", &format!("{sha}:ours.txt")]);
        assert_eq!(git(&dir, &["rev-parse", ":ours.txt"]), new_blob);
        assert_ne!(new_blob, old);
    }

    #[test]
    fn a_stale_stage_on_a_named_path_is_rewritten_to_the_new_blob() {
        let dir = scratch_repo("stale-sync");
        let parent_blob = git(&dir, &["rev-parse", "HEAD:base.txt"]);
        // Stage the parent's blob back into the shared index: a stale stage.
        git(
            &dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{parent_blob},base.txt"),
            ],
        );
        // The worktree carries the new content this commit lands.
        fs::write(dir.join("base.txt"), "new\n").unwrap();
        let sha = commit_paths(&dir, &["base.txt".into()], "new base").unwrap();
        let new_blob = git(&dir, &["rev-parse", &format!("{sha}:base.txt")]);
        assert_eq!(
            git(&dir, &["rev-parse", ":base.txt"]),
            new_blob,
            "the stale stage must point at the committed blob"
        );
        assert_ne!(new_blob, parent_blob, "the stale blob must be gone");
    }

    #[test]
    fn a_stale_delete_on_a_named_path_is_restored_to_the_new_blob() {
        let dir = scratch_repo("stale-del");
        // Stage a deletion (a stale delete) of the named path.
        git(&dir, &["rm", "--cached", "-q", "base.txt"]);
        fs::write(dir.join("base.txt"), "restored\n").unwrap();
        let sha = commit_paths(&dir, &["base.txt".into()], "restore").unwrap();
        let new_blob = git(&dir, &["rev-parse", &format!("{sha}:base.txt")]);
        assert_eq!(
            git(&dir, &["rev-parse", ":base.txt"]),
            new_blob,
            "the stale delete must be restored to the committed blob"
        );
    }

    /// bd-himh8: stale stages on OTHER paths are healed after the CAS, while
    /// an entry with unique staged content is left exactly alone and reported.
    #[test]
    fn stale_stages_outside_the_committed_paths_are_healed() {
        let dir = scratch_repo("heal-all");
        fs::write(dir.join("a.rs"), "a1\n").unwrap();
        fs::write(dir.join("b.rs"), "b1\n").unwrap();
        git(&dir, &["add", "a.rs", "b.rs"]);
        git(&dir, &["commit", "-qm", "c1"]);
        fs::write(dir.join("a.rs"), "a2\n").unwrap();
        git(&dir, &["add", "a.rs"]);
        git(&dir, &["commit", "-qm", "c2"]);

        let old_a = git(&dir, &["rev-parse", "HEAD^:a.rs"]);
        git(
            &dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{old_a},a.rs"),
            ],
        );
        // b.rs exists unchanged in HEAD and on disk: staged deletion is stale.
        git(&dir, &["rm", "--cached", "-q", "b.rs"]);
        // d.rs carries unique content nobody committed: must survive the heal.
        fs::write(dir.join("d.rs"), "unique\n").unwrap();
        git(&dir, &["add", "d.rs"]);
        fs::write(dir.join("c.rs"), "c\n").unwrap();

        let class = classify_staged(&Git::shared(&dir), &["c.rs".to_string()]).unwrap();
        assert!(class.stale.contains(&"a.rs".to_string()), "{class:?}");
        assert!(class.stale.contains(&"b.rs".to_string()), "{class:?}");
        assert_eq!(class.unclassifiable, vec!["d.rs".to_string()], "{class:?}");

        commit_paths(&dir, &["c.rs".to_string()], "add c").unwrap();
        let staged = git(&dir, &["diff", "--cached", "--name-only"]);
        assert!(
            !staged.lines().any(|p| p == "a.rs"),
            "a.rs still stale: {staged}"
        );
        assert!(
            !staged.lines().any(|p| p == "b.rs"),
            "b.rs still deleted: {staged}"
        );
        assert!(
            staged.lines().any(|p| p == "d.rs"),
            "unique stage was stripped: {staged}"
        );
    }

    /// bd-s3hzg: a holder/path pair in two projects is two notices.
    #[test]
    fn coordination_notice_key_is_project_scoped() {
        let coord = CoordinationCtx {
            requester: "ScarletCove".into(),
            project: "flywheel".into(),
            captain: "SlateHarbor".into(),
            notice_store: PathBuf::from("/tmp/unused.json"),
            mailer_bin: PathBuf::from("flywheel"),
            now_secs: 0,
        };
        let paths = vec!["src/a.rs".to_string()];
        let a = notice_key(&coord, "BoldCliff", &paths);
        let other = CoordinationCtx {
            project: "toron".into(),
            ..coord
        };
        assert_ne!(a, notice_key(&other, "BoldCliff", &paths));
        assert!(a.contains("flywheel") && a.contains("BoldCliff"));
    }

    /// bd-s3hzg: a holder/path pair in two projects is two notices.
    /// bd-ahkw1: parse the live/dead split toron already computes for us.
    #[test]
    fn reservation_conflicts_split_live_and_dead_holders() {
        let v = parse_reservations(
            r#"{"conflicts":[{"holders":["BoldCliff"],"reservation_ids":["r1"],"path":"x.rs"}],"soft_conflicts":[{"holders":["BuffyLark"],"reservation_ids":["r2"],"path":"old.rs"}]}"#,
        )
        .unwrap();
        assert_eq!(v.live, vec![("BoldCliff".into(), "r1".into())]);
        assert_eq!(v.dead, vec!["old.rs".to_string()]);
    }

    /// A fake toron owned by a TestSandbox: no process env is touched. `mode`
    /// is baked into the script, and every argv line is logged so a test can
    /// assert exactly which lease calls happened (bd-ahkw1 race fix).
    fn fake_toron(
        sandbox: &flywheel_test_sandbox::TestSandbox,
        mode: &str,
    ) -> (ReservationCtx, PathBuf) {
        let bin = sandbox.bin_dir().join("fake-toron");
        let log = sandbox.root().join("toron-calls.log");
        let answer = match mode {
            "hard" => {
                r#"{"conflicts":[{"holders":["BoldCliff"],"reservation_ids":["r1"],"path":"x.rs"}],"soft_conflicts":[]}"#
            }
            _ => {
                r#"{"conflicts":[],"soft_conflicts":[{"holders":["BuffyLark"],"reservation_ids":["r2"],"path":"old.rs"}]}"#
            }
        };
        let arm = match mode {
            "fail" => "echo 'toron down' >&2; exit 1 ;;".to_string(),
            "badjson" => "printf '%s' 'not json' ;;".to_string(),
            _ => format!("printf '%s' '{answer}' ;;"),
        };
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\ncase \"$1 $2\" in\n  \"reserve conflicts\") {arm}\nesac\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let ctx = ReservationCtx {
            pin: Some("ScarletCove".into()),
            project: "flywheel".into(),
            toron_bin: bin,
            coord: None,
        };
        (ctx, log)
    }

    /// bd-ahkw1: a live exclusive lease blocks the commit and names the
    /// holder plus the exact request-release command.
    #[test]
    fn a_live_reservation_refuses_the_commit() {
        let sandbox = flywheel_test_sandbox::TestSandbox::new();
        let (ctx, log) = fake_toron(&sandbox, "hard");
        let dir = scratch_repo("reserve-live");
        fs::write(dir.join("x.rs"), "x\n").unwrap();
        let err = commit_paths_checked(&dir, &["x.rs".into()], "blocked", false, false, &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("BoldCliff"), "{err}");
        assert!(err.contains("toron reserve request-release"), "{err}");
        assert!(err.contains("r1"), "{err}");
        let calls = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !calls.contains("reserve release"),
            "a refused commit freed a lease: {calls}"
        );
    }

    /// bd-ahkw1: a dead lease is cleared, and a successful commit frees the
    /// committer's own leases on exactly the committed paths.
    #[test]
    fn a_dead_lease_is_cleared_and_a_success_releases_own() {
        let sandbox = flywheel_test_sandbox::TestSandbox::new();
        let (ctx, log) = fake_toron(&sandbox, "dead");
        let dir = scratch_repo("reserve-dead");
        fs::write(dir.join("x.rs"), "x\n").unwrap();
        commit_paths_checked(&dir, &["x.rs".into()], "ok", false, false, &ctx).unwrap();
        let calls = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.contains("reserve force-release"),
            "dead lease survived: {calls}"
        );
        assert!(
            calls.contains("reserve release "),
            "own lease survived: {calls}"
        );
        assert!(calls.contains("x.rs"), "{calls}");
    }

    /// bd-ahkw1 hole 1: no pin is NOT consent — refuse and say how to escape.
    #[test]
    fn a_missing_pin_refuses_and_names_the_escape() {
        let sandbox = flywheel_test_sandbox::TestSandbox::new();
        let (mut ctx, _log) = fake_toron(&sandbox, "hard");
        ctx.pin = None;
        let dir = scratch_repo("reserve-nopin");
        fs::write(dir.join("x.rs"), "x\n").unwrap();
        let err = commit_paths_checked(&dir, &["x.rs".into()], "no pin", false, false, &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("AGENT_NAME"), "{err}");
        assert!(err.contains("--no-reservation-check"), "{err}");
    }

    /// bd-ahkw1 hole 2: toron failing or answering with garbage must refuse,
    /// never be read as "no conflicts".
    #[test]
    fn an_unanswerable_toron_refuses_instead_of_passing() {
        let sandbox = flywheel_test_sandbox::TestSandbox::new();
        let (ctx, _log) = fake_toron(&sandbox, "fail");
        let dir = scratch_repo("reserve-unreadable");
        fs::write(dir.join("x.rs"), "x\n").unwrap();
        let err = commit_paths_checked(&dir, &["x.rs".into()], "fail", false, false, &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("toron reserve conflicts failed"), "{err}");
        assert!(err.contains("--no-reservation-check"), "{err}");
    }

    /// bd-v7ucf: a fake mailer in the sandbox; every argv line is logged.
    fn fake_mailer(sandbox: &flywheel_test_sandbox::TestSandbox) -> PathBuf {
        let bin = sandbox.bin_dir().join("fake-mailer");
        let log = sandbox.root().join("mail.log");
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn coordination(
        sandbox: &flywheel_test_sandbox::TestSandbox,
        now_secs: u64,
    ) -> CoordinationCtx {
        CoordinationCtx {
            requester: "ScarletCove".into(),
            project: "flywheel".into(),
            captain: "SlateHarbor".into(),
            notice_store: sandbox.root().join("notices.json"),
            mailer_bin: fake_mailer(sandbox),
            now_secs,
        }
    }

    /// bd-v7ucf: B's commit hitting A's lease mails A once (naming the path
    /// and B), copies the captain, and repeats send nothing in the window.
    #[test]
    fn a_foreign_lease_mails_the_holder_once_and_copies_captain() {
        let sandbox = flywheel_test_sandbox::TestSandbox::new();
        let (mut ctx, _toron_log) = fake_toron(&sandbox, "hard");
        ctx.coord = Some(coordination(&sandbox, 1_000));
        let dir = scratch_repo("coord-mail");
        fs::write(dir.join("x.rs"), "x\n").unwrap();

        assert!(
            commit_paths_checked(&dir, &["x.rs".into()], "blocked", false, false, &ctx).is_err()
        );
        let log = sandbox.root().join("mail.log");
        let first = fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<&str> = first.lines().collect();
        assert_eq!(lines.len(), 2, "holder + captain, got: {first}");
        let holder_line = lines.iter().find(|l| l.contains("--to BoldCliff")).unwrap();
        assert!(holder_line.contains("--wake tasked"), "{holder_line}");
        assert!(
            holder_line.contains("x.rs"),
            "the path must be named: {holder_line}"
        );
        assert!(
            holder_line.contains("ScarletCove"),
            "the requester must be named: {holder_line}"
        );
        assert!(
            lines.iter().any(|l| l.contains("--to SlateHarbor")),
            "captain copy: {first}"
        );

        assert!(
            commit_paths_checked(&dir, &["x.rs".into()], "blocked", false, false, &ctx).is_err()
        );
        let second = fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(second, first, "no duplicate mail inside the window");
    }

    /// bd-v7ucf: past the window the notice is sent again.
    #[test]
    fn a_coordination_notice_is_repeated_after_the_window() {
        let sandbox = flywheel_test_sandbox::TestSandbox::new();
        let coord = coordination(&sandbox, 1_000);
        notify_holder(&coord, "BoldCliff", &["x.rs".to_string()]);
        let log = sandbox.root().join("mail.log");
        assert_eq!(
            fs::read_to_string(&log).unwrap_or_default().lines().count(),
            2
        );

        let later = CoordinationCtx {
            now_secs: 1_000 + NOTICE_WINDOW_SECS + 1,
            ..coord
        };
        notify_holder(&later, "BoldCliff", &["x.rs".to_string()]);
        assert_eq!(
            fs::read_to_string(&log).unwrap_or_default().lines().count(),
            4
        );
    }

    /// bd-ahkw1: the escape works but is never silent — the commit records it.
    #[test]
    fn the_recorded_escape_allows_a_commit_without_a_pin() {
        let sandbox = flywheel_test_sandbox::TestSandbox::new();
        let (mut ctx, _log) = fake_toron(&sandbox, "hard");
        ctx.pin = None;
        let dir = scratch_repo("reserve-escape");
        fs::write(dir.join("x.rs"), "x\n").unwrap();
        let sha =
            commit_paths_checked(&dir, &["x.rs".into()], "override", true, false, &ctx).unwrap();
        let msg = git(&dir, &["log", "-1", "--pretty=%B", &sha]);
        assert!(
            msg.contains("flywheel-reservation-check: skipped"),
            "the escape must be recorded in the commit: {msg}"
        );
    }

    fn install_real_hook(dir: &Path) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let hook_body = fs::read_to_string(root.join("scripts/hooks/pre-commit")).unwrap();
        // The real hook execs the repo's fence; a stub keeps this unit test
        // about the nonce gate, not a full build.
        fs::create_dir_all(dir.join("scripts/hooks")).unwrap();
        let fence = dir.join("scripts/fence.sh");
        fs::write(&fence, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fence, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = dir.join("scripts/hooks/pre-commit");
        fs::write(&path, &hook_body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(dir, &["config", "core.hooksPath", "scripts/hooks"]);
    }

    #[test]
    fn a_stale_shared_stage_is_reported() {
        let dir = scratch_repo("stale-stage");
        fs::write(dir.join("base.txt"), "changed\n").unwrap();
        git(&dir, &["add", "base.txt"]);
        git(&dir, &["commit", "-qm", "second"]);
        let old = git(&dir, &["rev-parse", "HEAD^:base.txt"]);
        git(
            &dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{old},base.txt"),
            ],
        );
        let warnings = stale_stage_warnings(&Git::shared(&dir), &["ours.txt".into()]).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("base.txt") && w.contains("stale")),
            "the stale stage must be reported: {warnings:?}"
        );
    }

    #[test]
    fn a_plain_git_commit_is_refused_with_the_flywheel_commit_hint() {
        let dir = scratch_repo("hook-refuse");
        install_real_hook(&dir);
        fs::write(dir.join("plain.txt"), "x\n").unwrap();
        git(&dir, &["add", "plain.txt"]);
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["commit", "-qm", "plain"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "plain git commit must be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("flywheel commit"), "{err}");
    }

    #[test]
    fn a_token_or_the_operator_escape_allows_the_hook() {
        let dir = scratch_repo("hook-escape");
        install_real_hook(&dir);
        for (tag, env, value) in [
            ("token", "FLYWHEEL_COMMIT_TOKEN", "nonce-1"),
            ("operator", "FLYWHEEL_COMMIT_OPERATOR", "1"),
        ] {
            fs::write(dir.join(format!("{tag}.txt")), "x\n").unwrap();
            git(&dir, &["add", format!("{tag}.txt").as_str()]);
            let out = StdCommand::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["commit", "-qm", tag])
                .env(env, value)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{tag}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn flywheel_commit_supplies_the_nonce_to_the_installed_hook() {
        let dir = scratch_repo("hook-fwc");
        install_real_hook(&dir);
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        let sha = commit_paths(&dir, &["ours.txt".into()], "ours").unwrap();
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]), sha);
    }

    /// bd-62ei8: a commit whose tree equals HEAD is refused (naming the
    /// unchanged paths) unless --allow-empty is given.
    #[test]
    fn an_unchanged_path_refuses_an_empty_commit_unless_allowed() {
        let dir = scratch_repo("empty-guard");
        fs::write(dir.join("a.rs"), "a\n").unwrap();
        commit_paths(&dir, &["a.rs".into()], "add a").unwrap();

        let err = commit_paths_mode(
            &dir,
            &["a.rs".into()],
            "no change",
            &ReservationCtx::skip(),
            ReservationMode::Skip,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("a.rs"),
            "error must name the unchanged path: {err}"
        );
        assert!(
            err.contains("empty"),
            "error must explain the refusal: {err}"
        );

        let sha = commit_paths_mode(
            &dir,
            &["a.rs".into()],
            "allowed empty",
            &ReservationCtx::skip(),
            ReservationMode::Skip,
            true,
        )
        .expect("--allow-empty must commit");
        assert!(!sha.is_empty());
    }

    #[test]
    fn absolute_and_parent_paths_are_refused() {
        let dir = scratch_repo("paths");
        assert!(commit_paths(&dir, &["/etc/passwd".into()], "no").is_err());
        assert!(commit_paths(&dir, &["../escape".into()], "no").is_err());
    }

    /// bd-uawqo: a REAL two-lander race. `commit_blobs` is called for l1; on
    /// its first splice it lands l2's commit, so l1's update-ref CAS fails and
    /// the callback is invoked again on the moved HEAD. The recorded event
    /// sequence goes through the pure TrackerCas converter.
    #[test]
    fn a_real_cas_retry_records_a_tracker_cas_trace() {
        use doorbell_rt::effect_trace::{TrackerCasTrace, TrackerEvent};

        let dir = scratch_repo("cas-conform");
        fs::write(dir.join("r1.txt"), "parent\n").unwrap();
        fs::write(dir.join("r2.txt"), "parent\n").unwrap();
        commit_paths(&dir, &["r1.txt".into(), "r2.txt".into()], "seed").unwrap();

        let mut events: Vec<TrackerEvent<'static>> = Vec::new();
        let mut calls = 0;
        let mut l2_calls = 0;
        let sha = commit_blobs(
            &dir,
            &["r1.txt".into(), "r2.txt".into()],
            "l1 closes r1",
            |parent| {
                calls += 1;
                if calls == 1 {
                    events.push(TrackerEvent::Refresh {
                        lander: "l1",
                        ver: 0,
                        splice: vec!["r1"],
                    });
                    // The other lander moves HEAD between our read and CAS.
                    commit_blobs(&dir, &["r2.txt".into()], "l2 closes r2", |_| {
                        l2_calls += 1;
                        if l2_calls == 1 {
                            events.push(TrackerEvent::Refresh {
                                lander: "l2",
                                ver: 0,
                                splice: vec!["r2"],
                            });
                        }
                        Ok(vec![("r2.txt".into(), b"closed by l2\n".to_vec())])
                    })
                    .expect("l2 lands");
                    events.push(TrackerEvent::CasOk { lander: "l2" });
                    events.push(TrackerEvent::CasFail { lander: "l1" });
                } else if calls == 2 {
                    // The re-read sees the moved HEAD; re-splice BOTH rows.
                    events.push(TrackerEvent::Refresh {
                        lander: "l1",
                        ver: 1,
                        splice: vec!["r1", "r2"],
                    });
                }
                // A third call only refreshes the shared index; no event.
                let other = format!("{}\n", git(&dir, &["show", &format!("{parent}:r2.txt")]));
                Ok(vec![
                    ("r1.txt".into(), b"closed by l1\n".to_vec()),
                    ("r2.txt".into(), other.into_bytes()),
                ])
            },
        )
        .expect("l1 lands after a retry");
        events.push(TrackerEvent::CasOk { lander: "l1" });
        assert!(calls >= 2, "the CAS must retry at least once: {calls}");

        // Real outcome: both closed rows are in the final HEAD.
        let head = git(&dir, &["show", &format!("{sha}:r1.txt")]);
        assert_eq!(head.trim(), "closed by l1");
        let other = git(&dir, &["show", &format!("{sha}:r2.txt")]);
        assert_eq!(other.trim(), "closed by l2");

        let mut trace = TrackerCasTrace::new();
        for event in &events {
            trace.observe(event);
        }
        let last = trace.observations.last().unwrap();
        assert_eq!(last["headRows"], serde_json::json!(["r1", "r2"]));
        if let Ok(path) = std::env::var("CAS_TRACE_OUT") {
            std::fs::write(path, trace.to_jsonl()).expect("write CAS trace");
        }
    }
}
