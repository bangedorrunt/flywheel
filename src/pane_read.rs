//! The pane read port (bd-0uzrq.3): one way to look at what a pane is showing.
//!
//! The wake path needs the input line before it types. The read lives behind
//! this trait so the guard is testable with a fake and so hc and rex answer the
//! same question — the same shape as `pane_write::PaneTyper` on the write side.

use crate::pane_screen::{InputLine, input_line_for, input_line_for_unknown_harness};

/// Read a pane's current screen tail.
pub trait PaneReader {
    fn tail(&self, pane_id: &str, lines: u32) -> Result<String, String>;
}

/// The production reader: whichever multiplexer the fleet is on.
pub struct PlaneReader<'a>(pub &'a dyn crate::backend::MultiplexerBackend);

impl PaneReader for PlaneReader<'_> {
    fn tail(&self, pane_id: &str, lines: u32) -> Result<String, String> {
        let value = self
            .0
            .agent_read(pane_id, "text", lines)
            .map_err(|e| format!("pane read {pane_id}: {e}"))?;
        value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("pane read {pane_id}: no text in the reply"))
    }
}

/// hc answers with its own client call (its `Client` is not a `Backend`).
pub struct HcReader<'a>(pub &'a crate::hc::Client);

impl PaneReader for HcReader<'_> {
    fn tail(&self, pane_id: &str, lines: u32) -> Result<String, String> {
        self.0
            .agent_read(pane_id, "text", Some(lines))
            .map_err(|e| format!("pane read {pane_id}: {e}"))
    }
}

/// What the wake path may do with this pane, right now.
///
/// A pane we cannot read is a DEFER, not a type: the box may be holding the
/// operator's half-typed line, and an unreadable pane is exactly the case where
/// guessing corrupts their input.
pub fn gate(
    reader: &dyn PaneReader,
    harness: &str,
    pane_id: &str,
    last_typed: Option<&str>,
) -> Result<InputLine, String> {
    let tail = reader.tail(pane_id, 0)?;
    // bd-e4gal: a profileless spawn's registry row has no harness, so the
    // guard gets "" and must not read that as "safe to inject". Classify the
    // box structurally instead — an unrecognised harness is a DEFER, not a
    // type, exactly like an unreadable pane.
    let line = if harness.trim().is_empty() {
        input_line_for_unknown_harness(&tail, last_typed)
    } else {
        input_line_for(harness, &tail, last_typed)
    };
    Ok(line)
}

/// The harness a registered pane runs, or empty (unknown) when the pane has no
/// registry row.
pub fn harness_of(project: &str, pane_id: &str) -> String {
    crate::state::load_spawn_registry(project)
        .into_iter()
        .find(|e| e.pane_id == pane_id)
        .and_then(|e| e.harness)
        .unwrap_or_default()
}

/// The pin a pane serves, for the deferral message.
fn who(project: &str, pane_id: &str) -> String {
    crate::state::load_spawn_registry(project)
        .into_iter()
        .find(|e| e.pane_id == pane_id)
        .and_then(|e| e.mail_identity)
        .unwrap_or_else(|| pane_id.to_string())
}

/// The one refusal every door that types into a shared pane asks for
/// (bd-0uzrq.3). `Err` is the typed deferral the caller returns: the text is
/// NOT written, and mail stays queued for the next idle edge.
///
/// Urgency is deliberately absent: `--urgent` does not override the operator's
/// half-typed line.
pub fn defer_if_operator_typing(
    reader: &dyn PaneReader,
    harness: &str,
    pane_id: &str,
    project: &str,
) -> Result<(), String> {
    let pin = who(project, pane_id);
    let input = match gate(reader, harness, pane_id, None) {
        Ok(input) => input,
        // A door the operator asked for by hand keeps working on a plane that
        // cannot read panes: the status gate still refuses a busy pane, and the
        // send itself fails loud if the target is wrong. Background doors use
        // the strict form below.
        Err(_) => return Ok(()),
    };
    if crate::pane_screen::may_inject(input) {
        return Ok(());
    }
    if crate::pane_screen::wake_queued(input) {
        return Err(format!(
            "deferred: a wake line is already queued for {pin}; nothing was typed, and the mail stays queued"
        ));
    }
    Err(format!(
        "deferred: operator is typing in {pin}; nothing was typed, and the mail stays queued for the next idle edge"
    ))
}

