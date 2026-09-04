# ADR-0007 — Catalog slots + agent-native catalog CLI

| Field | Value |
|-------|-------|
| **Status** | Accepted (2026-09-05) |
| **Date** | 2026-09-05 |
| **Decision Maker** | Captain (bangedorrunt) |
| **Relates to** | flywheel ADR-0005 (command surface; TTY class of `steer_brief`), toron ADR-0010 Spec D (model registry), toron ADR-0019 Wave 7 (difficulty-tiered dispatch — catalog only, not auto-dispatch), toron ADR-0026 (do not overload `Profile.fallback`), toron ADR-0028 (four strategy nouns) |
| **Governs** | `src/models.rs` (tier + slots), `src/profile.rs` (slot inherit, `model_fallback`, compiled-in profiles/squads), `flywheel {slot,model,profile,squad,catalog}`, `src/catalog_tui.rs` |
| **Verification** | `docs/decisions/_scripts/verify-0007.sh` (see §6) |

---

## 1. TL;DR

The catalog nouns (`model` / `profile` / `squad`) already exist but cannot set the fields that actually route, cannot classify cost bands, and cannot show who an agent is. omp's role map is the right *shape*; flywheel does not import omp's 10 roles.

**Decision:** a **slot** is harness + model (+ fallbacks). Three slots: `cheap` / `standard` / `premium`. Profiles alias `@cheap` etc. or pin a concrete id. CLI is dual-output + TTY multi-select. `flywheel catalog` is a ratatui hub. Wave 7 auto-dispatch stays out.

This ADR **supersedes ADR-0005's "no interactive UI" non-goal only for `flywheel catalog`**. Doorbell is still not a TUI. `steer_brief` already established TTY-gated prompts.

## 2. Decisions

### D1 — Three slots, harness is a tier dimension

```toml
[slots.cheap]
harness = "opencode"
model = "opencode/big-pickle"

[slots.standard]
harness = "pi"
model = "opencode-go/deepseek-v4-flash"

[slots.premium]
harness = "omp"
model = "zai/glm-5.2"
fallback_harness = "grok"
```

- Aliases: `free`/`smol` → cheap; omp `default`/`slow`/`plan` → standard/premium/premium. Slot *name* is never `default` (that is the compiled-in **squad**).
- `Profile.model` may be `@cheap`/`@standard`/`@premium` or a concrete `provider/model`. Empty profile harness inherits the slot's harness.
- New field `Profile.model_fallback: Vec<String>`. **Never** reuse `Profile.fallback` (those are other profile names, billing hop).
- `ModelEntry.tier: cheap|standard|premium`. Discovery + recommendation.
- Resolve order: profile pin → slot (harness+model) → `model_fallback[]` → `fallback_harness` + a caps-compatible model → registry `ModelEntry.fallback` (caps+429).
- Grok fallback fails **loud** until a grok registry row exists. No silent unauthenticated hop.
- Implicit: `spawn --kind opencode` with no profile uses the cheap slot's model.
- Inverse map: opencode→cheap, pi→standard, grok→premium. omp is premium-primary.

`save()` never bakes compiled-in slots/models into the user file. User overlay wins per slot name (whole `SlotSpec`). `slot set` loads routing, applies flags, writes that full spec to the user file.

### D2 — Six job profiles; `pa`/`jcode` are harnesses

Compiled-in profiles: oracle, planner, reviewer, coder, scout, housekeeper.

| profile | slot | harness after inherit |
|---|---|---|
| coder | `@standard` | pi |
| scout | `@cheap` | opencode |
| reviewer, planner, oracle | `@premium` | omp (fb grok) |
| housekeeper | `@standard` | pi |

Fill omp `smol`/`slow`/`plan` with slot aliases **only** on omp-resolved profiles (reviewer/planner/oracle).

**Delete the compiled-in `pa` profile.** Keep `[harness.pa]` and `[harness.jcode]` recipes. Bare-harness spawn (`spawn --kind pa`, `--harness jcode`) stays when the operator names it. They are not catalog profiles, not hidden `--all` rows.

### D3 — Compiled-in squads

- `default` — 3 coder + 1 scout + 1 reviewer (the former live `adr19` shape). Do **not** compile in the name `adr19`.
- `plan` — planner=1, oracle=1 (premium)
- `review` — reviewer=1, oracle=1 (premium, different-family verifier)
- `recon` — scout=2 (cheap)

User `recipes.toml` still overrides per name. Hide 1-member derived profile rows unless `--all`. `squad create` writes `recipes.toml`.

### D4 — Agent-native catalog CLI + ratatui hub

Noun-verb: `slot` / `model` / `profile` / `squad` / `catalog`.

