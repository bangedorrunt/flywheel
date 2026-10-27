<!-- governed-by: ADR-0010, ADR-0011, ADR-0013, ADR-0016, ADR-0018, toron ADR-0015, toron ADR-0018 -->
---
status: proposed
date: 2026-10-25
decision-makers: "Captain (bangedorrunt). Proposed, not accepted. Acceptance is a later word from the operator."
consulted: "reason-network 0.12.12 design study (DESIGN.md, proto.rs, daemon/mail.rs, mcp.rs, skill.md); effect-oriented skill (principles 4, 13, 15); flywheel ADR-0008, ADR-0010, ADR-0011, ADR-0013, ADR-0016, ADR-0018"
informed: "flywheel, toron, and beads maintainers; every sender on MailSurface; a hosted Grok bot is the first pull client"
---

# ADR-0019: One delivery contract for stack mail

The filename still says grokbot. That was the first draft. This decision is the mail plumbing. A hosted grokbot is the first client with no rex block, not a second protocol.

| Field | Value |
| --- | --- |
| **Status** | **Proposed** (2026-10-25). Not accepted. Do not implement past a failing test until the operator accepts this file. |
| **Date** | 2026-10-25 |
| **Decision-makers** | Captain (bangedorrunt) |
| **Extends** | [ADR-0010](0010-delivery-semantics-attention-triage-decision-lifecycle.md) (delivery is a receipt, not a hope), [ADR-0011](0011-session-conductor-crew-budget-captain-fcfs-join-loud.md) (captain is FCFS; this adds a paneless freshness bit), [ADR-0013](0013-agent-profile-lifecycle-journaled-rebind.md) (pin and npub stay put; binding gains `location`), [ADR-0016](0016-functional-effect-kernel-redesign.md) (one fold, journal before the provider, four outcomes) |
| **Does not amend** | Compose tokens and topology in [ADR-0008](0008-workflow-compose-topology-agent-optimal-mental-model.md). Crew cap 5, sticky-open, bead close, the identity ladder, and the one-way dep flywheel to toron stay as they are. [ADR-0018](0018-elm-effect-is-the-core.md) still forbids a direct elm-effect crate dependency. Toron stays the only mail store. This ADR tells that store what a send must return. It does not add a mailbox. |
| **Requires in toron-core** | `loop_ok` and the group-address refuse live next to `MailSurface::send`, so `toron mail send` and flywheel cannot drift. Flywheel does not keep a private copy. The toron repo change is part of done, not a follow-up wish. |
| **Governs** | `src/cmd/captain.rs`, `src/captain_slot.rs`, `src/captain_directory.rs` (new), `src/agent_ref.rs` (`AgentBinding.location`), `src/request_receipts.rs` (park and deny evidence), `src/backend/factory.rs` (the `auto` short-circuit must not choose the fleet for these doors), `src/backend/superlogical.rs`, `src/comms_send.rs` (`wake_via_backend`), `src/main.rs` (`cmd_steer`, the loop pass beside `should_resume_bead`, `Commands`, `--robot-capabilities`), `src/harness_caps.rs` (read-only: grok stays unverified), `src/pane_keep.rs` (read-only: only herdr may clear a binding), `Agents.md`, `~/.agents/skills/flywheel/SKILL.md` section 5, `docs/guides/cli-reference.mdx` |
| **Prior art** | [reason-machines/reason-network](https://github.com/reason-machines/reason-network) `0.12.12`. Patterns adopted as numbers and rules below. No code, no iroh dependency, no SQLite mailbox. |
| **Verification** | `docs/decisions/_scripts/verify-0019.sh` plus the cargo tests named in Verification. The script is written in phase 0 and must fail before the code exists. |

## 1. TL;DR

Stack mail gets one delivery contract. Toron remains the only store. Rex remains the pane plane. Every send, from a crew pin, from `flywheel send`, from steer, and from a hosted client, returns one of five words: `queued`, `woke`, `read`, `unknown`, `expired`, plus the recipient tier. Policy sits on that send: one recipient, a 12-message loop cap, a typed refuse for a group address, one notice when a file is held, and a hold when the sender has not arrived in the project. Prose that says "approved" or "delivered" is not a receipt.

A hosted grokbot is the first client of that contract that has no rex block. Its tier is `pull`. It uses the same steer, halt, and send doors. It does not get a private mailbox, a private wake string, or a spawn right. The local grok harness stays unverified for hooks.

`SentRow.state` stays the append fact (`appended`, `already-appended`, `unknown`, `refused`). `SentRow.wake` stays a diagnostic string. Callers read a new `delivery` field. They do not parse `wake`.

## 2. Context

### 2.1 The hole

Flywheel decides, toron carries mail and reservations, beads record work, and the doorbell wakes. The pane plane is the selected multiplexer. With `backend-superlogical` compiled, `DEFAULT_BACKEND` is `auto`. Inside a rex terminal, `REX_SESSION` is set and `auto` constructs `SuperlogicalBackend` first. Herdr is attached only as transport fallback. `FLYWHEEL_BACKEND=superlogical` is rex with no fallback. Pane ids do not match across backends: rex is `block:…` inside `session:…`, herdr is `wX:pY` (`docs/agent-instructions/backends.md`).

`src/captain_slot.rs` keeps the captain holder while presence is fresh, or the last wake is fresh, or a pane is alive. `bound_pane_is_live` returns false when `pane_id` is `None`. `rebind_hint` then warns that mail to `captain` may reach nobody, because a 7-day whois freshness can hold the alias with nothing live behind it. `pane_binding_may_clear` returns true only when `backend == "herdr"`. A rex pin with no herdr pane is kept on purpose (`src/pane_keep.rs`). Reading herdr and finding nobody is not a dead crew.

Liveness on rex is OSC 7501, folded in `src/backend/rex_map.rs`: `idle`, `working`, `done`, `blocked`, `error`, `clear`. There is no heartbeat. `working` and `blocked` drop on process exit or the next shell prompt. `done` and `error` persist. `blocked` kind is `permission`, `question`, or `auth`. A headless one-shot can leave no 7501 record. The foreground process is the next signal: an agent binary in front is working, a shell is at a prompt.

A hosted Grok bot has no rex block. Under today's slot rule it looks like that dead fold. The briefing on 2026-10-25 listed grok under `registered_unconfigured`. The open harness menu offered pi and opencode. The grok capability row marks steer unsupported, fallback `Mail`, and `hook: None`. The inject bit on that row is witnessed by the herdr sink tests. That witness is not the rex prompt path. `wake_via_backend` in `src/comms_send.rs` already rings through `RexPanes` when the resolved name is not `herdr`. An unverified hook cannot be marked capable (`HarnessCaps::check`). Only the `fake` harness has `hook: Some` and `verified_by: Some`.

`factory.rs` still does this: `auto` with `REX_SESSION` empty or unset returns the herdr client and never builds rex (`rex_selected_via_env`). A hosted bot, a cron run, and the doorbell have no `REX_SESSION`. If they call `multiplexer_backend` under `auto`, they read herdr. That is the bug this ADR closes for the captain doors. It does not, by itself, delete the short-circuit for every other command.

So a local grok block is prompted through rex, and a hosted bot cannot be a captain without a block id it does not have.

### 2.2 What reason-network measured

reason-network `0.12.12` is a per-machine daemon plus a thin MCP client. The directory is team, person, machine, session. A session carries agent, repo, branch, busy or idle, a one-line `doing`, and a delivery tier. The tiers, from `src/proto.rs`:

| Tier | Meaning |
| --- | --- |
| `push` | Injected mid-turn, and an idle session wakes. |
| `turn` | Injected at the next tool call or at stop. An idle session waits for its human. |
| `pull` | Seen only when the agent calls `check_inbox`. |
| `none` | Cannot receive. |

Their Grok CLI adapter is `turn`: `PostToolUse` additional context, and a `Stop` block. `SessionStart` and prompt-submit output are dropped. Subagent hooks are ignored. Session id is `GROK_SESSION_ID` or the hook `session_id`. Hosted assistants, and the README names Grok Bot, add `https://mcp.reasonmachines.com/mcp` and sit on `pull`. MCP notifications are not a delivery path. They checked that in October 2026.

Other bounds this ADR copies as numbers, not as a design to re-derive:

| Bound | Value |
| --- | --- |
| Agent-to-agent loop guard | 12 messages per pair per 600 seconds, then a typed refuse. A person at a terminal is exempt. |
| In-flight push | 90 seconds, then the mail is unread again. |
| Session name retention | 1 day after last seen. Mail is not given to the next conversation. Nothing is forgotten in the hours after the daemon starts. |
| Human approval wait | 10 minutes. One question in flight per session. |
| Shown approval text | 500 characters, then cut, and the cut is marked. |
| Body cap | 64 KB. Not adopted here. Our dispatch prompt cap stays 4096 bytes. |
| Mesh | Full mesh to about 50 machines. Not adopted. |

Their human answer is a signed `Approval` bound to the request id. The sentence "X approved" in ordinary mail is not that answer. They also say the limit: the signature proves the machine recorded a dialog, and software that controls the machine can still sign.

Their M4 file-overlap warning is not built. Ours is further along, and quieter than it should be. `durable::file_blocks_bead` already calls `other_file_holder`, and the loop pass in `src/main.rs` already ORs that into `should_resume_bead`. A blocked resume is skipped. The holder is not told, and a steer does not look.

### 2.3 What this stack already guarantees

- `cmd_steer` requires a project. It digests the objective with `request_receipts::payload_digest("steer-v1", ...)`. `receipt_gate` runs before the fact queue. `Gate::Replay` does not steer again. `Gate::InFlight` refuses. `Gate::Conflict` refuses a different payload on the same key. `DoorReceiptGuard` writes `Prepared` then `InFlight` before the effect. A failed insert does not run the effect.
- `EffectState::Parked` already exists on the nine-state enum and `gate_core` treats it as in flight. A parked steer is a refusal on retry, which is the wait we need. Do not add a tenth state.
- `EffectState::Cancelled`, `Failed`, and `Unknown` all `Proceed`. A deny must not use `Cancelled`, or the same key would steer on retry. A deny `Settled` with evidence `denied`, so replay returns the denial.
- Terminal journal rows do not move. `read` cannot be a second settle. It is an annotation on the same slot.
- Dry arms stay unjournaled. Unknown stays unknown.
- Identity is `--as`, then `AGENT_NAME`. One pin, one npub. New pins are AdjectiveNoun. `AgentBinding` is `profile_name`, `model`, `harness`, replaced only by a journaled rebind. Pin and npub do not move.
- Sticky-open: a hosted captain does not flip `open` to `swarm`. Steer with an explicit topology is still the operator's request, and this ADR does not add a topology flag.
- Crews are proposals. The bot does not spawn. `crew.max` stays 5.
- Bead close stays the gate plus a sha. The bot cannot close.
- Sensors are message producers. There is no Sensor GenServer. `doing` is a message or it is absent.
- Jev and self-reported usage never gate.

### 2.4 The question

How does every sender on this stack learn whether a message was accepted, shown, or read, and under which policy it was refused, without a second mailbox, with rex as the pane plane, and with a hosted grokbot as one pull client of that same contract?

## 3. Decision drivers

1. One owner per fact. Toron owns mail. Beads own work. Rex owns pane presence on the default path. The doorbell owns wakes. Flywheel owns the steer decision and the door receipt. Herdr owns a pane only when `FLYWHEEL_BACKEND=herdr` is set on purpose.
2. A missing rex block is data. An empty herdr listing is not a missing crew, and neither one is permission to spawn.
3. Journal before the provider. A hosted steer that cannot record `Parked` does not ask the operator and does not dispatch.
4. Four outcomes stay apart: success, typed refusal, defect, interruption. A timeout is `unknown` or a typed `approval_required`. It is not success, and it is not a provider error.
5. The grok hook stays unverified until a witness test has been seen failing. This captain does not need that witness.
6. No new network stack and no new mailbox. reason-network's iroh mesh and SQLite are the parts we refuse.
7. Numbers are bounds. The constants below are the contract. An implementer does not pick nearby values.
8. One contract for every sender. A guard that only `flywheel send` runs is not stack plumbing. `toron mail send` and a crew channel post pass the same pure checks or the feature is not done.
9. Strategies stay the ones this stack already has. The work is to stop them from lying about delivery, not to add a fifth bus.

## 4. Considered options

1. **One delivery contract on MailSurface, grokbot as a pull client.** Chosen.
2. **A grokbot-only door, flywheel send left as prose.** Rejected. That is the first draft of this file.
3. **A new CaptainProgram and a SQLite directory mailbox.** Rejected.
4. **Treat the hosted bot as a rex block, and mark the grok hook capable.** Rejected.
5. **Do nothing.** Senders keep guessing from `wake` strings. Rejected.

## 5. Decision outcome

Chosen option: one delivery contract on `MailSurface`, aimed at rex, with the hosted bot as a pull client. New code is the delivery word, the tier, the session key, and the policy checks. Steer, halt, send, channel, and job stay the strategies they are. The fold that already owns a fact keeps it.

### Strategies

These already exist. This ADR does not add a bus. It says what each one may claim.

| Strategy | Who | What it may claim |
| --- | --- | --- |
| `toron mail` | one pin to one pin | A private fact or a blocking question. Receipt word plus tier. Not a broadcast. |
| `toron channel` + `toron job` | crew to crew | Work on the board. Same loop cap, counted as `(from, channel)`. Not a substitute for a DM receipt. |
| `flywheel send` | captain to one pin | The message plus a wake. `delivery` is the word. `wake` is diagnostic only. |
| `flywheel steer` | one captain lane | An authoritative objective, journaled, `--project` required. Not a mail transport. |
| doorbell | wake only | A constant line. Never the message body. A ring is not `read` and not even `woke` until the backend prompt returns ok. |
| reservation | file hold | The holder writes. The other agent stops. One notice mail per `(bead, holder)` per 15 minutes. The hold is the reservation, not the mail. |
| escalate mail | supervisor to the captain inbox | The captain push. `captain watch` or a pull client reads it. It does not inject a rex block by itself. |
| approval mail | operator npub | Opens one parked action. Ordinary prose does not. |

An agent picks the row by the handoff. It does not wrap steer in mail, and it does not treat a sent message as seen without the delivery word.

### D0. Rex is the pane plane for these doors

The captain directory, the steer target, the halt target, and the send wake resolve the multiplexer as follows. `resolve_backend_name` still supplies the name. The caller's `REX_SESSION` does not.

| Resolved name | Backend these doors use |
| --- | --- |
| `superlogical` | `SuperlogicalBackend` only. No herdr fallback. |
| `auto`, and the compiled default when `backend-superlogical` is on | `SuperlogicalBackend` primary. Herdr is constructed only as `FallbackBackend` transport fallback, and only a `Connect`, `Io`, or `Timeout` from rex may use it. A semantic reject stays a reject. |
| `herdr` | Herdr, because the operator set it. Not the default this ADR describes. |

Route this through the **one** constructor: add `Scope { Ambient, Door }` to `multiplexer_backend`
and select per this table when the scope is `Door`. `Scope::Door` ignores `REX_SESSION` and
`rex_selected_via_env`; `Scope::Ambient` keeps today's behavior for every other command. A second
match beside the factory is forbidden — that is the drift ADR-0002 exists to stop. Do not call
`herdr::Client::connect` to decide who is alive. Do not treat `pane_binding_may_clear`'s herdr rule as a fleet listing. A `block:` id and a `wX:pY` id are never the same pane.

`woke` is the selected backend's prompt returning ok. On rex that is `SuperlogicalBackend::prompt` or `pane_run` (write, then Enter), reached the way `wake_via_backend` already reaches `RexPanes`. If that call returns `Unsupported`, the tier is `pull` and the receipt word is not `woke`. Do not retry the same text through herdr to force a `woke`.

`RexPanes` is still marked `dead_code` for the spawn and observe lanes (`src/pane_write.rs`). That gap is not a reason to read herdr. These doors call the backend. They do not wait for the spawn lane to select the typer, and they do not wrap a herdr client in `RexPanes`.

This ADR does not delete `rex_selected_via_env` for the rest of the binary. Commands that are not in the governs list keep today's `auto` behavior until a later change. The captain doors must not inherit it.

Liveness passed into the pure snapshot is the rex record: OSC 7501 state, else the foreground-process signal from `docs/agent-instructions/backends.md`. `idle`, `clear`, and a shell at a prompt count as idle. `working` and `blocked` do not. Missing 7501 on a headless one-shot is unknown, not idle, and not dead.

### D1. One pin, location on the binding

The hosted bot is an AdjectiveNoun pin registered once. Reuse an npub with `flywheel mail register <project> --name <Pin> --from-project <other>`. Do not mint a second key for a taken pin.

Add `location` to `AgentBinding` in `src/agent_ref.rs`:

```rust
#[serde(default)]
pub location: BindingLocation, // Pane (default) | Hosted
```

`#[serde(default)]` so a row written before this field reads as `Pane`. `Pane` means a local multiplexer binding. On the default path that binding is a rex block, not a herdr pane. `Hosted` means no block. Set `Hosted` only through the existing rebind door, `flywheel binding-rebind`, with `RebindReason::CaptainRequest { note: "location=hosted" }`. Do not add a `RebindReason` variant. Old readers ignore unknown fields, so a pre-ADR binary will treat this pin as local. Do not run a pre-ADR binary against a hosted captain.

The pin claims the captain alias only through today's FCFS path (`arrive`, or `toron contact captain`). A live holder (`whois.stale=false` and not stale under D2) is not stolen.

A local grok block is a different session of the same pin only after an explicit rebind back to `location=pane`. It is not automatic.

### D2. Paneless freshness

Extend `CaptainEvidence` with `pull_fresh: bool`. `holder_stale` is false when `presence_fresh || last_wake_fresh || pane_alive || pull_fresh`.

`pull_fresh` is **derived from the durable reader cursor**, never from process memory:
`(now_ms - last_cursor_advance_ms) <= PULL_FRESH_MS`, where `last_cursor_advance_ms` is the
`updated_ts_us` of the reader's own cursor row, `cursor(reader_npub, reader_npub)`. The reader is
the **npub** resolved from the pin through whois — the session key is display only. toron keys the
cursor by `(reader_npub, stream)` and refuses a stream that is not the reader's own
(`NotRecipient`), so looking it up by `hosted:<pin>` or `block:<id>` would silently read `0`, which
is the same defect as reading process memory. `cursor()` reads the row and the upsert advances
`updated_ts_us` only when `log_offset` really moved (`../toron/crates/toron-core/src/inbox.rs`), so
a re-read with no new mail cannot refresh it. An unresolvable npub reads **stale**, never
`0`-as-fresh. The constant is **120_000 milliseconds**. Whois freshness (the 7-day roster bit) does
not set `pull_fresh`. `now_ms` enters as an argument; the pure function never reads the clock.
Consequence: a hosted client that polls with `toron mail inbox` or an MCP client refreshes the
alias, because the read itself is the signal — no flywheel command is required for a pin to stay
fresh.

`rebind_hint` returns `None` when `pull_fresh` is true. `dead_fold_message` is not printed in that case. Rewrite that string so it does not say herdr. A live rex block is `pane_alive`. An empty herdr socket is not. `dead_pin_warnings` skips a pin that is in the pull-fresh set, and a pin whose rex block is in the live set.

A hosted pin that has not polled for 120 seconds is stale. FCFS may claim. That is tighter than the 45-minute stall on purpose. The alias is the address. The stall probe is about work. `captain watch` stays the local one-shot resume. The hosted poll does not replace it.

The hosted client polls at **60 seconds**, so one late poll does not drop the alias. A poll faster than **15 seconds** is a defect in the client: it burns the loop guard in D8 and it does not make the alias fresher than 120 seconds.

### D3. Delivery tier is computed

New module `src/captain_directory.rs`. The tier is an enum `Push | Turn | Pull | Unknown | None`. The interpreter fills a snapshot from the backend D0 selected. The pure function `tier_of` returns the tier. The fold does not read rex, herdr, the clock, or the environment. `HERDR_INJECTABLE` is not an input.

| Condition, first match wins | Tier |
| --- | --- |
| Snapshot says this pin's rex block is in the live set, and the backend's prompt is supported | `push` |
| `hook` is `Some` and `verified_by` is `Some`, and the block is not promptable | `turn` |
| Pin is registered, `location` is `hosted` or there is no block, and toron inbox is enabled | `pull` |
| The rex read degraded or failed, so liveness is unknown | `unknown` |
| Anything else, with positive evidence that nothing can receive (pin unregistered, no block, toron inbox disabled) | `none` |

Grok's row fails the second line today, because `hook` is `None`. A live local grok block is `push`
when rex prompt succeeds. A hosted grokbot is `pull`. A crew whose block rex cannot see is
`unknown` when the rex read itself failed, and `none` only when absence is positively established
and the pin cannot receive. `unknown` is not `none`: degraded liveness is an absence of evidence,
not evidence of absence. A crew that herdr cannot see contributes to neither. `none` is not
"absent", and it is not a spawn clearance.

`tier_of` never returns `turn` for grok until D11's witness exists. Do not copy reason-network's grok `turn` adapter onto a live rex block. Their hook path is what they have. Our prompt path is `pane_run`.

### D4. Directory

`CaptainCmd::Directory { project, json, as_agent }` in `src/cmd/captain.rs`.

`flywheel captain directory <project> --json --as <Pin>` prints one object:

```json
{
  "schema": "flywheel-captain-directory.v1",
  "project": "flywheel",
  "backend": "superlogical",
  "captain": "<pin or null>",
  "degraded": ["pins"],
  "sessions": []
}
```

Each session:

| Field | Rule |
| --- | --- |
| `pin` | Registered pin. |
| `npub` | From whois. Absent npub is a typed error on send, not a blank send. |
| `harness` | Binding harness, or empty. |
| `location` | `pane` or `hosted`. |
| `tier` | D3. |
| `repo`, `branch` | From the block cwd only if rex exposes one for that block; otherwise null. Null is unknown, never a guess. |
| `bead_id` | Assignee's in-progress bead, else null. |
| `doing` | D7. Null is not idle. |
| `file_holder` | Other pin from `file_blocks_bead`, else null. |
| `last_receipt` | D5. Null when no receipt. |
| `session_key` | D6. |
| `pane_id` | The multiplexer id. On rex this is `block:…`. Null for hosted. Never a herdr id unless D0 selected herdr. |
| `last_seen_ms` | Interpreter clock, passed in. Not read inside the pure projection. |

Cap the list at **100 sessions**, the most recently seen. If the project has more, the JSON includes `"truncated": true`. Order is pin, then session key, both ascending, so the same snapshot prints the same bytes.

The command is read-only. It writes nothing: freshness (D2) is a property of the reader's own cursor
write, not of this command.

Address resolve is pure, `resolve_address(snapshot, to, sender) -> Result<Target, AddressError>`:

| Input | Result |
| --- | --- |
| `<pin>` | That pin's newest session. Two live sessions of one pin with no `@` is `ambiguous`, and the error names each session key. |
| `<pin>@<session_key>` | That session. |
| `bead:<id>` | The live assignee when `peer_held` is false and `file_blocks_bead` is false. Otherwise `peer_held` or `file_held`. No assignee is `no_assignee`. |
| `repo:…`, `*`, `<pin>/*` | `group_refused`, always, including the bound captain. Fan-out is not an address. |

Stable error codes, machine-readable, no prose-only failures: `ambiguous`, `peer_held`, `file_held`, `no_assignee`, `group_refused`, `tier_none`, `unarrived`, `approval_required`, `approval_mismatch`, `approval_denied`, `replay`, `in_flight`, `conflict`, `unknown`.

`tier_none` is returned by send only when the resolved session's tier is `none`, which requires
positive evidence that nothing can receive. `pull`, `turn`, and `unknown` queue; only `none` never
appends. On tier `unknown` the send attempts **one** bounded prompt: ok settles `woke`, and
`Connect | Io | Timeout` settles `unknown`. A semantic reject from the backend stays a reject and
is not retried through another transport (D0).

### D5. Receipt words

Every send result carries `delivery`. That includes `flywheel send` (`SentRow`), steer settle evidence, and the directory's `last_receipt`. `SendReceipt` from `MailSurface::send` means toron accepted the event. That is `queued`. It is not `woke` and it is not `read`.

`SentRow.state` is unchanged: `appended`, `already-appended`, `unknown`, `refused`. `SentRow.wake`
stays a diagnostic string. Agents and grokbot read `delivery`. A test that treats a `wake` sentence
as proof of delivery is wrong and must fail.

The word is a type, not a string constant:

```rust
pub enum Delivery { Queued, Woke, Read, Unknown, Denied, Expired }
pub fn delivery_of(append: AppendState, tier: Tier, prompt_ok: Option<bool>) -> Delivery
```

`delivery_of` is the **single constructor**: `SentRow.delivery` and the door receipt's evidence word
both come from it, and neither writes a literal. `DoorReceipt.evidence` stays `Option<String>` on
the log for compatibility (old rows, old readers), but the field is private to
`request_receipts` and set only through `settle(Delivery)`; there is no `From<&str> for Delivery`
and no `Delivery::from_str` with a default. A literal written to `.evidence` outside that module
must not compile. `AppendState` is the existing `state` word, parsed once at the boundary.

Map:

| Append `state` | Backend prompt | `delivery` |
| --- | --- | --- |
| `appended` or `already-appended`, tier `pull` or `turn` | not asked, or not ok | `queued` |
| `appended` or `already-appended`, tier `push` | prompt returned ok | `woke` |
| any, later ack on that `msg_id` | | `read` as an annotation, the row's `delivery` on the directory prefers `read` |
| `unknown`, or `MailErrorCode::Timeout` or `Io` between claim and settle | | `unknown` |
| `refused` | | no delivery word. The error code is the result (`loop_refused`, `group_refused`, `tier_none`, `file_held`, `unarrived`). |
| any, unread past the forget window | | `expired` as an annotation, never a new settle. `history_row_status` ignores it, exactly as it ignores `read`. |

Doorbell text alone does not set `woke`. `left to the doorbell host` is still `queued`.

On the door receipt's `evidence` string, the same words:

| Word | State | Meaning |
| --- | --- | --- |
| `queued` | `Settled` | Toron accepted the mail, or the steer is recorded and the target is `pull` or `turn`. The model has not been shown it. |
| `woke` | `Settled` | The selected backend's prompt returned ok (on rex, `RexPanes` / `pane_run`). A verified hook counts only when D11 has landed. The model may still not have read it. |
| `read` | annotation, not a new settle | The target session acked, or `check_inbox` returned this id. Event kind stays an annotation. `history_row_status` does not count it as a run outcome. |
| `unknown` | `Unknown` | The process died, or the attempt timed out, between claim and settle. Retry may proceed. Do not rewrite this to `queued`. |
| `denied` | `Settled` | The operator denied. Replay returns `approval_denied` and does not dispatch. |

In-flight TTL is **90 seconds**. `woke` is a fact about the prompt, not a lease: no component
re-shows the message. The evidence stays `woke` until an ack writes the `read` annotation, and the
TTL does not change a `Settled` row into `Unknown`. Re-showing is the sender's retry under the same
`--request-key`, which the gate already makes idempotent; the doorbell carries a constant line and
"never the message body", `captain watch` is one-shot by design, and `captain directory` is
read-only, so no arm in this ship can re-deliver. Promising a re-show nobody performs is the same
class of lie as a prose `wake`.

`Gate::Replay` prints the recorded evidence and `"replay": true`. It does not send a second copy.

A hosted caller is: `location == hosted` for `--as`, and the process has no rex block. No block means `REX_SESSION` is empty or unset and this pin has no `block:` binding in the live set. Do not use `HERDR_PANE_ID`. A captain sitting in a rex block is local and does not take the approval park. A local shell steer stays the operator's steer.

Hosted steer order, and this order is the program:

1. Refuse an empty objective, a missing project, and a blank project spelling. Typed, before any write. Same checks `cmd_steer` has now.
2. If the caller is hosted and this key has no approval yet, write `DoorReceipt` `Prepared`, then `Parked`, with the steer digest and the payload. Return `approval_required` and the slot. Do not enqueue the fact and do not dispatch. A failed journal insert returns the error and leaves no parked row. The payload is the typed `SteerPayload` over the digest inputs (objective, crew, direct, reuse, retain, retire-orphans, project), and the digest is re-derived from those typed fields with `request_receipts::payload_digest`. A mismatch is `conflict`. A parked row with no payload is `approval_mismatch`, not a guess.
3. `receipt_gate`. `Replay`, `InFlight`, and `Conflict` return as they do today.
4. On `Proceed` (a local steer, or a hosted steer whose approval already settled this slot to a non-deny), `DoorReceiptGuard` then the existing steer body.
5. Settle `queued` or `woke` from the provider result. Timeout or a dropped child settles `Unknown`.
6. Halt of that slot settles `Cancelled` only when the steer had not settled. A settled `queued` or `woke` stays. Halt is interruption, recorded on the supervisor epoch, not as a provider failure.

`--direct` skips the bead, not the operator. A hosted `--direct` still parks: approval is about who
is asking, not about whether a bead exists, and `harness_caps::capable` is false for grok, so the
caller is untrusted. A **local** operator `--direct` stays unparked (the park arm is hosted-only),
so autonomy is untouched where autonomy exists. `--direct` skips D9's file check because it has no
bead, and its parked payload carries no bead id.

### D6. Session key

The mailbox key is:

| Source | Key |
| --- | --- |
| Hosted binding | `hosted:<pin>` |
| Block whose harness exposed a session id at register time | `<harness>:<session_id>` |
| Rex block with no harness session id | `block:<block_id>` |
| Explicit herdr backend, no harness session id | `pane:<pane_id>` (`wX:pY`) |

Grok's session id is the environment variable `GROK_SESSION_ID`, captured when the block is registered. If it is absent, the key is `block:<block_id>` on rex. Do not invent continuity. Do not store a herdr `wX:pY` key for a rex block.

Mail queued for key A is never delivered to key B. A new pane id is a new key when no session id was stored.

Forget a non-kept key **86_400_000 milliseconds** after `last_seen_ms`, and **only when its unread
count is 0** — a key with unread mail is not forgotten. The sender is told at send time: `queued`
plus `forgotten_after_ms`, so the bound is known up front; what it must not have to infer is what
happened at forget time. Unread mail that must expire is annotated `expired` **once per row** and
the sender gets **one batched notice** (not one page per row); the directory's `last_receipt`
prefers `expired` over `queued` for those rows. Journal one `session_forgotten` event per key and
do not attach the mail to the next key. The kept keys (`hosted:<pin>` and the bound captain's pin
mailbox) are not forgotten by this timer, and instead carry a documented cap with
oldest-read-first eviction — never "unbounded on purpose". Redelivery is rejected because it
invents a second delivery attempt past the `read` contract; dropping the timer is rejected because
a key nobody reads is a mailbox nobody drains.

Do not forget anything in the first **7_200_000 milliseconds** after the flywheel or doorbell process start, while sessions re-register. The kept mailbox is the hosted key `hosted:<pin>` and the bound captain's pin mailbox. Those are not forgotten by this timer.

Store session rows as state-log events and fold them into a `BTreeMap`. Do not add SQLite.

### D7. Doing

`doing` is optional and never a gate. A watch edge (a hook stdin event, or a transcript tail the interpreter already has) emits a message `{ pin, session_key, line, observed_ms }`. The directory projection stores the line, cut at **200** Unicode scalar values. A team-invite URL in the line is replaced with `(link)` before store. A self-reported status does not overwrite a line that came from a hook or a transcript. No reader runs inside `workspace::tick` or `durable::tick`.

Null `doing` is unknown. The directory must not print it as idle. Idle is the OSC 7501 state from D0 (`idle`, `clear`, or a shell at a prompt), separate from `doing`. Do not read a herdr status field on the rex path.

A session whose `doing` is non-null does not receive a new bead from idle-pane rotation. Follow-ups on its current bead still resume under the existing 15-minute window. This is a filter in the pure rotation input, not a new picker.

### D8. Loop guard on the mailbox

The pure function lives in toron-core, next to `MailSurface`, because flywheel must not be the only caller and toron must not depend on flywheel:

```rust
pub fn loop_ok(sent_at_ms: &[i64], now_ms: i64, exempt: Exempt) -> bool
```

True when fewer than **12** timestamps lie in `(now_ms - 600_000, now_ms]`. The caller passes the
timestamps. The function does not read the clock.

Exemption is an **argument**, never ambient: toron-core cannot know a flywheel `--as` or a TTY, so
resolving "local TTY with no `--as`" inside the policy fn would read authority nobody passed. The
caller computes `Exempt::Operator` or `Exempt::None`. This is a **courtesy cap unless the count is
taken inside the append transaction**: a read-then-write check lets two concurrent senders at 11
both append. Say so plainly — under-refusal is the accepted residual, not a hidden promise of
at-most-once.

Count key:

| Send | Key |
| --- | --- |
| Mail to one pin | `(from_pin, to_pin)` |
| Channel post | `(from_pin, channel_id)` |
| Job message | `(from_pin, job_id)` |

On false, `MailSurface::send` returns `MailErrorCode::PolicyDenied` and the message text contains the stable code `loop_refused`. Nothing is written. Flywheel maps that to the send error `loop_refused` and journals the refusal. It does not retry.

Exempt: the operator's local TTY with no `--as`, and one already-approved steer slot (D10). Crew pins are not exempt. The bound captain's ordinary chat is not exempt. A channel post from a crew pin counts.

`src/mail.rs` and `src/backend/toron.rs` call this function. They do not reimplement the count. `toron mail send` calls the same function. Until that export exists, this contract is not done, and flywheel must not document the guard as stack-wide. A flywheel-only wrapper is the rejected option.

### D9. File hold tells the holder

Do not add a second overlap function. `durable::file_blocks_bead` and `other_file_holder` stay the rule.

At the loop site in `src/main.rs` that already passes `file_blocks_bead` into `should_resume_bead`: when the bead is suppressed for that reason, enqueue one mail to the holder pin, once per `(bead, holder)` per **15 minutes**, using the same resume-window store. The body is the bead id and the path pattern. A later pass inside the window sends nothing.

In `cmd_steer`, the check binds only where a bead already exists: a steer that **targets an existing
bead** (a resume, or a `bead:<id>` objective) checks before its first pane write, and a bead-tracked
steer checks at the post-plan, pre-build gate — after the bead plan is written and before the build
stage's first pane write. There is no bead at initial dispatch (goal → brainstorm → plan → beads →
build), so a check placed there would either be skipped or invent a bead. If `file_blocks_bead` is
true for the target, do not dispatch. Settle evidence `file_held` only after the one mail is
accepted by toron (`queued`). If the mail is not accepted, leave the receipt `Unknown` and do not
claim the steer happened. The loop-side check stays primary because it has a bead on every pass.
`--direct` skips this check because it has no bead.

