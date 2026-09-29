# Default harness exists (bd-ttaa, 2026-09-29)

The gap is closed: `DEFAULT_HARNESS = "pi"` lives in models.rs (line 148),
wired into profile-route resolution with a structural Open-tier gate, and
`test --bin flywheel -- resolve_profile_route` is green (2 passed). The
operator's choice from bd-47fu part 1 is the compiled-in default. Resolved
by 47fu's landing; no change in this commit beyond the record.
