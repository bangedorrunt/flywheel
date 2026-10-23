//! In-process agent-mail shim (ADR-0003 Decision 3A/3B, bd-o8j.2/.3).
//!
//! `src/mail.rs` (the HTTP JSON-RPC client to the agent-mail MCP server) was
//! deleted. This shim keeps the `Mail` surface call sites depend on (method
//! signatures + JSON shapes) and routes every operation through one of two
//! backends:
//!
//! - IN-PROCESS (default): the `toron_core::mailbox::Mailbox` (InProcessBus +
//!   SQLite index, an operator key attached so `ensure_project`/send/register
//!   work) — zero HTTP, one mailbox per process, built eagerly at the first
//!   `Mail` construction (`init_mailbox`, se2.1 Phase A).
//! - REMOTE-DAEMON (OPTIONAL, bd-o8j.3): a standalone `toron serve --mcp-http
//!   <addr>` over Streamable HTTP (MCP), driven by a minimal MCP client
//!   (`crate::mcp_http::McpClient`). Selected when `[mail.toron] url` is set
//!   (`Config::toron_mail_config`); the legacy `[agent_mail] url` is never a
//!   routing source (bd-euav, ADR-0003 V2/V3).
//!
//! flywheel main.rs dispatch is sync, so each in-process op is `block_on`'d on
//! a shared tokio runtime (the remote path is ureq-sync, no runtime needed).
//! The pane-identity bridge helpers (`project_hash`,
//! `write_pane_identity`, …) are LOCAL filesystem ops regardless of backend —
//! they bridge flywheel's own herdr panes, which only the local process sees.
//!
//! Known limitations (reported, not papered over):
//! - toron message ids are hex event ids, not u64 sequence numbers;
//! - `ensure_product` needs an operator key; `demo_mailbox` has none.
//!
//! governed-by: ADR-0003

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::runtime::Runtime;

use toron_core::mailbox::{
    ContactPolicy, FetchInbox, ListAgentsOptions, RequestContact, RespondContact, SearchMessages,
    SendMessage, overlaps,
};
use toron_core::nostr::identity::AgentProfile;
use toron_mcp::ToJson;

use crate::config::MailConfig;
use crate::mcp_http::{McpClient, ServerKind};

/// Shared tokio runtime: ONE per process (c3b.1 / se2.1 Phase A), built by
/// `init_mailbox` — never a fresh `Runtime::new` per op. Construction failure
/// propagates as an error at `Mail::from_config`, never a panic mid-op. The
/// cell holds the runtime, not a `Result`: bd-tg4n, a memoised failure would
/// outlive its own cause and take mail down for the rest of the process.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The single shared in-process mailbox every `Mail` instance operates on
/// (ADR-0003 Decision 3A). Mirrors `toron_mcp::demo_mailbox` (InProcessBus +
/// SQLite index) but WITH an operator key, so write paths (`ensure_project`,
/// send, register) work — the demo mailbox is operator-less and would reject
/// every `ensure_project` on first use. Root dirs follow the durable D3
/// resolution (bd-t9ey): `$XDG_DATA_HOME/toron` / `~/.local/share/toron` by
/// default, `TORON_DATA_DIR` / `TORON_MCP_DATA_DIR` override.
static MAILBOX: OnceLock<toron_core::mailbox::Mailbox> = OnceLock::new();

pub(crate) fn runtime() -> Result<&'static Runtime> {
    init_once(&RUNTIME, &INIT_GATE, || {
        Runtime::new().map_err(|e| format!("failed to initialize tokio runtime: {e}"))
    })
    .map_err(|e| anyhow!("{e}"))
}

/// Serialises the two one-time builds. The fast path never takes it; only the
/// cold build does, and there are exactly two of those per process.
static INIT_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Memoise a successful build; never memoise a failure.
///
/// bd-tg4n: `OnceLock::get_or_init` stores the `Err` PERMANENTLY, so one failed
/// build made mail unavailable for the rest of the process even after the cause
/// was gone — the error outlived the condition and read as a bug somewhere else.
/// `get_or_try_init` is the obvious fix and is UNSTABLE (`once_cell_try`), so
/// this is the stable equivalent: the cell holds `T`, not `Result<T, String>`,
/// and a builder that returns `Err` returns before the `set`. The next call
/// therefore retries and a transient failure heals itself.
///
/// WHAT THIS DOES NOT DO: it does not fix the `wake_identity_tests` flake it was
/// found through. Two `register_spawned_agent_*` tests still go red together
/// roughly 2 runs in 8 under the module filter (measured after this change), so
/// the trigger is NOT a stale cached error — a retry re-resolves whatever caused
/// the failure and fails again. Two earlier theories were measured and
/// REJECTED rather than shipped: a `test_state_home()` fix on the two tests
/// (3/8 red), and a `cleanup_test_data_dir` that refuses to delete the dir the
/// singleton was built on (5/10 red, worse). Deleting the temp dir is therefore
/// not the trigger. The chain up to "a test can poison the process-global
/// mailbox" is verified link by link; the trigger itself is still open.
///
/// The fast path is a lock-free `get`. `gate` is taken only while building, and
/// the second `get` after acquiring it means a thread that lost the race reads
/// the winner's value instead of building a duplicate.
///
/// Takes the cell rather than reaching for the statics so a test can drive a
/// FRESH one; the process-global cells are already initialised by the time any
/// unit test runs, so testing through them would prove nothing.
fn init_once<T: Send + Sync + 'static>(
    cell: &'static OnceLock<T>,
    gate: &std::sync::Mutex<()>,
    build: impl FnOnce() -> Result<T, String>,
) -> Result<&'static T, String> {
    if let Some(v) = cell.get() {
        return Ok(v);
    }
    let _held = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(v) = cell.get() {
        return Ok(v); // lost the race; the winner's value is the answer
    }
    let built = build()?; // a failure returns HERE, leaving the cell empty
    let _ = cell.set(built);
    cell.get()
        .ok_or_else(|| "init_once: value lost to a concurrent build".to_string())
}

/// Build the shared runtime + mailbox eagerly, propagating failures as errors
/// (c3b.1: no panic on runtime/threadpool construction failure). Called from
/// `Mail::from_config`; every in-process op then reads the pre-built statics.
fn init_mailbox() -> Result<()> {
    let _rt = runtime()?;
    match init_once(&MAILBOX, &INIT_GATE, build_mailbox_once) {
        Ok(_) => Ok(()),
        Err(e) => bail!("flywheel mail: in-process mailbox: {e}"),
    }
}

/// Build the shared mailbox once, from ANY thread.
///
/// `RUNTIME.block_on` panics when the caller is already driving async tasks,
/// and two real callers are: the approval-drainer thread (which calls
/// `Mail::from_config` from inside its own `rt.block_on`) and the cron daemon.
/// Both used to die at exactly this line with "Cannot start a runtime from
/// within a runtime", leaving its thread dead — for the drainer that means
/// parked approvals were never resolved. Isolation is therefore chosen at the
/// call, never assumed: inside a runtime the build runs on a dedicated thread.
///
/// The build is still driven by the SHARED runtime, never a throwaway one:
/// `InProcessBus::open` spawns the bus prune tick (bd-741i) on it, and dropping
/// that runtime would cancel the tick.
fn build_mailbox_once() -> Result<toron_core::mailbox::Mailbox, String> {
    let Ok(rt) = runtime() else {
        return Err("failed to initialize tokio runtime".to_string());
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(move || rt.block_on(build_mailbox()))
            .join()
            .map_err(|_| "mailbox init thread panicked".to_string())?
            .map_err(|e| format!("{e:#}"));
    }
    rt.block_on(build_mailbox()).map_err(|e| format!("{e:#}"))
}

/// ADR-0037: pins are ONE namespace across projects. True when `pin` is
/// bound in a project other than `project` in the in-process store (a
/// remote mailbox has no such read; callers skip this check there).
pub fn pin_bound_elsewhere(project: &str, pin: &str) -> bool {
    if init_mailbox().is_err() {
        return false;
    }
    block_on(mailbox().pin_bindings_elsewhere(project, pin))
        .map(|b| !b.is_empty())
        .unwrap_or(false)
}

/// The shared in-process mailbox. Only reachable after a successful
/// `init_mailbox` (via `Mail::from_config`) — an unreachable panic here means
/// a call path skipped construction, which is a programming error.
///
/// bd-tg4n: there is no `Some(Err(..))` arm any more, and that is the point of
/// the change. The cell used to hold `Result<Mailbox, String>`, so a failed
/// build was readable here FOREVER — an error that outlived its own cause. A
/// failure now never reaches the cell; it surfaces once, at `Mail::from_config`,
/// and the next call retries.
pub fn mailbox() -> &'static toron_core::mailbox::Mailbox {
    match MAILBOX.get() {
        Some(mb) => mb,
        None => unreachable!("mailbox() before Mail::from_config"),
    }
}

/// The in-process mailbox for the comms inbox (ADR-0041): the shared durable
/// root's index, whatever mail transport the config selects. Builds it once.
pub fn comms_mailbox() -> Result<&'static toron_core::mailbox::Mailbox> {
    #[cfg(test)]
    pin_test_mail_root_once();
    init_mailbox()?;
    Ok(mailbox())
}

/// Non-panicking mailbox access for background ticks (GS-006 Ack-SLA).
pub fn try_mailbox() -> Option<&'static toron_core::mailbox::Mailbox> {
    MAILBOX.get()
}

/// The data dir the process-global mailbox was actually BUILT on, or `None` if
/// it has not been built yet (bd-tg4n).
///
/// This is [`BUILT_ROOTS`], NOT a fresh [`mailbox_roots`] call. Those differ
/// exactly when it matters: `BUILT_ROOTS` is memoised when the singleton is
/// first constructed, so by the time a later test asks, `TORON_DATA_DIR` may
/// point somewhere else entirely. A first attempt at the cleanup helper
/// compared against the live env and made the flake WORSE (5-in-10 red against
/// a 2-in-8 baseline) for precisely that reason — it deleted the singleton's
/// own directory while believing it was someone else's.
#[cfg(test)]
pub fn built_mail_root() -> Option<&'static std::path::Path> {
    BUILT_ROOTS.get().map(|(dir, _)| dir.as_path())
}