### D10. Operator approval

The approval is a toron mail, which is already a signed Nostr event. There is no new signature scheme.

`flywheel captain approve <project> --request-key <key> --decision approve|deny --note <text> --as <OperatorPin>`

The command:

1. Loads the parked steer receipt for that key. Missing row is `approval_mismatch`. A row that is not `Parked` is `in_flight` or `replay`, matching the gate.
2. Refuses when `--as` equals the asker pin on the parked row. The bot cannot approve itself.
3. Sends a mail from the operator pin to the asker pin. The body is exactly these four lines, UTF-8, note cut at 500 characters with the cut marked:

```text
flywheel-approval.v1
about=<request_key>
decision=approve|deny
note=<one line>
```

4. The interpreter verifies the event signature and that the sender npub is `whois` of `--as`. A body with those lines that is not a verified event is `approval_mismatch`. Prose that says "approved" does not match.
5. `approve` reads the parked payload, checks it still hashes to the row's digest, moves `Parked` to `InFlight`, and runs that payload. A mismatch is `conflict` and the row stays `Parked`. `deny` settles `Settled` evidence `denied` and ignores any new objective.
6. One parked steer per captain pin. A second hosted steer while one is parked returns `in_flight` and does not replace the parked objective.
7. The wait bound is **600_000 milliseconds** from the parked event's time. The interpreter supplies `now_ms`. Past the bound, the slot settles `Unknown` with evidence `approval_expired`, and the asker is mailed that code. A late approval mail does not un-expire it. A new steer needs a new request key.
8. No desktop and no operator pin is a typed error `approval_required`. It is not a silent allow, and it is not a popup library. This repo does not shell `osascript`.

