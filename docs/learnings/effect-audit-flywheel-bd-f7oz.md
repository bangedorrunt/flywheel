# Effect audit: flywheel (bd-f7oz, evaluation only, no code changed)

Crew A (PearlTern) audit through the effect-oriented skill. Full findings:
toron channel test-channel, 9 chunks. Strengths: Tick Law purity, journal
honesty (Enforced/Recording, CAS, terminal immobility), n86n
prepare/run_post/finish split, hermetic tests. Ranked gaps: G1 no
backoff/jitter in retry loops (thundering herd on outage); G2 no
breaker/rate-limit/hedge on remote paths, 429 unnamed; G3 pack re-parse
per reclassify, no cache; G4 env/process ambient authority at call sites;
G5 panic!/unreachable! in catalog paths (typed-error territory); G6 fixed
sleeps as sync (hope, not probe); G7 suite weight (~220s, no per-test
bounds). Proposals P1–P7 with sketches; suggested order P4 → P1 → P2 →
P3 → P5 → P6 → P7. Verification: tree clean, judgment 49/0 green.
Close vehicle: this doc is the deliverable diff citing the bead.