/// bd-tg4n: remove a test's temp dir, UNLESS it is the directory the
/// process-global mailbox was built on.
///
/// The mailbox is a process-wide singleton on a per-process SQLite store, and
/// `pin_test_mail_root_set` only pins a root when NEITHER `TORON_MCP_DATA_DIR`
/// nor `TORON_DATA_DIR` is set — so a test that sets `TORON_DATA_DIR` wins, and
/// whichever test first reaches `Mail::from_config` owns the store for every
/// later test in the process. Its closing `remove_dir_all` then pulls the
/// directory out from under live pooled connections.
///
/// The measured symptom, verbatim from a failing run:
/// `mail error subscription rejected: replay failure: error returned from
/// database: (code: 1) no such table: bus_events` — the pool is still open but
/// the store underneath it is gone. Every `register_spawned_agent*`,
/// `bind_pane_mail_identity_stamps` and `add_panes_get_distinct_*` test that
/// shared that process then returned `None` and failed on its `.expect`,
/// roughly 2 runs in 8 under the module filter and 0 in 6 with
/// `--test-threads=1`.
///
/// bd-gpaa: the equality guard this shipped with could never fire. Callers hand
/// it the DURABLE ROOT they set `TORON_DATA_DIR` to, but [`built_mail_root`]
/// returns the Mailbox DATA dir — `{root}/projects` (see [`mailbox_data_dir`]).
/// Two different paths, so the guard was a permanent no-op that read like a fix.
/// [`dir_holds_live_store`] is at-or-under for that reason.
#[cfg(test)]
pub fn cleanup_test_data_dir(dir: &std::path::Path) {
    if dir_holds_live_store(built_mail_root(), dir) {
        return;
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Would removing `dir` take the process-global mailbox's store with it?
///
/// At-or-under, not equality: `built` is `{root}/projects` and callers pass
/// `{root}`, so only the ancestor form fires. Component-wise `starts_with`, so
/// a sibling sharing a string prefix (`fw-bind-stamp-XY` against
/// `fw-bind-stamp-X`) is NOT protected. `None` means no mailbox exists yet, so
/// nothing is protected — the dir is still removed.
#[cfg(test)]
fn dir_holds_live_store(built: Option<&std::path::Path>, dir: &std::path::Path) -> bool {
    matches!(built, Some(root) if root.starts_with(dir))
}

/// Root dirs for the in-process mailbox (se2.1 Phase B, bd-t9ey): the
/// durable D3 root via `toron_core::config::resolve_data_root` (ADR-0007
/// D3) — `TORON_DATA_DIR` > `$XDG_DATA_HOME/toron` > `~/.local/share/toron`
/// > relative `toron-data`. NEVER a per-pid temp root: separate flywheel
/// > invocations share one durable mailbox. The `TORON_MCP_DATA_DIR` alias
/// > stays as the explicit demo/test isolation override. The SQLite index
/// > lives at the root. Returns (data_dir, sqlite db path).
///
/// **bd-enov (F1/F4): ONE index, toron's.** The db is `{root}/index.sqlite`
/// — the very file `ToronPaths::index_path()` resolves — not a private
/// `flywheel-mail.sqlite`. Two files meant two `agents` tables: the same pin
/// registered on both planes held two npubs (empirically: `DustySparrow`
/// @2428bb8e in index.sqlite, @514dc258 in flywheel-mail.sqlite), `whois`
/// resolved on one plane while `reserve` bound the shared key file, and the
/// strict guard attributed leases through a roster that disagreed with the
/// key it bound. Keys already landed under the shared `projects/<slug>/`
/// tree; only the index forked. Unify it.
pub fn mailbox_roots() -> (PathBuf, PathBuf) {
    // `TORON_DATA_DIR` wins (blank counts as unset — flywheel's old trim
    // semantics); `TORON_MCP_DATA_DIR` is the demo/test isolation alias;
    // otherwise the durable D3 default via resolve_data_root.
    let toron_data_dir = std::env::var("TORON_DATA_DIR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    // bd-enov: toron's shared index (`ToronPaths::index_path`), NOT a
    // private flywheel mail store — the identity plane is one roster.
    let db_at = |root: &Path| root.join("index.sqlite");
    if toron_data_dir.is_none()
        && let Ok(dir) = std::env::var("TORON_MCP_DATA_DIR")
        && !dir.trim().is_empty()
    {
        let root = PathBuf::from(dir.trim());
        return (mailbox_data_dir(&root), db_at(&root));
    }
    let env = toron_core::config::DataRootEnv {
        toron_data_dir,
        xdg_data_home: env_non_empty_os("XDG_DATA_HOME"),
        home: env_non_empty_os("HOME"),
        // bd-fize: the toron side grew this field so a cargo TEST binary
        // resolves a temp root instead of the operator's live store. The
        // flywheel's caller must say the same thing, or a test writes into
        // the operator's index.
        test_process: toron_core::config::is_test_process(),
    };
    let root = toron_core::config::resolve_data_root(&env);
    (mailbox_data_dir(&root), db_at(&root))
}

/// Root dirs of the LIVE in-process mailbox: recorded once by
/// `build_mailbox`, never re-resolved.
///
/// `mailbox_roots()` reads process env on EVERY call, so a reader that probes
/// the store by re-resolving can disagree with a mailbox built earlier — env
/// is process-global and several tests flip `TORON_DATA_DIR` /
/// `TORON_MCP_DATA_DIR` / `XDG_DATA_HOME` under the env lock. That is how the
/// product-digest identity fold reported "no key for `{name}` in any member —
/// refusing to mint" for a pin it had just registered: the key sat in the
/// pinned root while the probe resolved a flipped one. Readers that must
/// agree with the live store take these dirs; readers that must mirror an
/// EXTERNAL writer's own env resolution (presence, toron parity) keep
/// `mailbox_roots()`. Falls back to a fresh resolution while no mailbox
/// exists (nothing to agree with yet).
static BUILT_ROOTS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

/// The live mailbox's own root dirs (see [`BUILT_ROOTS`]).
pub(crate) fn built_mailbox_roots() -> (PathBuf, PathBuf) {
    BUILT_ROOTS.get().cloned().unwrap_or_else(mailbox_roots)
}

/// bd-hdec: the Mailbox data dir is the DURABLE ROOT'S `projects/` segment
/// (`{root}/projects/`), matching toron's MCP/CLI/bootstrap exactly
/// (toron-mcp passes `ToronPaths::projects_dir()` as the Mailbox data dir).
/// The shim previously passed the bare root, so flywheel-minted keys landed
/// at `{root}/{slug}/agents/...` — a legacy path-slug layout toron's whois
/// never resolves, and the documented `mail register` + whois flow minted
/// unresolvable pins. Project roots now land at the canonical
/// `{root}/projects/{slug}/agents/<name>/key.ncryptsec` for BOTH tools.
/// Legacy strays under the bare root keep resolving read-only through
/// toron's alias-reconciliation walk (bd-f5tt); register reuses them via
/// `alt_agent_key`, so no identity forks.
fn mailbox_data_dir(durable_root: &Path) -> PathBuf {
    durable_root.join(toron_core::config::PROJECTS_RELATIVE)
}

/// Canonical toron presence projection path — the file `toron watch`/`toron
/// presence` appends kind-20001 rows to (bd-m5ym, toron `plane.rs`
/// `presence_path`). Toron lifts the mailbox `data_dir` (`<root>/projects`)
/// back to the durable root via `toron_core::config::mailbox_root`, so rows
/// land at `<root>/presence.jsonl` — **never** `<root>/projects/presence.jsonl`.
///
/// Reading the projects-level path yields a dead file (nothing writes it),
/// which starves the doorbell presence map: every registry pin reads
/// Unknown-presence and the reducer's filter holds every wake for digest, so
/// the ring silently never fires. Both the embedded host and the `why`
/// replay fold read this one path.
pub fn presence_path() -> PathBuf {
    toron_core::config::mailbox_root(&mailbox_roots().0).join("presence.jsonl")
}

/// Mirror of toron's `env_non_empty` for the ambient os-string vars: empty
/// counts as unset, so `resolve_data_root` never sees a blank root.
fn env_non_empty_os(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
/// bd-zncc: test-process mail hermeticity. `Mail::from_config(None)` builds
/// the in-process mailbox on the DURABLE shared root (~/.local/share/toron)
/// — correct for production, wrong for tests: agent rows and key files
/// accumulate across `cargo test` runs until npubs diverge and gift-wrap
/// unwrap stops projecting (deliveries report count=N, inboxes stay empty).
/// Pin a per-process temp root unless the test explicitly set one.
#[cfg(test)]
pub fn pin_test_mail_root_once() {
    static PINNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if PINNED.get().is_some() {
        return;
    }
    // Same-thread re-entry: tests routinely hold lock_env ACROSS
    // from_config — a plain lock() here deadlocks the suite. try-lock and
    // mutate under it when free; when the caller already holds it, the
    // caller's guard IS the serialization — mutate directly (idempotent,
    // same per-pid value).
    let _guard = crate::config::ENV_LOCK.try_lock().ok();
    pin_test_mail_root_set();
    let _ = PINNED.set(());
}

#[cfg(test)]
fn pin_test_mail_root_set() {
    if std::env::var_os("TORON_MCP_DATA_DIR").is_none()
        && std::env::var_os("TORON_DATA_DIR").is_none()
    {
        let dir = std::env::temp_dir().join(format!("fw-mail-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        unsafe { std::env::set_var("TORON_MCP_DATA_DIR", &dir) };
    }
}

/// ADR-0037 W2 test fixtures: one pin holds ONE key across every project of
/// the shared per-process test store. A fixture registering a pin that
/// another test already registered in another project reuses that key with
/// proof of possession — the in-process equivalent of `toron agent
/// bootstrap` — instead of minting a second key (which W2 refuses). Held
/// under one lock so two tests never race the check-then-mint.
#[cfg(test)]
pub(crate) static FIXTURE_PIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn fixture_register_async(
    mb: &toron_core::mailbox::Mailbox,
    project: &str,
    profile: &AgentProfile,
) -> Result<toron_core::mailbox::RegisteredAgent> {
    let _g = FIXTURE_PIN_LOCK.lock().await;
    match mb.pin_reuse_source(project, &profile.name, None).await? {
        Some(from) => {
            let proof = mb
                .prove_pin_possession(&from, &profile.name, project)
                .await?;
            mb.register_agent_reusing(project, profile, &from, &proof)
                .await
        }
        None => mb.register_agent(project, profile).await,
    }
}

/// Blocking [`fixture_register_async`] on the shared in-process mailbox.
#[cfg(test)]
pub(crate) fn fixture_register(
    project: &str,
    profile: &AgentProfile,
) -> Result<toron_core::mailbox::RegisteredAgent> {
    pin_test_mail_root_once();
    init_mailbox()?;
    block_on(fixture_register_async(mailbox(), project, profile))
}

#[cfg(test)]
impl Mail {
    /// Test-fixture registration (see [`fixture_register`]).
    pub(crate) fn register_fixture(
        &self,
        project_key: &str,
        name: &str,
        program: &str,
        model: &str,
        task_description: &str,
    ) -> Result<Value> {
        let mut profile = AgentProfile::new(name);
        profile.program = Some(program.to_string());
        profile.model = Some(model.to_string());
        profile.task_description = Some(task_description.to_string());
        Ok(fixture_register(project_key, &profile)?.to_json())
    }
}

/// bd-vhia: `sqlx::migrate!` reports a store written by a NEWER embedded toron
/// as "migration 10 was previously applied but is missing in the resolved
/// migrations". That error names neither the store nor the remedy, and the
/// reporter of bd-vhia lost time to it (they reached for `toron mail send` as a
/// workaround). Name both, and never touch the store.
pub(crate) fn migrate_skew_hint(store: &str, path: &str) -> String {
    format!(
        "the {store} store at {path} carries migrations this binary does not — it was written by a \
         newer embedded toron, or by the standalone `toron` on the same revision (bd-vhia). \
         Remedy: rebuild and reinstall this binary at the current revision (`mbx \
         +nightly-aarch64-apple-darwin install --path . --force`), or run the standalone `toron \
         mail ...` against the same project. The store is left untouched."
    )
}

#[cfg(test)]
mod bd_vhia_migrate_hint_tests {
    use super::migrate_skew_hint;

    #[test]
    fn the_hint_names_the_store_the_path_and_the_remedy() {
        let h = migrate_skew_hint("mail", "/tmp/x/index.sqlite");
        assert!(h.contains("mail"), "names which store: {h}");
        assert!(h.contains("/tmp/x/index.sqlite"), "names the path: {h}");
        assert!(
            h.contains("install --path . --force") && h.contains("toron mail"),
            "names both remedies: {h}"
        );
        assert!(
            h.contains("left untouched"),
            "says the store is not destroyed: {h}"
        );
    }
}

async fn build_mailbox() -> anyhow::Result<toron_core::mailbox::Mailbox> {
    // Record the roots this store is built from: every reader that probes the
    // live store takes the same pair (BUILT_ROOTS).
    let (data_dir, db) = BUILT_ROOTS.get_or_init(mailbox_roots).clone();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("create mail data dir {}", data_dir.display()))?;
    // bd-enov: a legacy private store means this process used to fork the
    // identity plane. Name it (non-destructively — the file is left in
    // place for the operator); never migrate silently.
    // bd-d65y: stderr noise trains operators to ignore real warnings —
    // show only under FLYWHEEL_VERBOSE (documented in .env.example).
    let legacy = db.with_file_name("flywheel-mail.sqlite");
    if legacy.exists()
        && !std::env::var("FLYWHEEL_VERBOSE")
            .unwrap_or_default()
            .is_empty()
    {
        eprintln!(
            "flywheel mail: note — the legacy private store {} is superseded by the shared \
             toron index {} (bd-enov). It is left untouched for recovery; nothing reads it.",
            legacy.display(),
            db.display()
        );
    }
    let db_path = db.to_string_lossy().into_owned();
    let pool = toron_core::index::schema::connect(&db_path).await?;
    toron_core::index::schema::migrate(&pool)
        .await
        .with_context(|| migrate_skew_hint("mail", &db_path))?;
    let bus = toron_core::bus::InProcessBus::open(
        pool.clone(),
        toron_core::bus::in_process::InProcessBusConfig::default(),
    )
    .await?;
    let operator = toron_core::keys::generate();
    Ok(
        toron_core::mailbox::Mailbox::new(std::sync::Arc::new(bus), data_dir, Some(operator))
            .with_index_pool(pool),
    )
}

/// Run one async mailbox operation on the shared tokio runtime (flywheel
/// main.rs dispatch is sync; each op is a short-lived, sequential block_on).
/// The runtime is pre-built by `Mail::from_config` — the expect below is
/// unreachable post-init (c3b.1: no `Runtime::new` per op).
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    runtime()
        .expect("runtime initialized by Mail::from_config before any op")
        .block_on(f)
}

/// Default ack SLA in seconds (`ACK_SLA_DEFAULT_SECS`): the twin of toron's
/// own `check_ack_sla_expired` default (`sla_secs.unwrap_or(30 * 60)`) and of
/// the CLI's `toron ack-sla --sla-secs`. One number, named twice on purpose:
/// drift here would mean flywheel reports a different overdue set than the
/// wake-emitting scan that pops the ring.
pub const ACK_SLA_DEFAULT_SECS: u64 = 30 * 60;

/// Is one inbox row ack-overdue for the agent that read it? (bd-mail-ack-sla
/// -tracking subtask 4, wired bd-blocked-escalation-protocol.)
///
/// The predicate reads only fields the projection already carries, so the
/// in-process and remote backends share ONE definition:
/// - `ack_required` — the sender asked for an ack;
/// - `importance ∈ {high, urgent}` — toron's SLA scope (`check_ack_sla_expired`
///   scans exactly these);
/// - `read_ts_us` null — per-RECIPIENT read state (`message_reads`, migration
///   0002). toron's own scan counts reads for ANY recipient, so this door is
///   the stricter one: a co-recipient's read can never mask this agent's miss.
///   The projection records read and ack in the same row, and toron's SLA
///   scan treats either as handled (`fetch_inbox` itself never writes one) —
///   so "has a receipt" is the honest available fact, never "never fetched";
/// - age ≥ SLA — `created_ts_us` is i64 micros, same clock as `now_us`.
///
/// `sla_secs = 0` is legal and meaningful: "any unacked ack-required mail".
/// The blocked-escalation gate uses it (an ack at ANY age is the handling
/// fact there), which is why the SLA is a parameter and not a constant.
pub fn inbox_row_ack_overdue(row: &Value, now_us: i64, sla_secs: u64) -> bool {
    if row.get("ack_required").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    if !row
        .get("importance")
        .and_then(Value::as_str)
        .map(is_urgent)
        .unwrap_or(false)
    {
        return false;
    }
    // Absent or null `read_ts_us` both mean "no receipt" — unread is the
    // honest default when the projection omits the field.
    if row.get("read_ts_us").is_some_and(|v| !v.is_null()) {
        return false;
    }
    let created_us = row
        .get("created_ts_us")
        .and_then(Value::as_i64)
        .unwrap_or(now_us);
    now_us.saturating_sub(created_us) >= (sla_secs as i64).saturating_mul(1_000_000)
}

/// Filter bag for the ack-overdue read (bd-mail-ack-sla-tracking subtask 4).
/// Every field is optional: `AckOverdueFilter::default()` = toron's 30-min
/// SLA, all threads.
#[derive(Debug, Clone, Default)]
pub struct AckOverdueFilter {
    /// Age threshold in seconds. `None` = [`ACK_SLA_DEFAULT_SECS`].
    pub sla_secs: Option<u64>,
    /// Restrict the read to one thread — the blocked-escalation gate reads
    /// the `health-<agent>` lane so unrelated mail cannot hold the ladder up.
    pub thread_id: Option<String>,
}

/// Full inbox filter bag (bd-mail-ack-sla-tracking). Callers construct this
/// instead of passing 5 booleans positionally. Constructed by the toron
/// backend (`mail-toron` feature); suppressed as dead in the default build.
#[cfg_attr(not(feature = "mail-toron"), allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub struct MailInboxFilter {
    /// Keep only messages with importance ∈ {high, urgent} (c3b.3 — applied
    /// by the toron projection; rows carry `importance`).
    pub urgent_only: bool,
    pub unread_only: bool,
    /// Keep only ack-overdue rows — `ack_required` + high/urgent + no
    /// per-recipient receipt + past the default SLA (see
    /// [`inbox_row_ack_overdue`]). Applied as a post-filter over the rows the
    /// projection returns, on BOTH backends; rows carry every field it needs.
    pub ack_overdue_only: bool,
    pub include_bodies: bool,
    pub limit: u32,
    /// Keep only messages with `created_ts_us` strictly newer than this
    /// RFC3339 timestamp (c3b.3 — applied by the toron projection), or the
    /// literal `last` (bd-zd2v — the daemon-side cursor, resolved + bumped
    /// by the toron projection, never parsed as a timestamp here).
    pub since_ts: Option<String>,
    /// Keep only messages in this thread (bd-harvest-thread-filter-yp9r).
    /// Post-unwrap filter on the toron projection; `None` = no thread bound.
    pub thread_id: Option<String>,
}

/// c3b.3: parse an RFC3339 `since_ts` filter into epoch micros (the unit
/// `InboxMessage.created_ts_us` uses). A bad timestamp is an error — never a
/// silent "filter ignored".
fn parse_since_ts_us(ts: &str) -> Result<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp_micros())
        .map_err(|e| anyhow!("inbox since_ts {ts:?} is not RFC3339: {e}"))
}

/// c3b.3: `urgent_only` membership — importance ∈ {high, urgent}.
fn is_urgent(importance: &str) -> bool {
    matches!(importance, "high" | "urgent")
}

/// Filter bag for `Mail::search` (review-fix F8: folds the 10 positional args
/// into a struct — the trait's `SearchFilter` is the backend-agnostic twin).
/// Constructed by the toron backend (`mail-toron` feature); suppressed as dead
/// in the default build.
#[cfg_attr(not(feature = "mail-toron"), allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub struct MailSearchFilter {
    pub sender: Option<String>,
    pub importance: Option<String>,
    pub thread_id: Option<String>,
    /// Applied to the toron `search_messages` args (see `backend/toron.rs`).
    pub date_start: Option<String>,
    /// Applied to the toron `search_messages` args (see `backend/toron.rs`).
    pub date_end: Option<String>,
    pub ranking: Option<String>,
    pub include_body_md: bool,
}

/// Which backend a `Mail` routes through.
#[derive(Debug, Clone)]
enum BackendKind {
    /// Integrated toron mailbox in this process (the default, bd-o8j.2).
    InProcess,
    /// Standalone `toron serve --mcp-http` daemon over Streamable HTTP.
    Remote(McpClient),
}

/// Agent-mail facade. Every public method maps 1:1 onto either a
/// `toron_core::mailbox::Mailbox` call (in-process) or a toron MCP `tools/call`
/// against a remote daemon (see the deleted `src/mail.rs` for the surface this
/// replaces). JSON return shapes are identical across backends.
///
/// This is the TORON facade (ADR-0003 V2). Its full method surface is consumed
/// by the feature-gated toron backend (`mail-toron`) plus the CLI mail
/// commands; in the default build (toron backend off) the ops the CLI doesn't
/// call are dead — suppressed so the default build stays warning-free.
#[cfg_attr(not(feature = "mail-toron"), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct Mail {
    /// The agent identity this facade sends AS / reads the inbox for.
    pub name: String,
    /// Active backend (in-process default; remote when `url` is set).
    backend: BackendKind,
}

impl Mail {
    /// Set the agent identity this facade sends AS / reads the inbox for;
    /// self so `Mail` stays `&self`-callable afterwards (c3b.2 — no interior
    /// mutability needed; `name` is never written through `&self`).
    pub fn with_name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    /// Construct the mail facade from `Config::toron_mail_config()` — shared
    /// identity `name` from `[agent_mail]`, transport `url` from
    /// `[mail.toron]` only. Picks the backend: `cfg.url` Some → remote-daemon
    /// (bd-o8j.3); None/unset → integrated in-process mailbox (default). The
    /// config section stays OPTIONAL — no config means the integrated mailbox
    /// with identity `flywheel`.
    pub fn from_config(cfg: Option<&MailConfig>) -> Result<Option<Self>> {
        match cfg.and_then(|c| c.url.as_deref()) {
            Some(url) if !url.is_empty() => Self::remote(cfg),
            _ => {
                // bd-zncc: every test-path construction pins a per-process
                // mail root FIRST — see pin_test_mail_root_once.
                #[cfg(test)]
                pin_test_mail_root_once();
                // Eager shared-runtime + mailbox build (se2.1 Phase A / c3b.1):
                // failures propagate as errors here — no panic on runtime or
                // mailbox construction failure, and every op reuses the one
                // pre-built runtime.
                init_mailbox()?;
                Ok(Some(Self {
                    name: cfg
                        .and_then(|c| c.name.clone())
                        .unwrap_or_else(|| "flywheel".to_string()),
                    backend: BackendKind::InProcess,
                }))
            }
        }
    }

