# Vocabulary — one word for one thing

Five nouns, not interchangeable. Each row points at its source of truth (SoT).

| Noun | One line | SoT |
|------|----------|-----|
| **stage** | One named prompt (`send --stage ce-execute`). | `flywheel stage list` |
| **workflow** | An engine procedure (YAML steps or graph); a step *may* dispatch a stage. | toron-workflow engine; `flywheel workflow list` |
| **compose token** | A bare `--parts` name that expands to a compiled-in workflow's steps (`wave-swarm`, `repair-until-verified`, `beads-lifecycle`). | `workflow compose --parts <token> --run` |
| **profile** | A role template: harness + model + behavior + skills (`spawn --profile coder`). | `flywheel profile list`; spawn registry |
| **topology** | How work is supervised: **open \| loop \| swarm**. | [Operating topologies](agent-instructions/orchestration.md#operating-topologies) |

"Loop workflow" is forbidden speech: loop is a topology, never a workflow name.

`squad` (a group of profiles, i.e. a spawn shape) and `recipe` (legacy squad
surface) are not nouns in this table — see the command reference for those.
