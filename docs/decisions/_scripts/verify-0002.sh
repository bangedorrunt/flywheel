#!/usr/bin/env bash
# verify-0002.sh — compliance gate for ADR-0002 (Pluggable Backend Abstraction)
# governed-by: ADR-0002
# review-fix F6 (bd-review-fix-epic-m0b.6): the old check grepped for the
# literal string 'Box<dyn MultiplexerBackend>' which matches NOTHING (call
# sites use backend::multiplexer_backend(&cfg)). This version checks REAL
# behavior: the factory returns trait objects, the durable-loop leaves route
# through the factory (no concrete Client::connect in durable.rs — leaves
# shell to the binary which constructs via the factory), and the concrete
# main.rs sites are bounded (interactive CLI commands are a documented
# deferral, per ADR-0002 §3.1 amendment).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pass=0; fail=0
ok(){ echo "  [OK]   $1"; }; bad(){ echo "  [FAIL] $1"; fail=$((fail+1)); }; warn(){ echo "  [WARN] $1"; }

echo "== ADR-0002 verification =="
[[ -f src/backend/mod.rs ]] && ok "src/backend/mod.rs exists" || bad "src/backend/mod.rs missing"
grep -rq 'trait MultiplexerBackend' src/backend/ 2>/dev/null && ok "MultiplexerBackend trait defined" || bad "MultiplexerBackend trait missing"
grep -rq 'trait MailBackend' src/backend/ 2>/dev/null && ok "MailBackend trait defined" || bad "MailBackend trait missing"
grep -rq 'FLYWHEEL_BACKEND' src/ 2>/dev/null && ok "FLYWHEEL_BACKEND env wired" || warn "FLYWHEEL_BACKEND not yet wired"
grep -rq 'FLYWHEEL_MAIL' src/ 2>/dev/null && ok "FLYWHEEL_MAIL env wired" || warn "FLYWHEEL_MAIL not yet wired"
grep -q 'backend-herdr' Cargo.toml 2>/dev/null && ok "cargo feature backend-herdr present" || warn "backend-herdr feature missing"
grep -q 'backend-superlogical' Cargo.toml 2>/dev/null && ok "cargo feature backend-superlogical present (stub)" || warn "backend-superlogical feature missing"

# (a) factory exists and returns trait objects (the REAL contract)
grep -rq -e '-> Result<Box<dyn MultiplexerBackend>>' src/backend/factory.rs 2>/dev/null \
  && ok "factory returns Box<dyn MultiplexerBackend>" \
  || bad "factory does not return Box<dyn MultiplexerBackend>"
grep -rq -e '-> Result<Box<dyn MailBackend>>' src/backend/factory.rs 2>/dev/null \
  && ok "factory returns Box<dyn MailBackend>" \
  || bad "factory does not return Box<dyn MailBackend>"

# (b) the durable loop is backend-agnostic: durable.rs (the Resonate workflow)
# must never construct a concrete herdr client (it shells to the binary which
# uses the factory). A regression here breaks the ADR's core promise.
if grep -n 'Client::connect' src/durable.rs | grep -v '^[0-9]*:#' >/dev/null 2>&1; then
  bad "concrete Client::connect in src/durable.rs — loop is NOT backend-agnostic"
else
  ok "no concrete Client::connect in src/durable.rs"
fi

# (c) the __send leaf (the load-bearing dispatch path) uses the trait's coded
# prompt — proves the hard migration landed.
grep -q 'agent_prompt_coded' src/backend/mod.rs 2>/dev/null \
  && ok "trait exposes agent_prompt_coded (landing semantics)" \
  || bad "trait missing agent_prompt_coded"
grep -q 'backend.agent_prompt_coded' src/main.rs 2>/dev/null \
  && ok "__send leaf routes via factory agent_prompt_coded" \
  || bad "__send leaf does not use factory agent_prompt_coded"

# (d) remaining concrete sites in main.rs are the documented CLI deferral:
# interactive commands (spawn/send/watch/view/…) may stay concrete because the
# trait surface is operation-shaped and does not cover the full CLI API. Bound
# the count so a NEW concrete site in a durable-leaf path is caught by review.
CONCRETE=$(grep -c 'Client::connect' src/main.rs || true)
if [ "$CONCRETE" -le 30 ]; then
  ok "main.rs concrete Client::connect sites = $CONCRETE (CLI deferral, bound 30)"
else
  bad "main.rs concrete Client::connect sites = $CONCRETE exceeds bound 30 — migrate or document"
fi

[[ -f .env.example ]] && ok ".env.example exists" || warn ".env.example missing"

echo
if [ "$fail" -eq 0 ]; then echo "RESULT: ADR-0002 PASSED"; exit 0; else echo "RESULT: ADR-0002 FAILED ($fail)"; exit 1; fi
