# ADR-0002 — Pluggable Multiplexer + Mail Backend Abstraction (env-selected, trait-based)

| Field | Value |
|-------|-------|
| **Status** | Accepted (2026-08-13; implemented; captain audit flipped Proposed → Accepted) |
| **Date** | 2026-08-05 |
| **Decision Maker** | Captain (bangedorrunt) |
| **Relates to** | ADR-0001 (collaboration spine — its beads target these traits), toron ADR-0003 (the future embedded core), superlogical.com (future multiplexer) |
| **Precedes** | ADR-0001 Phase 1–3 implementation (the collaboration beads implement against these traits, not concretes) |
| **Governs** | `src/backend/` (new), `src/herdr.rs`→`HerdrBackend`, `src/mail.rs`→`AgentMailMcpBackend`, `src/config.rs`, `Cargo.toml`, `.env` |
| **Verification** | `docs/decisions/_scripts/verify-0002.sh` (see §10) |

---

## 1. TL;DR

flywheel today hard-depends on two concrete backends: **herdr** (`src/herdr.rs`, socket protocol 19) for terminal/agent lifecycle, and **agent-mail MCP** (`src/mail.rs`, HTTP JSON-RPC) for coordination. The captain has two named future replacements:

- **superlogical** (superlogical.com — Mitchell Hashimoto's "multiplexer for all work": durable sessions, web/native clients; **not yet released**) → a herdr-alternative multiplexer.
- **toron** (`~/workspace/toron`, ADR-0003) → a Nostr-native mailbox with **100% behavioral parity to `mcp_agent_mail_rust`**, **explicitly designed to be embedded in flywheel in-process**, and which itself defines a `Backend` trait (herdr default, tmux hold) that **flywheel's `herdr.rs` is the port source for**.

**Decision:** introduce two traits in flywheel — `MultiplexerBackend` (mirrors toron's planned `Backend`: `spawn/prompt/wait/read/send_keys/events`) and `MailBackend` (the operations flywheel needs, not the raw 38 tools). Wrap the current concrete clients as `HerdrBackend` and `AgentMailMcpBackend` (zero behavior change). Select the active backend **via env / `.env` / config** (`FLYWHEEL_BACKEND=herdr|superlogical|tmux`, `FLYWHEEL_MAIL=agentmail|toron`), with Cargo **feature flags** gating the not-yet-existing backends so the default build stays lean. Stub `SuperlogicalBackend` (unreleased) and `ToronMailBackend` (toron-core not shipped) behind features as `Unsupported`-returning placeholders so the env names are reserved + documented.

This is the **stepping-stone**: flywheel is pluggable today, and when toron-core ships, flywheel deletes its `herdr.rs`/`mail.rs` and embeds toron-core (per toron ADR-0003 §"flywheel changes") — the traits converge to toron's, and the swap is a config flip + feature toggle, not a rewrite. ADR-0001's 23 collaboration-spine beads then target the **traits**, so they survive the backend migration unchanged.

---

## 2. Background & Motivation

### 2.1 Current coupling (the thing we're breaking)

```
src/main.rs ──uses──▶ src/herdr.rs::Client  (concrete: unix-socket JSON-RPC to herdr)
            ──uses──▶ src/mail.rs::Mail     (concrete: HTTP JSON-RPC to agent-mail MCP)
src/durable.rs ──same concretes──▶ (the loop calls herdr::Client + Mail directly)
```

Every call site (`cmd_spawn`, `cmd_loop`, `poll_swarm`, `send_prompt`, `notify_blocked`, the planned event reactor) names the concrete types. Swapping herdr→superlogical or agent-mail→toron means editing every call site. That is exactly the coupling ADR-0001's 23 beads would deepen (they add ~14 new mail wrappers + an event reactor all naming `Mail`/`herdr::Client`).

### 2.2 The two future backends

**superlogical** (superlogical.com): a terminal multiplexer from Mitchell Hashimoto's team (Ghostty/HashiCorp alumni). Vision: "the multiplexer for all work" — durable sessions spanning interactive/automatic/production work, web + native macOS/iOS clients, built-in sharing. **Status: pre-release (beta signup).** Role-wise it is a herdr alternative: terminal/workspace/pane lifecycle + agent control, richer (web/native/sharing). It becomes a `MultiplexerBackend` impl when its SDK/protocol ships.

**toron** (`~/workspace/toron`, ADR-0003): a Nostr-native mailbox with **100% parity to `mcp_agent_mail_rust`** (38 tools, 25 resources, storage layout, invariants, pre-commit guard, robot CLI, metrics), rebuilt on Nostr (Schnorr identity, NIP-17 E2E mail, real-time bus push, opt-in federation). toron ADR-0003 §"flywheel changes" is explicit and load-bearing:

> - **Delete** `flywheel/src/herdr.rs` + `flywheel/src/mail.rs`.
> - **Add deps** `toron-core` + `toron-mcp`; `main.rs` builds a tokio runtime that starts `toron_mcp::serve()`, constructs `toron_core::Mailbox` in-process, and runs the Resonate loop via direct calls.
> - **Single source — flywheel uses it via toron-core.** (the `Backend` trait + `HerdrBackend` live in `toron-core::backend`; flywheel's copy is deleted.)

And toron's `Backend` trait surface (ADR-0003 §architecture layer 5): `spawn / prompt / wait / read / send_keys / events`; `HerdrBackend` (ported from `flywheel/src/herdr.rs`, protocol 19); `TmuxBackend` returns `Unsupported`. `TORON_BACKEND` env + `--backend` flag + config precedence (flags > env > project `.toron.toml` > user > system) already specified.

### 2.3 Why now, and why this shape

ADR-0001's beads are about to deepen the herdr/mail coupling. Doing the abstraction **first** (this ADR) means:
- ADR-0001 beads target traits → survive the toron/superlogical migration unchanged.
- The migration is a config flip + Cargo feature, not a multi-file rewrite.
- flywheel's trait signatures **mirror toron's planned `Backend` trait exactly**, so the convergence to "toron-core owns the trait, flywheel re-exports" is mechanical (or toron adopts flywheel's signatures, since flywheel's `herdr.rs` is the named port source).

The captain's directive — "configurable over `.env` is preferred" — sets the selection mechanism: env-first, `.env` file supported, config.toml fallback.

---

## 3. Decision

### 3.1 Two traits, env-selected, feature-gated

1. **`MultiplexerBackend` trait** (in new `src/backend/mod.rs`): the terminal/workspace/pane/agent-lifecycle surface. Method set mirrors toron ADR-0003's `Backend` exactly: `workspace_list/create/close`, `pane_split/list/close/run/wait_output`, `agent_start/prompt/wait/read/send_keys/rename/list`, `subscribe` (event stream), plus the lifecycle types (`AgentStatus`, `AgentInfo`, `PaneInfo`, `WorkspaceInfo`, `HerdrEvent`/`PushEvent`). Implementations: `HerdrBackend` (default, wraps current `src/herdr.rs`), `SuperlogicalBackend` (future, stub), `TmuxBackend` (legacy, stub).
2. **`MailBackend` trait** (in `src/backend/mod.rs`): the coordination surface — the operations flywheel needs (the 14+ wrappers from ADR-0001 Phase 3: `ensure_project`, `register_agent`, `list_agents`, `send`, `reply`, `inbox`, `ack`, `mark_read`, `reserve`, `release`, `renew`, `conflicts`, `slot_acquire/release`, `search`, `summarize_thread`, `request_contact/respond`, `health`). **Not** the raw 38 MCP tools — flywheel's abstraction is operation-shaped. Implementations: `AgentMailMcpBackend` (default, wraps current `src/mail.rs` HTTP client), `ToronMailBackend` (future, in-process toron-core).
3. **Env selection** (precedence: CLI flag > env var > `.env` file > `config.toml` > compile-time default):
   - `FLYWHEEL_BACKEND=herdr|superlogical|tmux` (default `herdr`)
   - `FLYWHEEL_MAIL=toron|agentmail` (default `toron` since R1 — 2026-08-08)
   - `.env` loaded via `dotenvy` (project-local; gitignored).
4. **Cargo feature flags** gate optional backends so the default build needs only herdr + toron + agent-mail:
   - `backend-herdr` (default), `backend-superlogical` (off, future), `backend-tmux` (off, legacy)
   - `mail-agentmail` (default), `mail-toron` (default since R1 — toron is the default *selection*, agentmail stays compiled in as opt-in)
   - A `dyn`-dispatch factory returns `Box<dyn MultiplexerBackend>` / `Box<dyn MailBackend>`; only the selected feature's impl is compiled in (dead-code elimination + faster builds).
5. **Stub the not-yet-existing backends** (`SuperlogicalBackend`, `ToronMailBackend`) behind their features, returning `Unsupported`/`NotImplemented` with an actionable message ("superlogical SDK not yet released; set FLYWHEEL_BACKEND=herdr"). This reserves the env names + documents the future, without depending on unreleased code.

### 3.2 The trait surfaces (signatures mirror toron ADR-0003)

```rust
// src/backend/mod.rs  (governed-by: ADR-0002)

#[async_trait]  // or sync where herdr's socket is sync — see §6 notes
pub trait MultiplexerBackend: Send + Sync {
    // workspace / pane / agent lifecycle — mirrors toron Backend (ADR-0003 layer 5)
    fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>>;
    fn workspace_create(&self, label: Option<&str>) -> Result<WorkspaceInfo>;
    fn workspace_close(&self, workspace_id: &str) -> Result<()>;
    fn pane_split(&self, parent: &str, dir: SplitDir, cwd: Option<&str>) -> Result<PaneInfo>;
    fn pane_close(&self, pane_id: &str) -> Result<()>;
    fn pane_run(&self, pane_id: &str, cmd: &str) -> Result<()>;
    fn pane_wait_output(&self, pane_id: &str, match_: &str, timeout_ms: u64) -> Result<PaneReadResult>;
    fn agent_start(&self, name: &str, kind: &str, pane_id: &str, args: &[String]) -> Result<AgentInfo>;
    fn agent_prompt(&self, target: &str, text: &str, wait: Option<WaitOpts>) -> Result<PromptResult>;
    fn agent_wait(&self, target: &str, until: &[AgentStatus], timeout_ms: u64) -> Result<AgentInfo>;
    fn agent_read(&self, target: &str, source: ReadSource, lines: u32) -> Result<PaneReadResult>;
    fn agent_send_keys(&self, target: &str, keys: &[String]) -> Result<()>;
    fn agent_rename(&self, target: &str, name: Option<&str>) -> Result<()>;
    fn agent_list(&self) -> Result<Vec<AgentInfo>>;
    fn subscribe(&self, filters: &[Subscription]) -> Result<Box<dyn EventStream>>;
    fn name(&self) -> &'static str;  // "herdr" | "superlogical" | "tmux"
}

pub trait EventStream: Send {
    fn read_event(&mut self) -> Result<Option<PushEvent>>;  // mirrors herdr::EventStream
}

#[async_trait]
pub trait MailBackend: Send + Sync {
    fn name(&self) -> &'static str;  // "agentmail" | "toron"
    fn health(&mut self) -> Result<JsonValue>;
    fn ensure_project(&mut self, project_key: &str) -> Result<JsonValue>;
    fn register_agent(&mut self, project_key: &str, name: &str, program: &str, model: &str, task: &str) -> Result<JsonValue>;
    fn list_agents(&mut self, project_key: &str) -> Result<JsonValue>;
    fn send(&mut self, project_key: &str, to: &[String], subject: &str, body: &str, importance: &str, ack: bool, thread_id: Option<&str>) -> Result<JsonValue>;
    fn reply(&mut self, project_key: &str, message_id: u64, body: &str, to: Option<&[String]>) -> Result<JsonValue>;
    fn inbox(&mut self, project_key: &str, agent: &str, filter: InboxFilter) -> Result<JsonValue>;
    fn ack(&mut self, project_key: &str, agent: &str, message_id: u64) -> Result<JsonValue>;
    fn mark_read(&mut self, project_key: &str, agent: &str, message_id: u64) -> Result<JsonValue>;
    fn reserve(&mut self, project_key: &str, agent: &str, paths: &[String], ttl: u64, exclusive: bool, reason: &str) -> Result<JsonValue>;
    fn release(&mut self, project_key: &str, agent: &str) -> Result<JsonValue>;
    fn renew(&mut self, project_key: &str, agent: &str, extend: u64, paths: Option<&[String]>) -> Result<JsonValue>;
    fn conflicts(&mut self, project_key: &str, agent: &str, paths: &[String]) -> Result<JsonValue>;
    fn slot_acquire(&mut self, project_key: &str, agent: &str, slot: &str, exclusive: bool, ttl: u64) -> Result<JsonValue>;
    fn slot_release(&mut self, project_key: &str, agent: &str, slot: &str) -> Result<JsonValue>;
    fn search(&mut self, project_key: &str, query: &str, limit: u32, filter: SearchFilter) -> Result<JsonValue>;
    fn summarize_thread(&mut self, project_key: &str, thread_id: &str, llm: bool) -> Result<JsonValue>;
    fn request_contact(&mut self, project_key: &str, from: &str, to: &str, to_project: Option<&str>, reason: &str) -> Result<JsonValue>;
    fn respond_contact(&mut self, project_key: &str, to: &str, from: &str, accept: bool) -> Result<JsonValue>;
    // ... the rest of the 14+ operations
}
```

The types (`AgentStatus`, `AgentInfo`, `PaneInfo`, `WorkspaceInfo`, `PushEvent`, `Subscription`, `PaneReadResult`, `ReadSource`, `SplitDir`, `WaitOpts`, `PromptResult`) move from `src/herdr.rs` into `src/backend/types.rs` (or stay in herdr.rs and get re-exported — see migration step R0.2). **Signatures match toron ADR-0003 so the future merge is mechanical.**

### 3.3 Selection + factory

```rust
// src/backend/factory.rs
pub fn multiplexer_backend(cfg: &Config) -> Result<Box<dyn MultiplexerBackend>> {
    match cfg.backend.as_str() {  // resolved: flag > env > .env > config.toml > "herdr"
        #[cfg(feature = "backend-herdr")]      "herdr"      => Ok(Box::new(HerdrBackend::from_config(cfg)?)),
        #[cfg(feature = "backend-superlogical")] "superlogical" => Ok(Box::new(SuperlogicalBackend::stub()?)),
        #[cfg(feature = "backend-tmux")]       "tmux"       => Ok(Box::new(TmuxBackend::stub()?)),
        other => bail!("FLYWHEEL_BACKEND={other} not compiled in (enable its cargo feature)"),
    }
}
// mirror for mail_backend()
```

---

## 4. Migration path (stepping-stone to toron embedding)

| Phase | When | What | Config state |
|---|---|---|---|
| **R0** (this ADR, now) | toron v0.1, superlogical pre-release | Introduce traits; wrap current clients as `HerdrBackend`/`AgentMailMcpBackend`; env+feature selection; migrate call sites to `Box<dyn Trait>`. **Zero behavior change.** Stub `Superlogical`/`Toron`/`Tmux`. | `FLYWHEEL_BACKEND=herdr` (default), `FLYWHEEL_MAIL=agentmail` (default) |
| **R1** (done, 2026-08-08) | toron-core ships (ADR-0003 v1) | Real `ToronMailBackend` (embeds `toron-core`); parity-conformance passed (bd-am5 live harness: AM 8765 + toron 18080); default flipped to `toron`. `src/mail.rs` deleted (bd-o8j.2/.3 — toron-core owns it per ADR-0003). | `FLYWHEEL_MAIL=toron` (default); `[mail.toron]` url/token for the daemon, absent → embedded in-process |
| **R1.5** | toron-core `Backend` trait lands | Adopt toron-core's `Backend` trait (re-export or signature-merge); `HerdrBackend` moves into toron-core; **delete** `src/herdr.rs` (single herdr client, per ADR-0003 verification gate). | trait source = toron-core |
| **R2** | superlogical releases | Add real `SuperlogicalBackend` (SDK/protocol TBD). | `FLYWHEEL_BACKEND=superlogical` |

R0 is **purely additive indirection** — the collaboration spine (ADR-0001) implements on top of it. R1/R1.5/R2 are config flips + feature toggles + (eventually) dead-code deletion, not rewrites.

---

## 5. Relationship to ADR-0001 (collaboration spine)

ADR-0001's 23 beads currently name `src/herdr.rs` and `src/mail.rs` concretely. After ADR-0002's R0 lands, they target the traits:

- Phase 1 (protocol-v19-bump, schema-event-typing) → edit `HerdrBackend` (the wrapped herdr client), not the raw `Client`.
- Phase 2 (event-subscribe-reactor, event-dispatch) → the loop holds `Box<dyn MultiplexerBackend>`, subscribes via the trait.
- Phase 3 (mail-register-on-spawn, reply, ack, …) → add methods to `MailBackend` trait + `AgentMailMcpBackend` impl; the `ToronMailBackend` impl is a R1 fill-in.
- Phase 5 recipes → call traits, so a debate loop works identically on herdr+agentmail today and toron+superlogical tomorrow.

**Dependency edges added:** the Phase-1/Phase-3 entry beads depend on the R0 impl beads (`herdr-backend-impl`, `agentmail-backend-impl`); the event-loop beads depend on `call-site-migration`. This makes R0 a hard prerequisite of the collaboration-spine implementation, which is correct — you don't want to deepen concrete coupling the week before you abstract it.

---

## 6. Considered alternatives

| Alternative | Verdict |
|---|---|
| **Wait for toron-core, then do one big migration.** | Rejected. ADR-0001's 23 beads would land meanwhile, all naming concrete `Mail`/`Client` — a 23-bead retrofit is more expensive than a 9-bead abstraction now. R0 is cheap insurance. |
| **Define traits that don't mirror toron's.** | Rejected. toron ADR-0003 names flywheel's `herdr.rs` as the port source for `HerdrBackend`; if flywheel's trait diverges, the R1.5 merge is a rewrite, not a move. Mirror exactly. |
| **Put the `Backend` trait in a shared crate now (not flywheel-local).** | Premature. toron-core doesn't exist as a crate yet (v0.1 is a single crate, hub/adapter shape). Define in flywheel-local `src/backend/`; when toron-core ships its `Backend`, move/merge per R1.5. |
| **Static dispatch (generics) instead of `Box<dyn>`.** | Rejected for the loop (runtime selection from env/config requires dynamic dispatch). generics fine for inner helpers, but the top-level factory returns `Box<dyn>`. One vtable per call is negligible vs socket/HTTP latency. |
| **No stubs for unreleased backends — error at compile time.** | Rejected. Reserving the env names + documenting the future (even as `Unsupported` stubs) is what makes the config contract stable. A user who sets `FLYWHEEL_BACKEND=superlogical` today gets a clear "not yet released" message, not a silent fallback to herdr. |
| **`async-trait` everywhere.** | Partial. herdr's socket client is currently sync (one-request-per-connection blocking). Keep the multiplexer trait methods that map to herdr sync; the event-stream + toron path can be async. Use `async-trait` only where an impl needs it; the trait itself can have sync methods with async impls wrapping them via `spawn_blocking` (toron path). Revisit at R1. |

---

## 7. Consequences

### Positive
- **Backend swap = config flip + feature toggle**, not a rewrite. ADR-0001 beads survive the toron/superlogical migration.
- **Trait signatures mirror toron** → R1.5 merge is mechanical.
- **Feature flags** keep optional backends opt-in (toron + agentmail are both default since R1 — the default *selection* is toron; superlogical/tmux stay lean until released).
- **`.env`** gives project-local backend selection without editing config.toml or rebuilds.
- **Honest stubs** document the future and give clear errors, not silent fallbacks.

### Negative
- **Indirection cost**: one `dyn` dispatch per call. Negligible vs socket/HTTP, but real. Measured in R0.8 parity test (must show <1% overhead).
- **Two sources of types** during R0→R1.5: flywheel's `src/backend/types.rs` and (later) toron-core's. Mitigated by mirror-exact signatures; resolved at R1.5 deletion.
- **More moving parts** to test: factory, feature combos, env precedence. R0.8 + a matrix CI job cover it.
- **`async`/sync friction** at the trait boundary if toron is async and herdr is sync (see §6 last row).

### Neutral
- `src/herdr.rs` and `src/mail.rs` shrink to "the `HerdrBackend`/`AgentMailMcpBackend` impl + the raw socket/HTTP client" during R0, then get deleted at R1/R1.5.
- AGENTS.md + README gain a "Backends" section + `.env.example`.

---

## 8. Implementation blueprint — Phase 0 (R0) beads

Nine beads (see `.beads/`, dependency DAG in §9):

1. **backend-traits-define** — define `MultiplexerBackend` + `MailBackend` + types in `src/backend/{mod,types,factory}.rs`; mirror toron ADR-0003 signatures.
2. **herdr-backend-impl** — wrap current `src/herdr.rs::Client` as `HerdrBackend: MultiplexerBackend`; move/re-export types; no behavior change.
3. **agentmail-backend-impl** — wrap current `src/mail.rs::Mail` as `AgentMailMcpBackend: MailBackend`; no behavior change.
4. **backend-env-config** — `.env` loader (`dotenvy`) + env-var resolution + config.toml field + precedence (flag>env>.env>config>default); `Config::backend`/`Config::mail_backend` fields.
5. **cargo-feature-flags** — `backend-{herdr,superlogical,tmux}` + `mail-{agentmail,toron}` features in `Cargo.toml`; default = herdr+toron+agentmail (R0: herdr+agentmail); factory `#[cfg(feature=...)]` gates.
6. **call-site-migration** — `main.rs`/`durable.rs` use `Box<dyn MultiplexerBackend>` + `Box<dyn MailBackend>` from the factory, not concrete `Client`/`Mail`. The loop holds trait objects.
7. **toron-superlogical-stubs** — `SuperlogicalBackend`, `TmuxBackend`, `ToronMailBackend` stubs behind features, returning `Unsupported` with actionable messages; reserve env names.
8. **backend-parity-test** — differential test: trait-routed calls produce byte-identical results to direct calls (no behavior regression from indirection); feature-combo matrix CI job.
9. **docs-backend-abstraction** — AGENTS.md/README "Backends" section + `.env.example` + cross-links to ADR-0001/toron-ADR-0003.

---

## 9. Dependency DAG (Phase 0 + re-anchored ADR-0001 entry beads)

```
Phase 0 (R0):
  backend-traits-define          (no dep)                [p0]
  herdr-backend-impl             → backend-traits-define [p0]
  agentmail-backend-impl         → backend-traits-define [p0]
  backend-env-config             → backend-traits-define [p0]
  cargo-feature-flags            → backend-traits-define [p1]
  call-site-migration            → herdr-backend-impl, agentmail-backend-impl, backend-env-config [p0]
  toron-superlogical-stubs       → backend-traits-define, cargo-feature-flags [p2]
  backend-parity-test            → call-site-migration   [p1]
  docs-backend-abstraction       → call-site-migration   [p2]

Re-anchored ADR-0001 entry beads (now depend on R0):
  protocol-v19-bump              → herdr-backend-impl    (edits HerdrBackend)
  schema-event-typing            → herdr-backend-impl    (edits HerdrBackend events)
  event-subscribe-reactor        → call-site-migration   (loop uses Box<dyn MultiplexerBackend>)
  mail-register-on-spawn         → agentmail-backend-impl (edits AgentMailMcpBackend)
```

(The rest of ADR-0001's beads inherit transitively — e.g. mail-reply-thread depends on mail-register-on-spawn, which now depends on agentmail-backend-impl.)

---

## 10. Verification (executable)

```bash
# docs/decisions/_scripts/verify-0002.sh
#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pass=0; fail=0
ok(){ echo "  [OK]   $1"; }; bad(){ echo "  [FAIL] $1"; fail=$((fail+1)); }

echo "== ADR-0002 verification =="
[[ -f src/backend/mod.rs ]] && ok "src/backend/mod.rs exists" || bad "src/backend/mod.rs missing"
grep -rq 'trait MultiplexerBackend' src/backend/ 2>/dev/null && ok "MultiplexerBackend trait defined" || bad "MultiplexerBackend trait missing"
grep -rq 'trait MailBackend' src/backend/ 2>/dev/null && ok "MailBackend trait defined" || bad "MailBackend trait missing"
grep -rq 'FLYWHEEL_BACKEND' src/ 2>/dev/null && ok "FLYWHEEL_BACKEND env wired" || bad "FLYWHEEL_BACKEND not wired"
grep -rq 'FLYWHEEL_MAIL' src/ 2>/dev/null && ok "FLYWHEEL_MAIL env wired" || bad "FLYWHEEL_MAIL not wired"
grep -q 'backend-herdr' Cargo.toml 2>/dev/null && ok "cargo feature backend-herdr present" || bad "backend-herdr feature missing"
grep -q 'backend-superlogical' Cargo.toml 2>/dev/null && ok "cargo feature backend-superlogical present (stub)" || bad "backend-superlogical feature missing"
grep -rq 'Box<dyn MultiplexerBackend>' src/main.rs src/durable.rs 2>/dev/null && ok "call sites use trait objects" || bad "call sites still concrete"
[[ -f .env.example ]] && ok ".env.example exists" || bad ".env.example missing"

echo
if [ "$fail" -eq 0 ]; then echo "RESULT: ADR-0002 PASSED"; exit 0; else echo "RESULT: ADR-0002 FAILED ($fail)"; exit 1; fi
```

---

## 11. Open questions

1. **superlogical protocol surface.** Unknown until release. The `MultiplexerBackend` trait is designed around herdr's capabilities (workspace/pane/agent + event stream); superlogical may expose richer primitives (durable sessions, web/native). R2 may require trait extension — design the trait to be **open to extension** (new methods default to `Unsupported`), not sealed.
2. **sync vs async at the trait boundary.** herdr's socket client is sync; toron is async (tokio). Decision deferred to R0.2/R0.3 — likely the trait is sync (matching herdr today) and the future `ToronMailBackend` wraps async toron-core calls via `tokio::runtime::Handle::block_on` or the Resonate leaf's runtime. Revisit if latency/throughput suffers.
3. **`.env` security.** `.env` may carry tokens (agent-mail bearer). `.gitignore` it; ship `.env.example` with placeholders only. Document never to commit real `.env`.
4. **Feature-flag combinatorics.** 3 multiplexer × 2 mail = 6 combos; not all meaningful (e.g. `backend-tmux` + `mail-toron`). CI matrix tests the meaningful subset (herdr+agentmail default; herdr+toron at R1; superlogical+* stubbed). Document invalid combos as factory errors.

---

## 12. References

- **toron ADR-0003** (`~/workspace/toron/docs/decisions/0003-nostr-mailbox-fabric.md`): the future embedded core; defines the `Backend` trait surface, names flywheel's `herdr.rs` as the port source, specifies `TORON_BACKEND` env + config precedence, lists the explicit "flywheel changes" (delete herdr.rs/mail.rs, embed toron-core).
- **superlogical.com** (fetched 2026-08-05): Mitchell Hashimoto team; "multiplexer for all work"; durable sessions; web/native clients; pre-release.
- **ADR-0001** (this repo, `docs/decisions/0001-...`): the collaboration spine whose beads target these traits.
- **Code anchors**: `src/herdr.rs` (Client → HerdrBackend), `src/mail.rs` (Mail → AgentMailMcpBackend), `src/main.rs`/`src/durable.rs` (call sites), `src/config.rs` (env/config resolution), `Cargo.toml` (features).

---

## 13. Changelog

| Date | Change |
|------|--------|
| 2026-08-05 | Initial draft (Proposed). Captures the captain's directive to make herdr/agent-mail swappable for superlogical/toron, env-configurable, ahead of ADR-0001 implementation. |
| 2026-08-05 | **Amendment (review-fix F6, bd-review-fix-epic-m0b.6):** R0 call-site migration scoped precisely. The durable-loop leaf surface is FULLY migrated (durable.rs never constructs a concrete client; leaves shell to the binary which builds via the factory): `__poll`, `__poll_events`, `__send`, `__mail-blocked`, `__diagnose`, `__kind`, `__read`, `__respawn`, `__xproj-inbox`. The trait gained `agent_prompt_coded` (landing semantics: status XOR code, so `__send` classifies identically to the concrete client) + `MailBackend::agent_name`. **Explicit deferral:** interactive CLI commands (`cmd_spawn`/`send`/`watch`/`view`/… + the `mail` CLI surface) stay on the concrete `Client`/`Mail` because the trait surface is operation-shaped for the LOOP, not the full CLI API (filter structs, explicit-param shapes, `start_agent_with_retry` retry loops). This is a deliberate, documented boundary — not an accidental gap. verify-0002.sh now checks real behavior: factory returns trait objects, no `Client::connect` in durable.rs, `agent_prompt_coded` wired in `__send`, main.rs concrete sites bounded at 30. |
