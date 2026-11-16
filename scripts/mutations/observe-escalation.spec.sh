# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.2 (2a): the observe escalation carrier. Every line must end with its
# lesson (ADR-0009), and one episode must ring once.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(observe::)

mutation "L1  an escalation line drops its lesson" \
    file=crates/doorbell-core/src/observe.rs \
    expect=every_escalation_line_carries_its_lesson \
    find='            "[observe] {} {}: {} · lesson: {}",
            self.kind.as_str(),
            self.subject,
            self.evidence.trim(),
            self.lesson.trim()' \
    replace='            "[observe] {} {}: {}",
            self.kind.as_str(),
            self.subject,
            self.evidence.trim()'

mutation "E1  the same episode rings on every sweep" \
    file=crates/doorbell-core/src/observe.rs \
    expect=a_blocked_bead_rings_once_per_episode \
    find='        self.live.retain(|k| current.contains(k));' \
    replace='        self.live.clear();
        let _ = current;'

mutation "B1  a release wait never expires" \
    file=crates/doorbell-core/src/observe.rs \
    expect=a_release_wait_escalates_once_after_one_lease_period \
    find='            .filter(|w| now.saturating_sub(w.asked_at) >= RELEASE_WAIT_SECS)' \
    replace='            .filter(|_w| false)'

mutation "F1  any subject with a colon reads as a fork" \
    file=crates/doorbell-core/src/observe.rs \
    expect=only_a_fork_subject_is_a_fork \
    find='    let rest = subject.trim().strip_prefix("[fork]")?.trim_start();' \
    replace='    let rest = subject.trim();'
