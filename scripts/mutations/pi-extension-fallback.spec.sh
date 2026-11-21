# Sourced by scripts/verify-mutations.sh.
# bd-zc7b4: a missing optional pi extension must NOT fail the spawn; the
# doorbell is the fallback. Restoring the bail makes the fallback test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(a_missing_pi_extension_returns_ok_without_a_package_so_the_doorbell_falls_back)

HC=src/hc.rs

mutation "F1  a missing pi extension bails instead of falling back" \
    file=$HC \
    expect=a_missing_pi_extension_returns_ok_without_a_package_so_the_doorbell_falls_back \
    find='        return Ok(());
    };
    let sanitize' \
    replace='        anyhow::bail!("pi events extension missing");
    };
    let sanitize'
