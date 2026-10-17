# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- A condemned wake rings death on a side bell. The brain restarts on the next turn while the watches already accepted stay queued. A full channel can no longer freeze `forwarded` until that backlog drains.

## [0.8.0] — 2026-10-17

### Added
- **Ready-bead appearances ring the doorbell, and the daemon runs the lane (bd-ux-papercuts-t2u5).** The beads poller now detects a bead BECOMING ready — a new open unblocked row, or a blocked bead unblocked in place, both read through the same `blocked_issues_cache` gate `br ready` applies — and emits `bead <id> ready` as a Fact, so an idle supervisor pane is woken instead of polling the board by hand. The first poll only primes the snapshot (a daemon restart must not re-announce the whole board and an appearance never re-fires). The lane is on by default (`beads_poller = false` opts out), and the `doorbell` daemon spawns it too — previously the poller existed only in the embedded flywheel host, so the live launchd daemon could never ring on ledger transitions. Host repo resolution: an absolute project key IS the repo, a slug resolves under `~/workspace`, and a host with no board says so loudly rather than polling a guess.
- **Swarm cockpit reads (bd-swarm-cockpit-pczs).** `status` binds each pane to its mail plane (npub + `last_active_ts_us`) and to the live board (`bead` claim + `reservations`) from one snapshot projection (`snapshot::bind_pins_to_board`; the reservation board is read once and both held-paths and the pane binding consume it). New `progress [<project>] [--json]` groups beads by their `wave-N` label — closed / in_progress / ready / blocked / unknown — and names `dispatching_wave`, the lowest wave holding a ready or in-progress bead (the same floor `wave_gate_held` enforces); an open bead counts `ready` only when `br ready` listed it, and a row whose status is missing/unrecognized reads `unknown`, never done. New `agent tail <pin> [--project <p>] [--json] [--n N]` prints a pin's recent actions (tool calls / commands) from the session herdr already reports on the pane (`agent_session`): a `path` session reads the pi JSONL, an `id` session reads the opencode store, both producing the same `{ts, kind, tool, detail}` rows, so "what is happening" no longer requires finding the raw session file by hand. Pins resolve by the pane's live `toron_pin`, falling back to the spawn registry's pinned mail identity (numbered herdr names still find their pin).
- **`flywheel loop-mode supervise <project> [--work] [--interval N]` (bd-swarm-loop-durable-panic-atlo).** Owns the once-pass cadence operators hand-rolled (`while true; do loop-mode run --once; sleep N; done`). Each pass is a CHILD process, so a crashed pass cannot kill the supervisor; the pass argv deliberately omits `--force` (a live loop must refuse loudly, never be silently taken over). Stops after 3 consecutive failed passes with one line naming the manual pass to run.
- **`loop-mode run --resume` (bd-swarm-loop-durable-panic-atlo).** Replaying a dead run's checkpoints is now opt-in; absent the flag, a pending invocation with no live owner starts FRESH and the zombie is rejected by the stale sweep. `--resume` conflicts with `--force` (fresh takeover).
- **`wake` on `/health` (GS-062 / PR #16).** Operator doorbell `:18980` and flywheel `serve` expose wake_liveness: `workspace` / `hosted`, `pipe.class` Quiet|Live|Deaf|NeverStamped (with class-specific stamps / `silent_for_ms`), split `last_event_ms` / `last_heartbeat_ms` (`.event` / `.hb`, never max-merged), `deaf_ceiling_ms` (60s), worst-of across hosted so one Deaf cannot hide behind another Live, and ADV-5 (`try_send` Watch; stamp `.event` only after Ok; Died via `blocking_send`). Guides: `docs/guides/doorbell.mdx` (`/health.wake`), `docs/guides/troubleshooting.mdx`.

### Removed
- **Inert R-E2 failure metadata (bd-mtyu, bd-xdqx).** `error_kinds` and its declaration-only lint rule are removed from toron-workflow. Runtime retry/escalation remains centralized in `FailureKind::is_retry_candidate()`.

### Fixed
- **Inbox `--json` and the wake channel.** `mail inbox --json` unwraps the daemon `{messages:[...]}` envelope. A condemned wake consumer stops on the first death instead of shedding the firehose. A quiet watch no longer fsyncs the journal or rewrites an unchanged cursor. A prompt still fsyncs before delivery.
- **`spawn --join` names the verb that adds panes** (bd-ux-papercuts-t2u5).** `flywheel spawn <p> --join` against a live fleet printed only "JOINING — not spawning" and attached to it; the operator who wanted MORE panes had to find `flywheel add` by trial. The refusal now names it: "to ADD panes to a live swarm use `flywheel add <project> --pi N`".
- **Wave gating reads br's typed `wave` field, and an in-flight wave is no longer mistaken for an empty one (bd-wave-gate-typed-authority-27et).** Schema-18 shipped the typed field and `br ready` now gates by it natively (a higher wave is held until every lower wave closes), but flywheel still gated on the legacy `wave-N` label only: the loop's `wave_gate_held` / `wave_scope_filter` and the wave-swarm GATE prompt re-derived ordering from a label intersection, and a wave whose ready set was empty because its beads were already in progress was reported as `blocked: matched zero beads` — a false failure over a running wave. `ReadyBead` now carries `wave` (parsed from the typed field), `bead_wave` reads the typed field first with the legacy label as fallback (a stale label can never overrule the tracker), `--wave-label wave-N` scopes by that same rule, and `progress` groups by it too. The wave-swarm GATE writes an `in_flight[]` list and treats an empty ready with in-flight beads as WORK IN FLIGHT — held:false, the completion marker written, no re-dispatch — while an empty ready with nothing in flight stays the loud failure it should be; DISPATCH re-reads `br ready` live (the artifact is a snapshot and was the stale list the mid-wave dispatch died on), and EXECUTE waits out the in-flight closes instead of greening a wave that ran nothing.
- **Compose ensures the declared steer channel and joins the crew; briefing binds the captain pin (bd-compose-plumbing-zdzi).** The wave-swarm token declared `steer_channel` and its stages posted to it, but `workflow compose --run` never created the room or joined the spawned pins (the operator created `elm-effect-steer` and joined five pins by hand), and briefing emitted a compose argv with no `--inputs`, leaving `captain_pin` at its `""` default so escalations targeted nobody. Compose now resolves the token's declared channel BEFORE anything spawns — an unresolvable channel is refused up front, never a strict-render failure with panes already up — and once the crew exists it runs the same ensure+join implementation `spawn --join-channel` uses (extracted into one helper so the two paths cannot diverge again). Briefing resolves the arriving pin (env ladder over `[agent_mail].name`) and binds it as `captain_pin` in the emitted argv's `--inputs`; when it cannot, `captain_pin` is named in `questions` and no dispatchable argv is emitted.
- **The wave-swarm composition sequences its stages on real completion artifacts, and its SIGNOFF now surfaces (bd-compose-stage-sequencing-zgi9).** The compose token dispatched all eight stage prompts within the first minutes: the planner got `compound` before any bead closed, the reviewer had zero closes to review, and the coder improvised from `br ready` instead of the gate artifact. Each gated pair is now separated by an await step — `retry` (240 x 30s, its own per-step budget) over a `verify` of `.flywheel/wave/<label>-<stage>.done`, the marker the predecessor's task writes (`-gate.done` only on GREEN, `-execute.done` after the LAST close, and so on). Writer and waiter name the same path, so a green await means the predecessor finished, not that time passed; exhaustion fails at that step with `wave in flight: <stage> has not written <marker> … extend with a larger --timeout`, instead of one total kill swallowing the cause. `--timeout` (or `FLYWHEEL_COMPOSE_MAX_SECONDS`) remains the run's outer bound. Composed runs also start the approval drainer the workflow-run arm already had: a parked SIGNOFF is announced with its `flywheel workflow approve …` command instead of parking silently, and a resumed run re-issues it until resolved.
- **Claim-watch and resume honor durable claims, not mail prose alone (bd-claim-watch-durable-signals-mixy).** The dispatch guard scanned only `thread-id=<bead>` mail, so the loop re-sent `GOAL Resume` every pass over a bead that was in_progress, assigned, and held by an unexpired toron lease; idle panes burned context standing down from duplicate prompts. The claim watchdog now consults live toron reservations first (`reason == bead id` on the active board): while a lease exists, the Redispatch/Resume arms hold (tracker nudges still fire) and release when the lease lapses — the reserve-before-work lifecycle is the durable "work is taken" record, visible even when a crew's harness cannot be polled. The guard collector also reads the kind-9 steer rooms (`steer-<project>` plus the bead's `steer-<bead>` room), so room claims count as peer signals.
- **The durable loop no longer panics on its own runtime, and a killed invocation is no longer auto-resumed into a serialization storm (bd-swarm-loop-durable-panic-atlo).** The renew-filtered leaf called the shared runtime's `block_on` from the loop's tokio worker — "Cannot start a runtime from within a runtime" once per poll, right before every `reclaimed stranded bead ... (re-dispatching)`. Its mail block now runs off-runtime through `backend::run_mail_off_runtime` (`spawn_blocking`, the same thread→runtime bridge the toron facade uses). Start-up no longer replays a dead owner's checkpoints by default, so the shape-mismatch storm (`invalid length 0, expected struct ReadyBead with 9 elements`) cannot fire; a resumed run that still hits the stale-checkpoint signature degrades to ONE fresh invocation, and a genuinely blind loop aborts naming a single fix command (`flywheel loop-mode doctor --fix`) instead of advising a blind `--force`.
- **Audit rows are appended whole, and the unreadable ones are reported (bd-sfvq, bd-2t65).** `append_audit` opened `O_APPEND` and then wrote the row with `writeln!` on a `serde_json::Value`, whose `Display` emits the document fragment by fragment, so every fragment became its own append and two writers spliced into lines that were neither row (513 unreadable rows in one workspace's audit, live). A row is now serialized to a buffer and appended in one write. `/health` gains `audit_torn_rows` (`{workspace, torn, scanned, tear_after_age_s, newest_row_age_s}`, `null` when clean) and the `audit_rows_torn` degrade, which fires only while a tear is live: a torn line cannot be dated, so it is bounded from below by the last datable row in front of it. History is reported without pinning the daemon red.
- **A CLI child that outgrows the pipe buffer is no longer killed with its output discarded (bd-239z).** `run_cli` read the child's stdout/stderr only after the child exited, so a child writing more than the pipe's capacity (65536 bytes on macOS) blocked in `write`, was killed at the deadline, and had every byte thrown away — audited as `toron spawn/timeout failed` for a fetch that had already succeeded. A cold `mail inbox --limit 50` page is 88925 bytes, which starved a pin's first fetch permanently (702 `inbox_error` rows over 21h, no cursor, self-reinforcing). Both pipes are now drained on reader threads for the child's whole life; past the deadline the readers are detached rather than joined, so the deadline still bounds the wait.
- **The `/health` store-skew probe sees the arm that actually fails (bd-r39p).** It grepped for the row kind `inbox_store_skew`, which only `fetch_inbox` writes, while the live refusal arrived through `scan_ack_sla` as `ack_sla_error` — so the store could refuse every minute with `degraded: []`. The probe now matches the machine marker `toron-error=store_skew` (or the refusal sentence) in any row, plus both classified kinds, and `scan_ack_sla` names its own refusals.
- **The GS-039 wake-save tripwire is scoped to `flush_wake` (bd-fkfi).** It asserted a string `brain.rs` stopped printing in `e0a1871`, so the flywheel bin suite read 2022 passed / 1 failed and the assertion proved nothing. It now checks the four parts of the one message inside the function that must print them, and was proven to discriminate by mutation.
- **OpenCode v2 and current Toron API compatibility (bd-l0y1, bd-76st).** Spawn argv now uses `--model` for OpenCode, and embedded Toron startup passes the current `serve_http` extra-hosts argument.
- **Composed workflow lifecycle honesty (bd-e1yz).** A compose run that exits after publishing its `running` row now terminalizes that same row as failed instead of leaving a permanently live history entry.

## [0.7.0] — 2026-09-19 doorbell vision leftovers

Classifier honesty, HITL bind, and the doorbell wake gaps from live flywheel×toron collab (bd-gpj0).

### Added
- **Briefing questions bind into argv (bd-g3li).** `briefing --json` topology/crew/model answers, plus CLI `--topology` / `--crew` / `--model` on `briefing` and `reclassify`, now rewrite compose argv. TTY `--commit` prompts unanswered questions. `--topology` is an explicit request (sticky-open may still flip).
- **HITL remix on steer / workflow run.** `--role-model ROLE=PROVIDER/MODEL`, `--role-harness ROLE=KIND`, `--crew role:n` bind the TTY remix into spawn (bd-pdpe).
- **`flywheel doorbell roster <project>` (bd-gpj0.2).** Nearby join of herdr panes ∪ registry pins ∪ reservations. Unbound panes stay listed. `--json` for rows.
- **`flywheel doorbell introduce <project> --pin <peer>` (bd-gpj0.3).** One verb: contact request + channel create (creator joins) + subscribe. `--dry-run` prints the plan. Defaults: channel `intro-<pin-lowercase>`, workspace `flywheel-<project>`.
- **Open-mode arrive binds `HERDR_PANE_ID` (bd-gpj0).** The current herdr pane is written into the spawn registry so doorbell can push. Without the env, arrive says pull-only.
- **`flywheel mail agents --disk` (bd-0cqt / bd-yfne).** Lists `key.ncryptsec` dirs not in the registry. Arrive upserts `--as` via `register_agent` before presence.
- **beads-lifecycle `from=` / `existing_bead`.** Babysit one bead without merging lifecycle into wave-swarm.

### Removed
- **Retired slash templates.** `flywheel integration` writes only `*-mode.md` (open/swarm/loop/peer-aware) and deletes leftover `beads-*.md` / `workflow-*.md`. Catalog + `br` own those verbs (bd-z9yx).

### Fixed
- **`inbox_fetch_failing` is a fetch-path outage, not any pin error (bd-gpj0.1 / bd-gpj0.4).** Auto-host skips project `.` / `..` / blank (`cli-flywheel-.-Buffy`). No-key / register-first / unknown-agent audits as `inbox_pin_miss`. Health degrades only when every hosted workspace has a fresh `inbox_error`. Arrive refuses `.` / `..`.
- **PATH mise shims no longer beat `~/.cargo/bin`.** Interactive PATH lists `.../mise/shims` first; those dispatchers are skipped so the real binary wins under a grok/login PATH.
- **Introduce does not treat `does not exist` as success (bd-gpj0).** Idempotent only on `already` / `idempotent`.
- **Classifier honesty.** Decide/briefing no longer invent `wave-swarm` under sticky-open; ready_beads matches `br ready`.

### Changed
- **Close citation is the full `%B` message (bd-qf99).** A trailing `.` or `:` is punctuation, not glue (`bd-0cqt.` cites; `bd-2qu9.1` does not). Lookup already grepped `%B`; the glue table was the reject. Agents verify with `git show -s --format=%B`.

## [0.6.0] — 2026-09-10 (cut from Unreleased)

### Added
- **The ack-SLA hit list is wired: the re-ring ladder registers from the wire, not only from the send path (ADR-0010 D1, bd-w98c residual).** `BrainMsg::SlaFired { posts }` — previously dropped on the floor in both hosts (`RtHost`/`FileHost` folded it to an empty effect list) — is now a real reducer `Msg::SlaFired`: `workspace::tick` emits one captain-relevant `ack-sla-hit` Audit naming the overdue corrs plus a `FetchInbox`, and the brain pipes the fetched rows back as `InboxFetched` (same shape as the sweep live arm). The `InboxRows` fold is the registrar: an unacked ack-overdue row (ack-required + high/urgent — toron's own scan scope) registers/refreshes its corr on the `rering` ladder with the window stamped from the tick clock (bd-bdpx rule), a per-recipient read (`read_ts_us` set) marks the entry acked, and both folds are idempotent under at-least-once redelivery. A corr seen overdue purely on the wire now climbs the ladder on the next `AckSlaTick` beat without any same-process send. The hit list never rings directly — the ladder owns re-rings.
- **The ack-overdue read is real, and the blocked escalation is gated on it (ADR-0001 A6, bd-mail-ack-sla-tracking subtask 4).** `Mail::ack_overdue(project, agent, AckOverdueFilter{sla_secs, thread_id})` returns the messages an agent owes an ack for — `ack_required` + `importance ∈ {high, urgent}` + **no per-recipient receipt** (`message_reads`, migration 0002, written only by `mark_message_read`/`acknowledge_message`) + past the SLA (`sla_secs`, default 1800 = toron's own `check_ack_sla_expired` default; `0` = "any unacked ack-required mail"). It is a READ: it never calls toron's scan, because that primitive *emits* an `ack-sla-expired` wake and the doorbell's 60s beat already owns the emit — a second emitter would double every ring. An unregistered identity fails loudly instead of answering an empty set that looks like "nothing overdue". `MailInboxFilter.ack_overdue_only` (previously an honest `Unsupported` error) now applies the same predicate as a post-filter on both backends. Consumer: the durable blocked-escalation ladder. `notify_blocked` reads the agent's own `health-<name>` lane at SLA 0 through a new hidden `__ack-overdue` door and an ack **resets** the ladder — budget restarts from the ack, ping credit drops, T0 is recorded as handled (so an ack never provokes an immediate re-ping); a read that cannot answer (no index, unregistered agent, remote backend) is NOT an ack, so the time ladder keeps its pre-gate behavior. The gate is also skipped until the first ping is actually out — an empty thread is not an ack, and treating it as one would silence the escalation entirely. ADR-0001's acceptance ("an agent that acks or un-blocks resets the escalation") is now enforced by the read rather than assumed.
- **Declared-pause resurface (ADR-0010 D4/D12).** A drained pane can now declare `paused: <owner>` instead of finishing its VERIFY fence. The drain leaf keeps a paused-since clock (`DrainState::paused`: owner, `since`, window, resurfaced count) and Tick **holds grace** for a declared wait — it is never force-stopped, and `--force-working` stays the only auto-kill. Every `FLYWHEEL_PAUSE_RESURFACE_SECS` (default 3600, the `pending_wait` TTL twin) the wait is re-surfaced to its named owner via `LoopAction::PausedResurface` → a captain-relevant `Audit` row (`kind: pause_resurface`, the AFK digest read) plus the loop log and event journal, then re-armed so the repeats are bounded and observable instead of a per-tick spam. Producer: the durable shell reads each drained pane's tail once per poll (`__read`, degrade-to-empty) and folds the declaration through the reducer — never by writing the model. `drain_prompt` now teaches the declaration form; `write_liveness::parse_paused_declaration` owns the parse and `paused_resurface_due` (previously a tested-but-unwired API) is the one "is this wait due" predicate.
- **Delivery-verdict loop projection in the fleet snapshot (ADR-0010 D10).** `flywheel-snapshot.v1` gains a `loop` section — per-bead `LoopPhase`, the mail-degraded dispatch hold, queue depth, and the event watermark — as a read-time projection of the persisted `ActiveLoop`. `flywheel loop status` renders the same `loop` object. `flywheel helm`'s UNREAD STATUS now names a mail-degraded dispatch hold. This wires `durable::LoopSnapshot` (previously declared but never rendered) and removes the `LoopPhase` suppression.
- **Failure taxonomy: `Cancelled` is now reachable (ADR-0026 Spec 1).** `classify`/`classify_with_stage` recognize an operator/agent cancellation (`cancelled`/`canceled`/`sigint`/`interrupted by user`) as `FailureKind::Cancelled` — never retried, never rerouted. Deliberately narrow needles: a bare `interrupted` substring stays `Unknown`.
- **The hash-aware lock-generation door is the live one.** `ensure_running` now calls `inspect_lock_generation_with_hash` (it holds the exe), so a same-second rebuild with an unchanged binary length supersedes the stale watcher as gap 10 intended; the exe-less `inspect_lock_generation` stays for callers without a binary path.

### Removed
- **Dead/duplicated surfaces after the plane-D cutover.** Deleted the plane-P `doorbell_persist_wake` / `doorbell_persist_wake_with_plane_s` pair — their caller never landed because wake-state persistence moved to plane D (`doorbell-rt::{brain,store}` writes `since_by_pin` + `plane_s_cursor_us`), so the GS-037 P0-C fail-loud contract now asserts on the host's live door, `doorbell_note_observed_wake`. Also deleted `Mail::search_args` (the live search arm builds args through `backend::wire::search_args`), `MailHealth::is_default` (the `Default` derive is the check), the never-read `SendEnvelope::verified_sender` / `DeliveryEnvelope::project` parse fields, and `PinBudget::record()` (a wall-clock wrapper — callers pass `now`).
- **Plane-P `PinBudget` duplicate.** `src/doorbell.rs` re-exports `doorbell_core::PinBudget` now (ADR-0012 Phase 0 one-implementation rule); the copy that lived there carried a never-read `started_unix` field.
- **One mail backend: toron (bd-euav).** `src/backend/agentmail.rs` and the `tmux` multiplexer stub (`src/backend/tmux.rs`) are deleted, with the `mail-agentmail` / `backend-tmux` cargo features and the `[mail.agentmail]` config section. `FLYWHEEL_MAIL` accepts `toron` only; `[mail.toron] url` selects the remote daemon and absent means in-process. The agent-mail env vars were renamed — `FLYWHEEL_AGENTMAIL_URL` / `FLYWHEEL_AGENTMAIL_TOKEN` are now `FLYWHEEL_TORON_URL` / `FLYWHEEL_TORON_TOKEN` — and the shared `[agent_mail]` section survives as the mail identity (`name`) and shared token source.

### Changed
- **Remaining `#[allow(dead_code)]` suppressions are scoped, not blanket.** Parked contract/parity surfaces use `#[cfg_attr(not(test), allow(dead_code))]` with their bead/ADR reason (cron P4 legacy manager + `row_json`/`CronStatus`/`cron_status`, `plan_reclaim_ex`, `write_liveness::paused_resurface_due`, `backend::wire::ensure_project_with_mode_args`, `Mail::set_identity_mode`, `inspect_lock_generation`), `DaemonState::RunningSystemd` is scoped to `not(target_os = "linux")`, and the `PinBudget` re-export is scoped to `any(test, feature = "serve")`. Stale suppressions were dropped where the item was live all along (`LoopPhase`, `crew::plan_crew`, `CoalitionStore::leave`, `Check::skip`, `make_verify_sink`, `MailSearchFilter::{date_start,date_end}`, the three `doorbell-core` pub items). Two tripwires the sweep un-silenced are repaired in the same pass: `durable::tests::{close_path_releases_the_beads_reservations,events_primary_grep_no_herdr_shim_in_loop_path}` slice the "non-test source" at the first inner test attribute, so `plan_reclaim_ex` is gated with `cfg_attr(not(test), ..)`; and the GS-037 P0-C assertion reads the whole `doorbell_note_observed_wake` body up to the next top-level fn instead of a fixed 800-char window.

### Changed

- **GS-052 hybrid `workflow list --brief` (FM-050-STEER → FM-052):** ONE live projection = `name` + `family` + `selection` + `run` + `topology` + `compose_token` (`workflow_list_brief_rows` / `cmd_workflow`). Sticky-open still sees `topology`. Bare `list --json` adds description + decide_trigger + rest; `show` is prose/graph detail (**not** decide_trigger/family/selection/topology). Docs (AGENTS, catalog-review, workflows, cli-reference, robot-capabilities) aligned to live fields — no invent-green GS-048-only brief docs; no dual `--robot-capabilities` `workflows` key. Rejected: Redis/OTP/new kinds/MCP/GS-046 cuts/GS-051 full packet.

### Added
- **GS-050 Reclassify `effect_key` honesty (FM-049-BAR residual) + H1 brief honesty:** `Cmd::Reclassify` carries a durable journal `effect_key` (`reclassify:<sha256>` length-prefixed over goal/reason/explicit/pin — `None` ≠ `Some("")`). Host `expand_reclassify_cmds` insert-or-loads Prepared→InFlight→Settled|Failed under that key (agent_ref `journal_reclassify`, ADR-0032 I8a); retries attach to the same journal primary key. Tests prove journal PK / idempotent resume — not only Cmd field equality. **H1 (historical):** initially kept GS-048 `--brief` as name/trigger/topology via `workflow_list_brief_rows` / `cmd_workflow` (family/selection compact stayed mid-work but omitted topology and was early-return shadowed — weak vs sticky-open); that brief shape is **superseded by GS-052** hybrid (see Changed). Full list retained family/selection/run. Fixed dual `--robot-capabilities` `workflows` key. Docs/guides/AGENTS synced at that tip (`.agents/skills/**` unchanged then).
- **GS-049 Elm deepen (GS-045 P1):** `workflow evaluate` — lint + partition candidates without journal dispatch (evaluate-without-commit). Source-Msg / post-effect retry identity tests (I8a; `effect_key` on Reclassify landed in GS-050). Sensor→Msg vocabulary in `docs/guides/doorbell.mdx` (WatchNdjson / Herdr / AckSla / InboxSweep; Presence via Herdr+roles — not a Sensor GenServer arm).

### Fixed
- **The toron embed doors reject a blank project spelling (bd-toron-embed-blank-project-doors-rmg9).** The blank-project sweep's last surface: `flywheel toron` passed the slug straight to the in-process embed, where `agents --project ""` answered `[]` (a silently authoritative-looking empty roster) and `mail send`/`mail inbox` garbled their actionable errors with the blank slug ("register it in project  first"). `cmd_toron` now validates every project-bearing door (`agents`, `mail send`, `mail inbox`) before the tokio embed runtime boots — same house message, no embed-side changes, `Status`/`StartMcp`/`SetupMcp` untouched. Pinned by a handler-level test that fails loud on all three doors with `Config::default()` (no runtime, no store touch).
- **The blank-project spelling fails loud on every door, not just mail (bd-blank-project-spelling-non-mail-doors-76hi).** The bkr3 mail fix validated the winning spelling only in `mail_project`; the same `Some("")` passed straight through the other families, and `project_dir(cfg, "")` joins the workspace root — so `workflow list/run/compose --project ""` read (or with `--execute`, journaled and pane-spawned against) the workspace root's tier, `cron` doors read the workspace project's crons, `squad`/`reclassify`/`briefing`/`decide` resolved topology against it, `steer --project ""` ran AUTHORITATIVE steer there, and the required-positional doors (`spawn`/`init`/`add`/`rebind`/`recover`, all `doorbell` verbs) defeated their own `dir.exists()` bails because the workspace root always exists. One shared guard (`check_project_spelling` + `project_door`/`project_door_opt`/`ensure_project_opt` next to `mail_project`) now sits at every door: a blank winning spelling fails loud with the actionable message, `None` keeps each door's own default (cron's no-filter and doorbell subscribe's workspace-scope semantics untouched), and there is no silent fallback. `cron::detect_project`'s cwd-inheritance path validates too. Pinned by a helper-contract unit test and a CLI-level parse test (blank `--project` parses; the door errors).
- **An empty `--project` spelling fails loud instead of silently retargeting (bd-empty-project-silently-retargets-workspace-bkr3).** `mail_project` accepted `--project ""`; `project_dir(cfg, "")` joined the workspace root, so the empty spelling aliased every mail door (send/inbox/search/agents/…) onto the **workspace default project** — a send then failed with `unknown recipient` for a project the caller never named (the "one-off" from the SLA proof was this, not a flake), and `mail inbox --project ""` answered exit 0 with the wrong project's inbox. The resolver now rejects a blank **winning** spelling (empty or whitespace, positional or flag) with the same actionable message as a missing project — and never silently falls back to the other spelling. Flag-wins precedence is unchanged; a real flag still overrides a blank positional. Pinned by a resolver unit test and a clap end-to-end parse test (parse succeeds; resolution fails loud).
- **mail search arg order (bd-g5q4):** `mail search` declared an optional `[PROJECT]` positional before the required `<QUERY>` — clap's debug asserts panic on that shape, so the subcommand was unusable in every arg order on debug builds. Required positional now precedes the optional one (both spellings pinned by a parse test). Search is project-scoped FTS, so there is no `--as` (the identity plays no role in the contract).
- **GS-049 sticky_open_airtight:** sticky-held open clears invent workflow/`wave-swarm` argv (fail-closed empty — `--commit` cannot headless-run invent swarm under open). DecisionReady announce is `none: raw` + sticky-open held (never invent `vehicle: wave-swarm` under open). Loop sticky vs swarm rewrites to loop-work only.
- **GS-049 residuals (GS-048):** briefing aligns with sticky `session_topology` after `[reclassify]` announce (no dual topology SoT). Sticky-held argv rewrite covers open×loop|swarm|join|none (commit must not exec suppressed loop under open / skip crew-confirm). Held flag from `sticky_open_topology` (single SoT). `steer` requires `--project` (or cwd inherit) for AUTHORITATIVE MaterialContext. `live_decide_snapshot` / briefing use real assemble `launch_pending` (no invent-green hardcoded 0). Phase-bearing failures carry `stage` (ActionStage: `classify_with_stage`; Blocked→park) on ClassifiedFailure, pane/effect journal outs, and run ledger rows.

### Fixed — the workflow catalog layers the project tier at every door
- `flywheel workflow run <name> --project <p>` resolves built-ins < user catalog < `<p>/.flywheel/workflows/*.yaml` — the catalog contract the skill and ADR-0032 both state, and the one `workflow compose` already honored (W6). Before this, a project-tier workflow failed at `workflow run` with `unknown workflow` while `compose` could resolve it, and `call_workflow` sub-runs resolved user-tier only. Sub-runs resolve against the same layered catalog the entry workflow came from.
- The read doors land the same tiering (`bd-workflow-catalog-read-doors-project-tier-jah1`): `workflow {list,show,lint,crew}` take `--project` and read the layered catalog, so a project-tier workflow is visible, inspectable, crew-previewable, and — the point of the fail-stop gate — LINTED with the project's own workflows. A broken project-tier workflow now fails `workflow lint --project <p>` instead of shipping unlinted; bare (no `--project`) stays user-tier because nothing names the project.
### Added — effect journal on the engine run path (ADR-0032 Spec 5/6, bd-engine-run-path-journaling-fwlx)
- flywheel now injects a host-owned `EffectRunner` (`src/effect_journal.rs`) into every engine it builds for a run that really dispatches (`workflow run --execute`, `workflow compose --run`, `cron run --live`, and the cron daemon). An effectful `dispatch_job` leaf commits prepared intent, claims the attempt under a fencing token, invokes its provider exactly once, and settles — or records `failed`/`unknown` through the engine's one ambiguity rule instead of guessing. Previously the leaf fell through to the compat runner and wrote no journal row at all (the silent bypass Spec 6 forbids).
- Rows land in the same `workflow_effects` tables toron-mcp's `WorkflowHost` writes, keyed by the engine boundary's own derivation, so a retry reuses one key and two runs never collide on a byte-identical payload. A terminal effect replays its stored outcome instead of re-dispatching.
- The journal opens the same toron index the mailbox uses, so it engages on CLI runs that never build a mailbox — without calling `init_mailbox`, whose shared-runtime `block_on` panics inside the cron daemon's async context.
- The dry arms stay unjournaled on purpose: `workflow run` without `--execute` and `cron run` without `--live` use the recording runner, which performs no external effect, so a settled row there would claim work nobody did. They keep the engine's `bypassed-no-effect-runner` marker.
- The mailbox sink is wired for real and journaled the same way. `send_message`/`send_dm` previously fell under `pane_dispatch`'s documented "recorded, never dispatched" carve-out, so journaling them would have settled an effect nobody performed; they now deliver through the in-process toron mailbox under the same intent -> claim -> settle fence, carrying the `toron:workflow-emitted` stamp (a workflow can never re-trigger itself), `corr` equal to the effect key (Spec 5.4), and a non-null `wake` tag. The sender is flywheel's one actor ladder, resolved against the project's live roster and registered best-effort — a workflow dispatch never impersonates an agent pin.
- When this install's mail plane is a remote `[mail.toron] url`, mailbox leaves are not journaled: that wire cannot carry `corr` (the same reason `mail_shim` refuses a corr-preserving resend), so the leaves keep the engine's `bypassed-sink-uncovered` marker with a loud reason instead of a row the daemon's mailbox would never see.
- The Spec 6 flip landed (`bd-engine-run-path-journaling-fwlx`): a journaled constructor is now `EffectJournalMode::Enforced`, so an effectful leaf with no injected runner **refuses** instead of dispatching unjournaled. The dry arms (`run` without `--execute`, `cron run` without `--live`) stay `Recording` — they record instead of perform, so the marker (not a refusal) is the honest outcome there. With no toron index open the engine keeps the loud marker rather than a run that only looks journaled.
- `verify` now journals through the same effect surface (it previously had its own direct-to-`VerifySink` arm that never reached an injected runner). The engine runs the embedding's `VerifySink` first and hands the already-executed result to the runner through `RunContext.provider_result`, so the journal observes an effect that already happened — it never re-runs the command (ADR-0024 Spec 2 keeps the sink the executor), and the leaf runner is deliberately not invoked for a verify action so nothing can double-perform it.
- Fixed: a verify step's `command` is a template (`command: "{{inputs.verify}}"` in the builtin repair loop), and the engine's verify arm skipped the render — so the sink executed the literal `{{inputs.verify}}` instead of the command. Verify now renders through the same path as every other leaf; a live `workflow run --execute` of a templated verify writes the rendered command to the receipt and the journal row.
- Run-lifecycle rows now come from the engine too (`bd-mi9n`, ADR-0032 Spec 5/6 tail): `src/effect_journal.rs` builds BOTH halves of the host-owned journal from one store — the effect runner and a new `RunJournal` the engine feeds its own `Started` → `Running` → `Succeeded`/`Failed` transitions through. A flywheel-driven run therefore has a `workflow_runs` row without any embedding lifecycle call, and the run row and its effect rows agree on `run_id` by construction. Idempotent by design (a durable run replays): create-if-absent, CAS only when the status differs, and a terminal row never moves — so a replayed tail cannot resurrect a finished run. Every journaled constructor passes it (`pane.rs` live arms, both `cron.rs` live engines); the dry arms pass `None` on purpose, since an engine that performs nothing must not mint a row claiming it ran. Best-effort by the effect path's rule: a journal failure is reported loud and never changes the run's outcome.
### Added — bounded recursive workflows and open-mode routing
- `call_workflow` now carries a serialized lineage envelope through Resonate child inputs and refuses recursive chains, nesting beyond 8 workflows, or an exhausted 32-child budget per lineage branch before creating a child promise.
- Open mode can use ordinary workflows and stages in the current pane without changing topology; integration guidance now re-classifies every follow-up/material context change, selects the smallest catalog vehicle, and announces `none: raw` when no vehicle is useful.

### Added — Compound/pstack evaluation evidence and parity tracking
- Added bounded workflow discovery (`workflow list --brief --json`) plus a one-by-one 40-workflow review guide. **Historical (GS-048 → GS-050 H1):** `--brief` initially shipped as name/trigger/topology only; family/selection/run cues lived on bare `workflow list --json` (show carried description/prose, not family/selection); a dead compact brief path that never ran was removed. **Superseded by GS-052:** live `--brief` is now the hybrid contract (name + family + selection + run + topology + compose_token) — see Changed.
- Added the verify-phase `ce-evaluate` stage and wired all three `repair-until-verified` evaluation passes to its fresh-context, read-only JSON contract. Its `done` field is advisory; host `VerifySink` and phase contracts remain authoritative.
- Ship-chain links now persist parent-run `EvaluationEvidence` with bounded redacted snapshots, complete-value SHA-256 digests, evaluator identity, contract evidence, and integrity hashes. `artifacts stitch <run_id>` renders the evidence, and failed persistence fails the gate in block mode or produces `gate-warnings` in warn mode.
- Added deep-dive guides for Compound Engineering and pstack/poteto-mode plus `docs/upstream-parity.toml`, an offline-validated manifest exposed through `--robot-capabilities`.

### Fixed — gate sha binding survives the close ceremony (`derive_head_sha`)
- `derive_head_sha` walked the `--grep` matches newest-first and took the first, but the close ceremony itself writes `.beads/*` and that bookkeeping commit routinely cites the bead id — so the newest match became the bookkeeping commit rather than the revision the gate row was bound to, and the row you had just bound read clean-false (unrepairable by policy once the bead is closed). It now **skips bookkeeping-only commits** (`.beads/**`, `.flywheel/**` — the same two ADR-0006 D3 names) and binds the newest commit that carries the id AND carries work. Fail-closed: an empty path list is NOT bookkeeping (a merge commit prints none without `-m`, an empty commit has none) — skipping on absence would silently retarget the bound revision, which is the bug this guards — and when every candidate is bookkeeping-only the newest one is still returned, because "no commit carries the id" would be a lie that reopens a legal close.
- The working sequence is pinned in `AGENTS.md`: keep the bead id out of the bookkeeping commit message anyway (a skip is a heuristic, a token-free message is a fact).
### Fixed — approval drainer died on its own runtime
- The approval-drainer thread panicked with "Cannot start a runtime from within a runtime" the first time it built the mail facade, killing the thread that resolves parked approvals — so a `workflow run --execute` that parked on `request_approval` could never be approved by mail. The mailbox build now isolates itself when the caller is already driving async tasks, while still being driven by the shared runtime the bus prune tick (bd-741i) lives on.
### Fixed — goal workflow render (bd-pqwx)
- The `goal` meta-workflow no longer dies at render with `unresolved template ref {{inputs.data_shape}}` on a bare objective: `phase-plan`'s `data_shape` input now defaults to empty (every caller — goal, ship, the beads-lifecycle graph — renders clean), and `goal` declares `data_shape` and passes it through to `phase-plan` like the `ship` chain already did, so a caller that knows the data shape can feed it through.
### Added — portable profile codec (ADR-0013 P2-11, bd-8ana)
- `flywheel profile export <name> [--out <file>]` writes the versioned portable doc (`{"codec":1,"name",..."profile"}` JSON); `flywheel profile import <path|-> [--name] [--dry-run]` upserts it into the user `profiles.toml` (importing a built-in name lands as a deliberate local override). Unknown codec versions refuse loud naming the supported one; absent fields take the same defaults as the TOML loader.
### Fixed — run-ledger truth (ADR-0032 Spec 9 tail)
- `runs list`, `--robot-snapshot`, and `cron history` no longer default a run row's status to `done`. A row with no recorded status now reads `unknown`, the same honest reader `workflow history` already used; measured on the live state dir this stops 354 of 2658 rows (13%) from being reported as `completed` with no evidence behind them.
- `runs list` rows carry the failure artifacts the run wrote (`artifacts`, a stable `[]` when none), so a failed row is diagnosable without a second lookup.
- A ship chain's link-gate annotations share the `Workflow` event kind with real runs but are not run outcomes. `runs list` and `--robot-snapshot` label them `kind: "phase-gate"` and carry the gate's own `gate_ok` verdict; `workflow history` labels them too, and `cron history` no longer lists them as cron runs.
- The remaining status-less writers now say what their rows are: the legacy loop monitor writes a self-declared start notice (`kind: "loop-monitor"`), the warn-first plan-artifact gate declares its telemetry (`kind: "plan-warning"`), and the legacy workflow runner records the `completed` outcome it actually observed. Rows written before the `notice` label existed are recognized by the markers they carry, so no historical row is left for the reader to guess at.
### Fixed — swarm coordination hardening (bd-ytr6, bd-8vci, bd-ykqd, bd-0gmw)
- Pane-mode workflow steps that pin a harness no longer strand the run when the host's profile resolution yields no such pane: dispatch falls back to the step's `@role` pane with a loud downgrade warning, and only bails when no role pane exists (bd-ytr6).
- `send --wait` timeouts no longer discard the landed fact: observed rows are emitted with an `observed_status` read-back and an ambiguous may-have-started error; same-second wake stamps retry once past the second boundary instead of false-failing the advance gate (bd-8vci).
- `flywheel deps --ensure` and `flywheel daemon start` ensure a loopback resonate: probe `/health`, start a detached sqlite-backed `resonate serve` when down, refuse loudly for remote urls (bd-ykqd).
- Configured doorbell daemons are ensured at spawn (typed outcome, `FLYWHEEL_NO_DOORBELL_ENSURE=1` opt-out), degradation prints a WARNING with remediation, and preflight carries an advisory doorbell entry (bd-0gmw).
### Added — ADR-0012 Phase 4 behavior flags (all default-off, live-proven)
- `arrival_triple`: presence publish + role auto-bind + join ring + scope-bound roster headers on arrive; flag off is byte-identical.
- `beads_poller`: read-only WAL poll of the br sqlite emitting Fact Msgs (hold expiry, closes); never writes the ledger.
- `cron_doorbell_sink`: cron tick completion journals a Fact wake, digest-visible with zero swarm.
- `doorbell emit` endpoint + tokened service identities (scoped × channeled × rated, revocable) ringing kind-9 through toron.
- `cross_workspace`: explicit Scope::Many widening, `(workspace, corr)` journal envelope, same-host external re-ring driver in doorbell-rt.
- `doorbell inject` console verb (ratcheted, budgeted) and `subscribe`/`subscriptions`/`unsubscribe` store verbs (D8).
- Wake analytics learning artifact + phase4 gate in verify-0012.sh.
- `doorbell install` autostart unit (launchd/systemd, idempotent); `flywheel daemon start` rides the doorbell daemon along.
### Fixed — ADR-0011 vision proofs
- `"N crews"` / `coder:N` parse into `--crew N` (total panes). `"swarm with 2 crews"` classifies wave-swarm, not steer.
- `"join the fleet"` → `arrive`. One-line typo → open spawn-send.
- Arrive starts a doorbell even with no prior swarm workspace; helm assembles after start-or-attach.
- `doorbell status` lists arrive-started dispatchers (empty spawn registry no longer hides them).
- Helm dispatcher token is `absent`/`down`/`up` (lowercase `down` is no longer mapped to absent).
- Helm/snapshot pick the live doorbell workspace, not stale spawn-meta (`adrx28`).
- Doorbell child uses `try_mailbox` so a remote `[mail.toron] url` no longer panics the watcher.
- Helm reservations use the Mail facade (`list_active`), so a remote daemon no longer always degrades.
- Skill/AGENTS/QUICK-REF session door is arrive+briefing.
- Month-old coalition JSON is dropped from JOINING (48h live window). Held paths are a count, not a dump.
- SIGNOFF residual is non-closed lanes (not a `status contains "review"` miss). SIGNOFF assemble uses the live doorbell workspace.

### Added — ADR-0011 session conductor Waves B–D
- `flywheel briefing` emits `flywheel-briefing.v1` (world + proposal + topology/N questions). `--commit`/`--yes` run argv. Joinable worlds propose `arrive`, not a second compose.
- `decide --json` emits `topology`. `--project` fills Snapshot from helm (ready beads, pins, contention).
- `flywheel arrive` + `spawn --join`: JOINING banner, coalition join, FCFS captain (stale eviction + override), doorbell start. Watch child is `toron watch --live`.
- Helm mail/reservations read `toron receipt list` / `reserve conflicts` (degrade only on miss).
- Rot Wave 1 scorer on `LoopMsg::Rot` (Restart→Respawn, Compact→steer). Nudge cap 3 (`interrupt_nudge`; 4th+ mail-only). WaitCorr drains on Sweep as well as WatchNdjson. SIGNOFF auto-pass iff REVIEW residual empty AND OPEN DECISIONS empty.
- Slash `/swarm` `/loop` `/open` start with arrive+briefing.

### Added — ADR-0011 session conductor Wave A (crew budget N)
- Unique-role infer + `pack_crew` into total **N**, cap `crew.max` default **5** (operator-only; `FLYWHEEL_CREW_MAX` / `--crew-max`). Overflow roles alias onto the first kept role.
- `workflow compose --crew N` (also `auto` / `role:n,role:n` / squad). `--allow-crew-expand` is a no-op and does not raise the cap.
- TTY briefing asks N; confirmed budget mutates spawn (no more preview-only edits).
- Loud pack line on every compose/run: `packing <wf> into N panes (cap=…, unique=…)`.

### Added — ADR-0010 delivery semantics, attention triage, decision lifecycle
- One live `WorkspaceModel` (doorbell loop owns it); `poll_mail_digests` deleted as wake producer; doorbell-with-inbox delivery with 90s/3-ring re-ring ladder and `--fire-and-forget` skip.
- Generation-bound wake ack watermark; Busy/Idle/Unknown/Dead verdicts (Unknown never promoted); reservation write-liveness defers respawn; captain turn-end guard; wake classification in tick with AFK digest; OPEN DECISIONS fold with typed captain-hold and divergence reporting; ask-user authority policy.
- `crew control` (interrupt/exit/relaunch) with `FLYWHEEL_OP:` marker; degraded receipts with printed corr-preserving resend; `--resolve-bead` structural done-hold; captain subagent-guard; `flywheel-snapshot.v1` + `helm`; tiered learnings with bounded GATE projection.
- Coordination breakthroughs: edge triage (benign/routine zero-prompt, urgent injects), outer urgency tag behavior, deadline-attached holds with earliest-first digest.
- Dispatch hardening: role-bind failure no longer aborts multi-pane spawn; CLI sends load the project stage tier; message text appends (never dropped); BEAD header on every send; unconsumed `--param` warns; `--fire-and-forget` id by equals-only; `mail inbox --unread` carries the limit.

### Fixed — GS-039 CoS-honest-5 (hold merge)
- **P0-1 Wake (reprobe):** stamp `last_wake` from REAL observations — herdr Status events, poll **status transitions** into `working|idle|done|blocked` (not static already-idle rows every poll), successfully delivered prompts, **and** `send --wait` landed+settled status (covers idle→idle when Status push has no transition). Never from planned injects / `doorbell_persist_wake` invent-green. `doorbell_note_observed_wake` advances only when wall clock moves past prior (no `saturating_add(1)` ratchet). Robot never-wake after meaningful dispatch stays hard-fail.
- **P0-3 Solo dry-run (reprobe):** compose `--parts repair-until-verified` preserves recipe Explicit crew `coder:1`+`reviewer:1` (no Auto-infer balloon to 3+3 over repair/evaluate nodes).
- **P0-1 Wake (prior):** Mail send defaults `--wake tasked`. Poll/sweep observed-wake saves loud-warn (no `let _ =`).
- **P0-2 Open-mode:** operator/`--raw` framed AUTHORITATIVE; open.md.tpl + peer-aware carve-out so create-file tasks are not chatter.
- **P0-3 Solo compose:** `--retire-orphans` defaults **off**. Multi-crew requires `--allow-crew-expand`; robot/CI/`--json`/`FLYWHEEL_ROBOT`/non-interactive **hard-bail** (no `read_line`); interactive TTY y/N ask is humans only. `repair-until-verified` crew is `coder:1` + `reviewer:1`.
- **P0-4 Name-binding:** post-settle rename + fail spawn if any pane missing, empty-named, **or wrong-named** (fail closed under verify).
- **P1:** presence publish retries on unknown-agent race; repair `error_kinds` for R-E2; peer-aware prefers `flywheel mail --as <pin>`.
- **P2:** swarm ready advertises `--stage ce-execute` (p1 ghost gone); opencode-go missing keys fail-closed in provider-funded preflight.
- **UX:** default model remains `opencode-go/muse-spark-1.3-contributor` (@standard); compose multi-crew asks humans before silent N.
- **test-attr:** restored `#[test]` on `spawn_starts_doorbell_dispatcher` + `provider_funded_zero_balance_fails_naming_provider`.


### Added — ADR-0009 compound spine (bd-4el4, 2026-09-06)
- **ce- stage vocabulary** (compound-engineering loop): `ce-brainstorm` (new), `ce-plan` (was decompose-beads/p4), `ce-plan-review` (was plan-review/p5), `ce-execute` (was systematic-execute/p6), `ce-simplify` (new), `ce-review` (was code-review/p2 — + detection conditions, quoted-evidence confidence, independence rule, autofix classes, settled_conflict), `ce-compound` (new — the learning gate). **p-code aliases RETIRED** (clean cutover: `--stage p6` errors naming the fix).
- **Compounded wave-swarm token** — now EIGHT phases: GATE (grounding-reads `docs/learnings/`) → DISPATCH (`--param learnings=` when matched) → EXECUTE (ce-execute: idempotent re-entry + simplify-as-you-go) → OBSERVE (`lesson:` line on every escalation) → **REVIEW** (ce-review audits the diff pre-verify) → VERIFY (**residual gate**: every finding fixed or filed as a bead) → **COMPOUND** (ce-compound, `phase: compound`, `on_error: continue`) → SIGNOFF.
- **Learnings corpus** `docs/learnings/` (per-project, bug/knowledge track, frontmatter, `rg`-able) + `ce-compound`/`ce-sweep` workflows (standalone capture any topology; corpus maintenance cron-able) + `decide` rows ("compound the learning", "sweep the learnings").
- **Loop-mode wiring**: beads-body lifecycle gains the grounding read + the post-verify compound gate (learning ships in the same commit). **Open-mode wiring**: standing orders + one-shot `workflow run ce-compound --execute`.
- Terminal states (`done|clean_skip|blocked|failed`) + `settled:` decision labels + independence doctrine, in stage contracts.
- toron ADR-0022 addendum: `Phase` enum gains `compound` (six-value closed vocabulary, deserialize-gated as before).
- **Agent-native workflow selection (ADR-0009 addon):** every `workflow list --json` row now carries `description` + `run` (exact invocation) + `compose_token` + `decide_trigger` — select from `list` like a skill catalog. `decide` routes five previously-unpickable intents: debate, brainstorm→ce-brainstorm, bead-ready plan→ce-plan, ship the chain→ship, convert to beads→beads (+ trigger phrases). `robot_capabilities` gains the `workflows` discovery object.
- **Compound `init` (bd-bhri):** `flywheel init <project>` now scaffolds the project-scope compound spine — `.flywheel/stages.toml` (project tier template), `.flywheel/standing-orders.md` (fence + compound orders), `docs/learnings/README.md` (corpus convention). Re-runnable, never overwrites; prints the AGENTS.md discoverability pointer instead of editing it.

### Changed — ADR-0009
- Builtins renamed where they ARE CE concepts: `brainstorm`→`ce-brainstorm`, `plan`→`ce-plan`, `plan-review`→`ce-plan-review` (slash short-forms `/plan` `/plan-review` `/brainstorm` keep their names, retargeted). `goal`/`goal-direct`/`beads`/`beads-lifecycle` stage refs updated to ce- names. `stage list` drops the alias column; robot_capabilities `stages` carries the ce- loop.


## [0.4.0] — 2026-09-05 ADR-0007 catalog slots + daemon lifecycle

### Added
- Catalog slots (ADR-0007 D1): cheap/standard/premium = harness+model; profiles alias `@cheap/@standard/@premium` and inherit the slot's harness on read. Compiled-ins: cheap=opencode/big-pickle, standard=pi/muse-spark-1.3-contributor (model_fallback zai/glm-5.3-flash), premium=omp/zai/glm-5.3 (fallback_harness grok). Six compiled-in job profiles (no `pa`), four compiled-in squads (default/plan/review/recon).
- Slot semantics corrected (operator 2026-09-05): the grok HARNESS accepts only its own proprietary model (grok hop = model-opaque, bare id, no caps check); grok MODELS are available on pi/omp/opencode and route normally.
- Full D1 resolve ladder: `resolve_profile_route_full` walks pin → slot → `model_fallback[]` → slot `fallback_harness` → registry fallback, returns the route taken, fails loud naming the rungs. Grok hops are model-opaque (closed-source proprietary model — no caps check, bare-id flag).
- CLI: `slot {list,show,set,remove}`, `model {list,show,add,set,remove}` (`set <id> --slot <s>` selects a slot default), `profile set` (routing fields) + `profile show --json` (resolved route), `squad create`, `catalog` ratatui hub (refuses non-TTY/NO_COLOR naming the CLI), `model remove` N-ids + TTY multi-select.
- `daemon {start,stop,restart,status}` (toron-parity): launchd/systemd/pidfile supervision of `serve`; start/restart verify the daemon answers; stop refuses unmanaged holders without `--force`.
- Agent-native discovery: `--robot-capabilities` carries the catalog + daemon objects; flywheel skill §5 synced.

### Fixed
- Slot ladder wired into pane crews and the captain briefing (previously bailed/rendered empty on slot-aliased profiles); respawn argv verified to route through the same ladder.
- Preflight: herdr/br checks no longer run for cron mode ("needs resonate only"); resonate URL fallbacks `localhost` → `127.0.0.1` (::1-first resolution false-DOWNed healthy servers).

### Changed
- Live `~/.config/flywheel/profiles.toml` migrated off ox-alpha-free/jcode pins (backup: `profiles.toml.bak-adr0007`).

## [0.3.5] — 2026-09-02 inbox cursor + fact budget

### Fixed
- Inbox high-water reads live `created_ts_us` (ToJson), not only legacy `created_ts` RFC3339. In-process sweep was jumping to wall-clock `now`.
- Fact doorbell injects go through `take_allowed` (5/30/3). Watch/Status/Sweep already did; Fact did not.
- Prompt extract is one helper (`prompt_injects`). Publish kind-14 send errors are loud.

## [0.3.4] — 2026-09-02 swarm WorkspaceModel lifetime

### Fixed
- `swarm_loop` keeps one `WorkspaceModel` for the run (same as the doorbell dispatcher). No more `..Default` wipe each poll. DiagnoseFailed ticks that live model.

## [0.3.3] — 2026-09-02 GS-014 leftover finish

### Fixed
- One heal map: `LoopModel.heal_states` (no loop-local HashMap). Diagnose still re-enters as `Msg::DiagnoseFailed`.
- Poll events enter as `Msg::Herdr`, not a world-dump into `pending_events` before clock Tick.
- `captain_fact_inject` queues for `workspace::tick` (no herdr-direct inject).
- HumanDecision live producer: InboxRows / rumor-14 `verdict=` or approve/deny body on an open corr.
- Watch kinds include rumor 14. `handle_workspace_cmds` is exhaustive.

## [0.3.2] — 2026-09-02 GS-014 Tick Law leftovers

### Fixed
- Doorbell CLI fallback watch kinds match in-process: `9,20001,30013,43001,30031,39002` (respawn child no longer dropped presence/profile).
- Swarm fold no longer calls `durable::tick` on unmapped LoopMsg.
- Diagnose I/O failure reverts `awaiting_clear` via `Msg::DiagnoseFailed` (tick), not a post-tick HashMap mutate.
- `WaitCorr` keeps pending corrs and re-peeks; `Publish` kind-14 sends a rumor (audience=human).

### Docs
- ADR-0027 Tick Law sentence: live path is held.

## [0.3.1] — 2026-09-01 workflow/stage seam

### Fixed
- **Wave-dispatch briefs `systematic-execute` (p6), not p1.** p1 is scout `fresh-eyes-explore`; coder execution is p6. Recipe YAML + stage body + slash `/swarm-mode` agree.
- **`decide` argv is runnable.** `loop-work` is `loop-mode run <project> --work`. `compose-parts` always has a `--parts` value (`parts: a,b` extracted, else `<role:stage,...>`). `repair until verified` maps to `compose --parts repair-until-verified --run`.
- **`stage list` shows seed by default.** `--json` accepted on `stage list` and `workflow list`. `workflow show` prints graph node labels (was `steps: []` on graph workflows).
- **Slash pins the primitive.** `/swarm-mode` does not re-run `decide`. `/loop-mode` pins `loop-mode run --work`.
- **`send --stage` help** names any stage / p-code (`stage list`), not only p1–p8.

### Added
- **`error_kinds` on workflow steps/graph nodes** (toron-workflow 1.3.1). R-E2 warns only when `fail_run` has no kinds. `wave-swarm` fail_run steps classify `[timeout, rate_limit, unknown]`.

## [0.3.0] — 2026-08-31 Tick Law + doorbell bind (GS-005 → GS-009, 0.2.0 → 0.3.0)


### ✨ Added (0.3.0 — GS-005 → GS-009, Tick Law)

- **Doorbell Elm tick bound + `doorbell why` (GS-006/007, bd-yw4s, `c96a7ae`)** — `doorbell_tick::tick` is now the live dispatcher's decider for Watch/Status/Died/AckSla (`WatchNdjson`/`HerdrStatus`/`WatchDied`/`AckSlaTick` → `Prompt`/`RestartWatch`/`CheckAckSla`); every tick journaled to `doorbell-journal-<ws>.jsonl`; `flywheel doorbell why <project> <pin>` replays it; `sc_d1` gate updated to tick contract.
- **Loop `--workflow <name>` now `engine.invoke` with VerifySink (bd-i3lw, GS-009)** — `build_queue` no longer bails English; builtins run via `workflow run --execute --mode pane --project <proj>` subprocess with `make_verify_sink` (was `verify sink missing` on `compose --run` before `ecdb86f`).
- **Repair loop honest (bd-s1yy, GS-009)** — `verify-1/2` → `on_error: continue` (was `fail_run`, blocked c2 after c1 failure); description no longer claims early-exit `Unrolled`; final `verify-3` still gates the run.
- **Plane-P sweep via tick (bd-9hco, `4fe50e0`)** — `InboxRows {rows, pin}` through `tick` (pre-parsed `Vec<Value>`, no JSON round-trip); `bell_model` carries `queues`/`statuses`/`index` for the sweep; `watch/status` already via tick, now sweep too; `doorbell why` journals sweep rows.
- **Hold loop tick — Tick Law wave-2+3 (bd-nsb3, `1ab0223`)** — `heal`/`route_event` (queue pops + `dispatch_pending`) + `mail-hold` (Blocked for idles when degraded) + retention + watch-sends Nudge all inside `tick`; `per_bead_phase` + `dispatch_pending` persist loop-local via `model'` and crash-safe via `ActiveLoop` file (was `Default` every poll); `events: Vec<PollEvent>` travels as `Tick.events` and is replayable; dedup for `pane_exited` Diagnose already covered by `self_heal`.
- **Persist `per_bead_phase` + `dispatch_pending` via `ActiveLoop` file (bd-5fbv, `aaa86f5`)** — `ActiveLoop` gains `per_bead_phase: BTreeMap<String,String>` + `dispatch_pending: HashSet<String>` with `#[serde(default)]`; `swarm_loop` loads at start and saves after every `Tick` via `save_active_loop`; `loops.json` v3 additive.
- **Loop graph vehicle typed `VehicleOutcome` (bd-nmk5, `118fb41`→`982094c` audit)** — `AlreadySpawned` is a distinct ok-arm (loud, typed — never silent `Ok(())`); caller keeps the bead vehicle-owned and never falls through to a normal brief (was double-dispatch regression).
- **Watch supervisor never dead (bd-umto)** — `watch_retry_at` gate retries every 16s forever; `heal_tick` is backup, not the only life; `spawn_watch_child` helper centralizes kinds (`9,43001,30031,39002`).
- **ApprovalSink real resolve (bd-1ub5, `21a3f44`)** — drainer scans inbox every ~2s for `approve:<id>`/`reject:<id>` and calls `engine.resolve_approval`; `workflow approve` sends that decision mail (was `bail!` admitting it could not reach the engine).
- **Ingress seam typed (bd-0or2, `ecdb86f`→`982094c` audit)** — `ingress_to_loop_msg` with real `pane.agent_status_changed`/`pane.exited` strings (were invented `agent_status`/`pane_exited` that `apply_events_to_states` ignored); only `Closed` maps to `BrClose`.

### ✨ Added (0.2.0 — 2026-08-29/30 orchestration campaign, ADR-0024/0025/0026)

  node with backoff (bounded by the enclosing `retry`, default 3) before escalating;
  `auth`/`blocked`/`cancelled` abort. R-E1/E2/E3 lint rules fire at compose time.
- **RECIPE compose expansion keeps `graph`** — `--parts repair-until-verified` yields the full
  3-cycle graph (was: steps-only copy expanded empty). Collision-safe node merging; builtin
  `on_error` preserved verbatim through expansion.
- **Loop graph vehicle (bd-27d0/bd-bpd9)** — a bead opting in via `## WORKFLOW <name>` (or a
  `workflow:<name>` label) runs its composed builtin through a detached pane-mode engine
  invocation; in-graph verify writes a workflow receipt; close still requires loop VERIFY +
  `CommandVerified`; bare path byte-identical; marker-file dedupe survives loop recovery.
  Title text never opts in (the c57dfd4-era stub removed).
- **Failure taxonomy + funded preflight + fallback chains (ADR-0026 W1)** — `FailureKind`
  classifier at spawn/send/headless (signature table, no-match = Unknown with evidence),
  one-reroute-per-class fallback chains with audit rows, provider-funded preflight, pane-path
  fallback hop, unknown fallback names fail at catalog load.
- **Bounded mail-health escalation (ADR-0026 W2)** — `loops.json` v3 `mail_health`; 3 consecutive
  renew/lease failures stop new dispatch + escalate (kind-14) until a success; loop-start
  fails loud without a usable identity key.
- **Lease reclaim (ADR-0025 W1)** — `pane_exited` reclaim with respawn-race suppression,
  7200s/86400s staleness tiers, reservation release by reason, audit comment; never `br close`.
- **Typed artifacts + stitcher (ADR-0025 W2)** — `ArtifactRecord` (four status vocabularies +
  grounding), `flywheel artifacts <run_id> [--json]` / `artifacts stitch`; admission requires
  embedding support, never worker self-rating.
- **Loop reducer + journal (ADR-0025 W3)** — `tick(&LoopModel, &LoopMsg)` total reducer,
  `LoopPhase` illegal transitions unrepresentable, Msg journal + `loop replay` / `loop why`,
  `LoopSnapshot` (delivery-verdict carrier, deferred).
- **Parent-death policy (ADR-0025 W4a)** — `on_captain_death: stop|continue|adopt` (default
  stop; stop implies `--force-working` for attributed pins); `toron contact captain --clear`.
- **Plane tripwires (ADR-0025 W4b)** — secret→public and job-assign→private fail closed at
  send with named remedies; `--unsafe-plane` audited; `--raw` does not bypass.
- **Ingress + launch idempotency (ADR-0025 W5)** — `IngressEvent` seam + `handle_ingress`;
  spawn/compose keyed by `(project, run_id|bead, profile)`, dead-pane = respawn; compose
  `--run` replays recorded executions.
- **Evidence-bearing approvals (ADR-0024 Spec 3)** — promise payload carries
  `nodes_completed/failed/pending`, artifact paths, `ledger_path`, `evidence_hash`; a different
  hash is a new question; reject is a decision.
- **PaneStalled classification** — stall detection with evidence-tail sampling classifies
  `rate_limit`/`network`/`model_truncated`/`silent_idle`; watching tier before Diagnose.
- **Mailbox/channel wake completeness** — `MailDelivered` metadata ring (content stays sealed),
  `AckSlaExpired` events, watch kinds include channel join/leave (39002).
- **GitFact producer** — the strict pre-commit guard appends attributed commit facts to the
  flywheel fact journal (best-effort, never blocks).

### 🔧 Fixed (0.3.0 — GS-006 → GS-009, operator doors + Tick Law)

- **Doorbell delivery (GS-006, `ecdb86f`)** — kind-9 `mail-delivered` → `EventClass::PlanePUrgent` (`toron mail inbox --since`, not `channel read` on a thread id); `ack-sla-expired` also a mail wake; 39002 `join`/`leave` classified as Channel and subscribed in-process + CLI (`9,43001,30031,39002`); `watch_targets` `p`/`assignee` for any kind; `captain_fact_inject` loud `FactMiss` (was silent `Ok(())`, never `registry.first()`); `Ack-SLA` 60s production caller + pin-name `p` tags (was hex-only, toron `messaging.rs:713`).
- **Watch supervisor never dead (GS-007, `118fb41`→`a86eb0c`)** — `watch_retry_at` gate retries every 16s forever; `heal_tick` is backup, not the only life; `spawn_watch_child` helper centralizes kinds.
- **Spawn vehicle loud `AlreadySpawned` (bd-nmk5, `982094c`)** — typed `VehicleOutcome` (loud, distinct ok-arm) — caller keeps the bead vehicle-owned and never falls through to a normal brief (was double-dispatch regression where a `bail!` fell through to a brief every poll).
- **Ingress seam typed (bd-0or2, `ecdb86f`→`982094c` audit)** — `ingress_to_loop_msg` with real `pane.agent_status_changed`/`pane.exited` strings (were invented strings `agent_status`/`pane_exited` that `apply_events_to_states` ignored); only `Closed` maps to `BrClose` (was Claimed → BrClose); journal snapshot + replay now include `queue` + `dispatch_pending`/`per_bead_phase`/`mail_degraded`.
- **Tick arms real (bd-ugd1, `118fb41`→`982094c` audit)** — `Herdr` buffers into `pending_events` + drains at next `Tick` through `apply_events_to_states` + `route_event`; `WaveGate`/`CommandVerified` via `LoopEvent` state machine (were `let _ =` no-ops on live variants); `LoopModel` gains `dispatch_pending` + `pending_events`.
- **Doorbell `InboxRows` + `doorbell why` journal + `AckSlaTick` lie fix (GS-009 cheap set, `24b5051`→`a86eb0c`)** — `InboxItems` wrapper `{"messages":[...]}` expanded; `AckSlaTick` → `CheckAckSla` (was `FetchInbox` lie); `swarm.md.tpl:48` legalized; dead `walk_action` flatten deleted; fact adapter `let _ =` → loud `FactMiss` at the seam.
- **Repair loop honest (bd-s1yy, `e5f2477`)** — `verify-1/2` `continue` so c2 can run after c1 failure (was `fail_run`); description no longer claims early-exit `Unrolled`.
- **Loop `--workflow` invoke (bd-i3lw, `e5f2477`)** — `build_queue` actually runs the builtin via `workflow run --execute --mode pane --project <proj>` subprocess with `VerifySink` (was bail English).
- **Plane-P sweep via tick (bd-9hco, `4fe50e0`)** — `InboxRows {rows, pin}` through `tick`; `bell_model` carries `queues`/`statuses`/`index`; legacy queue for sweep removed; `doorbell why` journals sweep rows.
- **Hold loop tick fold (bd-nsb3, `1ab0223`)** — `heal`/`route_event`/`mail-hold`/`dispatch_pending`/`queue`/`per_bead_phase` all inside `tick`; `events: Vec<PollEvent>` travels as `Tick.events`; `watch_sends` Nudge appended last; `ActiveLoop` file persists `per_bead_phase`/`dispatch_pending` crash-safe.

### 🔧 Fixed (0.2.0 — 2026-08-29/30 orchestration campaign, ADR-0024/0025/0026)

- **Non-vacuous VERIFY evidence** — receipt/loop paths parse test counts; a zero-match filter
  warns (and refuses in strict mode) instead of closing green on nothing.
- **channel react after post** — react target hydrates from the bus (was: unknown-message
  failure right after post; also unblocked e2e sc30).
- **reopen_flow fixture** — carries the loop-ready fence `br ready` now requires.

### 🧹 Removed

- Unused `figure-it-out` builtin YAML (decide routes it to `compose --agent`).
- `assert!(true)` padding tests and the title-contains-`graph` vehicle stub.




### ✨ Added

- **Standalone cron CLI (ADR-0012, `bd-25g7.3`, 2026-08-11)** — `flywheel cron` operates on the
  new cron YAMLs (`CronSpec`): `lint` (fail-stop; engine + flywheel tiers), `run` (on-demand, via
  the shared engine with `call_workflow` resolution), `list` (source + scope + steps), `show`,
  `pause`/`resume`, `status`, `history`, `start`, `remove`. User dirs: global
  `~/.config/flywheel/crons/` + per-project `.flywheel/crons/`, precedence project > global >
  builtin. The weekly-review cron migrated to a standalone YAML (bd-25g7.4/6); crons.toml retired.
- **Coalition CLI (ADR-0011, `bd-foh8.3a`, 2026-08-10)** — `flywheel coalition
  {register,form,status,list,intent,reserve,detect,auto-coalesce,negotiate-*,disband}` over the
  intent/soft-conflict substrate.
- **prime-agent (pa) as a spawnable harness (H1, `bd-harness-prime-agent-818f`, 2026-08-15)** —
  compiled-in `pa` profile + `[harness.pa]` spawn recipe (`prime-agent --offline`); when herdr
  rejects an unknown kind (`unsupported_agent_kind`), `start_agent_with_retry[(_dyn)]` falls back
  to `pane.run` the recipe with shell-quoted launch args and waits/renames. prime-agent's built-in
  herdr socket extension self-reports lifecycle (hook-authority Accept route; verified smoke: spawn
  → agent listed as `prime-agent`, boot prime answered READY). `agent.prompt` remains blocked
  upstream (herdr closed Agent enum; kind request: herdrdev/herdr#2840).
- **Crew/agent model rework (`bd-42fs`, 2026-08-10)** — all profiles default harness `omp`;
  per-role models (oracle=glm-5.2/max/fb grok-4.5, planner/coder/reviewer/housekeeper=
  deepseek-v4-flash, scout=big-pickle), effort-tuned, retry-on-429 fallback wired. Per-step
  harness override (`spec.harness`) for review workflows.
- **Authoring guide + references** — `docs/workflow-authoring.md` + the skill references
  (workflow-authoring, coalition-orchestration, cron-authoring standalone model).

### 🐛 Fixes

- **Loop stale-landed dedup (`bd-dedup-stale`, 2026-08-11)** — a re-queued (un-claimed) bead kept
  its stale `landed` dedup record, so the loop dedup-skipped its re-dispatch forever.
  `clear_stale_landed_for_idle` invalidates landed records (same invocation) for idle/done agents
  before each dispatch; working/blocked/unknown preserved (crash-resume dedup intact). 3 new
  `keep_landed_record` tests; 515 flywheel tests pass.
- **`cron run` `call_workflow` resolution (2026-08-11)** — the on-demand cron run injected the
  WorkflowResolver + ProfileResolver (`local_with_resolvers`) so a cron body's `call_workflow`
  resolves against the catalog/built-ins.

- **Live toron ↔ agent-mail search parity** (2026-08-08) — toron's
  unwrap-loop projection now attributes inbound gift-wraps to their project
  via the `agents` index: registration upserts `agents` with the verbatim
  project slug, the pipeline resolves `project_slug` from the recipient key
  at projection time (was hardcoded empty → project-filtered search always
  returned `[]`); `search_messages` emits the AM-shaped
  `{result, next_cursor}` envelope; `mail_shim` passes the daemon envelope
  through instead of double-wrapping it; the live parity test polls the async
  projection (bounded 10s) and `live_parity_full_surface` passes against both
  backends end-to-end.
- **Collaboration patterns carry real content** — debate/plan-review harvest reply
  **bodies** (not subjects); plan-review gate is a real phase-driven loop honoring
  `--max-change-rounds`; debate/plan-review default agents resolve to spawned
  `flywheel-*` names (review-fix F1/F2/F7, 2026-08-05).
- **Blocked escalation halts** — T3 (bounded) exits the durable loop with a
  distinguishable `halted:` result under `--on-blocked mail` (review-fix F4).
- **xproj honesty** — `--to-project` is routing context (validated + carried in the
  body, never silently dropped); loop-triggered spawns are background, no focus
  steal (review-fix F3/F5).
- **Backend migration completed for the durable loop** — `__send`/`__kind`/`__read`/
  `__respawn`/`__xproj-inbox` route through the backend factory; trait gained
  `agent_prompt_coded` (landing semantics) + `MailBackend::agent_name`; CLI commands
  stay concrete per documented deferral (review-fix F6).
- **Quality gates honest** — verify-0001/0002 check real behavior; new
  `verify-review-fixes.sh` regression gate; live `--ignored` suite runbook documented;
  clippy `--workspace` 0 warnings 0 errors (review-fix F8/F9).

### 🚀 Features

- **Unified workflow CLI (ADR-0005 P3.7)** — `flywheel workflow {list,show,run,
  lint,new,history,crew}`: one vocabulary over built-ins + canonical YAML +
  absorbed legacy. `run` invokes through the engine (lint gate first); `crew
  <name>` previews the auto-crew; `lint` is the fail-stop gate adding the
  flywheel-tier checks the engine cannot do (profile `@name`, `stage:` alias,
  `{{inputs.x}}` resolution) on top of engine lint.
- **Collaboration spine (ADR-0001)** — event-wake loop + poll floor; patterns
  `debate`, `plan-review`, `xproj`; mail contact/product; recipe catalog
  `docs/collab/recipes.md`; agent ack/lease/build-slot contracts in spawn prompts.
- **Pluggable backends (ADR-0002 R0)** — `MultiplexerBackend` + `MailBackend`
  traits; factory env/`.env`/config selection; `flywheel backend list|status|default`;
  feature-gated stubs (superlogical/tmux/toron). Defaults unchanged: herdr + agentmail.
- Herdr swarm orchestrator — ntm-parity CLI + durable loop (Resonate SDK 0.6.0)
- Flywheel loop --work — bead-driven mode sourcing from br ready (dynamic plans)
- Work-mode resilience — interrupt resume (in-progress beads) + context-limit respawn
- __send --confirm prompt-landing confirmation in hidden send leaf
- *(loop)* Confirmed sends through durable workflow (bd-send-confirm-durable-8ut)
- *(agentstate)* State-file contract for context signals (bd-agent-state-contract-ukt)
- *(herdr)* Typed pane wait-output helper (bd-pane-wait-output-helper-efq)
- Escalate stalled sends, hard-stop at 3 failures
- Respawn agents stuck in unknown for 2+ polls
- State-file contract in bead/resume prompts
- Context verdict from terminal + state file
- --interval-active config surface for adaptive cadence
- Per-agent last-sent prompt registry
- Herdr protocol drift check in flywheel deps
- __wait-output leaf + README recipe
- Dedupe re-sends against the last-sent registry
- Integration gate for the hardening epic
- Flywheel integration: beads SUBAGENT generator for opencode/pi/grok
- Flywheel integration generator epic (bd-cor) — 8 beads documenting the subagent architecture, all closed with evidence

### 🐛 Bug Fixes

- *(loop)* Never dispatch work to unknown agents (bd-unknown-skip-dispatch-s03)
- *(loop)* Adaptive poll cadence (bd-adaptive-interval)
- Bd-nil + bd-w17: duplicate-label workspace resolution + spawn-time agent naming

### 💼 Other

- Add herdr agent-automation hardening epic + 17 task beads
- Correct bd-ojx to herdr 0.7.5-preview, complete stall-path evidence
- Close epic bd-herdr-automation-hardening-q9t — all 16 tracks closed
- Restore epic close reason (backtick mangling)

### 📚 Documentation

- AGENTS.md + README launchd note; beads tracker init

### ⚙️ Miscellaneous Tasks

- Bump herdr protocol docs v17->v18 (bd-protocol-v18-kkd)
- Zero warnings across all targets
