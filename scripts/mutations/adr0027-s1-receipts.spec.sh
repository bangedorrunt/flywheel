# shellcheck shell=bash
# Sourced by scripts/verify-mutations.sh.
# shellcheck disable=SC2034

# A test that returns early on a setup failure (bd-rvma).
#
# `adr0027_s1_tasked_mail_corr_survives_walker_death` drove a real `toron`
# subprocess and reported ok on the default suite having asserted nothing:
#
#     if stderr.contains("unknown recipient") || "no local key" || "no operator key" {
#         eprintln!("warn: S1 CLI send skipped …");
#         return;
#     }
#
# Measured: that branch fired every run ("unknown recipient: adr27-recip"), so
# the receipt-contract half — corr, wake, state, result_path — never executed.
# What sat underneath it was exit-code tautology: `assert!(out.status.success())`
# on a `receipt get` that was also passing a `--project` flag the verb does not
# accept.
#
# A skip is not a weaker assertion; it is a test whose green does not mean its
# name. So every value assertion in the repaired test is mutated below to its
# opposite. RED each time is the only evidence any of them is load-bearing —
# and a RED that needs a mutation to appear is a test that would have passed
# either way, which is exactly the bug being fixed.

CARGO_ARGS=(--test adr0027_slice1 -- --ignored adr0027_s1_tasked)
TEST_FILTER=(adr0027_s1_tasked_mail_corr_survives_walker_death)

# R1 — the bd-2u27 claim itself. `--wake` must ride SendMessage.extra_tags into
# the durable receipt row; a leak onto the 1059 envelope would not appear here.
# If this does not go red, the repaired test still is not checking extra_tags.
mutation "R1  the receipt expects the wrong wake class" \
    file=tests/adr0027_slice1.rs \
    expect=adr0027_s1_tasked_mail_corr_survives_walker_death \
    find='        v["wake"].as_str(),
        Some("tasked"),' \
    replace='        v["wake"].as_str(),
        Some("standing"),'

# R2 — the send opens its receipt owed. Anchored through the wake assert because
# a comment sits between the corr and state asserts, and through the message
# because the second state assert (R3's) reads the same `Some("owed")` literal.
mutation "R2  the send opens its receipt acked, not owed" \
    file=tests/adr0027_slice1.rs \
    expect=adr0027_s1_tasked_mail_corr_survives_walker_death \
    find='        "--wake tasked must ride extra_tags into the receipt row, got {v}"
    );
    assert_eq!(
        v["state"].as_str(),
        Some("owed"),' \
    replace='        "--wake tasked must ride extra_tags into the receipt row, got {v}"
    );
    assert_eq!(
        v["state"].as_str(),
        Some("acked"),'

# R3 — the walker-death half: a second, fresh process over the same store must
# still read `owed` back. This is the assertion that distinguishes a durable
# receipts TABLE from a receipt_cache HashMap, so it is the one a regression to
# in-memory state would hide. Anchored on its own message.
mutation "R3  the fresh process reads back acked instead of owed" \
    file=tests/adr0027_slice1.rs \
    expect=adr0027_s1_tasked_mail_corr_survives_walker_death \
    find='        v2["state"].as_str(),
        Some("owed"),' \
    replace='        v2["state"].as_str(),
        Some("acked"),'

# R4 — `--result-path` must ride extra_tags too, and the observable is the
# receipt's own field (measured: the send does NOT create the file). This is the
# assertion the old comment promised and never made.
mutation "R4  the receipt records no result_path" \
    file=tests/adr0027_slice1.rs \
    expect=adr0027_s1_tasked_mail_corr_survives_walker_death \
    find='        v3["result_path"].as_str(),
        result_path.to_str(),' \
    replace='        v3["result_path"].as_str(),
        None,'

# R5 — corr itself. Every value assert above is indexed off a `receipt get` that
# returned SOME row; if `corr` were unchecked, a row belonging to a different
# corr would satisfy R1–R4.
mutation "R5  the receipt is keyed on the wrong corr" \
    file=tests/adr0027_slice1.rs \
    expect=adr0027_s1_tasked_mail_corr_survives_walker_death \
    find='        v["corr"].as_str(),
        Some(corr),' \
    replace='        v["corr"].as_str(),
        Some("FM-GS-999"),'

# NOT PROVABLE HERE, and named rather than padded into the list: `receipt()`s
# `assert!(out.status.success())` only fires when the toron CLI breaks, which a
# passing run cannot arrange. Two mutations confirmed that — dropping the
# helper's `--json` flag (VACUOUS: the verb already answers in JSON, so the flag
# was inert and the test no longer carries it) and neutering the status assert
# into `true || out.status.success()` (VACUOUS: the read succeeds, so the assert
# is never reached and the JSON parse below still succeeds). The second is also
# the honest shape of the guard: the parse-panic IS the loud path a failed read
# takes, and it now carries stderr and the exit code. Proving that guard needs a
# broken binary, so it is stated here instead of claimed as evidence.

# R6 — the pins are registered into the project the send names. Without this the
# send reports an unknown recipient, which is the exact failure the old skip
# turned into a green test, so this is the setup half of the same proof.
mutation "R6  the pins are bootstrapped into a project the send does not name" \
    file=tests/adr0027_slice1.rs \
    expect=adr0027_s1_tasked_mail_corr_survives_walker_death \
    find='            &["agent", "bootstrap", "--project", proj, "--as", pin],' \
    replace='            &["agent", "bootstrap", "--project", "adr27-elsewhere", "--as", pin],'