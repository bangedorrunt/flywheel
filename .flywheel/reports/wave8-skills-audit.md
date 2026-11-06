# bd-wave8-skills-audit-1wecs — skills native audit

status: done
flywheel-sha: e4afb93d / toron-sha: 5cd390a (report committed in flywheel)
verdict: worker-receipt
terminal: done

## Method

Toron side: fresh HEAD binary (`target/debug/toron`, rebuilt 22:00) — every
taught command/flag ran through `--help` or the catalog. Flywheel side: HEAD
tree is red on the wave5-a eviction WIP (dup imports + `toron_core::backend`
fallout), so flywheel truth came from HEAD source (`Commands::*` dispatch
sites + struct defs) cross-checked against the last-green 18:54 binary where
the surface is unchanged. No deviation from "code is truth" — just the oracle
that compiles.

## Command table (command -> resolves/fixed/removed)

Toron (binary): daemon start/stop/restart/status/install/uninstall, serve
--mcp-stdio/--mcp-http, status, catalog (38 frozen + 3 additives = 41,
resources 25), config, mail send/reply/inbox/ack/read/search/thread-summarize/whois
(+overseer), ack-sla, reserve (full 15-verb set incl. yield/sweep/with-lease),
slot, contact (+captain), guard install/uninstall/check/record, agent
bootstrap/prune, channel (9 verbs), job (7), react, presence publish/list,
watch --once/--live, wait --corr, doctor --check, metrics, key, project,
relay, backend, bench, tui, workflow, receipt get/list/set, role pin/list/clear,
run-state replay/fork, next/ack/done/inbox — ALL resolve. MCP required-args
table verified against catalog schemas (send_message/fetch_inbox/ack/forms
exact; additives confirmed in `catalog::ADDITIVE_TOOLS`).
Flywheel (source + last-green binary): arrive/briefing/decide (leaf),
setup-mcp (top-level ✓), squad/workflow/loop-mode (run/supervise/status/doctor
+ --work/--resume/--force/--confirm), send (--stage/--param/--wait/--force/--raw/--to),
spawn (--profile/--join-channel/--retire-orphans), steer --direct, cron run
(25s dry-run, --live), captain watch, daemon start/stop/restart/status,
doorbell status/start, ledger check/receipt, principles list/show,
binding-rebind, profile/dep/config/status/coalition/mail — ALL resolve.
Removed doors confirmed gone in source: `flywheel toron`, recipes, template,
workflows, pipeline, bare loop, loop-status.

## Fixes (all in ~/.agents working tree, uncommitted by design)

- flywheel SKILL §5: "Slash templates regen" → "Slash prompts regen" (was
  tripping the VERIFY `template` pattern; no removed door was taught).
- flywheel SKILL §7: plane home `toron-core` → `toron-plane`
  (`toron_plane::backend::plane`) — wave7 fallout.
- flywheel GLOSSARY: hc 0.1.16 → 0.1.18; "hc is the only backend" → rex
  primary / hc fallback; path → `toron_plane::backend`.
- flywheel harness-integration: `hc_support.rs` → `src/hc.rs`,
  `hc_support::` → `hc::` (×2), toron-core backend/hc/osc7501 paths →
  toron-plane (×2).
- flywheel scripts/status-summary.sh: `loop-status` → `loop-mode status`
  (removed door; replacement verified in source).
- toron SKILL rule 4: `--since last` → `--since last` + `30s|15m|6h|7d`
  offsets (code accepts all three forms; MCP `since_ts` stays RFC3339 per rule 2).
- toron SKILL REQUIRED args: `reason` marked policy-required, not
  schema-required (`Option<String>` in code).
- toron SKILL Daemon: +freshness-triple sentence (config-strict landed).
- toron backend.md: 5 path retargets to toron-plane + `src/hc.rs` correction
  (hc_support.rs gone; mail.rs NOT deleted — half the old sentence was wrong).

## Non-fixes (checked, correctly taught as-is)

- `doorbell install`: the doorbell binary's door (verified `doorbell --help`),
  not `flywheel doorbell install` — skill wording already correct.
- `--wait`/`--confirm`: --wait is send's flag, --confirm is loop-mode run's —
  both resolve; claim stands.
- `flywheel watch` hc terminal status: kept per brief (wave3 verdict).
- 41-tool / 25-resource counts: match binary + catalog source exactly.

## Line counts / links

flywheel SKILL.md 180/180, toron SKILL.md 178/180 (all edits line-neutral
except the zero-newline freshness append). All SKILL.md links resolve.
MANDATORY sections untouched. No ~/.agents commit.

## Residual

- Flywheel HEAD tree red (wave5-a WIP): this audit used source truth for
  flywheel; re-run the binary half once that lane lands green.
- Full toron-cli suite has 1 pre-existing failure from the rex-default lane
  (stale hc-default assertion, filed wave7-strict); unrelated to skills.

PRINCIPLES: prove-it-works — every table row ran against a binary or a
dispatch site, VERIFY green at HEAD; laziness-protocol — stale teaching
deleted/retargeted, no warnings added, smallest same-line diffs;
sequence-verifiable-units — check (counts/patterns/links) before report.
