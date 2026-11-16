//! Is there operator text in a pane's input line? (bd-0uzrq.3)
//!
//! The operator supervises through the same panes crews run in. An injected
//! prompt that lands while a half-typed line sits in the input box corrupts
//! both, so the wake path refuses to type when the box is not empty and defers
//! instead.
//!
//! Scope: this answers ONE question from ONE captured frame — is the input line
//! empty? It deliberately does not try to answer "is the harness idle": for
//! opencode's chat view the working and finished frames are visually identical
//! (measured, bd-0uzrq.3), so idle/working stays with the plane status gate and
//! the transcript readers. Anything unrecognized is `Unknown`, which never
//! blocks a wake on its own.

/// What the input line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLine {
    /// the box is empty (a placeholder, or nothing after the prompt marker)
    Empty,
    /// the operator has typed something that has not been submitted
    OperatorText,
    /// this screen's input box is not recognised (no capture for this harness)
    Unknown,
}

/// The one rule the wake path asks: may a prompt be written into this pane?
pub fn may_inject(line: InputLine) -> bool {
    !matches!(line, InputLine::OperatorText)
}

/// The prompt markers whose line IS the input box, per harness. A harness with
/// no entry here returns [`InputLine::Unknown`].
fn box_marker(harness: &str) -> Option<&'static str> {
    match harness.trim().to_ascii_lowercase().as_str() {
        "opencode" | "oc" => Some("┃"),
        _ => None,
    }
}

/// Placeholders that mean "the box is empty" for that harness.
fn placeholders(harness: &str) -> &'static [&'static str] {
    match harness.trim().to_ascii_lowercase().as_str() {
        "opencode" | "oc" => &["Ask anything…", "Ask anything..."],
        _ => &[],
    }
}

/// Classify the LAST captured screen (terminal tail) for `harness`.
///
/// The box is the last marker line above the status footer. `last_typed` is the
/// line the wake path itself wrote: opencode keeps the SUBMITTED text visible
/// in the box, so a box holding exactly that line is not operator typing —
/// measured in the corpus (`working.txt`, `idle-after-turn.txt`). Without this,
/// every wake after the first turn would defer forever.
pub fn input_line_for(harness: &str, tail: &str, last_typed: Option<&str>) -> InputLine {
    let Some(marker) = box_marker(harness) else {
        return InputLine::Unknown;
    };
    let placeholders = placeholders(harness);
    let mut saw_box = false;
    let mut content: Option<&str> = None;
    for line in tail.lines().rev() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix(marker) else {
            continue;
        };
        let text = rest
            .trim()
            .trim_end_matches(|c: char| c == '█' || c.is_whitespace());
        // The same box character draws the model/status footer under the input
        // line; that is not the input.
        if text.starts_with("Build ") || text.contains("shift+tab") {
            continue;
        }
        saw_box = true;
        // The block pads the content line with empty box lines on both sides.
        if !text.is_empty() {
            content = Some(text);
            break;
        }
    }
    if !saw_box {
        return InputLine::Unknown;
    }
    let Some(content) = content else {
        return InputLine::Empty;
    };
    if content.is_empty()
        || placeholders.iter().any(|p| content.starts_with(p))
        || last_typed.is_some_and(|typed| !typed.is_empty() && content == typed.trim())
    {
        return InputLine::Empty;
    }
    InputLine::OperatorText
}
