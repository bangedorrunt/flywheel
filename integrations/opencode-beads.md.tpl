---
# flywheel beads agent — flywheel {{VERSION}}, generated {{GENERATED}}; source: integrations/opencode-beads.md.tpl
name: beads
description: Bead executor and swarm coordinator. Picks ready beads, claims them, reserves files, announces via Agent Mail, executes in narrow slices, runs quality gates, closes beads, syncs and commits, then hands off. Operates on the Beads issue tracker (br/bv) for the current project.
mode: primary
temperature: 0.1
tools:
  mcp-agent-mail_*: true
  read: true
  write: true
  edit: true
  bash: true
  grep: true
  glob: true
  task: true
---

{{BODY}}
