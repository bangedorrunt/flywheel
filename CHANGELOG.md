# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### 🚀 Features

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