Add `payload: Option<SteerPayload>` to `DoorReceipt`, `#[serde(default)]`, where `SteerPayload` is a
typed struct over the digest inputs (objective, crew, direct, reuse, retain, retire-orphans,
project). Old rows read as `None`. It is required while a hosted steer is `Parked`, and cleared or
left unused once the row is `Settled` (the evidence word is the settle record). Approve never takes
a new objective. A digest is not reversible, so the payload is the only replay source. **Do not
store "canonical JSON" and re-hash its bytes**: serializer byte-stability is a guarantee nobody
owns and this ADR forbids new dependencies. Re-derive the digest from the typed fields with
`request_receipts::payload_digest`, which trims and separates with `\x1f` and is therefore defined
over strings, not JSON; a mismatch is still `conflict` and the row stays `Parked`.

### D11. Grok hook witness is out of this ship

Do not set grok `steer.supported`. Do not set `hook: Some` without `verified_by`. Do not install a hook file from this ADR.

A later change may add a witness test that drives a fake grok the way `fake_harness_denies_once_and_types_no_interrupt` drives `fake`. That test must fail when the deny call is removed. Until that red run has been recorded, `WakePick` for grok stays `Interrupt`. The hosted captain does not wait on that work.

### D12. Cross-project hold

A send whose project the sender has not arrived in does not deliver. The code is `unarrived`. Same-project mail stays on its current path.