    /// Construct a REMOTE-daemon-backed facade: `cfg.url` must be Some
    /// (`http://host:port` of a `toron serve --mcp-http` daemon). `Ok(None)`
    /// when no url is set — callers treat that as mail-not-configured.
    pub fn remote(cfg: Option<&MailConfig>) -> Result<Option<Self>> {
        let name = cfg
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| "flywheel".to_string());
        let Some(url) = cfg.and_then(|c| c.url.clone()) else {
            return Ok(None);
        };
        let client = McpClient::new(
            &url,
            cfg.and_then(|c| c.token.clone()),
            ServerKind::Toron, // this facade IS the toron transport (ADR-0003 V1)
        )
        .map_err(|e| anyhow::anyhow!("agent-mail remote daemon {url}: {e:#}"))?;
        Ok(Some(Self {
            name,
            backend: BackendKind::Remote(client),
        }))
    }

    /// Route a tool call through the remote daemon (in-process backend errors).
    fn call_remote(&self, tool: &str, args: Value) -> Result<Value> {
        match &self.backend {
            BackendKind::Remote(client) => {
                // McpError (Transport|Tool) -> anyhow via the blanket From.
                client.call_tool(tool, args).map_err(anyhow::Error::from)
            }
            BackendKind::InProcess => bail!("{tool} requires the remote-daemon backend"),
        }
    }

    /// `health_check` (in-process) / `health_check` (remote).
    pub fn health(&self) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let report = block_on(mailbox().health_check());
                Ok(report.to_json())
            }
            BackendKind::Remote(_) => {
                // `project_key` is unused by the toron tool; empty string.
                let mut v =
                    self.call_remote("health_check", crate::backend::wire::health_check_args())?;
                v["transport"] = json!("http"); // distinguishing marker: remote mode
                Ok(v)
            }
        }
    }

    /// `ensure_project`.
    pub fn ensure_project(&self, project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let state = block_on(mailbox().ensure_project(project_key))?;
                Ok(state.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "ensure_project",
                crate::backend::wire::ensure_project_args(project_key),
            ),
        }
    }

    // bd-7bvi: production callers land with the pin-public-plane wave
    // (spawn/loop startup switches projects to swarm mode); today only
    // tests exercise it — allow dead_code outside tests so the clippy
    // gate stays green until the wave wires it.
    #[cfg_attr(not(test), allow(dead_code))]
    /// bd-pin-public-plane-s-easy.2: switch a project to `swarm` identity
    /// D3/D4; open mode passes names through and the slug alone is not a
    /// registered identity). In-process writes the mode row; the remote
    /// daemon carries it via `ensure_project`'s identity_mode arg.
    pub fn set_identity_mode(&self, project_key: &str, mode: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let canonical = block_on(mailbox().canonical_project(project_key));
                block_on(mailbox().set_identity_mode(&canonical, mode));
                Ok(json!({ "project": canonical, "identity_mode": mode }))
            }
            BackendKind::Remote(_) => {
                let mut v = self.call_remote(
                    "ensure_project",
                    crate::backend::wire::ensure_project_with_mode_args(project_key, mode),
                )?;
                v["identity_mode"] = json!(mode);
                Ok(v)
            }
        }
    }

    /// `list_agents` (project's registered identities). `active_within_days`
    /// filters to agents active within that window (surface parity — the
    /// daemon supports it on the wire; in-process passes it to the mailbox).
    pub fn list_agents(&self, project_key: &str, active_within_days: Option<u64>) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let agents = block_on(
                    mailbox().list_agents(project_key, ListAgentsOptions { active_within_days }),
                )?;
                Ok(Value::Array(agents.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => {
                // Daemon envelope onto the in-process bare-array shape; loud
                // on unrecognized shape (never a silent empty roster).
                let v = self.call_remote(
                    "list_agents",
                    crate::backend::wire::list_agents_args(project_key, active_within_days),
                )?;
                remotes_to_agents(v)
            }
        }
    }

    /// `whois` (project + agent name → profile). Surface parity — the mailbox
    /// owns name resolution (G2); the daemon tool of the same name.
    pub fn whois(&self, project_key: &str, agent_name: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let who = block_on(mailbox().whois(project_key, agent_name))?;
                Ok(who.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "whois",
                crate::backend::wire::whois_args(project_key, agent_name),
            ),
        }
    }

    /// bd-pin-public-plane-s-easy.2: record a legacy-name fold (slug → pin)
    /// so whois(slug) resolves to the pin's npub (ADR-0018 D4). The fold row
    /// is written under the CANONICAL project (whois resolves via
    /// canonical_project — a raw path key would miss). In-process writes the
    /// index row directly; remote composes `register_agent` with optional
    /// `alias` (no new MCP tool).
    pub fn fold_agent_name_alias(
        &self,
        project_key: &str,
        alias: &str,
        canonical: &str,
    ) -> Result<()> {
        match &self.backend {
            BackendKind::InProcess => {
                let canonical_project = block_on(mailbox().canonical_project(project_key));
                block_on(mailbox().fold_agent_name_alias(&canonical_project, alias, canonical));
                Ok(())
            }
            BackendKind::Remote(_) => {
                self.call_remote(
                    "register_agent",
                    json!({
                        "project_key": project_key,
                        "name": canonical,
                        "alias": alias,
                    }),
                )?;
                Ok(())
            }
        }
    }

    /// GS-030/GS-033 B8: bind a crew role to its spawn pin (alias table +
    /// signed 30013 `role=` charter). In-process writes through
    /// `Mailbox::set_role_pin` (the same function as `toron role pin`).
    /// Remote has no role-pin MCP op (ADR-0027 CLI-only surface) and
    /// **fails loud** naming the CLI — no silent skip (NEW-L7).
    pub fn pin_role_alias(&self, project_key: &str, role: &str, pin: &str) -> Result<String> {
        match &self.backend {
            BackendKind::InProcess => {
                let name = block_on(mailbox().set_role_pin(project_key, role, pin))?;
                Ok(name)
            }
            BackendKind::Remote(_) => anyhow::bail!(
                "role bind failed (remote mailbox): run `toron role pin {role} {pin} --project {project_key}` (no remote role-pin MCP op per ADR-0027; refusing a silent skip)"
            ),
        }
    }

    /// Undo [`Self::pin_role_alias`] (GS-033 compensate path when
    /// `emit_role_bound` fails after the mail pin landed). In-process
    /// clears via `Mailbox::clear_role_pin`; remote has no role-unpin MCP
    /// op and fails loud naming the CLI.
    pub fn clear_role_alias(&self, project_key: &str, role: &str) -> Result<Option<String>> {
        match &self.backend {
            BackendKind::InProcess => {
                let prior = block_on(mailbox().clear_role_pin(project_key, role))?;
                Ok(prior)
            }
            BackendKind::Remote(_) => anyhow::bail!(
                "role unpin failed (remote mailbox): run `toron role unpin {role} --project {project_key}` (no remote role-unpin MCP op per ADR-0027; refusing a silent skip)"
            ),
        }
    }

    /// `register_agent` (upsert on the agent's key).
    pub fn register_agent(
        &self,
        project_key: &str,
        name: &str,
        program: &str,
        model: &str,
        task_description: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let mut profile = AgentProfile::new(name);
                profile.program = Some(program.to_string());
                profile.model = Some(model.to_string());
                profile.task_description = Some(task_description.to_string());
                let agent = block_on(mailbox().register_agent(project_key, &profile))?;
                Ok(agent.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "register_agent",
                crate::backend::wire::register_agent_args(
                    project_key,
                    name,
                    program,
                    model,
                    task_description,
                ),
            ),
        }
    }

    /// `fetch_inbox` (simple filter set).
    pub fn inbox(
        &self,
        project_key: &str,
        agent_name: &str,
        urgent_only: bool,
        limit: u32,
    ) -> Result<Value> {
        // bd-mail-surface-drift-7r2o: the flag is honored on BOTH backends —
        // id-set parity with `inbox_filtered(urgent_only=true)` / MCP
        // fetch_inbox(urgent_only). The legacy surface ignored it entirely,
        // so --urgent callers (and anything counting "urgent" off this dump)
        // saw every row regardless of importance.
        match &self.backend {
            BackendKind::InProcess => {
                let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
                    project_slug: project_key.to_string(),
                    agent_name: agent_name.to_string(),
                    unread_only: false,
                    limit: Some(limit as usize),
                    include_bodies: true,
                    urgent_only,
                    since_ts: None,
                    thread_id: None,
                    ack_overdue_only: false,
                }))?;
                Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "fetch_inbox",
                crate::backend::wire::inbox_args(
                    project_key,
                    agent_name,
                    urgent_only,
                    false,
                    false,
                    true,
                    limit,
                    None,
                    None,
                ),
            ),
        }
    }

    /// `fetch_inbox` with the full filter bag (c3b.3). Applies every filter
    /// the toron row model supports: `urgent_only` (rows carry `importance`),
    /// `since_ts` (rows carry `created_ts_us`), `ack_overdue_only` (rows carry
    /// `ack_required` + `read_ts_us` — see [`inbox_row_ack_overdue`]) on top of
    /// the already-supported `unread_only` / `include_bodies` / `limit`. Every
    /// filter is a real read or a loud error; none is a silent no-op.
    pub fn inbox_filtered(
        &self,
        project_key: &str,
        agent_name: &str,
        filter: &MailInboxFilter,
    ) -> Result<Value> {
        let now_us = chrono::Utc::now().timestamp_micros();
        // bd-zd2v: the literal `last` is the daemon-side cursor
        // (read-then-bump, resolved by the toron projection) — not RFC3339,
        // so it bypasses timestamp parsing AND Remote client-side filtering
        // (the server applies the cursor; filtering here would be wrong).
        let is_last = filter.since_ts.as_deref() == Some("last");
        let since_us = if is_last {
            None
        } else {
            filter
                .since_ts
                .as_deref()
                .map(parse_since_ts_us)
                .transpose()?
        };
        match &self.backend {
            BackendKind::InProcess => {
                // bd-55nx.12: since_ts is applied ONCE by the toron
                // projection (fetch_inbox strict-newer, equal-ts excluded).
                // No second retain here — two RFC3339 parsers can diverge
                // on fractional seconds / offsets at the boundary.
                let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
                    project_slug: project_key.to_string(),
                    agent_name: agent_name.to_string(),
                    unread_only: filter.unread_only,
                    limit: Some(filter.limit as usize),
                    include_bodies: filter.include_bodies,
                    urgent_only: filter.urgent_only,
                    since_ts: filter.since_ts.clone(),
                    thread_id: filter.thread_id.clone(),
                    ack_overdue_only: false,
                }))?;
                let rows = msgs
                    .iter()
                    .filter(|m| !filter.urgent_only || is_urgent(&m.importance))
                    .map(ToJson::to_json)
                    .filter(|row| {
                        !filter.ack_overdue_only
                            || inbox_row_ack_overdue(row, now_us, ACK_SLA_DEFAULT_SECS)
                    })
                    .collect();
                Ok(Value::Array(rows))
            }
            BackendKind::Remote(_) => {
                let v = self.call_remote(
                    "fetch_inbox",
                    crate::backend::wire::inbox_args(
                        project_key,
                        agent_name,
                        filter.urgent_only,
                        filter.unread_only,
                        filter.ack_overdue_only,
                        filter.include_bodies,
                        filter.limit,
                        filter.since_ts.as_deref(),
                        filter.thread_id.as_deref(),
                    ),
                )?;
                // Same policy on the wire rows (InboxMessageOut carries
                // `importance` + `created_ts_us`): filter client-side.
                // bd-zd2v: the daemon answers `{"messages": [...]}` (object),
                // not a bare array — accept both shapes (the bare-array arm
                // keeps InProcess-shaped remotes byte-identical). Without
                // this every Remote filtered read (incl. --unread) is [].
                let rows_src: Vec<Value> = match &v {
                    Value::Array(a) => a.clone(),
                    v => v
                        .get("messages")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                };
                let rows = rows_src
                    .into_iter()
                    .filter(|row| {
                        !filter.urgent_only
                            || row
                                .get("importance")
                                .and_then(Value::as_str)
                                .map(is_urgent)
                                .unwrap_or(false)
                    })
                    .filter(|row| match since_us {
                        Some(s) => row
                            .get("created_ts_us")
                            .and_then(Value::as_i64)
                            .map(|ts| ts > s)
                            .unwrap_or(true),
                        None => true,
                    })
                    // bd-harvest-thread-filter-yp9r: belt-and-braces client
                    // filter for remote backends whose projection cannot push
                    // the thread predicate (AM parity rows carry thread_id).
                    .filter(|row| match &filter.thread_id {
                        Some(t) => row
                            .get("thread_id")
                            .and_then(Value::as_str)
                            .map(|row_t| row_t == t)
                            .unwrap_or(false),
                        None => true,
                    })
                    // bd-mail-ack-sla-tracking: the ack-overdue post-filter
                    // runs on the wire rows too (same predicate, same clock).
                    .filter(|row| {
                        !filter.ack_overdue_only
                            || inbox_row_ack_overdue(row, now_us, ACK_SLA_DEFAULT_SECS)
                    })
                    .collect();
                Ok(Value::Array(rows))
            }
        }
    }

    /// `mark_message_read`.
    pub fn mark_read(
        &self,
        project_key: &str,
        agent_name: &str,
        message_id: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                // toron ids are hex event ids — pass through verbatim.
                let r = block_on(mailbox().mark_message_read(project_key, agent_name, message_id))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => {
                // The remote daemon is toron (`[mail.toron] url`): its
                // message ids are hex event ids and its `mark_message_read`
                // tool takes a String — pass the id through verbatim, never
                // coerce it to a number (se2.2; the deleted AM backend owned
                // the decimal-id edge).
                self.call_remote(
                    "mark_message_read",
                    crate::backend::wire::mark_read_args(
                        project_key,
                        agent_name,
                        json!(message_id),
                    ),
                )
            }
        }
    }

    /// `acknowledge_message`.
    pub fn ack(&self, project_key: &str, agent_name: &str, message_id: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                // toron ids are hex event ids — pass through verbatim.
                let r =
                    block_on(mailbox().acknowledge_message(project_key, agent_name, message_id))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => {
                // Same as mark_message_read above: the toron daemon's
                // `acknowledge_message` takes the hex event id as a String —
                // pass it through verbatim (se2.2).
                self.call_remote(
                    "acknowledge_message",
                    crate::backend::wire::ack_args(project_key, agent_name, json!(message_id)),
                )
            }
        }
    }

    /// Messages this agent owes an ack for, past the SLA — the read the
    /// blocked-escalation protocol polls (ADR-0001 A6, bd-mail-ack-sla
    /// -tracking subtask 4).
    ///
    /// Read-only by construction: it reuses the SAME per-recipient read state
    /// `fetch_inbox` already projects (`message_reads`, migration 0002: only
    /// `mark_message_read` / `acknowledge_message` ever write it, and
    /// `fetch_inbox` itself never does), then applies
    /// [`inbox_row_ack_overdue`]. It deliberately does NOT call toron's
    /// `check_ack_sla_expired`: that primitive EVENTS an `ack-sla-expired`
    /// wake per overdue post, and the doorbell's 60s scan already owns the
    /// emit — a second emitter here would double every ring.
    ///
    /// The rows are the projection's own shape (`id`, `thread_id`,
    /// `subject`, `sender`, `created_ts_us`, `read_ts_us`, …); `age_secs` and
    /// the `sla_secs` that produced the row are added for readers.
    pub fn ack_overdue(
        &self,
        project_key: &str,
        agent_name: &str,
        filter: &AckOverdueFilter,
    ) -> Result<Value> {
        let sla_secs = filter.sla_secs.unwrap_or(ACK_SLA_DEFAULT_SECS);
        let now_us = chrono::Utc::now().timestamp_micros();
        let rows: Vec<Value> = match &self.backend {
            BackendKind::InProcess => {
                // Guard the "empty because unknown" trap: an unregistered
                // agent has no receipts, so the read would return an empty set
                // that LOOKS like "nothing overdue". Fail loudly instead —
                // this is exactly the distinction the escalation gate needs.
                let canonical = block_on(mailbox().canonical_project(project_key));
                if block_on(mailbox().agent_pubkey_hex(&canonical, agent_name)).is_none() {
                    return Err(anyhow!(
                        "toron ack_overdue: agent {agent_name} is not registered in {canonical} — \
                         there is no ack state to read (register the agent first; an empty set from \
                         an unknown identity is not 'nothing overdue')"
                    ));
                }
                // The projection applies `unread_only` + `urgent_only` +
                // `thread_id` itself (per-recipient read state, importance
                // scope, thread bound) — the row post-filter below is the
                // same predicate the remote half uses, so both halves agree.
                let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
                    project_slug: project_key.to_string(),
                    agent_name: agent_name.to_string(),
                    unread_only: true,
                    limit: None,
                    include_bodies: false,
                    urgent_only: true,
                    since_ts: None,
                    thread_id: filter.thread_id.clone(),
                    ack_overdue_only: false,
                }))?;
                msgs.iter()
                    .map(ToJson::to_json)
                    .filter(|row| inbox_row_ack_overdue(row, now_us, sla_secs))
                    .collect()
            }
            BackendKind::Remote(_) => {
                // The daemon has no ack-overdue tool or resource read; a
                // remote caller must not be handed an unfiltered inbox that
                // LOOKS like an overdue set (c3b.3's no-silent-no-op rule).
                return Err(anyhow!(
                    "toron ack_overdue: no remote projection (the daemon exposes no ack-overdue \
                     tool; toron://views/ack-overdue is a sender-side view of the unread write \
                     path) — read the shared index in-process, or add the tool to toron"
                ));
            }
        };
        let with_age: Vec<Value> = rows
            .into_iter()
            .map(|mut row| {
                let age_secs = row
                    .get("created_ts_us")
                    .and_then(Value::as_i64)
                    .map(|created| now_us.saturating_sub(created) / 1_000_000)
                    .unwrap_or(0);
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("age_secs".into(), json!(age_secs));
                    // name the SLA that produced the row, so a reader can
                    // never mistake a 0-second gate for the 30-min default.
                    obj.insert("sla_secs".into(), json!(sla_secs));
                }
                row
            })
            .collect();
        Ok(Value::Array(with_age))
    }

    /// `send_message`.
    #[allow(clippy::too_many_arguments)] // mirrors the MailBackend::send trait surface
    pub fn send(
        &self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack: bool,
        thread_id: Option<&str>,
    ) -> Result<Value> {
        self.send_with_wake(
            project_key,
            to,
            subject,
            body,
            importance,
            ack,
            thread_id,
            Some("tasked"),
            None,
            None,
        )
    }

    /// GS-039 P0-1: mail send with an explicit wake tag (default tasked) so
    /// deliveries are not wake:null theater. Pass None to omit the tag.
    /// `bead` stamps the ADR-0010 D9 structural done-hold (`bead=<id>`) — the
    /// dispatch guard matches the TAG, never the `[bd-###] done` subject, so a
    /// subject without this tag never fires the hold (bd-7qg6).
    #[allow(clippy::too_many_arguments)]
    pub fn send_with_wake(
        &self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack: bool,
        thread_id: Option<&str>,
        wake: Option<&str>,
        corr: Option<&str>,
        bead: Option<&str>,
    ) -> Result<Value> {
        // bd-pncn: `all` (the broadcast token) is expanded to the project's
        // registered roster BEFORE the backend call — the toron surfaces
        // (in-process mailbox + daemon tool) reject a literal "all"
        // recipient. Zero registered agents is a clear error.
        let targets = self.expand_broadcast(project_key, to)?;
        // bd-pin-npub-binding-ggym: a name whose binding diverges from the key
        // its pane acts under is a silent loss — the send reports ok and the
        // recipient reads nothing. The mailbox owns name resolution, so ask it
        // (`whois`) and refuse on positive two-sided divergence; a missing side
        // is not evidence and still routes. One query per named recipient.
        guard_targets(&targets, |name| self.whois(project_key, name).ok())?;
        // ADR-0010 D8 (bd-d8e4): a corr-preserving resend stamps the ORIGINAL
        // corr — toron's idempotent claim dedupes within TTL and the receipt
        // stays corr-keyed. The remote wire surface does not carry extra_tags,
        // so refuse rather than silently drop the corr (a dropped corr is a
        // NEW corr = duplicate).
        if corr.is_some() && matches!(self.backend, BackendKind::Remote(_)) {
            bail!(
                "mail resend requires the in-process toron mailbox (no [mail.toron] url) — \
                 the remote wire surface cannot carry a corr tag"
            );
        }
        // bd-7qg6: same refuse-don't-drop rule for the structural bead tag —
        // a dropped bead tag is a `[bd-###] done` subject that never fires the
        // dispatch-guard hold (the exact theater D9 was cut to kill). Remote
        // callers use `toron mail send --resolve-bead` directly.
        if bead.is_some_and(|b| !b.trim().is_empty())
            && matches!(self.backend, BackendKind::Remote(_))
        {
            bail!(
                "mail --resolve-bead requires the in-process toron mailbox (no [mail.toron] url) — \
                 the remote wire surface cannot carry a bead tag; use `toron mail send --resolve-bead` directly"
            );
        }
        let extra_tags = match wake {
            Some(w) if !w.is_empty() => {
                let w = w.trim().to_ascii_lowercase();
                if w != "standing" && w != "tasked" {
                    bail!(
                        "mail --wake must be standing|tasked (got {w:?}) — refuse inventing a free-string wake (GS-039 P0-1)"
                    );
                }
                vec![
                    toron_core::nostr::Tag::parse(["wake", w.as_str()])
                        .map_err(|e| anyhow!("wake tag: {e}"))?,
                ]
            }
            _ => Vec::new(),
        };
        let mut extra_tags = match corr {
            Some(c) if !c.trim().is_empty() => {
                let mut v = extra_tags;
                v.push(
                    toron_core::nostr::Tag::parse(["corr", c.trim()])
                        .map_err(|e| anyhow!("corr tag: {e}"))?,
                );
                v
            }
            _ => extra_tags,
        };
        // ADR-0010 D9 (bd-l0fz): structural done-hold tag. Empty = absent.
        if let Some(b) = bead.filter(|s| !s.trim().is_empty()) {
            extra_tags.push(
                toron_core::nostr::Tag::parse(["bead", b.trim()])
                    .map_err(|e| anyhow!("bead tag: {e}"))?,
            );
        }
        match &self.backend {
            BackendKind::InProcess => {
                let result = block_on(mailbox().send_message(&SendMessage {
                    project_slug: project_key.to_string(),
                    sender_name: self.name.clone(),
                    to: targets,
                    subject: subject.to_string(),
                    body_md: body.to_string(),
                    importance: Some(importance.to_string()),
                    ack_required: ack,
                    thread_id: thread_id.map(String::from),
                    extra_tags,
                    ..Default::default()
                }))?;
                // Echo wake in the JSON receipt so operators see non-null.
                let mut v = result.to_json();
                if let Some(w) = wake.filter(|s| !s.is_empty())
                    && let Some(obj) = v.as_object_mut()
                {
                    obj.insert("wake".into(), serde_json::json!(w));
                }
                Ok(v)
            }
            BackendKind::Remote(_) => {
                let mut args = crate::backend::wire::send_args(
                    project_key,
                    &self.name,
                    &targets,
                    subject,
                    body,
                    importance,
                    ack,
                    thread_id,
                );
                if let Some(w) = wake.filter(|s| !s.is_empty()) {
                    args["wake"] = serde_json::json!(w);
                }
                self.call_remote("send_message", args)
            }
        }
    }

    /// bd-pncn: expand the `all` broadcast token into the project's
    /// registered agent roster (backend-agnostic — `list_agents` serves both
    /// the in-process mailbox and the remote daemon). Explicit recipients
    /// pass through untouched; an explicit name also present in the roster
    /// is deduped (one gift-wrap per agent). Errors when the roster is empty.
    fn expand_broadcast(&self, project_key: &str, to: &[String]) -> Result<Vec<String>> {
        let has_broadcast = to.iter().any(|t| t.trim() == "all");
        if !has_broadcast {
            return Ok(to.to_vec());
        }
        let roster = self.list_agents(project_key, None)?;
        let mut out: Vec<String> = roster
            .as_array()
            .ok_or_else(|| anyhow!("list_agents returned a non-array: {roster}"))?
            .iter()
            .filter_map(|a| a.get("name").and_then(Value::as_str).map(String::from))
            .collect();
        if out.is_empty() {
            bail!(
                "cannot broadcast to 'all': project {project_key} has zero registered agents (register agents first, or name recipients explicitly)"
            );
        }
        for t in to {
            let t = t.trim();
            if t != "all" && !out.iter().any(|n| n == t) {
                out.push(t.to_string());
            }
        }
        Ok(out)
    }

    /// `reply_message` (threaded).
    pub fn reply(
        &self,
        project_key: &str,
        message_id: &str,
        body: &str,
        to: Option<&[String]>,
    ) -> Result<Value> {
        // toron's reply_message REQUIRES explicit recipients (no AM-style
        // "default to original sender") — resolve the parent's sender from
        // the inbox when the caller did not pass `to` (the common case:
        // `flywheel mail reply --message-id X --body Y`).
        let to: Option<Vec<String>> = match to {
            Some(t) if !t.is_empty() => Some(t.to_vec()),
            _ => self.parent_sender(project_key, message_id)?,
        };
        match &self.backend {
            BackendKind::InProcess => {
                // toron: the id IS the thread anchor (hex event id or the
                // free-form thread id — both flow through the `thread` tag).
                let result = block_on(mailbox().reply_message(&SendMessage {
                    project_slug: project_key.to_string(),
                    sender_name: self.name.clone(),
                    to: to.clone().unwrap_or_default(),
                    subject: String::new(),
                    body_md: body.to_string(),
                    thread_id: Some(message_id.to_string()),
                    ..Default::default()
                }))?;
                Ok(result.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "reply_message",
                crate::backend::wire::reply_args(
                    project_key,
                    &self.name,
                    &to.unwrap_or_default(),
                    // c3b.5: reply subject populated from the parent (`Re: `);
                    // was hardcoded `""` (unverified wire contract).
                    &self.parent_reply_subject(project_key, message_id)?,
                    body,
                    "thread_id",
                    json!(message_id),
                ),
            ),
        }
    }

    /// Resolve the sender of the message being replied to (for toron, which
    /// requires explicit reply recipients). Looks the parent up in the
    /// project inbox; returns `None` when the parent cannot be found (the
    /// caller then fails with a clear message instead of a silent drop).
    fn parent_sender(&self, project_key: &str, message_id: &str) -> Result<Option<Vec<String>>> {
        let v = self.inbox_filtered(
            project_key,
            &self.name,
            &MailInboxFilter {
                unread_only: false,
                include_bodies: false,
                limit: 100,
                ..Default::default()
            },
        )?;
        let arr = v.as_array().cloned().unwrap_or_default();
        for m in &arr {
            let id = m
                .get("id")
                .or_else(|| m.get("message_id"))
                .and_then(|i| i.as_str());
            let thread = m.get("thread_id").and_then(|t| t.as_str());
            if id == Some(message_id) || thread == Some(message_id) {
                let sender = m
                    .get("sender")
                    .or_else(|| m.get("from"))
                    .and_then(|s| s.as_str())
                    .map(String::from);
                if sender.is_some() {
                    return Ok(sender.map(|s| vec![s]));
                }
            }
        }
        Ok(None)
    }

    /// c3b.5: resolve the `Re: <parent subject>` subject for a reply. Looks
    /// the parent up in the project inbox (the message being replied to must
    /// be visible to this identity); falls back to `""` when the parent
    /// cannot be found — the wire builder still carries the subject field, so
    /// the hardcoded-empty contract is gone. Works for BOTH backends (the
    /// inbox fetch is the common projection).
    fn parent_reply_subject(&self, project_key: &str, message_id: &str) -> Result<String> {
        let v = self.inbox_filtered(
            project_key,
            &self.name,
            &MailInboxFilter {
                unread_only: false,
                include_bodies: false,
                limit: 500,
                ..Default::default()
            },
        )?;
        let parent = v.as_array().and_then(|rs| {
            rs.iter().find(|r| {
                r["id"].as_str() == Some(message_id) || r["thread_id"].as_str() == Some(message_id)
            })
        });
        Ok(match parent.and_then(|r| r["subject"].as_str()) {
            Some(subj) if !subj.is_empty() => format!("Re: {subj}"),
            _ => String::new(),
        })
    }

    /// `summarize_thread`.
    pub fn summarize_thread(
        &self,
        project_key: &str,
        thread_id: &str,
        include_examples: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let s = block_on(mailbox().summarize_thread(
                    project_key,
                    thread_id,
                    include_examples,
                    false,
                    None,
                ))?;
                Ok(s.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "summarize_thread",
                crate::backend::wire::summarize_thread_args(
                    project_key,
                    thread_id,
                    include_examples,
                ),
            ),
        }
    }

    /// `summarize_thread_product` — summarize a thread across a product's
    /// member projects (ADR-0003 V6 surface restore).
    pub fn summarize_thread_product(
        &self,
        product_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let s =
                    block_on(mailbox().summarize_thread_product(product_key, thread_id, llm_mode))?;
                Ok(s.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "summarize_thread_product",
                crate::backend::wire::summarize_thread_product_args(
                    product_key,
                    thread_id,
                    llm_mode,
                ),
            ),
        }
    }

    /// `search_messages`. The JSON envelope is `{"result": [...]}` so
    /// `harvest_latest` (debate / plan-review) reads `result[0].body_md`.
    pub fn search(
        &self,
        project_key: &str,
        query: &str,
        limit: u32,
        filter: &MailSearchFilter,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let msgs = block_on(mailbox().search_messages(&SearchMessages {
                    project_key: project_key.to_string(),
                    query: query.to_string(),
                    limit: Some(limit as usize),
                    ranking: filter.ranking.clone(),
                    sender_name: filter.sender.clone(),
                    importance: filter.importance.clone(),
                    thread_id: filter.thread_id.clone(),
                    include_body_md: filter.include_body_md,
                    ..Default::default()
                }))?;
                Ok(json!({ "result": msgs.iter().map(ToJson::to_json).collect::<Vec<Value>>() }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "search_messages",
                    crate::backend::wire::search_args(
                        project_key,
                        query,
                        limit,
                        filter.include_body_md,
                        filter.sender.as_deref(),
                        filter.importance.as_deref(),
                        filter.thread_id.as_deref(),
                        filter.date_start.as_deref(),
                        filter.date_end.as_deref(),
                        filter.ranking.as_deref(),
                    ),
                )?;
                // The toron daemon already returns the envelope
                // (`{"result": [...], "next_cursor": ...}`); legacy
                // agent-mail returned a bare array. Pass the envelope
                // through when present, else wrap.
                Ok(match &res {
                    Value::Object(m) if m.contains_key("result") => res,
                    _ => json!({ "result": res }),
                })
            }
        }
    }

    // (The former `Mail::search_args` wrapper is gone: the live `search_messages`
    // arm builds its args through `backend::wire::search_args` directly, and the
    // wrapper's only other caller was the deleted HTTP client.)

    /// `file_reservation_paths`.
    pub fn reserve(
        &self,
        project_key: &str,
        agent_name: &str,
        paths: &[String],
        ttl_seconds: u64,
        exclusive: bool,
        reason: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().file_reservation_paths(
                    project_key,
                    agent_name,
                    paths,
                    Some(ttl_seconds),
                    exclusive,
                    Some(reason),
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "file_reservation_paths",
                crate::backend::wire::reserve_args(
                    project_key,
                    agent_name,
                    paths,
                    ttl_seconds,
                    exclusive,
                    reason,
                ),
            ),
        }
    }

    /// `release_file_reservations` (agent-scoped; `paths` empty = all own).
    pub fn release_paths(
        &self,
        project_key: &str,
        agent_name: &str,
        paths: &[String],
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r =
                    block_on(mailbox().release_file_reservations(project_key, agent_name, paths))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "release_file_reservations",
                crate::backend::wire::release_args(project_key, agent_name),
            ),
        }
    }

    /// `release_by_reason` (bd-h3oj): operator-signed revocation of every
    /// live lease whose `reason` tag is the bead id — the loop-side
    /// after-close hook. `released == 0` is a clean no-op.
    pub fn release_by_reason(
        &self,
        project_key: &str,
        requestor_name: &str,
        bead_id: &str,
        notify_previous: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().release_by_reason(
                    project_key,
                    requestor_name,
                    bead_id,
                    notify_previous,
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "release_file_reservations_by_reason",
                crate::backend::wire::release_by_reason_args(
                    project_key,
                    requestor_name,
                    bead_id,
                    notify_previous,
                ),
            ),
        }
    }

    /// `check_file_reservation_conflicts` (agent-scoped).
    pub fn conflicts_for(
        &self,
        project_key: &str,
        agent_name: &str,
        paths: &[String],
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().check_file_reservation_conflicts(
                    project_key,
                    agent_name,
                    paths,
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "check_file_reservation_conflicts",
                crate::backend::wire::conflicts_args(project_key, agent_name, paths),
            ),
        }
    }

    /// `active_reservations` (bd-dsu1 operator lock board): every active
    /// held lease in the project as JSON rows — path, holder, exclusive,
    /// reason, expiry, reservation id (the kind-30031 event id needed for
    /// diagnostics). Optional filters: `holder` (exact agent name) and
    /// `path` (glob overlap — shows the holders of one path). Read-only:
    /// never registers or publishes.
    pub fn list_active(
        &self,
        project_key: &str,
        holder: Option<&str>,
        path: Option<&str>,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let rows = block_on(mailbox().active_reservations(project_key))?;
                let reservations: Vec<Value> = rows
                    .into_iter()
                    .filter(|r| holder.is_none_or(|h| r.agent_name == h))
                    .filter(|r| path.is_none_or(|p| overlaps(&r.path_pattern, p)))
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "path": r.path_pattern,
                            "holder": r.agent_name,
                            "exclusive": r.exclusive,
                            "reason": r.reason,
                            "expires_ts_us": r.expires_ts_us,
                        })
                    })
                    .collect();
                Ok(json!({
                    "project": project_key,
                    "count": reservations.len(),
                    "reservations": reservations,
                }))
            }
            BackendKind::Remote(_) => {
                // bd-locks-list-daemon-xetz: the daemon exposes the lease
                // board via the additive read tool (toron-mcp
                // `list_file_reservations`) — same shape as the in-process
                // arm, so `mail locks list` works at both ends of the seam.
                let v = self.call_remote(
                    "list_file_reservations",
                    serde_json::json!({
                        "project_key": project_key,
                        "holder": holder,
                        "path": path,
                    }),
                )?;
                Ok(remotes_to_board(project_key, v))
            }
        }
    }

    /// bd-reserve-yield-taqf: the LIVE yield pairs `(bead_id, holder)` — a
    /// structured stand-down (toron `reserve yield`) says "that bead is this
    /// live pane's work", which the loop's peer-hold rule consumes as one
    /// more peer. Read through the SAME toron-core join that `reserve yields`
    /// prints (markers x live lease board), so a released holder stops
    /// suppressing on its own and no caller re-derives either half.
    ///
    /// Ceiling: a `[mail.toron] url` remote plane carries no yield tool (the
    /// toron-side verb is CLI-only by design), so this returns no pairs
    /// there. That is the absence of a read, never a fabricated hold — the
    /// same shape as an unreadable reservation board.
    pub fn yielded_pairs(&self, project_key: &str) -> Result<Vec<(String, String)>> {
        match &self.backend {
            BackendKind::InProcess => {
                let (views, _suppressed) =
                    block_on(mailbox().reservation_yield_board(project_key))?;
                Ok(views
                    .into_iter()
                    .filter(|v| v.holder_live)
                    .map(|v| (v.bead_id, v.holder))
                    .collect())
            }
            BackendKind::Remote(_) => Ok(Vec::new()),
        }
    }

    /// `renew_file_reservations`. The response keeps the `file_reservations`
    /// envelope `list_own_reservations` parses (ids are toron hex event ids).
    pub fn renew(
        &self,
        project_key: &str,
        agent_name: &str,
        extend_seconds: u64,
        paths: Option<&[String]>,
        _ids: Option<&[u64]>,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().renew_file_reservations(
                    project_key,
                    agent_name,
                    paths.unwrap_or(&[]),
                    extend_seconds,
                ))?;
                Ok(json!({
                    "renewed": r.renewed,
                    "file_reservations": r.reservations.iter().map(|res| json!({
                        "id": res.id,
                        "path_pattern": res.path_pattern,
                        // bd-evqs: the reason (owning bead id) rides through
                        // the renew envelope so the loop can release
                        // reservations whose bead is CLOSED — the renewal
                        // storm fix needs the holder to know WHAT each lease
                        // is for.
                        "reason": res.reason,
                        "old_expires_ts": "",
                        "new_expires_ts": res.expires_ts_us.map(|u| u.to_string()).unwrap_or_default(),
                    })).collect::<Vec<Value>>(),
                }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "renew_file_reservations",
                    crate::backend::wire::renew_args(
                        project_key,
                        agent_name,
                        extend_seconds,
                        paths,
                    ),
                )?;
                // Remap the tool's `reservations` array onto the in-process
                // `file_reservations` envelope `list_own_reservations` reads.
                let mut out = json!({ "renewed": res.get("renewed").cloned().unwrap_or(json!(0)) });
                let fr = res
                    .get("reservations")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .map(|r| {
                                json!({
                                    "id": r.get("id").cloned().unwrap_or(json!(0)),
                                    "path_pattern": r.get("path_pattern").cloned().unwrap_or(json!("")),
                                    // bd-evqs: keep the reason through the
                                    // remap (the loop's closed-bead sweep
                                    // reads it to release stale leases).
                                    "reason": r.get("reason").cloned().unwrap_or(json!("")),
                                    "old_expires_ts": "",
                                    "new_expires_ts": r.get("expires_ts_us")
                                        .and_then(Value::as_u64)
                                        .map(|u| u.to_string())
                                        .unwrap_or_default(),
                                })
                            })
                            .collect::<Vec<Value>>()
                    })
                    .unwrap_or_default();
                out["file_reservations"] = json!(fr);
                Ok(out)
            }
        }
    }

    /// `force_release_file_reservation` — operator force-release of a stale
    /// reservation (ADR-0003 V6 surface restore). bd-cumj: resolves the lease
    /// by `path` (+ optional `holder`) — no caller-supplied event id. In-process
    /// requires the operator key (attached by `build_mailbox`); remote delegates
    /// to the daemon's tool.
    pub fn force_release_file_reservation(
        &self,
        project_key: &str,
        agent_name: &str,
        path: &str,
        holder: Option<&str>,
        note: Option<&str>,
        notify_previous: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                block_on(mailbox().force_release_file_reservation(
                    project_key,
                    agent_name,
                    path,
                    holder,
                    note,
                    notify_previous,
                ))?;
                Ok(json!({ "ok": true }))
            }
            BackendKind::Remote(_) => self.call_remote(
                "force_release_file_reservation",
                crate::backend::wire::force_release_args(
                    project_key,
                    agent_name,
                    path,
                    holder,
                    note,
                    notify_previous,
                ),
            ),
        }
    }

    /// `cleanup_pane_identities` — prune identities whose panes are absent
    /// from herdr's live pane list (ADR-0003 V6 surface restore). toron holds
    /// no per-pane identity files (live herdr mapping), so in-process "prune"
    /// reports the stale names.
    ///
    /// Honest errors (c3b.7): a herdr CONNECT failure when herdr IS the
    /// active multiplexer is a real outage — the caller must see Err, not an
    /// Ok note that reads like success. The Ok-note path fires ONLY for the
    /// genuinely-not-applicable configuration (active multiplexer is not
    /// herdr — the cleanup is herdr-specific; that is a config fact, not a
    /// failure).
    pub fn cleanup_pane_identities(&self, _project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                if !multiplexer_is_herdr() {
                    return Ok(json!({
                        "note": "active multiplexer is not herdr; pane identity cleanup not applicable",
                    }));
                }
                match toron_core::backend::herdr::HerdrBackend::connect(None) {
                    Ok(backend) => {
                        let pruned = mailbox().cleanup_pane_identities(&backend)?;
                        Ok(json!({ "pruned": pruned }))
                    }
                    Err(e) => Err(anyhow!(
                        "herdr unreachable; pane identity cleanup skipped: {e}"
                    )),
                }
            }
            BackendKind::Remote(_) => self.call_remote(
                "cleanup_pane_identities",
                crate::backend::wire::cleanup_pane_identities_args(_project_key),
            ),
        }
    }

    // ---- cross-project contact + products (bd-mail-cross-project-contact) ----

    /// `request_contact`.
    pub fn request_contact(
        &self,
        project_key: &str,
        from_agent: &str,
        to_agent: &str,
        to_project: Option<&str>,
        reason: &str,
        _ttl_seconds: Option<u64>,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let link = block_on(mailbox().request_contact(&RequestContact {
                    project_slug: project_key.to_string(),
                    from_agent: from_agent.to_string(),
                    to_agent: to_agent.to_string(),
                    to_project: to_project.map(String::from),
                    reason: Some(reason.to_string()),
                }))?;
                Ok(link.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "request_contact",
                crate::backend::wire::request_contact_args(
                    project_key,
                    from_agent,
                    to_agent,
                    to_project,
                    reason,
                ),
            ),
        }
    }

    /// `respond_contact`.
    pub fn respond_contact(
        &self,
        project_key: &str,
        to_agent: &str,
        from_agent: &str,
        accept: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let link = block_on(mailbox().respond_contact(&RespondContact {
                    project_slug: project_key.to_string(),
                    to_agent: to_agent.to_string(),
                    from_agent: from_agent.to_string(),
                    accept,
                    from_project: None,
                }))?;
                Ok(link.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "respond_contact",
                crate::backend::wire::respond_contact_args(
                    project_key,
                    to_agent,
                    from_agent,
                    accept,
                ),
            ),
        }
    }

    /// `list_contacts`.
    pub fn list_contacts(&self, project_key: &str, agent_name: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let links = block_on(mailbox().list_contacts(project_key, agent_name))?;
                Ok(Value::Array(links.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "list_contacts",
                crate::backend::wire::list_contacts_args(project_key, agent_name),
            ),
        }
    }

    /// `set_contact_policy`.
    pub fn set_contact_policy(
        &self,
        project_key: &str,
        agent_name: &str,
        policy: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let id = block_on(mailbox().set_contact_policy(
                    project_key,
                    agent_name,
                    ContactPolicy::parse(policy)?,
                ))?;
                Ok(json!({ "policy": policy, "event_id": id }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "set_contact_policy",
                    crate::backend::wire::set_contact_policy_args(project_key, agent_name, policy),
                )?;
                Ok(json!({ "policy": policy, "event_id": res }))
            }
        }
    }

    /// `ensure_product` (idempotent product manifest; `name` defaults to the
    /// key when the caller has no separate display name).
    pub fn ensure_product(&self, product_key: &str, name: Option<&str>) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let info =
                    block_on(mailbox().ensure_product(product_key, name.unwrap_or(product_key)))?;
                Ok(info.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "ensure_product",
                crate::backend::wire::ensure_product_args(product_key),
            ),
        }
    }

    /// `products_link`.
    pub fn products_link(&self, product_key: &str, project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let info = block_on(mailbox().products_link(product_key, project_key))?;
                Ok(info.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "products_link",
                crate::backend::wire::products_link_args(product_key, project_key),
            ),
        }
    }

    /// `search_messages_product`.
    pub fn search_product(&self, product_key: &str, query: &str, limit: u32) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let res = block_on(mailbox().search_messages_product(
                    product_key,
                    query,
                    Some(limit as usize),
                    true,
                ))?;
                Ok(json!({ "result": res.iter().map(|(p, m)| {
                    let mut v = m.to_json();
                    v["project"] = json!(p);
                    v
                }).collect::<Vec<Value>>() }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "search_messages_product",
                    crate::backend::wire::search_product_args(product_key, query, limit),
                )?;
                Ok(json!({ "result": res }))
            }
        }
    }

    /// `fetch_inbox_product`.
    pub fn inbox_product(&self, product_key: &str, agent_name: &str, limit: u32) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let res = block_on(mailbox().fetch_inbox_product(product_key, agent_name))?;
                Ok(
                    json!({ "messages": res.iter().take(limit as usize).map(|(p, m)| {
                    let mut v = m.to_json();
                    v["project"] = json!(p);
                    v
                }).collect::<Vec<Value>>() }),
                )
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "fetch_inbox_product",
                    crate::backend::wire::inbox_product_args(product_key, agent_name, limit),
                )?;
                let msgs = res
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit as usize)
                            .cloned()
                            .collect::<Vec<Value>>()
                    })
                    .unwrap_or_default();
                Ok(json!({ "messages": msgs }))
            }
        }
    }

    // ---- build slots (bd-mail-build-slot-gating) ----

    /// `acquire_build_slot`.
    pub fn acquire_slot(
        &self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
        exclusive: bool,
        ttl_seconds: u64,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().acquire_build_slot(
                    project_key,
                    agent_name,
                    slot,
                    Some(ttl_seconds),
                    exclusive,
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "acquire_build_slot",
                crate::backend::wire::slot_acquire_args(
                    project_key,
                    agent_name,
                    slot,
                    exclusive,
                    ttl_seconds,
                ),
            ),
        }
    }

    /// `renew_build_slot` (ADR-0003 V6: the `MailBackend::slot_renew` surface
    /// consumes this; no longer dead code).
    pub fn renew_slot(
        &self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
        extend_seconds: u64,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().renew_build_slot(
                    project_key,
                    agent_name,
                    slot,
                    extend_seconds,
                ))?;
                Ok(match r {
                    Some(b) => b.to_json(),
                    None => json!({ "slot": slot, "renewed": false }),
                })
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "renew_build_slot",
                    crate::backend::wire::slot_renew_args(
                        project_key,
                        agent_name,
                        slot,
                        extend_seconds,
                    ),
                )?;
                if res.is_null() {
                    Ok(json!({ "slot": slot, "renewed": false }))
                } else {
                    Ok(res)
                }
            }
        }
    }

    /// `release_build_slot`.
    pub fn release_slot(&self, project_key: &str, agent_name: &str, slot: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                block_on(mailbox().release_build_slot(project_key, agent_name, slot))?;
                Ok(json!({ "slot": slot, "released": true }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "release_build_slot",
                    crate::backend::wire::slot_release_args(project_key, agent_name, slot),
                )?;
                Ok(json!({
                    "slot": slot,
                    "released": res.get("released").cloned().unwrap_or(json!(true)),
                }))
            }
        }
    }

    // ---- pane identity bridge (bd-mail-pane-identity-bridge, ADR-0001 A4) ----

    /// Write the per-pane identity file the agent-mail server reads for
    /// `resolve_pane_identity`: `<config>/agent-mail/identity/<project_hash>/
    /// <pane_id>` containing `<name>\n`. A filesystem op, not an HTTP call —
    /// byte-for-byte what agent-mail core writes (verified against the live
    /// server 2026-08-05). Returns the path written.
    pub fn write_pane_identity(
        &self,
        project_key: &str,
        pane_id: &str,
        agent_name: &str,
    ) -> Result<PathBuf> {
        write_identity_for(project_key, pane_id, agent_name)
    }

    /// Remove the per-pane identity file (pane exited / respawned). No-op if
    /// absent — duplicate exit events must not fail. Returns the path.
    pub fn clear_pane_identity(&self, project_key: &str, pane_id: &str) -> Result<PathBuf> {
        clear_identity_for(project_key, pane_id)
    }

    /// Resolve the agent name for a pane from the canonical identity file
    /// (in-process read — the deleted HTTP client delegated to the agent-mail
    /// MCP tool, which read the same file). No file → a JSON without
    /// `agent_name` (callers treat that as not-found).
    ///
    /// bd-k6cw: live pane tokens first — a `toron:mail-identity` stamp
    /// (`toron_pin` token) wins over the file, so a reused pane id resolves
    /// to the current pin even when the identity file still names the
    /// previous occupant. Any herdr failure degrades to the file bridge
    /// below, never an error.
    pub fn resolve_pane_identity(&self, project_key: &str, pane_id: &str) -> Value {
        if multiplexer_is_herdr()
            && let Ok(backend) = toron_core::backend::herdr::HerdrBackend::connect(None)
        {
            let client = backend.client();
            if let Ok(workspaces) = client.workspace_list() {
                for ws in workspaces {
                    let panes = client.pane_list(Some(&ws.workspace_id)).unwrap_or_default();
                    match panes.iter().find(|p| p.pane_id == pane_id) {
                        Some(pane) => {
                            if let Some(pin) = pane
                                .tokens
                                .get(toron_core::backend::herdr::TOKEN_PIN)
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            {
                                return json!({
                                    "agent_name": pin,
                                    "pane_id": pane_id,
                                    "source": "pane-token",
                                });
                            }
                            break; // pane live but unstamped → file bridge
                        }
                        None => continue, // not in this workspace → keep looking
                    }
                }
            }
        }
        let path = canonical_identity_path(project_key, pane_id);
        match std::fs::read_to_string(&path) {
            Ok(name) if !name.trim().is_empty() => json!({
                "agent_name": name.trim(),
                "pane_id": pane_id,
                "identity_path": path.to_string_lossy(),
            }),
            _ => json!({
                "error": "no identity file",
                "pane_id": pane_id,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Pane identity files (bd-mail-pane-identity-bridge, ADR-0001 A4)
// ---------------------------------------------------------------------------

/// c3b.7: is herdr the ACTIVE multiplexer? Distinguishes "herdr is the
/// configured mux but the daemon is down" (a REAL failure the caller must
/// see) from "the mux is not herdr" (cleanup genuinely not applicable).
/// Mirrors factory precedence: `FLYWHEEL_BACKEND` env > `[backend]
/// multiplexer` config > default (`herdr`). Config load is best-effort —
/// no config file means the default applies.
fn multiplexer_is_herdr() -> bool {
    crate::config::Config::load()
        .map(|cfg| crate::backend::factory::resolve_backend_name(&cfg))
        .unwrap_or_else(|_| {
            // Config unreadable (e.g. tests without a config file): env or
            // the documented default.
            std::env::var("FLYWHEEL_BACKEND")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| crate::backend::factory::DEFAULT_BACKEND.to_string())
        })
        == "herdr"
}

//
// flywheel's bridge for herdr panes: agent-mail writes per-pane identity
// files only for tmux panes; herdr panes (wT:p1) never get one, so
// `resolve_pane_identity` can't discover agents spawned by flywheel. The
// bridge writes the same file agent-mail core would have: 12-hex-char SHA-1
// of the canonicalized absolute project path as the directory name, the
// sanitized pane id as the filename, the agent name as a single trimmed line.
// All derivation + format details below were verified byte-for-byte against
// the live agent-mail server on 2026-08-05 (probe file resolved with the
// exact `identity_path` returned).

/// Config base for identity files: `$XDG_CONFIG_HOME` when set, else
/// `~/.config` — identical to agent-mail core's `config_base_dir()`, so the
/// bridge writes exactly where the server reads.
///
/// `#[cfg(test)]`: a process-global override pins the base to a temp dir so
/// tests never touch the real `~/.config` AND stay immune to parallel tests
/// that mutate `XDG_CONFIG_HOME`/`HOME` (the write+clear pair must resolve the
/// same path regardless of env races).
#[cfg(test)]
static TEST_IDENTITY_BASE: std::sync::LazyLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

fn identity_base() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_IDENTITY_BASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return path;
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg);
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from("/tmp/.config")
}

/// Truncated SHA-1 (lower hex, 12 chars) of the canonicalized absolute
/// project path — the exact derivation agent-mail core uses for
/// `identity/<project_hash>/`. Existing paths are canonicalized before
/// hashing (symlinked checkouts converge); a missing path is hashed as-is.
pub fn project_hash(project_key: &str) -> String {
    let key = PathBuf::from(project_key);
    let normalized = if key.is_absolute() {
        std::fs::canonicalize(&key).unwrap_or(key)
    } else {
        key
    };
    let mut hasher = Sha1::new();
    hasher.update(normalized.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize()).chars().take(12).collect()
}

/// Sanitize a pane id for use as a filename: strip a tmux `%` prefix, map
/// `:` to `-` and other filesystem-unsafe chars to `_` (agent-mail core
/// parity — herdr `w1:p1` → `w1-p1`). Empty → `unknown`.
fn sanitize_pane_id(pane_id: &str) -> String {
    let stripped = pane_id.strip_prefix('%').unwrap_or(pane_id);
    let mut out = String::with_capacity(stripped.len());
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch == ':' {
            out.push('-');
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// Canonical identity file path for a project + pane:
/// `<config>/agent-mail/identity/<project_hash>/<sanitized_pane_id>`.
pub fn canonical_identity_path(project_key: &str, pane_id: &str) -> PathBuf {
    identity_base()
        .join("agent-mail")
        .join("identity")
        .join(project_hash(project_key))
        .join(sanitize_pane_id(pane_id))
}

/// Write the identity file atomically (temp + rename), mirroring agent-mail
/// core's `write_identity_file` (0600, overwrite-only, no symlink).
pub fn write_identity_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename onto {}", path.display()))?;
    Ok(())
}

/// Write a per-pane identity file with the agent-mail-core guardrails (empty-
/// name refusal, symlink refusal). Shared by the toron `Mail` facade and the
/// real agent-mail backend (ADR-0003 V3) — both write the SAME file the
/// agent-mail server reads.
pub fn write_identity_for(project_key: &str, pane_id: &str, agent_name: &str) -> Result<PathBuf> {
    let name = agent_name.trim();
    if name.is_empty() {
        bail!("refusing to write empty pane identity for {pane_id}");
    }
    let path = canonical_identity_path(project_key, pane_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!(
            "refusing to overwrite symlinked pane identity {}",
            path.display()
        );
    }
    write_identity_file(&path, format!("{name}\n").as_bytes())?;
    Ok(path)
}

/// Remove a per-pane identity file (no-op when absent). Shared by the toron
/// `Mail` facade and the real agent-mail backend.
pub fn clear_identity_for(project_key: &str, pane_id: &str) -> Result<PathBuf> {
    let path = canonical_identity_path(project_key, pane_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(e) => Err(e).context("remove pane identity"),
    }
}

/// One renewed reservation, as reported by `renew_file_reservations`.
/// The id is a String: toron reservation ids are hex event ids, agent-mail's
/// are decimal — both pass through without numeric coercion (se2.2).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RenewedReservation {
    pub id: String,
    pub path_pattern: String,
    pub old_expires_ts: String,
    pub new_expires_ts: String,
}

/// Extract the renewed reservations from a `renew_file_reservations`
/// response. Agent-mail exposes no list-reservations MCP tool (verified
/// against the full tools/list surface), so the renew response is the
/// authoritative "what did I renew" source — the durable heartbeat logs this
/// list each tick (bd-mail-lease-renewal-durable).
pub fn list_own_reservations(resp: &Value) -> Vec<RenewedReservation> {
    let mut out = Vec::new();
    let Some(list) = resp.get("file_reservations").and_then(Value::as_array) else {
        return out;
    };
    for r in list {
        out.push(RenewedReservation {
            id: match r.get("id") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            },
            path_pattern: r
                .get("path_pattern")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            old_expires_ts: r
                .get("old_expires_ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            new_expires_ts: r
                .get("new_expires_ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}

/// bd-mail-inbox-json-a7wc: the DOCUMENTED `mail inbox --json` contract.
/// One JSON array; each row carries EXACTLY these keys with stable types:
/// id (string — legacy numeric ids are stringified), thread_id (string|null),
/// sender, subject, importance, body_md (string), ack_required (bool —
/// AM 0/1 coerced), read_ts_us / created_ts_us (i64|null), duplicate_of
/// (string|null). Unknown upstream keys are dropped so the shape cannot
/// drift under callers; a non-array input yields an empty array.
///
/// The remote daemon wraps the same rows as `{"messages":[...]}`. Unwrap
/// that one key. Any other object stays empty so a foreign envelope
/// (`{"workflows":[]}`) cannot be mistaken for mail.
pub fn inbox_json_rows(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::json;
    let rows = match v.as_array() {
        Some(a) => a,
        None => match v.get("messages").and_then(serde_json::Value::as_array) {
            Some(a) => a,
            None => return json!([]),
        },
    };
    let opt_str = |r: &serde_json::Value, k: &str| {
        r.get(k)
            .and_then(|x| x.as_str())
            .map(|s| json!(s))
            .unwrap_or(json!(null))
    };
    let str_coerce = |r: &serde_json::Value, k: &str| match r.get(k) {
        Some(serde_json::Value::String(s)) => json!(s),
        Some(serde_json::Value::Number(n)) => json!(n.to_string()),
        _ => json!(null),
    };
    let ts = |r: &serde_json::Value, k: &str| {
        r.get(k)
            .and_then(serde_json::Value::as_i64)
            .map(|n| json!(n))
            .unwrap_or(json!(null))
    };
    let ack = |r: &serde_json::Value| match r.get("ack_required") {
        Some(serde_json::Value::Bool(b)) => json!(b),
        Some(serde_json::Value::Number(n)) => json!(n.as_i64().unwrap_or(0) != 0),
        _ => json!(false),
    };
    serde_json::Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "id": str_coerce(r, "id"),
                    "thread_id": opt_str(r, "thread_id"),
                    "sender": opt_str(r, "sender"),
                    "subject": opt_str(r, "subject"),
                    "importance": opt_str(r, "importance"),
                    "ack_required": ack(r),
                    "read_ts_us": ts(r, "read_ts_us"),
                    "created_ts_us": ts(r, "created_ts_us"),
                    "duplicate_of": opt_str(r, "duplicate_of"),
                    "body_md": opt_str(r, "body_md"),
                })
            })
            .collect(),
    )
}

/// bd-pin-npub-binding-ggym: the send door's recipient guard, with the whois
/// lookup injected so the wiring (skip npubs, one query per NAME, refusal
/// propagated) is testable without a live mailbox. `whois` returns the mailbox
/// payload for a name, or `None` when it cannot answer — absence is not
/// divergence and still routes.
fn guard_targets(targets: &[String], mut whois: impl FnMut(&str) -> Option<Value>) -> Result<()> {
    for target in targets {
        if crate::recipient_guard::is_explicit_npub(target) {
            continue;
        }
        let who = whois(target);
        let binding = crate::recipient_guard::binding_from_whois(who.as_ref());
        if let Err(refusal) = crate::recipient_guard::guard_recipient(target, &binding) {
            bail!("{refusal}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd-pin-npub-binding-ggym, wiring half: the send door asks whois once per
    /// NAME, never for an explicit npub, and a divergent name refuses the send
    /// before the backend call. `recipient_guard::tests` owns the decision.
    #[test]
    fn guard_targets_is_one_query_per_name_and_refuses_the_divergent_one() {
        let divergent = serde_json::json!({
            "npub": "6778eb30f5bb7b08ac2a47049378ff6b5c7fcc4f6a9c227f2ef5e06809999469",
            "pane_npub": "facd8c1f2a3b4c5d6e7f8091a2b3c4d5e6f70819a2b3c4d5e6f70819a2b3c4d5",
            "identity_divergent": true,
        });
        let bound = serde_json::json!({
            "npub": "aa11",
            "pane_npub": "aa11",
            "identity_divergent": false,
        });
        let asked = std::cell::RefCell::new(Vec::<String>::new());
        let whois = |name: &str| {
            asked.borrow_mut().push(name.to_string());
            match name {
                "Buffy" => Some(divergent.clone()),
                "Someone" => Some(bound.clone()),
                _ => None,
            }
        };

        let targets = vec![
            "Buffy".to_string(),
            "npub1qqqqqqqqqq".to_string(),
            "Someone".to_string(),
        ];
        let refusal = guard_targets(&targets, whois).expect_err("Buffy is divergent");
        let refusal = refusal.to_string();
        assert!(refusal.contains("re-register"), "{refusal}");
        assert_eq!(
            *asked.borrow(),
            vec!["Buffy".to_string()],
            "the divergent name is refused first and an explicit npub is never queried"
        );

        // Without the divergent recipient the same targets go through, and the
        // npub target is still never queried.
        asked.borrow_mut().clear();
        let ok_targets = vec!["Someone".to_string(), "npub1qqqqqqqqqq".to_string()];
        guard_targets(&ok_targets, whois).expect("bound + npub route");
        assert_eq!(*asked.borrow(), vec!["Someone".to_string()]);

        // A name the mailbox cannot answer for is not divergence.
        asked.borrow_mut().clear();
        guard_targets(&["Nobody".to_string()], whois).expect("absence routes");
        assert_eq!(*asked.borrow(), vec!["Nobody".to_string()]);
    }

    #[test]
    fn from_config_none_defaults_to_in_process_flywheel() {
        let mail = Mail::from_config(None)
            .expect("from_config ok")
            .expect("Some");
        assert_eq!(mail.name, "flywheel");
        assert!(matches!(&mail.backend, BackendKind::InProcess));
    }

    /// One row per config: an absent url and an empty url both mean the
    /// embedded in-process backend (never a remote dial), and a configured
    /// name is carried onto the facade.
    #[test]
    fn from_config_without_or_empty_url_is_in_process() {
        let rows = [
            ("url-none", None, Some("orchestrator")),
            ("url-empty", Some(String::new()), None),
        ];
        for (name, url, expect_name) in rows {
            let cfg = MailConfig {
                url,
                token: None,
                name: expect_name.map(str::to_string),
            };
            let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
            assert!(
                matches!(&mail.backend, BackendKind::InProcess),
                "row {name}: in-process"
            );
            if let Some(n) = expect_name {
                assert_eq!(mail.name, n, "row {name}: name carried");
            }
        }
    }

    #[test]
    fn from_config_with_url_picks_remote() {
        let cfg = MailConfig {
            url: Some("http://127.0.0.1:19001".into()),
            token: Some("secret".into()),
            name: Some("orchestrator".into()),
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        assert_eq!(mail.name, "orchestrator");
        assert!(matches!(&mail.backend, BackendKind::Remote(_)));
    }

    #[test]
    fn remote_without_url_returns_none() {
        assert!(Mail::remote(None).expect("ok").is_none());
        let cfg = MailConfig {
            url: None,
            token: None,
            name: None,
        };
        assert!(Mail::remote(Some(&cfg)).expect("ok").is_none());
    }

    #[test]
    fn remote_with_url_constructs_facade() {
        let cfg = MailConfig {
            url: Some("http://127.0.0.1:19001".into()),
            token: None,
            name: Some("smoke".into()),
        };
        let mail = Mail::remote(Some(&cfg)).expect("ok").expect("Some");
        assert_eq!(mail.name, "smoke");
        assert!(matches!(&mail.backend, BackendKind::Remote(_)));
    }

    #[test]
    fn call_remote_on_in_process_backend_errors() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        assert!(mail.call_remote("health_check", json!({})).is_err());
    }

    /// se2.1 Phase A (c3b.1): all ops share ONE tokio runtime per process —
    /// `from_config` builds it eagerly, and ops run on it (no `Runtime::new`
    /// per op).
    #[test]
    fn block_on_uses_shared_runtime() {
        let _mail = Mail::from_config(None).expect("ok").expect("Some");
        assert!(
            RUNTIME.get().is_some(),
            "from_config initialized the runtime"
        );
        assert!(block_on(mailbox().ensure_project("se2.1-runtime-test")).is_ok());
        assert!(
            block_on(mailbox().ensure_project("se2.1-runtime-test-2")).is_ok(),
            "second op reuses the same runtime"
        );
    }

    /// se2.1 Phase B (bd-t9ey): the default root is DURABLE and XDG-based —
    /// `$XDG_DATA_HOME/toron` / `~/.local/share/toron` via
    /// `toron_core::config::resolve_data_root` (ADR-0007 D3) — NEVER a
    /// per-pid temp `flywheel-mail-{pid}.sqlite`. `TORON_DATA_DIR` still
    /// wins; `TORON_MCP_DATA_DIR` stays the demo/test isolation alias.
    #[test]
    fn mailbox_roots_env_and_default() {
        // Env mutation is process-global — serialize under the crate-wide
        // env lock (193.2); siblings (factory FLYWHEEL_MAIL, config XDG)
        // share the same lock.
        let _g = crate::config::lock_env();
        unsafe { std::env::set_var("TORON_DATA_DIR", "/tmp/flywheel-se2.1-durable") };
        unsafe { std::env::set_var("TORON_MCP_DATA_DIR", "/tmp/flywheel-se2.1-mcp") };
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/flywheel-se2.1-xdg") };
        let (data_dir, db) = mailbox_roots();
        // bd-hdec: the mailbox data dir carries the canonical `projects/`
        // segment (toron parity) while the index stays at the root.
        assert_eq!(
            data_dir,
            PathBuf::from("/tmp/flywheel-se2.1-durable/projects"),
            "canonical projects/ segment (bd-hdec)"
        );
        // bd-enov: ONE index — the file toron's ToronPaths::index_path()
        // resolves, never a private flywheel-mail.sqlite.
        assert_eq!(
            db,
            PathBuf::from("/tmp/flywheel-se2.1-durable/index.sqlite")
        );
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
        let (data_dir, _) = mailbox_roots();
        assert_eq!(
            data_dir,
            PathBuf::from("/tmp/flywheel-se2.1-mcp/projects"),
            "alias fallback, canonical segment"
        );
        unsafe { std::env::set_var("TORON_DATA_DIR", "   ") };
        let (data_dir, _) = mailbox_roots();
        assert_eq!(
            data_dir,
            PathBuf::from("/tmp/flywheel-se2.1-mcp/projects"),
            "blank counts as unset"
        );
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
        unsafe { std::env::remove_var("TORON_MCP_DATA_DIR") };
        let (data_dir, db) = mailbox_roots();
        assert_eq!(
            data_dir,
            PathBuf::from("/tmp/flywheel-se2.1-xdg/toron/projects")
        );
        assert_eq!(
            db,
            PathBuf::from("/tmp/flywheel-se2.1-xdg/toron/index.sqlite")
        );
        assert!(
            !db.to_string_lossy()
                .contains(&format!("flywheel-mail-{}", std::process::id())),
            "no pid-scoped sqlite"
        );
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        unsafe { std::env::set_var("HOME", "/tmp/flywheel-se2.1-home") };
        let (data_dir, _) = mailbox_roots();
        // bd-fize: HOME is not an explicit pin — in a TEST process it must
        // not resolve to a live-looking root. The temp root wins instead, so
        // a test never writes identities into a real-looking store.
        assert!(
            !data_dir.starts_with("/tmp/flywheel-se2.1-home"),
            "bare HOME in a test process must not resolve under HOME: {data_dir:?}"
        );
        assert!(
            data_dir.starts_with(std::env::temp_dir()),
            "test process without an explicit pin resolves to temp: {data_dir:?}"
        );
        unsafe { std::env::remove_var("HOME") };
        let (data_dir, db) = mailbox_roots();
        // Same with nothing set at all: the relative `toron-data` fallback
        // is production-only; a test process gets temp, never relative.
        assert!(
            data_dir.starts_with(std::env::temp_dir()),
            "no env in a test process resolves to temp, never the relative fallback: {data_dir:?}"
        );
        assert!(
            !db.to_string_lossy().contains("toron-data"),
            "no relative index in a test process: {db:?}"
        );
    }

    /// bd-fize: the production half of the contract, pinned without env
    /// mutation — an explicit struct, not the ambient process. Explicit pins
    /// win in any process; HOME and the relative fallback only apply
    /// outside a test binary.
    #[test]
    fn data_root_production_contract_is_pure() {
        use toron_core::config::{DataRootEnv, resolve_data_root};
        // An explicit pin wins even in a test process.
        let env = DataRootEnv {
            toron_data_dir: Some(PathBuf::from("/env/root")),
            xdg_data_home: None,
            home: Some(PathBuf::from("/home/alice")),
            test_process: true,
        };
        assert_eq!(resolve_data_root(&env), PathBuf::from("/env/root"));
        // Production HOME fallback, unchanged.
        let env = DataRootEnv {
            toron_data_dir: None,
            xdg_data_home: None,
            home: Some(PathBuf::from("/home/alice")),
            test_process: false,
        };
        assert_eq!(
            resolve_data_root(&env),
            PathBuf::from("/home/alice/.local/share/toron")
        );
        // Production no-env fallback, unchanged.
        let env = DataRootEnv::default();
        assert_eq!(resolve_data_root(&env), PathBuf::from("toron-data"));
    }

    /// bd-enov: the embedded mailbox's index is byte-identical to toron's
    /// `ToronPaths::index_path()` — one roster, no forked identity plane.
    #[test]
    fn mailbox_index_is_torons_shared_index() {
        let _g = crate::config::lock_env();
        unsafe { std::env::set_var("TORON_DATA_DIR", "/tmp/flywheel-bd-enov") };
        unsafe { std::env::remove_var("TORON_MCP_DATA_DIR") };
        let (_data_dir, db) = mailbox_roots();
        assert_eq!(
            db,
            toron_core::config::ToronPaths {
                root: PathBuf::from("/tmp/flywheel-bd-enov"),
            }
            .index_path()
        );
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
    }

    /// bd-*: the presence projection lives at the durable ROOT, not under
    /// `projects/`. Toron lifts the mailbox data dir via `mailbox_root`
    /// (bd-m5ym), so a pin's 20001 rows land at `<root>/presence.jsonl`.
    /// Reading `<root>/projects/presence.jsonl` finds a file nothing writes:
    /// every pin reads Unknown-presence, the filter holds every wake for
    /// digest, and the ring never fires (the silent-deafness this guards).
    #[test]
    fn presence_path_is_toron_root_level() {
        let _g = crate::config::lock_env();
        unsafe { std::env::set_var("TORON_DATA_DIR", "/tmp/flywheel-presence-path") };
        unsafe { std::env::remove_var("TORON_MCP_DATA_DIR") };
        let (data_dir, _db) = mailbox_roots();
        let expected = toron_core::config::mailbox_root(&data_dir).join("presence.jsonl");
        assert_eq!(
            presence_path(),
            PathBuf::from("/tmp/flywheel-presence-path/presence.jsonl"),
            "presence projection is a root-level file (toron parity)"
        );
        assert_eq!(presence_path(), expected, "mirrors toron's presence_path");
        assert!(
            presence_path().parent().and_then(|p| p.file_name())
                != Some(std::ffi::OsStr::new("projects")),
            "never the dead projects/presence.jsonl"
        );
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
    }

    /// se2.2: toron reservation ids are hex event ids — `list_own_reservations`
    /// must keep them as strings, never coerce a hex id to 0.
    #[test]
    fn list_own_reservations_keeps_hex_id() {
        let resp = json!({
            "renewed": 1,
            "file_reservations": [{
                "id": "a1b2c3d4e5f60718293a4b5c6d7e8f90",
                "path_pattern": "src/*.rs",
                "old_expires_ts": "",
                "new_expires_ts": "1750000000000000",
            }]
        });
        let out = list_own_reservations(&resp);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].id, "a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "hex id passes through untouched"
        );
        assert_ne!(out[0].id, "0", "hex id never becomes 0");
    }

    /// se2.2: agent-mail's numeric reservation ids surface as their decimal
    /// string — never "".
    #[test]
    fn list_own_reservations_keeps_numeric_id() {
        let resp = json!({
            "renewed": 1,
            "file_reservations": [{
                "id": 42,
                "path_pattern": "src/*.rs",
                "old_expires_ts": "",
                "new_expires_ts": "",
            }]
        });
        let out = list_own_reservations(&resp);
        assert_eq!(out[0].id, "42");
    }

    /// bd-reserve-yield-taqf: the flywheel read of toron's structured
    /// stand-downs. Only yields whose holder is LIVE come back, a yield
    /// naming a holder with no lease claims nothing, and a release clears the
    /// pair with no delete path (suppression is derived from the board).
    #[test]
    fn yielded_pairs_reads_the_live_board() {
        // Reads the shared mail root — serialize under the crate-wide env
        // lock; sibling env tests swap TORON_*_DATA_DIR.
        let _g = crate::config::lock_env();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "yield-pairs-board";
        mail.ensure_project(project).expect("ensure");
        for name in ["BlueLake", "RoseDune"] {
            mail.register_fixture(project, name, "test", "test", "yield-read test")
                .expect("register");
        }
        mail.reserve(
            project,
            "RoseDune",
            &["src/**".into()],
            3600,
            true,
            "lane-work",
        )
        .expect("RoseDune holds the lane");

        // No marker yet: a live lease alone yields no pair.
        assert!(
            mail.yielded_pairs(project).expect("read").is_empty(),
            "a live lease is not a stand-down"
        );

        block_on(mailbox().record_reservation_yield(
            project,
            "BlueLake",
            "lane-bead",
            "RoseDune",
            "peer works it",
        ))
        .expect("record yield");
        assert_eq!(
            mail.yielded_pairs(project).expect("read"),
            vec![("lane-bead".to_string(), "RoseDune".to_string())],
            "a yield naming a live holder is a peer pair"
        );

        // A yield naming a holder who holds nothing is inert.
        block_on(mailbox().record_reservation_yield(
            project,
            "BlueLake",
            "ghost-bead",
            "GhostPane",
            "no lease",
        ))
        .expect("record yield");
        assert_eq!(
            mail.yielded_pairs(project).expect("read").len(),
            1,
            "an unresolvable holder claims nothing"
        );

        // Release: the pair clears, so the loop resumes the bead again.
        mail.release_paths(project, "RoseDune", &["src/**".into()])
            .expect("release");
        assert!(
            mail.yielded_pairs(project).expect("read").is_empty(),
            "a released holder stops suppressing with no delete path"
        );
    }

    /// bd-dsu1: the operator lock board dumps every active lease (id +
    /// holder) and filters by holder / path-glob overlap. Read-only — no
    /// project creation, no identity registration.
    #[test]
    fn list_active_boards_and_filters() {
        // bd-0rzv: reads the shared mail root — serialize under the
        // crate-wide env lock; sibling env tests swap TORON_*_DATA_DIR.
        let _g = crate::config::lock_env();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "locks-list-board";
        mail.ensure_project(project).expect("ensure");
        for name in ["BlueLake", "RoseDune"] {
            mail.register_fixture(project, name, "test", "test", "bd-dsu1 test")
                .expect("register");
        }
        mail.reserve(
            project,
            "BlueLake",
            &["src/**".into()],
            3600,
            true,
            "bd-dsu1",
        )
        .expect("BlueLake hold");
        mail.reserve(
            project,
            "RoseDune",
            &["docs/**".into()],
            3600,
            true,
            "bd-dsu1",
        )
        .expect("RoseDune hold");

        let board = mail.list_active(project, None, None).expect("board");
        assert_eq!(
            board["count"].as_u64().unwrap_or(0),
            2,
            "both leases on the board: {board}"
        );
        for row in board["reservations"].as_array().expect("rows") {
            assert!(
                !row["id"].as_str().unwrap_or("").is_empty(),
                "row carries the reservation id: {row}"
            );
            assert!(
                !row["holder"].as_str().unwrap_or("").is_empty(),
                "row names its holder: {row}"
            );
            assert!(
                row["exclusive"].as_bool().unwrap_or(false),
                "exclusive flag present: {row}"
            );
            assert_eq!(row["reason"].as_str().unwrap_or(""), "bd-dsu1");
        }

        let blue = mail
            .list_active(project, Some("BlueLake"), None)
            .expect("holder filter");
        assert_eq!(
            blue["count"].as_u64().unwrap_or(0),
            1,
            "holder filter narrows: {blue}"
        );
        assert_eq!(
            blue["reservations"][0]["path"].as_str().unwrap_or(""),
            "src/**"
        );

        let src = mail
            .list_active(project, None, Some("src/lib.rs"))
            .expect("path filter");
        assert_eq!(
            src["count"].as_u64().unwrap_or(0),
            1,
            "glob overlap narrows: {src}"
        );
        assert_eq!(
            src["reservations"][0]["holder"].as_str().unwrap_or(""),
            "BlueLake"
        );

        let none = mail
            .list_active(project, Some("Nobody"), None)
            .expect("no match");
        assert_eq!(
            none["count"].as_u64().unwrap_or(0),
            0,
            "unknown holder → empty board"
        );
    }

    /// S8.4: the swarm-start register_agent upsert clears stale profile task
    /// fields from a prior session. A 2026-08-14 identity still advertised
    /// `bd-qk9q`; re-registering the canonical name with the spawn profile
    /// must overwrite the task — never preserve the stale bead id.
    #[test]
    fn register_upsert_clears_stale_task_on_reuse() {
        // bd-0rzv: reads the shared mail root — serialize under the
        // crate-wide env lock; sibling env tests swap TORON_*_DATA_DIR.
        let _g = crate::config::lock_env();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "upsert-stale-task";
        mail.ensure_project(project).expect("ensure");
        // Prior session: the agent self-registered with a stale bead task.
        mail.register_fixture(
            project,
            "flywheel-proj-oc",
            "opencode",
            "deepseek-v4.1-flash",
            "bd-qk9q implementer: stranded-bead reclaim fix",
        )
        .expect("prior session registration");
        // Swarm start: the spawn profile upserts the canonical name with a
        // fresh task stamp (S8.3/S8.4 — the same call register_spawned_agent
        // makes).
        let profile = crate::spawn_agent_profile("flywheel-proj-oc", "opencode", project);
        block_on(mailbox().register_agent(project, &profile)).expect("upsert");
        let who = mail.whois(project, "flywheel-proj-oc").expect("whois");
        let task = who["task_description"].as_str().unwrap_or("");
        assert!(
            task.contains("flywheel agent for upsert-stale-task"),
            "fresh task stamp wins: {task}"
        );
        assert!(!task.contains("bd-qk9q"), "stale bead task cleared: {task}");
    }

    /// bd-pncn: `--to all` broadcasts to every registered agent's inbox
    /// (roster expansion), not a literal "all" recipient.
    #[test]
    fn send_to_all_broadcasts_to_every_registered_agent() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "broadcast-all";
        mail.ensure_project(project).expect("ensure");
        for name in ["flywheel", "BlueLake", "RoseDune"] {
            mail.register_fixture(project, name, "test", "test", "bd-pncn test")
                .expect("register");
        }
        let v = mail
            .send(
                project,
                &["all".into()],
                "br",
                "broadcast body",
                "normal",
                false,
                Some("bd-pncn"),
            )
            .expect("broadcast");
        assert_eq!(
            v["count"].as_u64().unwrap_or(0),
            3,
            "one delivery per registered agent: {v}"
        );
        for name in ["BlueLake", "RoseDune"] {
            let inbox = mail.inbox(project, name, false, 10).expect("inbox");
            assert!(
                inbox
                    .as_array()
                    .expect("rows")
                    .iter()
                    .any(|m| m["subject"] == "br"),
                "{name} inbox sees the broadcast: {inbox}"
            );
        }
    }

    /// bd-pncn: zero registered agents → clear error, never a silent no-op.
    #[test]
    fn send_to_all_with_zero_agents_errors() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "broadcast-empty";
        mail.ensure_project(project).expect("ensure");
        let err = mail
            .send(project, &["all".into()], "br", "x", "normal", false, None)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("zero registered agents"),
            "clear error names the problem: {msg}"
        );
    }

    /// bd-pncn: `all` + explicit recipients → each agent delivered exactly
    /// once (dedupe).
    #[test]
    fn send_to_all_plus_explicit_dedupes() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "broadcast-dedupe";
        mail.ensure_project(project).expect("ensure");
        for name in ["flywheel", "BlueLake", "RoseDune"] {
            mail.register_fixture(project, name, "test", "test", "bd-pncn test")
                .expect("register");
        }
        let v = mail
            .send(
                project,
                &["all".into(), "BlueLake".into()],
                "br",
                "x",
                "normal",
                false,
                None,
            )
            .expect("send");
        assert_eq!(v["count"].as_u64().unwrap_or(0), 3, "deduped: {v}");
        let recipients: Vec<&str> = v["deliveries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["payload"]["recipient"].as_str())
            .collect();
        assert_eq!(recipients.len(), 3);
        assert_eq!(
            recipients.iter().filter(|r| **r == "BlueLake").count(),
            1,
            "no double-delivery: {recipients:?}"
        );
    }

    /// se2.2: the remote edge is the toron daemon — a hex message id must be
    /// forwarded verbatim, never parsed. With a dead daemon the error is a
    /// transport failure; a "decimal"/"invalid digit" error would mean the
    /// id was coerced before it ever left (the F3 bug). Both id-taking remote
    /// verbs owe the same pass-through honesty.
    #[test]
    fn remote_id_verbs_pass_hex_id_through() {
        let cfg = MailConfig {
            url: Some("http://127.0.0.1:1".into()),
            token: None,
            name: Some("smoke".into()),
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        let hex = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
        let ack = mail
            .ack("proj", "smoke", hex)
            .expect_err("hex id forwarded; dead daemon must still fail the call");
        let marked = mail
            .mark_read("proj", "smoke", hex)
            .expect_err("hex id forwarded; dead daemon must still fail the call");
        for (verb, err) in [("ack", ack), ("mark_read", marked)] {
            let s = format!("{err:#}");
            assert!(
                !s.contains("decimal"),
                "{verb}: no id parse before dial: {s}"
            );
            assert!(
                !s.contains("invalid digit"),
                "{verb}: no id parse before dial: {s}"
            );
        }
    }

    /// Live remote-daemon smoke: with a `toron serve --mcp-http` daemon up and
    /// `FLYWHEEL_TORON_URL` set, exercises the facade's remote path
    /// end-to-end — `health` carries the `http` transport marker and
    /// `list_agents` returns `[]` from the daemon's fresh mailbox.
    #[test]
    #[ignore = "requires a live toron serve --mcp-http daemon (FLYWHEEL_TORON_URL)"]
    fn remote_facade_against_live_daemon() {
        let Some(url) = std::env::var("FLYWHEEL_TORON_URL")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("skipping: FLYWHEEL_TORON_URL not set");
            return;
        };
        let cfg = MailConfig {
            url: Some(url),
            token: None,
            name: Some("smoke-test".into()),
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        let h = mail.health().expect("remote health");
        assert_eq!(h["transport"], json!("http"), "remote marker present");
        let agents = mail
            .list_agents("/tmp/flywheel-remote-smoke", None)
            .expect("remote list_agents");
        assert_eq!(agents, json!([]), "fresh daemon mailbox has no agents");
    }

    /// c3b.3: `urgent_only` + `since_ts` filter the toron inbox; the row
    /// model supports them (importance + created_ts_us). Seeded via real
    /// send_message round trips so the projection is the code under test.
    #[test]
    fn inbox_honors_urgent_flag_parity_with_mcp() {
        // bd-0rzv: reads the shared mail root — serialize under the
        // crate-wide env lock; sibling env tests swap TORON_*_DATA_DIR.
        let _g = crate::config::lock_env();
        // bd-mail-surface-drift-7r2o: `inbox(urgent_only=true)` must return
        // the SAME id set as `inbox_filtered(urgent_only=true)` — the flag
        // is not decorative. The legacy surface ignored it entirely, so an
        // --urgent caller counted every row in an unfiltered dump.
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("drift-urgent-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        mail.register_fixture(&project, "DriftRecv", "opencode", "test", "seed")
            .expect("recipient registered");
        mail.register_fixture(&project, &mail.name, "opencode", "test", "seed")
            .expect("facade identity registered");
        let recv = vec!["DriftRecv".to_string()];
        mail.send(&project, &recv, "u1", "body", "urgent", false, None)
            .expect("send");
        mail.send(&project, &recv, "n1", "body", "normal", false, None)
            .expect("send");

        let via_inbox = mail.inbox(&project, "DriftRecv", true, 100).expect("inbox");
        let via_filtered = mail
            .inbox_filtered(
                &project,
                "DriftRecv",
                &MailInboxFilter {
                    urgent_only: true,
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("filtered inbox");

        let ids = |v: &Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect()
        };
        assert_eq!(
            ids(&via_inbox),
            ids(&via_filtered),
            "--urgent must match the MCP urgent_only id set"
        );
        assert_eq!(
            ids(&via_inbox).len(),
            1,
            "only the high/urgent row survives"
        );
    }

    #[test]
    fn inbox_filtered_applies_urgent_and_since_on_toron() {
        // bd-0rzv: reads the shared mail root — serialize under the
        // crate-wide env lock; sibling env tests swap TORON_*_DATA_DIR.
        let _g = crate::config::lock_env();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("c3b3-filter-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        mail.register_fixture(&project, "FilterSender", "opencode", "test", "seed")
            .expect("sender registered");
        mail.register_fixture(&project, "FilterRecv", "opencode", "test", "seed")
            .expect("recipient registered");
        // The facade sends AS its own identity (`mail.name` = "flywheel") —
        // it must be registered too or send_message fails on the local key.
        mail.register_fixture(&project, &mail.name, "opencode", "test", "seed")
            .expect("facade identity registered");
        let recv = vec!["FilterRecv".to_string()];

        let send = |subject: &str, importance: &str| {
            mail.send(&project, &recv, subject, "body", importance, false, None)
                .expect("send ok");
        };
        send("urgent-note", "urgent");
        send("high-note", "high");
        send("normal-note", "normal");
        send("low-note", "low");

        // urgent_only: only high/urgent rows survive.
        let urgent = mail
            .inbox_filtered(
                &project,
                "FilterRecv",
                &MailInboxFilter {
                    urgent_only: true,
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("filtered inbox");
        let subjects: Vec<&str> = urgent
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["subject"].as_str())
            .collect();
        assert!(
            subjects
                .iter()
                .all(|s| *s == "urgent-note" || *s == "high-note"),
            "urgent_only keeps only high/urgent: {subjects:?}"
        );
        assert!(subjects.contains(&"urgent-note"), "urgent present");
        assert!(subjects.contains(&"high-note"), "high present");

        // since_ts: only rows strictly newer than the bound survive.
        let since = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(3600))
            .expect("1h ago")
            .to_rfc3339();
        let recent = mail
            .inbox_filtered(
                &project,
                "FilterRecv",
                &MailInboxFilter {
                    since_ts: Some(since),
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("since-filtered inbox");
        let recent_subjects: Vec<&str> = recent
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["subject"].as_str())
            .collect();
        assert_eq!(
            recent_subjects.len(),
            4,
            "all four sent within the last hour: {recent_subjects:?}"
        );

        // since_ts in the future: nothing matches.
        let future = chrono::Utc::now()
            .checked_add_signed(chrono::Duration::seconds(3600))
            .expect("1h ahead")
            .to_rfc3339();
        let none = mail
            .inbox_filtered(
                &project,
                "FilterRecv",
                &MailInboxFilter {
                    since_ts: Some(future),
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("future since-filtered inbox");
        assert_eq!(
            none.as_array().unwrap().len(),
            0,
            "nothing is newer than a future bound"
        );
    }

    /// bd-harvest-thread-filter-yp9r: `thread_id` keeps ONLY the target
    /// thread's rows on the toron projection; the filter is pushed into the
    /// shared core inbox stage (post-unwrap — the thread tag lives inside the
    /// encrypted gift wrap), so `inbox_filtered` with the filter set returns
    /// the thread, not a full-inbox drain.
    #[test]
    fn inbox_filtered_applies_thread_filter_on_toron() {
        // bd-0rzv: reads the shared mail root — serialize under the
        // crate-wide env lock; sibling env tests swap TORON_*_DATA_DIR.
        let _g = crate::config::lock_env();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("harvest-thread-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        mail.register_fixture(&project, "ThreadSender", "opencode", "test", "seed")
            .expect("sender registered");
        mail.register_fixture(&project, "ThreadRecv", "opencode", "test", "seed")
            .expect("recipient registered");
        mail.register_fixture(&project, &mail.name, "opencode", "test", "seed")
            .expect("facade identity registered");
        let recv = vec!["ThreadRecv".to_string()];

        for (subject, thread) in [
            ("t1-a", "debate-1"),
            ("t1-b", "debate-1"),
            ("t2-a", "debate-2"),
        ] {
            mail.send(
                &project,
                &recv,
                subject,
                "threaded",
                "normal",
                false,
                Some(thread),
            )
            .expect("send threaded");
        }

        let all = mail
            .inbox_filtered(
                &project,
                "ThreadRecv",
                &MailInboxFilter {
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("unfiltered inbox");
        assert_eq!(all.as_array().unwrap().len(), 3);

        let t1 = mail
            .inbox_filtered(
                &project,
                "ThreadRecv",
                &MailInboxFilter {
                    include_bodies: false,
                    limit: 100,
                    thread_id: Some("debate-1".into()),
                    ..Default::default()
                },
            )
            .expect("thread-filtered inbox");
        let t1_subjects: Vec<&str> = t1
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["subject"].as_str())
            .collect();
        assert_eq!(t1_subjects.len(), 2, "only debate-1 rows: {t1_subjects:?}");
        assert!(
            t1_subjects.iter().all(|s| *s == "t1-a" || *s == "t1-b"),
            "all rows belong to debate-1: {t1_subjects:?}"
        );

        let t2 = mail
            .inbox_filtered(
                &project,
                "ThreadRecv",
                &MailInboxFilter {
                    include_bodies: false,
                    limit: 100,
                    thread_id: Some("debate-2".into()),
                    ..Default::default()
                },
            )
            .expect("thread-filtered inbox 2");
        assert_eq!(t2.as_array().unwrap().len(), 1);

        let missing = mail
            .inbox_filtered(
                &project,
                "ThreadRecv",
                &MailInboxFilter {
                    include_bodies: false,
                    limit: 100,
                    thread_id: Some("no-such-thread".into()),
                    ..Default::default()
                },
            )
            .expect("empty thread inbox");
        assert_eq!(missing.as_array().unwrap(), &[] as &[serde_json::Value]);
    }

    /// bd-zqdf: the two live contracts behind a thread-scoped inbox read.
    ///
    /// 1. The store window is newest-N. `fetch_inbox` orders rows ASC and
    ///    applies `limit` to the tail, so a limited read shows what just
    ///    arrived. Pinned here so a regression surfaces as a failed test
    ///    instead of a collab timeout nobody can explain.
    /// 2. A thread-scoped read is scoped BEFORE the limit applies, so it
    ///    returns that thread's newest rows even when other threads hold more
    ///    rows than the limit. This is what makes `mail watch` independent of
    ///    the store window.
    ///
    /// The send order is interleaved and ENDS on the other thread on purpose.
    /// If the newest rows overall were the watched thread's own rows, a scoped
    /// and an un-scoped read would return the same thing and this test could no
    /// longer tell scoping apart from the store window — it would pass for
    /// either reason. Ending on `busy-*` keeps the two answers distinct.
    #[test]
    fn a_thread_scoped_read_survives_a_limit_smaller_than_the_inbox() {
        // Reads the shared mail root — serialize under the crate-wide env lock;
        // sibling env tests swap TORON_*_DATA_DIR.
        let _g = crate::config::lock_env();
        // Unique per run: a pid-only slug re-runs accumulate into the same
        // project and the exact-count assertions below would drift.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("thread-limit-{}-{stamp}", std::process::id());
        mail.ensure_project(&project).expect("project");
        mail.register_fixture(&project, "LimitSender", "opencode", "test", "seed")
            .expect("sender registered");
        mail.register_fixture(&project, "LimitRecv", "opencode", "test", "seed")
            .expect("recipient registered");
        mail.register_fixture(&project, &mail.name, "opencode", "test", "seed")
            .expect("facade identity registered");
        let recv = vec!["LimitRecv".to_string()];

        // Interleaved, and the last two sends are the OTHER thread. Distinct
        // subjects: `collapse_duplicates` folds identical (sender, subject,
        // body) rows.
        for (subject, thread) in [
            ("reply-a", "target-thread"),
            ("busy-a", "busy-thread"),
            ("reply-b", "target-thread"),
            ("busy-b", "busy-thread"),
            ("reply-c", "target-thread"),
            ("busy-c", "busy-thread"),
            ("busy-d", "busy-thread"),
        ] {
            mail.send(
                &project,
                &recv,
                subject,
                "body",
                "normal",
                false,
                Some(thread),
            )
            .expect("send");
        }

        let subjects_of = |v: Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .filter_map(|r| r["subject"].as_str().map(str::to_owned))
                .collect()
        };

        // Contract 1: the un-scoped window is the newest rows of the WHOLE
        // inbox, which here are the last two sends — and they belong to the
        // other thread.
        let unscoped = mail
            .inbox_filtered(
                &project,
                "LimitRecv",
                &MailInboxFilter {
                    include_bodies: false,
                    limit: 2,
                    ..Default::default()
                },
            )
            .expect("unscoped inbox");
        assert_eq!(
            subjects_of(unscoped),
            vec!["busy-c", "busy-d"],
            "the store limit is a NEWEST-N window, not an oldest-N head"
        );

        // Contract 2: the same limit, scoped, is the newest rows OF THE THREAD
        // — a different answer, which is what proves the filter ran first.
        let scoped = mail
            .inbox_filtered(
                &project,
                "LimitRecv",
                &MailInboxFilter {
                    include_bodies: false,
                    limit: 2,
                    thread_id: Some("target-thread".into()),
                    ..Default::default()
                },
            )
            .expect("thread-scoped inbox");
        assert_eq!(
            subjects_of(scoped),
            vec!["reply-b", "reply-c"],
            "a thread-scoped read returns that thread's newest rows, so the \
             watch does not depend on the store window"
        );
    }

    /// c3b.3: `ack_overdue_only` has no toron projection (rows carry no
    /// ack_ts) — the request fails with an honest error naming the filter,
    /// never a silent unfiltered inbox.
    #[test]
    fn inbox_filtered_ack_overdue_is_a_real_read() {
        // bd-0rzv: reads the shared mail root — serialize under the env lock.
        let _g = crate::config::lock_env();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("c3b3-ackoverdue-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        mail.register_fixture(&project, "OverdueSender", "opencode", "test", "seed")
            .expect("sender registered");
        mail.register_fixture(&project, "OverdueRecv", "opencode", "test", "seed")
            .expect("recipient registered");
        mail.register_fixture(&project, &mail.name, "opencode", "test", "seed")
            .expect("facade identity registered");
        mail.send(
            &project,
            &["OverdueRecv".to_string()],
            "ack me",
            "please ack",
            "high",
            true,
            Some("overdue-1"),
        )
        .expect("send ack-required");

        let unfiltered = mail
            .inbox_filtered(
                &project,
                "OverdueRecv",
                &MailInboxFilter {
                    include_bodies: false,
                    limit: 50,
                    ..Default::default()
                },
            )
            .expect("unfiltered inbox");
        assert_eq!(
            unfiltered.as_array().unwrap().len(),
            1,
            "the ack-required mail is in the plain inbox"
        );
        // The row is unacked BUT younger than the 30-min SLA: the filter is a
        // real predicate, not an unfiltered passthrough (the c3b.3 rule —
        // never a silent no-op).
        let filtered = mail
            .inbox_filtered(
                &project,
                "OverdueRecv",
                &MailInboxFilter {
                    ack_overdue_only: true,
                    include_bodies: false,
                    limit: 50,
                    ..Default::default()
                },
            )
            .expect("ack-overdue inbox");
        assert!(
            filtered.as_array().unwrap().is_empty(),
            "a fresh ack starts inside the SLA window: {filtered}"
        );
    }

    /// bd-mail-ack-sla-tracking subtask 4 (wired): the dedicated ack-overdue
    /// read is per-recipient and real. `sla_secs = 0` is the escalation gate's
    /// form ("any unacked ack-required mail"); the agent's own ack clears it.
    #[test]
    fn ack_overdue_reads_real_state_and_clears_on_ack() {
        let _g = crate::config::lock_env();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("acksla-read-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        mail.register_fixture(&project, "SlaSender", "opencode", "test", "seed")
            .expect("sender registered");
        mail.register_fixture(&project, "SlaRecv", "opencode", "test", "seed")
            .expect("recipient registered");
        mail.register_fixture(&project, &mail.name, "opencode", "test", "seed")
            .expect("facade identity registered");
        let receipt = mail
            .send(
                &project,
                &["SlaRecv".to_string()],
                "[flywheel] you are blocked",
                "resolve or report",
                "high",
                true,
                Some("health-SlaRecv"),
            )
            .expect("send ack-required");
        let message_id = receipt
            .get("deliveries")
            .and_then(|d| d.get(0))
            .and_then(|d| d.get("payload"))
            .and_then(|p| p.get("event_id"))
            .and_then(Value::as_str)
            .expect("session id")
            .to_string();

        let gate = AckOverdueFilter {
            sla_secs: Some(0),
            thread_id: Some("health-SlaRecv".into()),
        };
        // Unacked ping → the gate reports exactly it, with its age + the SLA
        // that produced the row (a 0-second gate must say so).
        let rows = mail
            .ack_overdue(&project, "SlaRecv", &gate)
            .expect("ack-overdue read");
        let rows = rows.as_array().expect("array");
        assert_eq!(rows.len(), 1, "one unacked ping: {rows:?}");
        assert_eq!(rows[0]["sla_secs"].as_u64(), Some(0));
        assert!(rows[0]["age_secs"].is_u64(), "age is reported");
        assert_eq!(rows[0]["thread_id"].as_str(), Some("health-SlaRecv"));

        // The recipient acks → the read is empty (the handling fact).
        mail.ack(&project, "SlaRecv", &message_id)
            .expect("ack the ping");
        let after = mail
            .ack_overdue(&project, "SlaRecv", &gate)
            .expect("ack-overdue read after ack");
        assert!(
            after.as_array().unwrap().is_empty(),
            "ack clears the row: {after}"
        );

        // A sibling read narrowed to another thread sees nothing.
        let other_thread = mail
            .ack_overdue(
                &project,
                "SlaRecv",
                &AckOverdueFilter {
                    sla_secs: Some(0),
                    thread_id: Some("other-thread".into()),
                },
            )
            .expect("other-thread read");
        assert_eq!(
            other_thread.as_array().unwrap(),
            &[] as &[serde_json::Value]
        );
    }

    /// The door must never answer "nothing overdue" for an identity it knows
    /// nothing about: an unregistered agent has no receipts, and an empty set
    /// from THAT is indistinguishable from a real all-clear (the exact trap
    /// the escalation gate would fall into). Loud error instead.
    #[test]
    fn ack_overdue_unregistered_agent_is_loud() {
        let _g = crate::config::lock_env();
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("acksla-unknown-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        let err = mail
            .ack_overdue(&project, "GhostAgent", &AckOverdueFilter::default())
            .expect_err("an unknown identity must not read as all-clear");
        let msg = err.to_string();
        assert!(
            msg.contains("GhostAgent") && msg.contains("not registered"),
            "error names the agent + the reason: {msg}"
        );
    }

    /// c3b.3: a malformed `since_ts` is an error, never a silently-ignored
    /// filter.
    #[test]
    fn inbox_filtered_bad_since_ts_is_an_error() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("c3b3-badsince-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        let err = mail
            .inbox_filtered(
                &project,
                "Nobody",
                &MailInboxFilter {
                    since_ts: Some("not-a-timestamp".into()),
                    include_bodies: false,
                    limit: 10,
                    ..Default::default()
                },
            )
            .expect_err("bad since_ts must fail loudly");
        assert!(
            err.to_string().contains("RFC3339"),
            "names the parse failure: {err}"
        );
    }

    #[test]
    fn inbox_filtered_since_applied_once_by_toron() {
        // bd-55nx.12 RED: the InProcess arm must not re-filter since_ts —
        // toron's fetch_inbox already applies it (strict-newer). Two
        // RFC3339 parsers can diverge at the boundary.
        // (needle split so this self-scan does not match its own source)
        let needle = ["created_ts_us >", " s"].concat();
        let src = include_str!("mail_shim.rs");
        assert!(
            !src.contains(&needle),
            "RED (55nx.12): duplicate since_ts retain still in inbox_filtered"
        );
    }

    #[test]
    fn inbox_filtered_since_boundary_equal_ts_excluded() {
        // bd-0rzv: reads the shared mail root — serialize under the
        // crate-wide env lock; sibling env tests swap TORON_*_DATA_DIR.
        let _g = crate::config::lock_env();
        // bd-55nx.12: a row stamped exactly at the bound is excluded once
        // (toron uses strict-newer), never twice-wrong.
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("since-bound-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        mail.register_fixture(&project, "BoundSender", "opencode", "test", "seed")
            .expect("sender registered");
        mail.register_fixture(&project, "BoundRecv", "opencode", "test", "seed")
            .expect("recipient registered");
        mail.register_fixture(&project, &mail.name, "opencode", "test", "seed")
            .expect("facade identity registered");
        mail.send(
            &project,
            &["BoundRecv".to_string()],
            "bound-note",
            "body",
            "normal",
            false,
            None,
        )
        .expect("send ok");
        let all = mail
            .inbox_filtered(
                &project,
                "BoundRecv",
                &MailInboxFilter {
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("unfiltered inbox");
        let rows = all.as_array().expect("array");
        assert_eq!(rows.len(), 1, "one row sent: {rows:?}");
        let us = rows[0]
            .get("created_ts_us")
            .and_then(serde_json::Value::as_i64)
            .expect("row carries created_ts_us micros");
        let bound = chrono::DateTime::from_timestamp_micros(us)
            .expect("valid micros")
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let got = mail
            .inbox_filtered(
                &project,
                "BoundRecv",
                &MailInboxFilter {
                    since_ts: Some(bound),
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("bound-filtered inbox");
        assert_eq!(
            got.as_array().expect("array").len(),
            0,
            "equal-ts row excluded exactly once"
        );
    }

    /// c3b.7: herdr IS the active multiplexer (default) but the daemon is
    /// unreachable → the cleanup must return Err naming the outage, never an
    /// Ok note that reads like success.
    #[test]
    fn cleanup_pane_identities_herdr_down_is_err() {
        let _g = crate::config::lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        // Point the herdr socket at a dead path so connect fails fast.
        unsafe { std::env::set_var("HERDR_SOCKET_PATH", "/tmp/flywheel-c3b7-dead.sock") };
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let err = mail
            .cleanup_pane_identities("c3b7-herdr-down")
            .expect_err("herdr down must be an error, not Ok(note)");
        let msg = err.to_string();
        assert!(
            msg.contains("herdr unreachable"),
            "names the outage + skip: {msg}"
        );
        unsafe { std::env::remove_var("HERDR_SOCKET_PATH") };
    }

    /// c3b.7: the active multiplexer is NOT herdr (superlogical) → the cleanup is
    /// genuinely not applicable → Ok with the not-wired note (a config fact,
    /// not a failure).
    #[test]
    fn cleanup_pane_identities_non_herdr_mux_is_ok_note() {
        let _g = crate::config::lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "superlogical") };
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let v = mail
            .cleanup_pane_identities("c3b7-not-herdr")
            .expect("non-herdr mux is not a failure");
        let note = v.get("note").and_then(Value::as_str).unwrap_or_default();
        assert!(
            note.contains("not herdr"),
            "note explains the config fact: {note}"
        );
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }

    /// bd-k6cw: tokens-first contract — the stamped pin wins over a stale
    /// occupant name (pinned against toron's token-first core), and with
    /// herdr unreachable the branch degrades to the legacy file bridge
    /// (same file-miss shape as before, never Err).
    #[test]
    fn resolve_prefers_pane_tokens_over_files() {
        let pane: toron_core::backend::herdr::PaneInfo =
            serde_json::from_value(serde_json::json!({
                "pane_id": "w1:p1",
                "terminal_id": "term_test",
                "workspace_id": "w1",
                "tab_id": "w1:t1",
                "focused": false,
                "agent_status": "working",
                "revision": 1,
                "tokens": { "toron_pin": "DustySparrow" },
            }))
            .expect("test pane parses");
        let agent: toron_core::backend::herdr::AgentInfo =
            serde_json::from_value(serde_json::json!({
                "agent_status": "working",
                "focused": false,
                "name": "OldOccupant",
                "pane_id": "w1:p1",
                "revision": 1,
                "tab_id": "w1:t1",
                "terminal_id": "term_test",
                "workspace_id": "w1",
            }))
            .expect("test agent parses");
        assert_eq!(
            toron_core::mailbox::identity::resolve_pin_for_pane(&[pane], &[agent], "w1:p1"),
            Some("DustySparrow".to_string()),
            "stamp beats the stale occupant name"
        );
        // Degradation: dead socket → file bridge, file-miss shape, no error.
        let _g = crate::config::lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        unsafe { std::env::set_var("HERDR_SOCKET_PATH", "/tmp/flywheel-k6cw-dead.sock") };
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let v = mail.resolve_pane_identity("k6cw-proj", "w9:p9");
        assert_eq!(v.get("pane_id").and_then(Value::as_str), Some("w9:p9"));
        assert!(
            v.get("error").is_some(),
            "no stamp, no file → file-miss shape"
        );
        unsafe { std::env::remove_var("HERDR_SOCKET_PATH") };
    }
}

#[cfg(test)]
mod cleanup_test_data_dir_tests {
    use super::{cleanup_test_data_dir, dir_holds_live_store};
    use std::path::Path;

    /// bd-tg4n: a test temp dir that is NOT the singleton's root is removed, or
    /// every run leaves a directory behind.
    #[test]
    fn an_unrelated_temp_dir_is_still_removed() {
        let dir = std::env::temp_dir().join(format!("fw-cleanup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create");
        std::fs::write(dir.join("marker"), b"x").expect("write");
        assert!(dir.exists());

        cleanup_test_data_dir(&dir);

        assert!(
            !dir.exists(),
            "a dir that is not the singleton's root must be cleaned up, or every \
             test run leaves one behind"
        );
    }

    /// And it must not remove anything when handed a path that never existed —
    /// a cleanup that can fail must stay quiet about it.
    #[test]
    fn a_missing_dir_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("fw-cleanup-none-{}", uuid::Uuid::new_v4()));
        cleanup_test_data_dir(&dir); // must not panic
    }

    /// bd-gpaa: THE defect. `BUILT_ROOTS` holds the Mailbox DATA dir
    /// (`{root}/projects`) while every caller passes the DURABLE ROOT (`{root}`)
    /// it set `TORON_DATA_DIR` to. The equality guard this replaced never
    /// matched, so the singleton's own store was deleted out from under its
    /// pooled connections and `wake_identity_tests` kept failing 2 runs in 12
    /// with `no such table: bus_events`.
    #[test]
    fn the_durable_root_holding_the_store_is_protected() {
        let root = std::env::temp_dir().join(format!("fw-root-{}", uuid::Uuid::new_v4()));
        let built = root.join("projects");
        assert!(
            dir_holds_live_store(Some(&built), &root),
            "deleting {{root}} deletes {{root}}/projects — the live store. This is \
             the exact shape every wake_identity_tests caller passes."
        );
    }

    /// The data dir itself, so protecting the ancestor does not regress into
    /// protecting only that one spelling.
    #[test]
    fn the_store_dir_itself_is_protected() {
        let built = std::env::temp_dir()
            .join(format!("fw-self-{}", uuid::Uuid::new_v4()))
            .join("projects");
        assert!(
            dir_holds_live_store(Some(&built), &built),
            "the store dir is protected"
        );
    }

    /// Further up: `remove_dir_all(&tmp_root)` in a test that never set
    /// `TORON_DATA_DIR` still takes a pinned per-process root with it.
    #[test]
    fn an_ancestor_of_the_store_is_protected() {
        let built = Path::new("/tmp/fw-anc/store/projects");
        assert!(
            dir_holds_live_store(Some(built), Path::new("/tmp/fw-anc")),
            "an ancestor holds the store too"
        );
        assert!(
            dir_holds_live_store(Some(built), Path::new("/tmp")),
            "and so does the temp root every one of these lives under"
        );
    }

    /// `Path::starts_with` is component-wise, so a sibling sharing a STRING
    /// prefix is not protected. A text-prefix guard would leak every temp dir
    /// whose sibling name shares a few characters, and would look correct.
    #[test]
    fn a_string_prefix_sibling_is_not_protected() {
        assert!(
            !dir_holds_live_store(
                Some(Path::new("/tmp/fw-bind-stamp-XYZ/projects")),
                Path::new("/tmp/fw-bind-stamp-XY")
            ),
            "fw-bind-stamp-XY does not contain fw-bind-stamp-XYZ"
        );
    }

    /// An unrelated dir, and no mailbox at all: neither is protected, or every
    /// temp dir on the box survives the suite.
    #[test]
    fn an_unrelated_dir_and_a_missing_store_are_unprotected() {
        let built = Path::new("/tmp/fw-some-other/store/projects");
        assert!(!dir_holds_live_store(
            Some(built),
            Path::new("/tmp/fw-unrelated")
        ));
        assert!(!dir_holds_live_store(None, Path::new("/tmp/fw-unrelated")));
    }
}

#[cfg(test)]
mod init_once_tests {
    use super::init_once;
    use std::sync::OnceLock;

    /// bd-tg4n: the defect this exists for. `get_or_init` stores the `Err`
    /// permanently, so one failed build turned mail off for the rest of the
    /// process — the error outliving its own cause. The fixture drives a FRESH
    /// cell, because the process-global `MAILBOX` is long since initialised by
    /// the time any unit test runs and would prove nothing here.
    #[test]
    fn a_failed_build_is_retried_and_a_later_success_sticks() {
        static CELL: OnceLock<u32> = OnceLock::new();
        static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // A fresh cell per test would need a `&'static`; statics in a test fn
        // are per-test-binary, so each test names its own pair.

        let first = init_once(&CELL, &GATE, || Err("no such directory".to_string()));
        assert_eq!(
            first.err().as_deref(),
            Some("no such directory"),
            "a failed build reports its own error"
        );
        assert!(
            CELL.get().is_none(),
            "a failure must not be memoised, or it outlives its cause"
        );

        let second = init_once(&CELL, &GATE, || Ok(7));
        assert_eq!(*second.expect("a later build succeeds"), 7);
        assert!(
            CELL.get().is_some(),
            "a success is memoised — the whole point of the cell"
        );
    }

    /// The other half: once a success lands, the builder does not run again.
    /// Without this, "memoise success" would be indistinguishable from "retry
    /// forever", and the shared mailbox would be rebuilt on every op.
    #[test]
    fn a_successful_build_runs_exactly_once() {
        static CELL: OnceLock<u32> = OnceLock::new();
        static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let calls = std::sync::atomic::AtomicUsize::new(0);

        for _ in 0..3 {
            let seen = init_once(&CELL, &GATE, || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(1)
            });
            assert_eq!(*seen.expect("memoised"), 1);
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the builder must not run again after a success"
        );
    }
}

#[cfg(test)]
mod inbox_json_rows_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stable_contract_full_row() {
        let row = json!({
            "id": "a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "thread_id": "t-1",
            "sender": "TealDune",
            "subject": "s",
            "importance": "urgent",
            "ack_required": true,
            "read_ts_us": 1_700_000_000_000_000i64,
            "created_ts_us": 1_699_999_000_000_000i64,
            "duplicate_of": null,
            "body_md": "body",
            "extra_upstream_key": "dropped"
        });
        let out = inbox_json_rows(&json!([row]));
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let mut keys: Vec<&str> = arr[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "ack_required",
                "body_md",
                "created_ts_us",
                "duplicate_of",
                "id",
                "importance",
                "read_ts_us",
                "sender",
                "subject",
                "thread_id"
            ],
            "exactly the 10 contract keys, no extras"
        );
        assert_eq!(arr[0]["thread_id"], "t-1");
        assert_eq!(arr[0]["read_ts_us"], 1_700_000_000_000_000i64);
        assert_eq!(arr[0]["duplicate_of"], serde_json::Value::Null);
        assert!(
            arr[0].get("extra_upstream_key").is_none(),
            "unknown keys dropped"
        );
    }

    #[test]
    fn stable_contract_coercions_and_missing_fields() {
        let out = inbox_json_rows(&json!([{ "id": 42, "subject": "legacy", "ack_required": 1 }]));
        let r = &out[0];
        assert_eq!(r["id"], "42", "legacy numeric id stringified");
        assert_eq!(r["ack_required"], json!(true), "AM 0/1 coerced");
        assert_eq!(r["thread_id"], serde_json::Value::Null);
        assert_eq!(r["body_md"], serde_json::Value::Null);
        assert_eq!(r["created_ts_us"], serde_json::Value::Null);
    }

    #[test]
    fn non_array_input_yields_empty_array() {
        assert_eq!(inbox_json_rows(&json!({"workflows": []})), json!([]));
    }

    #[test]
    fn inbox_json_unwraps_daemon_messages() {
        let out = inbox_json_rows(&json!({
            "messages": [{ "id": "m-1", "subject": "landed", "body_md": "hi" }]
        }));
        let arr = out.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "m-1");
        assert_eq!(arr[0]["subject"], "landed");
        assert_eq!(arr[0]["body_md"], "hi");
    }
}

/// bd-locks-list-daemon-xetz: map the daemon `list_file_reservations`
/// envelope onto the in-process board shape (`project`/`count`/
/// `reservations[{id,path,holder,exclusive,reason,expires_ts_us}]`) so
/// `mail locks list` is byte-shape-identical at both ends of the seam.
fn remotes_to_board(project_key: &str, v: Value) -> Value {
    let rows = v
        .get("reservations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let count = rows.len();
    Value::Object(
        json!({
            "project": project_key,
            "count": count,
            "reservations": rows,
        })
        .as_object()
        .expect("literal object")
        .clone(),
    )
}

#[cfg(test)]
mod locks_list_tests {
    use super::*;

    /// Remote envelope -> board shape mapping (holder/path filters are
    /// applied daemon-side; the wrapper only reshapes).
    #[test]
    fn locks_list_remote_envelope_maps_to_board() {
        let envelope = json!({
            "project_key": "beads",
            "count": 1,
            "reservations": [
                {"id": "abc", "path": "src/**", "holder": "BlueLake",
                 "exclusive": true, "reason": "br-123",
                 "expires_ts_us": 1787500000i64}
            ]
        });
        let board = remotes_to_board("beads", envelope);
        assert_eq!(board["project"], "beads");
        assert_eq!(board["count"], 1);
        assert_eq!(board["reservations"][0]["holder"], "BlueLake");
        assert_eq!(board["reservations"][0]["path"], "src/**");
        // degenerate daemon reply: empty rows still a well-formed board
        let empty = remotes_to_board("x", json!({}));
        assert_eq!(empty["count"], 0);
        assert_eq!(
            empty["reservations"].as_array().expect("rows"),
            &[] as &[serde_json::Value]
        );
    }
}
/// Map the daemon `list_agents` envelope (`{"agents": [...]}`) onto the
/// in-process bare-array shape so pin draw + roster read byte-identical at
/// both ends of the seam. Bare-array passthrough for forward-compat;
/// anything else is a loud error (the caller warns + draws without a
/// collision check — never a silent empty roster).
fn remotes_to_agents(v: Value) -> anyhow::Result<Value> {
    if v.is_array() {
        return Ok(v);
    }
    match v.get("agents").and_then(Value::as_array) {
        Some(arr) => Ok(Value::Array(arr.clone())),
        None => anyhow::bail!(
            "list_agents remote shape mismatch: expected [..] or {{\"agents\": [..]}}, got {v}"
        ),
    }
}

/// bare-array shape, same precedent as `remotes_to_board`).
#[cfg(test)]
mod agents_list_tests {
    use super::*;

    #[test]
    fn list_agents_remote_envelope_unwraps_to_array() {
        let envelope = json!({"agents": [{"name": "BlueLake", "npub": "abc"}]});
        let arr = remotes_to_agents(envelope).expect("envelope unwraps");
        assert_eq!(arr.as_array().expect("array").len(), 1);
        // bare-array passthrough (in-process shape / newer daemon)
        let bare = json!([{"name": "BlueLake"}]);
        assert!(remotes_to_agents(bare).expect("passthrough").is_array());
        // degenerate: loud error, never silent empty
        assert!(remotes_to_agents(json!({})).is_err());
        assert!(remotes_to_agents(json!({"agents": "nope"})).is_err());
    }

    /// bd-hdec regression: on one shared TORON_DATA_DIR, flywheel's register
    /// writes the key at the CANONICAL `{root}/projects/{slug}/agents/{name}/`
    /// slot, and a whois through the SAME mailbox resolves the pin — the
    /// documented session-start flow no longer mints an unresolvable identity
    /// (nor a second npub on fallback bootstrap).
    #[test]
    fn hdec_register_then_whois_resolve_on_shared_root() {
        let _g = crate::config::lock_env();
        let root = std::env::temp_dir().join(format!(
            "fw-hdec-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        unsafe { std::env::set_var("TORON_DATA_DIR", &root) };
        let (data_dir, _) = mailbox_roots();
        assert_eq!(
            data_dir,
            root.join("projects"),
            "mailbox data dir carries the projects segment"
        );
        let mail = Mail::from_config(None)
            .expect("in-process mail ok")
            .expect("Some (embedded backend)");
        let v = mail
            .register_agent(
                "hdec-proj",
                "QuietAnvil",
                "test-harness",
                "m",
                "bd-hdec test",
            )
            .expect("register");
        let key_path = v
            .get("key_path")
            .and_then(|k| k.as_str())
            .unwrap_or("")
            .to_string();
        assert!(
            key_path.contains("projects/hdec-proj/agents/QuietAnvil"),
            "key lands in the canonical layout: {key_path}"
        );
        let who = mail.whois("hdec-proj", "QuietAnvil").expect("whois");
        assert_eq!(
            who.get("name").and_then(|n| n.as_str()),
            Some("QuietAnvil"),
            "register+whois resolve against one shared root: {who}"
        );
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&root);
    }
}
