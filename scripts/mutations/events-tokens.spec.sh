# Sourced by scripts/verify-mutations.sh.
# bd-5vi2x: the token lifecycle must keep the secret out of config, journal and
# argv, and rotate before expiry.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    minted_token_is_0600_and_journal_has_no_plaintext
    rotation_triggers_before_expiry_not_after
    token_file_is_reread_after_rotation
)

mutation "T1  the token file is world-readable" \
    file=src/events_tokens.rs \
    expect=minted_token_is_0600_and_journal_has_no_plaintext \
    find='            .mode(0o600)' \
    replace='            .mode(0o644)'

mutation "T2  the plaintext leaks into the mint journal" \
    file=src/events_tokens.rs \
    expect=minted_token_is_0600_and_journal_has_no_plaintext \
    find='    .to_json_line(now_us);
    journal(journal_path, &done, now_us)?;

    Ok(TokenState {' \
    replace='    .to_json_line(now_us);
    journal(journal_path, &format!("{done} {plaintext}"), now_us)?;

    Ok(TokenState {'

mutation "T3  rotation waits until expiry" \
    file=src/events_tokens.rs \
    expect=rotation_triggers_before_expiry_not_after \
    find='        now_us >= self.expires_us.saturating_sub(before_us)' \
    replace='        now_us >= self.expires_us'
