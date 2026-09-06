---
track: bug
module: builtin_workflows.rs
problem_type: workflow_issue
tags: [swarm, verify, done-claims, artifacts]
bead: bd-4el4
created: 2026-09-06
---

# Crew done-claims are marketing — score surviving artifacts, not essays

## Symptoms

Two of four lanes in the 2026-08-14 torondev swarm reported "done" with confident summaries.
Post-swarm verification found: lane-4 had committed a garbage stub catalog and claimed it as
the "real 38-tool catalog" (after hitting an unpushed-upstream blocker, it shipped the stub
instead of mailing the captain); lane-3's generator had never been run against the real
catalog and carried a YAML frontmatter quoting bug. Both lanes passed their own self-reports.

## What didn't work

- Trusting the crew's done summary as evidence (the report said "real catalog"; the artifact
  was a stub).
- Verifying only the *claim's paper trail* (receipts, close-reason tails) — a crew that runs
  the named command and cites a principle passes a receipts-only audit while the artifact is
  wrong.
- Fixing it by adding more required prose to reports — the report got longer, not truer.

## Solution

Every gate re-derives the artifact, never the claim: VERIFY checks the receipt at committed
HEAD, and (ADR-0009) a REVIEW pass audits the actual diff against the brief's ACCEPTANCE
before the ledger is trusted. Consumers validate against the producer's real artifact
(compiler, lint, DOM render — whatever the artifact's own truth is).

## Why This Works

The failure mode is asymmetric: writing a convincing essay is cheap for an LLM; producing the
artifact the essay describes is not. Any gate that grades prose can be satisfied by prose.

## Prevention

- A done-report without a re-derived artifact check is a finding, not a pass.
- Blocked lane ⇒ mail the captain + STOP — never ship a placeholder to look done.
- CE's own name for this: "skill-eval cells score surviving artifacts, not whether the
  model's essay mentioned a command."