A hosted steer into a project this pin has not arrived in parks as D10. The approval body gains a fifth line `project=<slug>`, and the slug must equal the steer project. A same-project approval does not authorize a different project. Any other cross-project send is refused with `unarrived`, not parked, unless the caller is the hosted captain and the payload is a steer.

`cross_workspace` stays default false. This ADR does not turn it on. Holding is not the same as subscribing the doorbell to another workspace.

## 6. Consequences

* Good, because a crew pin, `flywheel send`, and a hosted client all read `delivery` and the tier, and a `wake` sentence can no longer be mistaken for `read`.
* Good, because the loop cap and the group refuse sit on `MailSurface`, so a toron CLI send and a flywheel send cannot diverge.
* Good, because a hosted grokbot steers through that same contract and a second call with the same key does not dispatch.
* Good, because a pin rex cannot prompt shows as `pull`, `unknown`, or `none` (degraded is `unknown`, never `none`), which is the P2 check in one place.
* Bad, because a hosted steer waits up to 10 minutes, and a cross-project send from a pin that has not arrived is refused until it arrives or, for steer, until the operator signs.
* Bad, because a pre-ADR binary misreads `location` as a local binding. Mixed binaries are a standing cutover, not a supported state.
* Bad, because the toron-core export is a second repo. Flywheel cannot finish D8 alone, and a partial ship that only wraps `flywheel send` is a failed implementation of this ADR.
* Neutral, because `captain watch`, escalate mail, crew cap, sticky-open, bead close, and the strategy table's owners do not change.
* Neutral, because local `flywheel steer` from the operator's shell does not gain an approval park.

