#!/usr/bin/env bash
# ADR-0009 verification: compound spine (ce- vocabulary, 8-step wave-swarm,
# learnings corpus). Pure-check portion runs offline against the source; the
# test portions shell out to cargo.
set -euo pipefail
cd "$(dirname "$0")/../../.."
export PATH="$HOME/.cargo/bin:$PATH"
unset RUSTUP_TOOLCHAIN
FAIL=0
ck() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; FAIL=1; fi; }

ck "p-code seed aliases gone"             "! rg -q '^                \"p[0-9]\",$' src/stages.rs"
ck "compound is phase vocabulary"           "rg -q '\"compound\"' src/stages.rs"
ck "wave-dispatch briefs ce-execute"        "rg -q -- '--stage ce-execute' src/stages.rs"
ck "wave gate carries grounding read"       "rg -q 'GROUNDING READ' src/stages.rs"
ck "wave verify carries residual gate"      "rg -q 'RESIDUAL GATE' src/stages.rs"
ck "wave-swarm token is 8 steps"            "test \"\$(rg -c '  - name: (gate|dispatch|execute|observe|review|verify|compound|signoff)\$' src/builtin_workflows.rs)\" -ge 8"
ck "ce-compound workflow exists"            "rg -q '\"ce-compound\"' src/builtin_workflows.rs"
ck "ce-sweep workflow exists"               "rg -q '\"ce-sweep\"' src/builtin_workflows.rs"
ck "learnings corpus seeded"                "test -f docs/learnings/README.md && test -f docs/learnings/orchestration/crew-done-claims-are-marketing.md"
ck "toron phase enum carries compound"      "rg -q 'Compound' ../toron/crates/toron-workflow/src/schema.rs"
ck "robot capabilities name ce- stages"     "rg -q '\"ce-compound\"' src/main.rs"

if command -v cargo >/dev/null 2>&1; then
  if cargo test --bin flywheel stage_p_codes_retired_ce_names_resolve 2>/dev/null | rg -q 'test result: ok'; then
    echo "ok   alias retirement test"
  else
    echo "FAIL alias retirement test"; FAIL=1
  fi
  if cargo test --bin flywheel wave_swarm_compose_token_expands_eight 2>/dev/null | rg -q 'test result: ok'; then
    echo "ok   8-step expansion test"
  else
    echo "FAIL 8-step expansion test"; FAIL=1
  fi
else
  echo "warn cargo absent — static checks only"
fi

exit "$FAIL"
