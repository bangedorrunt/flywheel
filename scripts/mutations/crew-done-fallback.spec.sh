# Sourced by scripts/verify-mutations.sh.
# bd-ezlme: a done reply whose pane wake cannot resolve the captain (a mail-only
# identity, no agent row) falls back to plane-P mail. Drop the fallback and the
# brief closes while the captain hears nothing - the lost hand-off the verb
# exists to prevent.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_failed_pane_wake_falls_back_to_the_mail_path)

mutation "F1  a failed pane wake does not fall back to mail" \
    file=src/cmd/crew.rs \
    expect=a_failed_pane_wake_falls_back_to_the_mail_path \
    find='        Err(pane_error) => match mail() {
            Ok(line) => Ok((line, None)),
            Err(mail_error) => bail!(
                "the brief is closed but the captain heard nothing: pane wake failed ({pane_error}); \
                 mail fallback failed ({mail_error})"
            ),
        },' \
    replace='        Err(pane_error) => Err(anyhow::anyhow!("{pane_error}")),'