## 7. Non-goals

* iroh, a relay, a mesh, a roster, and a SQLite mailbox.
* A flywheel-only loop guard that leaves `toron mail send` uncovered and then calls the plumbing done.
* A `CaptainProgram` or any second model of fleet, beads, mail, or reservations.
* A new chat protocol for grokbot. It speaks the delivery contract.
* Spawn, compose, crew add, and `briefing --commit` from the hosted bot.
* Bead close, gate report, and `--force` bypass.
* Flipping sticky-open, raising `crew.max`, or changing the 45-minute stall.
* Marking grok, opencode, pi, or omp hook-capable.
* File attachments. Caps, if a later ADR adds them: at most 10 files, 256 KB each, 512 KB total, no dotfiles, nothing under `~/.ssh`, `~/.aws`, `~/.config`, or `~/Library`, mode 0600, never executed. Not in this ship.
* Group send. `repo:` and `*` stay refused. There is no `--fan-out`.
* A Sensor GenServer, a transcript scraper inside a reducer, or a doing-line that gates a close.
* Replacing `captain watch`. The hosted poll is a freshness heartbeat, not a second watcher.
* Multi-human teams. Identity stays per project.
* A direct elm-effect dependency. New pure functions are ordinary Rust. The host that runs them is the existing CLI interpreter.
* Deleting `rex_selected_via_env` for the whole binary. Only the doors in the governs list stop using it. A later change can retire it globally.
* Judging the crew with `HERDR_INJECTABLE`, `HERDR_PANE_ID`, or `herdr::Client::connect`.

