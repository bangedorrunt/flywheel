# Tripwire healed under mainline (bd-xa1k, 2026-09-29)

`gs039_cos_honest_folds_present` was red at HEAD: it asserts the
`refuse silent drop (GS-039)` warn string exists in the committed source,
and the string was absent. No code fix was needed — bd-fkfi (cb0d925)
re-scoped the GS-039 wake-save tripwire to `flush_wake` and the asserted
string is present again. Verified green 3x at HEAD.

Lesson, kept small: source-scan tripwires go red AND green silently under
unrelated refactors. The bead naming the exact asserted string is what made
attribution a two-command job — keep that convention. No production change.
