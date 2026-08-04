#!/usr/bin/env bash
# verify-0002.sh — compliance gate for ADR-0002 (Pluggable Backend Abstraction)
# governed-by: ADR-0002
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
grep -rq 'Box<dyn MultiplexerBackend>' src/main.rs src/durable.rs 2>/dev/null && ok "call sites use trait objects" || warn "call sites still concrete"
[[ -f .env.example ]] && ok ".env.example exists" || warn ".env.example missing"

echo
if [ "$fail" -eq 0 ]; then echo "RESULT: ADR-0002 PASSED"; exit 0; else echo "RESULT: ADR-0002 FAILED ($fail)"; exit 1; fi