- TTY: human table. `--json` or stdout non-TTY: JSON.
- `--no-input` never prompts. `--force` skips confirm. `--dry-run` on destructive.
- `remove` takes N ids; TTY + no ids → dialoguer MultiSelect. Resolve bare model ids. Refuse compiled-in models unless `--shadow`.
- `profile set` is the missing verb (`--model`/`--provider`/`--slot`/`--model-fallback`/`--harness`/`--effort`/`--smol`/`--slow`/`--plan`/`--skills`).
- `flywheel catalog`: ratatui persistent multi-panel (slots | models | profiles/squads). hacker-night semantic slots in `output.rs`. Min 80×24. `NO_COLOR`/`TERM=dumb`/non-TTY refuses the TUI and names the CLI. Keys: `j/k g/G`, `Space` multi-select, `d` delete, `e` edit, `?` help, `q` quit, `Tab` focus.

Skill Sync Contract: Commands enum, `--robot-capabilities` (`catalog` object), AGENTS.md, flywheel skill §5.

### D5 — Out of scope

- Wave 7 auto-dispatch (bead difficulty → slot).
- Fifth strategy noun. slot is registry config, not a topology.
- Importing omp vision/tiny/commit/advisor/designer.
- New MCP / kinds / second doorbell.
- Compatibility shims.

## 3. Implementation Plan

* **Affected paths**: `src/models.rs`, `src/profile.rs`, `src/main.rs` (cmd enums + handlers + `profile_launch_args`), `src/catalog_tui.rs` (new), `src/steer_brief.rs`, `src/output.rs`, `src/config.rs` (squad create persistence), tests in those files, `docs/decisions/README.md`, skill sync files.
* **Dependencies**: `dialoguer`, `ratatui`, `crossterm`.
* **Patterns to follow**: `ModelRegistry::load()` vs `load_routing()`; field-level profile layering; `print_json` + TTY tables in `output.rs`; TTY skip matrix of `briefing_enabled`.
* **Patterns to avoid**: overloading `Profile.fallback`; baking compiled-ins into user `models.toml`; treating `pa`/`jcode` as profiles; naming a slot `default`.
* **Migration**: wipe live `ox-alpha-free` pins in `~/.config/flywheel/profiles.toml`. User `[adr19]` may remain as an alias.

### Verification

- [ ] `cargo test models` — slot resolve `@cheap`→opencode+big-pickle; `@standard`→pi+deepseek-v4-flash; user slot overlay; save does not bake compiled-in slots; tiers on compiled-in models
- [ ] `cargo test profile` — 6 built-ins, no `pa` profile, harness recipes still present; coder/housekeeper inherit standard; omp smol/slow/plan filled on oracle/planner/reviewer; `model_fallback` ≠ `fallback`
- [ ] compiled-in squads `default`/`plan`/`review`/`recon`
- [ ] `model remove` N ids, bare→composite, refuse compiled-in
- [ ] `NO_COLOR=1 flywheel catalog` refuses TUI and names the CLI
- [ ] `flywheel profile show coder --json` has slot/harness/model (harness=pi)
- [ ] skill sync: Commands, robot_capabilities.catalog, AGENTS.md, SKILL.md §5

## 4. Consequences

* Good, because one slot edit moves every aliased profile; harness and model travel together.
* Good, because agents get JSON + structured capabilities; humans get tables + a hub.
* Bad, because coder leaves jcode; operators who want jcode must `--harness jcode` explicitly.
* Bad, because a ratatui binary dep is new.
* Neutral, because live `adr19` in user recipes.toml is not deleted.

## 5. Alternatives considered

* **Model-only slots** — rejected; operator wants harness as a tier dimension (pi=standard, opencode=cheap, grok=premium fallback).
* **Import omp's 10 roles** — rejected; flywheel has no vision/advisor/designer features.
* **Keep `pa` as a hidden profile** — rejected; `pa`/`jcode` are bare harnesses.
* **dialoguer-only, no ratatui** — rejected; captain asked for the hub in this wave.

## 6. More Information

`docs/decisions/_scripts/verify-0007.sh` is the executable gate (14/14 green 2026-09-05). Revisit Wave 7 auto-dispatch only if P0–P1 model spend or quality justifies it (toron ADR-0019).

Landed surface: `SlotSpec` + `Tier` + `slot_alias` in `src/models.rs`; slot inherit in `resolve_profile_route` (profile pin → slot → model_fallback → fallback_harness → registry fallback); `Profile.model_fallback`/`Profile.when`; 6 compiled-in profiles (no `pa`) + 4 compiled-in squads; CLI verbs `slot {list,show,set,remove}`, `profile set`, `model add --tier`, `model remove <ids…>`, `squad create`, `catalog [--json]` + the `src/catalog_tui.rs` ratatui hub (hacker-night slots only, dialoguer multiselect on `model remove`). Live `ox-alpha-free`/jcode pins migrated out of `~/.config/flywheel/profiles.toml` (backup: `profiles.toml.bak-adr0007`).