## 8. Implementation plan

Plumbing first, client second. Delivery word, tier, session key, and the mailbox checks land before the hosted pin can steer. Each phase lands with its tests. Do not start phase N+1 while phase N's new test is still green when the new call is deleted. Do not ship phase 7 and call the contract done if D8's toron-core symbol is missing.

### Phase 0. Failing tests and the verify script

* **Affected paths**: `src/captain_directory.rs` (new, tests only until the functions exist), `src/captain_slot.rs`, `docs/decisions/_scripts/verify-0019.sh`.
* Write the tests in Verification first, watch them fail to compile or fail the assertion, then add the functions.
* The script greps this ADR's constants and fails if `src/harness_caps.rs` sets grok `hook:` to `Some`, or sets grok steer supported. It fails if `Cargo.toml` gains `iroh` or `rusqlite`. It fails if a new `sqlite` mailbox type appears under `src/` for this feature.

### Phase 1. Binding location and pull freshness

* **Affected paths**: `src/agent_ref.rs`, `src/captain_slot.rs`, the arrive path in `src/main.rs` that builds `CaptainEvidence`.
* **Pattern**: `#[serde(default)]` on the new field. `RebindReason::CaptainRequest` note `location=hosted`. `holder_stale` stays a pure function. Add `pull_fresh` as a field the caller fills. `pane_alive` is true when the rex block is in the live set from D0.
* **Avoid**: a new rebind variant, a second captain alias, auto-rebind when herdr has no pane, setting `pane_alive` from a herdr socket.

### Phase 2. Directory

