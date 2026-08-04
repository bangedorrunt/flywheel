---
# flywheel beads agent — flywheel {{VERSION}}, generated {{GENERATED}}; source: integrations/grok-beads.md.tpl
name: beads
description: >
  Bead executor for the Beads issue tracker (br/bv). Picks ready beads, claims them,
  reserves files via Agent Mail, executes one narrow slice per bead, verifies acceptance
  criteria, closes and syncs the bead, then hands off. Uses br/bv CLIs and herdr for
  multi-agent coordination. Use for autonomous task execution, bead triage,
  plan-to-beads conversion, and swarm participation.
prompt_mode: full
model: inherit
permission_mode: default
agents_md: true
---

{{BODY}}