/// The background form (doorbell ring, mail wake): these will be retried, so a
/// pane we cannot read is a deferral — the box may be holding operator text and
/// guessing costs them their line.
pub fn defer_if_operator_typing_strict(
    reader: &dyn PaneReader,
    harness: &str,
    pane_id: &str,
    project: &str,
) -> Result<(), String> {
    let pin = who(project, pane_id);
    let input = gate(reader, harness, pane_id, None).map_err(|e| {
        format!(
            "deferred: cannot read {pin}'s pane ({e}); nothing was typed, and the mail stays queued"
        )
    })?;
    if crate::pane_screen::may_inject(input) {
        return Ok(());
    }
    if crate::pane_screen::wake_queued(input) {
        return Err(format!(
            "deferred: a wake line is already queued for {pin}; nothing was typed, and the mail stays queued"
        ));
    }
    Err(format!(
        "deferred: operator is typing in {pin}; nothing was typed, and the mail stays queued for the next idle edge"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Result<String, String>);

    impl PaneReader for Fixed {
        fn tail(&self, _pane_id: &str, _lines: u32) -> Result<String, String> {
            self.0.clone()
        }
    }

    fn opencode_fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/pane-screens/opencode/{name}.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("fixture")
    }

    /// bd-0uzrq.3: the box holds the operator's half-typed line, so the door
    /// refuses with the pin named and writes nothing.
    #[test]
    fn a_typed_box_defers_with_the_pin_named() {
        let typed = opencode_fixture("partial-input");
        let err = defer_if_operator_typing(&Fixed(Ok(typed)), "opencode", "block:1", "read-probe")
            .expect_err("operator text must defer");
        assert!(err.starts_with("deferred: operator is typing in "), "{err}");
        assert!(err.contains("nothing was typed"), "{err}");
    }

    /// bd-e4gal: a profileless spawn's registry row has no harness, so the
    /// guard is handed "". The real captured screen below holds the operator's
    /// half-typed line inside an opencode box; treating Unknown as injectable
    /// wrote a wake straight over it. The guard must defer without a harness.
    #[test]
    fn an_unknown_harness_defers_when_the_box_holds_operator_text() {
        let screen = std::fs::read_to_string(format!(
            "{}/tests/fixtures/pane-screens/opencode/multiline-operator-and-wake.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("captured screen fixture");
        let err = defer_if_operator_typing(&Fixed(Ok(screen)), "", "block:probe", "e4gal-probe")
            .expect_err("an unknown harness over typed text must defer, not inject");
        assert!(err.starts_with("deferred: "), "{err}");
    }

    /// bd-8wdgn: a box holding OUR OWN queued wake line is not operator typing.
    /// It still defers, but it says so - the operator never touched the box.
    #[test]
    fn a_queued_wake_line_defers_with_its_own_message() {
        let wake = opencode_fixture("wake-line-queued");
        for err in [
            defer_if_operator_typing(
                &Fixed(Ok(wake.clone())),
                "opencode",
                "block:1",
                "read-probe",
            ),
            defer_if_operator_typing_strict(
                &Fixed(Ok(wake.clone())),
                "opencode",
                "block:1",
                "read-probe",
            ),
        ] {
            let err = err.expect_err("a queued wake line must defer");
            assert!(
                err.starts_with("deferred: a wake line is already queued for "),
                "the message names the wake line, not the operator: {err}"
            );
            assert!(
                !err.contains("operator is typing"),
                "the operator is not typing: {err}"
            );
        }
    }

    /// An empty box (or a harness whose frame we do not know) passes.
    #[test]
    fn an_empty_box_does_not_defer() {
        let idle = opencode_fixture("idle-fresh");
        defer_if_operator_typing(&Fixed(Ok(idle)), "opencode", "block:1", "read-probe")
            .expect("an empty box is promptable");
        defer_if_operator_typing(&Fixed(Ok(String::new())), "codex", "block:1", "read-probe")
            .expect("an uncaptured harness never blocks");
    }

    /// A pane we cannot read is a deferral, not a guess.
    #[test]
    fn an_unreadable_pane_defers_only_the_background_doors() {
        defer_if_operator_typing(
            &Fixed(Err("socket closed".into())),
            "opencode",
            "block:1",
            "read-probe",
        )
        .expect("a door the operator asked for keeps working on an unreadable plane");
        let err = defer_if_operator_typing_strict(
            &Fixed(Err("socket closed".into())),
            "opencode",
            "block:1",
            "read-probe",
        )
        .expect_err("a background door must defer when it cannot read");
        assert!(err.starts_with("deferred: cannot read"), "{err}");
    }

    /// Every door that types into a shared pane asks the same guard: the
    /// captain-sent prompt, the two send paths, and the mail wake.
    #[test]
    fn pane_screen_every_prompt_door_asks_the_input_guard() {
        for file in [
            "src/cmd/crew.rs",
            "src/cmd/send.rs",
            "src/cmd/internal.rs",
            "src/mail_wake.rs",
        ] {
            let src = std::fs::read_to_string(format!("{}/{file}", env!("CARGO_MANIFEST_DIR")))
                .unwrap_or_else(|e| panic!("{file}: {e}"));
            assert!(
                src.contains("defer_if_operator_typing"),
                "{file} types into a pane without asking the operator-typing guard"
            );
        }
    }
}
