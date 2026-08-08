# ADR-0003 — Reintroduce a Real agent-mail Backend (per-backend config, XDG, parity)

> **Partially superseded 2026-08-09 (bd-adr7-mail-adapt-se2.3):** the
> flywheel-local `MailBackend` trait this ADR's "first-class backend" describes
> is DELETED. Both backends now implement the unified
> `toron_core::mailbox::surface::MailSurface` trait (toron ADR-0007 D2/R1):
> `AgentMailBackend` is an adapter (u64↔String ids only at the MCP edge), and
> `ToronMailBackend` implements the same async trait. Config split, per-backend
> routing, and the parity harness survive unchanged. See toron ADR-0007
> Verification (se2.3) + `docs/parity/known-divergences.md §9`.

| Field | Value |
|-------|-------|
| **Status** | Accepted (via 3-agent opencode debate, 2026-08-06) |
| **Date** | 2026-08-06 |
| **Decision Maker** | Captain (bangedorrunt) |
| **Relates to** | ADR-0002 (pluggable mail backend), toron ADR-0003 (Nostr mailbox), bd-o8j.2/.3 |
| **Precedes** | the 6 rebuild beads (`bd-am1`..`bd-am6`) |
| **Governs** | `src/mail_shim.rs`, `src/mcp_http.rs`, `src/backend/{factory.rs,agentmail.rs,toron.rs}`, `src/config.rs`, `src/main.rs` (`setup_flywheel_backend`), `Cargo.toml` |
| **Verification** | `cargo build` + `clippy -D warnings` + `fmt` clean; bd-am5 JSON-shape parity harness green against live AM(8765)+toron(18080) |

---

## 1. TL;DR

`bd-o8j.2` deleted flywheel's real `src/mail.rs` (the mcp_agent_mail MCP client) and replaced it with a toron-only `Mail` facade. As a result the `"agentmail"` backend label in `factory.rs` is now a **misnomer**: `AgentMailMcpBackend` (`src/backend/agentmail.rs`) is a thin wrapper over the **toron** `Mail` facade, not the agent-mail server. Worse, both the `"agentmail"` factory path (`factory.rs:85`) and the toron facade read the **same** `[agent_mail] url` field — so `FLYWHEEL_MAIL=agentmail` today silently dials the toron daemon when `[agent_mail] url` points at toron. This is a genuine latent misrouting bug (debate BUG1).

**Decision:** reintroduce a **real** `mcp_agent_mail` backend as a first-class `MailBackend`, keep toron as a peer, and fix the shared-config misrouting by giving each backend its own config section. Convergence of the config path onto real XDG (`~/.config/flywheel/`) like toron. Verified during the debate: one parameterized `McpClient` (a `ServerKind{AgentMail,Toron}` label) can drive **both** servers — the strict toron envelope returns 200 on the agent-mail server's `/mcp`.

---

## 2. Background & Motivation

### 2.1 What broke

- `bd-o8j.2` (`3f55c94`) deleted `src/mail.rs` (real agent-mail client, ~30 methods, verified against the live agent-mail server).
- Replaced with `src/mail_shim.rs` carrying `Mail` (`BackendKind::InProcess | Remote(McpClient)`), **both toron**.
- `factory.rs` "agentmail" branch → `Mail::from_config(cfg.agent_mail)` → **toron**, not agent-mail.
- `AgentMailMcpBackend` docstring itself admits it "wraps `crate::mail_shim::Mail` (the in-process toron-core mailbox facade)".

### 2.2 The live evidence (debate)

- agent-mail server: `127.0.0.1:8765`, v0.3.24, 21 projects, 479 messages, exposes 38 MCP tools.
- toron server: `127.0.0.1:18080`, exposes the same tool surface (it is the AM-Rust port).
- **Wire-compat verified:** POSTing the strict toron envelope (protocol `2026-07-28`, `_meta` in `params`, `Mcp-Method`/`Mcp-Name` headers) at AM's `/mcp` with the bearer token → **200 + correct `health_check`**. AM also tolerates the minimal envelope (string id, no headers). So a single transport client works for both; the label is what differs.

---

## 3. Decision (V1–V6, from the debate)

### V1 — One parameterized transport; no second McpClient
Give `McpClient` (in `src/mcp_http.rs`) a `ServerKind { AgentMail, Toron }` label that (a) names error strings truthfully (`"agent-mail <tool>"` vs `"toron <tool>"` — fixes debate BUG3), and (b) optionally relaxes to the minimal envelope for AM (AM accepts both; strict envelope is fine as a single code path). Per-tool argument mapping lives in the backend impls, not the transport — that is where AM and toron actually diverge.

