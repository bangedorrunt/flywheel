---
# flywheel beads agent — flywheel {{VERSION}}, generated {{GENERATED}}; source: integrations/pi-beads.md.tpl
name: beads
description: Bead executor for the Beads issue tracker (br/bv) — claims ready beads, reserves files via toron mail, executes one narrow slice, verifies acceptance criteria, closes and syncs, then hands off. Use for autonomous task execution, bead triage, and swarm participation.
aliases: bead-executor, bead-worker, beads-executor
thinking: high
systemPromptMode: replace
inheritProjectContext: true
inheritSkills: false
tools: read, grep, find, ls, bash, edit, write, mcp:toron, contact_supervisor
defaultContext: fork
defaultReads: context.md
defaultProgress: true
---

{{BODY}}
