//! The pane read port (bd-0uzrq.3): one way to look at what a pane is showing.
//!
//! The wake path needs the input line before it types. The read lives behind
//! this trait so the guard is testable with a fake and so hc and rex answer the
//! same question — the same shape as `pane_write::PaneTyper` on the write side.

use crate::pane_screen::{InputLine, input_line_for};

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
    Ok(input_line_for(harness, &tail, last_typed))
}
