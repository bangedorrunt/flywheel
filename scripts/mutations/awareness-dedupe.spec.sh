#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-i4fqc: one bead per CONDITION. A repeat observation comments on the open
# bead; it never files a second one, and the condition key ignores the noise a
# digest line adds.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(awareness)

AWR=src/awareness.rs

mutation "A1  every escalation files a new bead again" \
    file=$AWR \
    expect=emitting_the_same_condition_twice_updates_one_open_bead \
    find='    if let Ok(raw) = crate::br::run_br(None, &["list", "--json", "--status", "open"])' \
    replace='    if let Ok(raw) = crate::br::run_br(None, &["list", "--json", "--status", "open"]).map(|_| String::new())'

mutation "A2  the condition key drops the subject" \
    file=$AWR \
    expect=a_condition_key_ignores_count_and_case_noise \
    find='    format!("{}|{}", normalize(kind), normalize(detail))' \
    replace='    let _ = detail;
    normalize(kind)'

mutation "A3  the condition key keeps a drifting count" \
    file=$AWR \
    expect=a_condition_key_ignores_count_and_case_noise \
    find='            .map(|c| if c.is_ascii_digit() { '"'"'#'"'"' } else { c })' \
    replace='            .map(|c| c)'

mutation "A4  the condition key drops the kind" \
    file=$AWR \
    expect=a_condition_key_ignores_count_and_case_noise \
    find='    format!("{}|{}", normalize(kind), normalize(detail))' \
    replace='    let _ = kind;
    normalize(detail)'

mutation "A5  a repeat observation does not comment on the survivor" \
    file=$AWR \
    expect=emitting_the_same_condition_twice_updates_one_open_bead \
    find='        if let Err(e) = crate::br::run_br(None, &["comments", "add", &id, "--message", &note]) {
            eprintln!("cron {NAME}: could not update bead {id} ({e})");
        }' \
    replace='        let _ = &note;'
