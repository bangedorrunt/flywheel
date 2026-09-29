# Tool-relay degradation findings (bd-bqr0, 2026-09-29)

## Defect (severe): caveman_retrieve returns wrong-session content

Handle `ccr://ccr_obj_e689b140a97c9dff47d1b23c755db612`, issued in-session
for local test-log content, deterministically resolves to another session's
working notes (toron wake-bus replay TDD planning). Reproduced 3x, including
once with a nonsense query — retrieval ignores the query, so the
handle-to-object binding itself is wrong (shared namespace, collision, or
display corruption; needs store-side inspection).

Silent: no warning, no checksum. Caught only because the content was
obviously alien; a same-domain wrong object would be UNDETECTABLE. Treat
retrieve results as untrusted (verify against direct reads) until fixed.

## Working notes (not defects, undocumented)

- Size offload: results under ~150 chars usually inline, larger ones arrive
  as handles. Constant all session, not degrading. Workaround that held all
  session: redirect to /tmp, extract with tiny greps.
- Secret-scanner redaction (`redacted:sha256:...`) hit once on test output
  containing key material. Third mechanism alongside inline/offload.

## Recommended operator actions

1. Tenant-isolate handles; checksum mismatch must fail loud, never return
   wrong bytes. 2. Document the offload threshold and redaction trigger.