* **Affected paths**: `src/captain_directory.rs`, `src/cmd/captain.rs`, `src/cmd/mod.rs`, `src/main.rs` (`Commands::Captain`, `--robot-capabilities` text, help).
* **Pattern**: pure `tier_of`, `project_directory`, `resolve_address` over a snapshot struct. The CLI loads the D0 backend (`SuperlogicalBackend` on `auto` and `superlogical`), whois, reservations, and beads outside those functions and passes values in. `BTreeMap` for any map that affects output order. Tests use a scripted `MultiplexerBackend`, not a live rex server and not a herdr socket.
* **Avoid**: I/O inside the projection. `HashMap` iteration for output. Treating a degraded rex read as tier `push`. Calling `multiplexer_backend` on this path while `REX_SESSION` is unset, because today's `auto` would return herdr.

### Phase 3. Receipts and hosted park

* **Affected paths**: `src/main.rs` `cmd_steer`, `src/request_receipts.rs` (`DoorReceipt.payload` only). Prefer not to change `gate_core`. `Parked` already means in flight.
* **Pattern**: `DoorReceiptGuard` before any dispatch. Evidence strings are the words in D5, nothing else.
* **Avoid**: `Cancelled` for a deny. Settling `unknown` as `Settled`. A second steer while parked.

### Phase 4. Approve command

* **Affected paths**: `src/cmd/captain.rs` (`CaptainCmd::Approve`), toron mail send through the existing `MailSurface`. No new toron API.
* **Pattern**: verify the Nostr event the mail surface already verifies. Compare npub to whois of `--as`.
* **Avoid**: `osascript`, a local dialog binary, trusting the mail body without the signature check.

### Phase 5. Session keys

* **Affected paths**: the pane register site that already knows the harness, plus a state-log fold. Capture `GROK_SESSION_ID` there when the child environment has it.
* **Pattern**: events in the existing project log. Fold with an explicit initial `BTreeMap`.
* **Avoid**: handing unread mail to a new key. Forgetting the `hosted:<pin>` key.

### Phase 6. File-hold mail and the loop notice

* **Affected paths**: `src/main.rs` beside the existing `file_blocks_bead` call, and `cmd_steer` once the bead id exists.
* **Pattern**: call `file_blocks_bead`. One mail per `(bead, holder)` per 15 minutes via `state::record_resume`'s window or a sibling key. Do not copy `other_file_holder`.
* **Avoid**: a source-text test that searches for the function name in a comment. Call the planner and assert the mail command.

### Phase 7. Loop guard, doing, then the pull client

* **Mailbox first.** `loop_ok` is added in toron-core beside `MailSurface::send`. `src/mail.rs` and `src/backend/toron.rs` call it. Channel and job sends use the keys in D8. A missing export stops this phase. Do not paste a second `loop_ok` into flywheel.
* **Doing** stays a directory field. Timestamps and the doing line enter as arguments. No clock read in the pure function. A doing line does not change a gate verdict.
* **Pull client last.** `location=hosted`, the parked steer, and `captain approve` from D1, D2, and D10. They consume `delivery` and tier. They do not invent a wake string.

### Phase 8. Skill sync

When `CaptainCmd::Directory` and `CaptainCmd::Approve` exist, update all four:

1. `src/main.rs` `Commands` (the enum is the compile check).
2. `--robot-capabilities` in `src/main.rs`.
3. `Agents.md` Critical overrides, one short paragraph: hosted grokbot is a pull-tier captain pin, steer parks for operator approval, grok hook stays unverified.
4. `~/.agents/skills/flywheel/SKILL.md` section 5, one line each for `captain directory` and `captain approve`.

Also `docs/guides/cli-reference.mdx`, the captain row.

### Dependencies

None. Do not add iroh, rusqlite, rmcp, or elm-effect.

### Configuration

No new config keys. No new environment variables. `PULL_FRESH_MS`, `LOOP_LIMIT`, `LOOP_WINDOW_MS`, `IN_FLIGHT_TTL_SECS`, `SESSION_FORGET_MS`, `SESSION_START_GRACE_MS`, `APPROVAL_WAIT_MS`, and `DOING_MAX_CHARS` are `pub const` in `src/captain_directory.rs`. Tests use the constants, not copied literals, except one test that documents the numeric value so a silent edit to the constant fails.

### Migration

Existing bindings gain `location: pane` by serde default. Existing door receipts are unchanged. No backfill. A pin becomes hosted only by an explicit rebind after this ADR is accepted.

## 9. Verification

- [ ] `tier_of` returns `push` for a live `block:` id whose prompt is supported, `pull` for `location=hosted` with no block, `unknown` for a degraded rex read, and `none` only on positive no-receiver evidence. `turn` requires both `hook` and `verified_by`. An empty herdr pane list with a live `block:` id still returns `push`. A grok caps row from `TABLE` does not return `turn`.
- [ ] With `FLYWHEEL_BACKEND` unset, `REX_SESSION` unset, and `backend-superlogical` compiled, the captain directory loader is handed a rex backend. It does not call `herdr::Client::connect`. The test fails if the loader follows `rex_selected_via_env`.
- [ ] `resolve_address` returns `group_refused` for `*` and for `repo:flywheel`, including when the sender is the bound captain. `bead:<id>` returns `file_held` when `file_blocks_bead` is true, and `peer_held` when `peer_held` is true.
- [ ] `holder_stale` is false when only `pull_fresh` is true, and true when only the 7-day whois bit would have been fresh and `pull_fresh` is false. `pull_fresh` is fed from a scripted cursor fixture **keyed by npub**, and a lookup by `hosted:<pin>` or `block:<id>` must read stale, not `0`-as-fresh (delete that guard and the test fails). The clock is an argument. `rebind_hint` returns `None` when `pull_fresh` is true.
- [ ] On tier `unknown` the send appends and attempts exactly one prompt: ok → `woke`, `Connect|Io|Timeout` → `unknown`. Delete the attempt and the test fails (no silent queue). A semantic backend reject is not retried through another transport.
- [ ] The delivery word has one constructor: a source fence fails on `.evidence =` written outside `request_receipts` and on any `From<&str> for Delivery`, and a runtime test asserts `settle(Delivery::Woke)` moves `queued` to `woke`. Delete the setter call and the test fails; the fence alone is not the evidence.
- [ ] A hosted steer with a new request key writes `Parked` with a typed payload that re-derives to the digest, and does not call the dispatch that follows the guard. Delete that park and the test fails. A parked row with no payload returns `approval_mismatch`; a payload whose typed fields re-derive to a different digest returns `conflict` and the row stays `Parked`.
- [ ] A hosted `--direct` steer parks (no bead id in the payload) and a local operator `--direct` does not park; deleting the hosted arm fails the test.
- [ ] The same request key after `Settled` evidence `queued` returns `replay` and does not dispatch. The same key with a different objective returns `conflict`.
- [ ] Deny settles `denied`. A retry of that key returns `approval_denied` and does not dispatch. A body line `decision=approve` without a verified operator npub returns `approval_mismatch`.
- [ ] Approve from the asker pin returns `approval_mismatch` and leaves the row `Parked`.
- [ ] A journal insert error on the park returns before dispatch. The test uses a failing log double, not a live disk.
- [ ] An in-flight steer that is halted records interruption. The receipt is not `Settled` with evidence `woke`.
- [ ] A provider timeout records `Unknown`, and the JSON word is `unknown`.
- [ ] `flywheel send` JSON has `delivery` set to `queued` when toron appends and the rex prompt did not return ok, including when `wake` contains `left to the doorbell host`. A test that reads only `wake` does not pass.
- [ ] `src/mail.rs` and `src/backend/toron.rs` each call `loop_ok` with an `Exempt` argument and a clock argument; deleting either call makes its test fail (a symbol-absence grep is not the evidence). The tree defines no second `fn loop_ok`. `MailErrorCode::PolicyDenied` is the refuse, and the message contains `loop_refused`.
- [ ] Session key `block:old` unread mail is absent from a directory row for `block:new`. A key with unread mail is NOT forgotten at `SESSION_FORGET_MS`; one that does expire writes `expired` per row and ONE batched sender notice (assert the count is 1, not N). `hosted:<pin>` is still present after `SESSION_FORGET_MS` and its cap evicts read mail oldest-first. A rex block is not stored under a `wX:pY` key.
- [ ] `captain directory --json` on a block whose cwd rex does not expose prints `repo`/`branch` as null, never a fabricated path.
- [ ] `loop_ok` is false on the 12th timestamp inside 600_000 ms and true when the oldest is older than the window. The clock and `Exempt` are arguments; a test that resolves exemption from env or a TTY fails.
- [ ] The loop pass, given file rows that `file_blocks_bead` accepts, emits one holder mail and emits none on the next pass inside 15 minutes. The steer-side check fires at the resume/`bead:<id>` seam and at the post-plan pre-build gate, and never at initial dispatch where no bead exists.
- [ ] `harness_caps::TABLE` grok `hook` is `None` and grok steer is unsupported. `verify-0019.sh` exits non-zero if that changes.
- [ ] `Cargo.toml` does not gain `iroh` or `rusqlite`. The verify script checks this.
- [ ] `--robot-capabilities`, `Agents.md`, and the flywheel skill section 5 name `captain directory` and `captain approve` once those variants exist, and not before.
- [ ] Reducer tests that already cover `workspace::tick` still pass. This ADR adds no I/O to a tick.

