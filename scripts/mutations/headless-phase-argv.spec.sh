# shellcheck shell=bash
# Sourced by scripts/verify-mutations.sh.
# shellcheck disable=SC2034

# An invariant held by another table's contents (bd-vbrj).
#
# `build_argv_roles_phase` inserts phase-registry args "before the trailing
# prompt positional", on a stated invariant: *every harness arm ends its argv
# with the prompt*. That is false for grok. The grok arm returns early with the
# prompt embedded in `--single=<prompt>`, so its trailing element is
# `--output-format`'s VALUE:
#
#     ["grok", "--single=PROMPT", "--output-format", "streaming-messages-json"]
#
# Splicing at `len - 1` there lands between the flag and its value, and grok
# reads the phase flag as the output format. The `saturating_sub` that hid this
# made the wrong answer look safe — it clamped instead of revealing that the
# assumed shape was wrong.
#
# Latent only because `phase_launch_args` has no grok arm, so the splice never
# runs for grok. An invariant held by the emptiness of another match table is
# not an invariant; adding a grok phase mechanic would have landed here with no
# test able to see it.
#
# `phase_argv_insert_at` is pure for the same reason `dir_holds_live_store` is
# (bd-gpaa): both shapes must be drivable without the thing that would have
# triggered the bug.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(headless::tests::grok_argv_does_not_end_with_the_prompt headless::tests::phase_args_never_split_a_flag_from_its_value)

# H1 — back to the old assumption, with the clamp intact. The grok case in the
# test is the one that goes red; opencode would still pass, which is why the
# old code looked right.
mutation "H1  the insertion assumes a trailing prompt positional" \
    file=src/headless.rs \
    expect=phase_args_never_split_a_flag_from_its_value \
    find='    match argv.last() {
        Some(last) if last == prompt => argv.len() - 1,
        _ => argv.len(),
    }' \
    replace='    let _ = prompt;
    argv.len().saturating_sub(1)'

# H2 — grok stops embedding the prompt in --single= and appends it instead. That
# is a plausible future change to grok's arm, and it is EXACTLY what would make
# the shape assertion wrong. If this mutation does not go red the shape test is
# asserting a literal rather than the builder's behaviour.
mutation "H2  grok appends a positional prompt like every other arm" \
    file=src/headless.rs \
    expect=grok_argv_does_not_end_with_the_prompt \
    find='            return Ok(argv);
        }
        other => {' \
    replace='            argv.push(prompt.to_string());
        }
        other => {'

# H3 — the contrast half of the shape test. pi and opencode DO end with the
# prompt; if that loop stops being load-bearing the first half of the test
# (the grok assertion) would be the only thing standing, and the failure mode
# it guards is an arm silently changing shape.
mutation "H3  no harness ends with the prompt positional any more" \
    file=src/headless.rs \
    expect=grok_argv_does_not_end_with_the_prompt \
    find='    argv.push(prompt.to_string());
    Ok(argv)
}' \
    replace='    argv.insert(0, prompt.to_string());
    Ok(argv)
}'