### V2 — Two truthful backend names; rename the misnomer
- `FLYWHEEL_MAIL=agentmail` → a **real** AM backend (new `src/backend/agentmail.rs`, implementing `MailBackend` over an AM-targeting `McpClient`).
- `FLYWHEEL_MAIL=toron` → the existing toron facade (`Mail::from_config`), whose in-process-vs-remote decision is made by `[mail.toron] url` presence (same logic `BackendKind` already has).
- Rename `AgentMailMcpBackend` → `ToronMailBackend` in a new `src/backend/toron.rs`; `name()` returns `"agentmail"` / `"toron"` truthfully. `compiled_backends()` + `flywheel backend list` stay consistent.

### V3 — Per-backend config; fix the shared-url misroute (debate BUG1)
Split `[agent_mail]`:
- `[agent_mail]` keeps the shared identity/name fields only (`name`, `token` if shared).
- `[mail.agentmail]` — `url`, `token` for the AM server.
- `[mail.toron]` — `url`, `token` for the toron daemon; absent → embedded in-process toron.
The factory routes the *selected* backend to its own section. `FLYWHEEL_AGENTMAIL_URL` env may keep its name but must feed `[mail.agentmail]` only (not toron). This is the highest-severity fix; it makes `FLYWHEEL_MAIL=agentmail` actually reach the agent-mail server.

### V4 — Lazy client + honest error (debate BUG2)
The AM backend must NOT eagerly dial on construction. `health()`/ops return an actionable `BackendError` ("agent-mail server not reachable at {url}; start `am` or set FLYWHEEL_MAIL=toron") when the server is down. Honors ADR-0002 §6 (no silent fallback) without regressing the zero-config default build.

### V5 — XDG config convergence (debate V3)
`config.rs` already *claims* `~/.config/flywheel/...` but `dirs::config_dir()` on macOS returns `~/Library/Application Support` — the doc lies, and `setup_flywheel_backend` hardcodes the macOS path. Fix:
1. Real XDG dir (`$XDG_CONFIG_HOME || ~/.config/flywheel`).
2. `Config::load()` dual-reads: XDG first, legacy macOS path as fallback.
3. On first successful load from legacy, **copy** (not atomic-move — rename across volumes fails, crash mid-move loses config) to XDG; keep legacy as backup; write-through thereafter to XDG.
4. `setup_flywheel_backend` → `config::config_file()` so setup-mcp and load can never drift. Precedence: XDG > legacy > default. The live `[agent_mail]` block must survive migration.

### V6 — Restore the full non-operator surface (debate GAP5/V4)
Add to `MailBackend` trait + both impls: `slot_renew`, `summarize_thread_product`, `force_release_file_reservation`, `cleanup_pane_identities`. Exclude `macro_*`, `install_precommit_guard`, `create_agent_identity` (operator-only; flywheel orchestrators don't need them — documented out-of-scope). Toron returns `BackendError::Unsupported` only where it genuinely lacks an op.

---

## 4. TDD parity contract (debate V6)

One integration harness parameterized by backend kind (`agentmail`|`toron`), running the same `MailBackend` method against a live AM server (8765) and live toron daemon (18080), asserting **JSON shape parity** (same top-level keys, same semantic fields) with a documented allowlist for the id-type difference (AM `u64` message_id vs toron hex event id; DB-field differences). The `#[ignore]` parity tests in `src/backend/parity.rs` become runnable. Trait-level unit tests keep Unsupported-honesty (no method defaults to `Ok`).

---

## 5. Consequence / Blast Radius

- **Forbidden:** forking a second raw MCP transport; regressing `health` to REST `/health` (use the MCP `health_check` tool — BUG4); eager-dial on construct; silent fallback.
- **Affected:** `src/mail_shim.rs` (keep as toron facade, unchanged surface), `src/mcp_http.rs` (add `ServerKind`), `src/backend/{factory.rs,agentmail.rs,new toron.rs}`, `src/config.rs`, `src/main.rs` (`setup_flywheel_backend`), `Cargo.toml` (features unchanged default).
- **Config migration:** the existing `[agent_mail]` toron-URL block moves/clones to `[mail.toron]`; the AM url (8765) is new in `[mail.agentmail]`.

---

## 6. Verification

1. `cargo build && cargo clippy -- -D warnings && cargo fmt --check` — clean.
2. `FLYWHEEL_MAIL=agentmail` → `flywheel mail health` hits the AM server (8765).
3. `FLYWHEEL_MAIL=toron` → hits the toron daemon (18080) or embedded in-process.
4. No shared `[agent_mail] url` misroute: config has `[mail.agentmail]` and `[mail.toron]`.
5. bd-am5 parity harness green against both live servers.
6. Config reads from XDG (`~/.config/flywheel/`), legacy macOS path migrated + copied.

// governed-by: ADR-0002 (backend abstraction is a peer of this)
// governed-by: toron ADR-0003 (the embedded toron reference)