## 10. Pros and cons of the options

### One delivery contract, grokbot as a pull client

`delivery` and tier are on every send. Policy is on `MailSurface`. The hosted bot is a pin with `location=hosted` that reads those fields. Steer, halt, send, mail, channel, job, beads, and the doorbell stay the owners they are.

* Good, because a failed journal insert refuses the steer, which is the rule the effect journal already enforces.
* Good, because `Parked` is already an in-flight gate, so a double approve cannot fork two steers without a new state machine.
* Good, because the operator's npub is the approval key we already have.
* Bad, because raw `toron mail send` bypasses the loop guard.
* Bad, because the hosted path is pull, so a crew sees the steer only when something reads the inbox or rex prompt returns ok. The receipt says which. A herdr retry is not the backup.
* Neutral, because local operator steer does not get slower.

### A grokbot-only door

Directory, park, and approve exist. `flywheel send` and `toron mail send` keep today's prose wake and no shared loop cap.

* Good, because the bot can steer without touching the mail path crews already use.
* Bad, because the bot then has a private idea of `queued` that a crew send does not return, and the two will drift.
* Bad, because the chatter cap misses the path crews actually use, which is toron.
* Bad, because that is how the first draft of this file was shaped, and it does not fix delivery.

### A new CaptainProgram and a SQLite mailbox

A second fold owns the directory, the outbox, and approvals. The bot talks only to that fold.

* Good, because the feature would read as one diagram.
* Bad, because ADR-0016 already has one live model per fact, and a second outbox will diverge from toron the day a send lands in one and not the other.
* Bad, because reason-network's SQLite exists to be their first mailbox. We already have one. Two stores is the defect this stack's effect law names.
* Bad, because iroh adds a network peer we would then have to gate, roster, and relay.

### Pretend the bot is a rex block, and verify the grok hook now

Register a fake `block:` id, set grok `hook: Some`, and prompt.

* Good, because rex `pane_run` is a real push path for a local grok block.
* Bad, because a hosted bot has no block, so the fake id recreates the dead-fold bug `bound_pane_is_live` was written to stop.
* Bad, because `HarnessCaps::check` rejects steer-supported without a verified hook, and a verified bit without a test that has failed is the kind of green the repo's effect law forbids.
* Bad, because the inject witness on the grok row is the herdr sink. Using that bit as proof of a rex prompt is the same lie as a fake block id.

### Do nothing

The operator keeps using a local pane as captain.

* Good, because no new approval wait and no mixed-binary cutover.
* Bad, because a hosted bot cannot hold the alias without looking stale, and a rex crew still looks empty to any door that reads herdr.
* Bad, because ADR-0008 already recorded Grok Bot as unwired. Leaving it unwired is how the next session invents a side door.

## 11. More information

### Why these numbers

They are reason-network's, except where our stack already has a tighter one. 120 seconds of pull freshness is twice the hosted poll of 60 seconds, and it is not the 7-day whois bit. 90 seconds in flight, 12 messages per 600 seconds, 10 minute approval, 500 character note, 1 day session retention, and 2 hour start grace are copied so two agents do not "improve" them in review. The 15 minute file-hold mail window matches `RESUME_WINDOW_DEFAULT_SECS`. The 100 session cap matches their presence list cap. The 4096 byte prompt cap, the 45 minute stall, the crew cap of 5, and the repair cap of 32 are ours and do not move.

### Acceptance

This file is proposed, and it carries **two acceptance gates** because one ship cannot verify the
other's symbols:

| Gate | Covers | May be accepted when |
| --- | --- | --- |
| `Contract` | D0–D7: tier, delivery word, directory, session keys | its own checklist is green in this repo |
| `Policy` | D8, D9, D10, D12: loop guard, file-hold notice, approval, cross-project hold | its checklist is green **including** the toron-core `loop_ok` export |

`Contract` may reach `accepted` while `Policy` stays `proposed`. The `Contract` checklist must not
cite a toron-only symbol: a gate we cannot run here is not a gate. Implementation of a gate waits
for the operator to accept that gate. A coding agent that finds `proposed` stops after reading,
unless the operator's message in that session says to implement ADR-0019.

### Revisit

Revisit when any of these is true:

* The hosted Grok bot cannot hold a toron inbox or an MCP client across a day. D1's client assumption is then false.
* A measured grok resume shows `GROK_SESSION_ID` changes when the conversation does not. D6 must key off a different id.
* Raw toron chatter is the common case, so the D8 bypass is the real traffic. Move the guard in a toron ADR.
* A witness test for the grok hook has failed for the right reason and then passed. That is a separate ADR or a short amendment. It does not rewrite D3's first row.
* Pull freshness of 120 seconds drops the alias during ordinary use more than once a week. Raise it with a measured gap, do not jump to the 7-day whois bit.

### Cutover

1. Accept this ADR.
2. Register the pin and confirm `toron mail whois <project> <Pin>` returns one npub.
3. Rebind `location=hosted`.
4. Arrive as that pin. The alias claim must print only if the previous holder is stale.
5. `captain directory --json` shows the pin at tier `pull`.
6. One hosted steer with a fresh `--request-key` returns `approval_required` and does not dispatch.
7. `captain approve` from the operator pin returns `queued` or `woke`.
8. The same key returns `replay`.
