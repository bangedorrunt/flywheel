//! Is there operator text in a pane's input line? (bd-0uzrq.3)
//!
//! The operator supervises through the same panes crews run in. An injected
//! prompt that lands while a half-typed line sits in the input box corrupts
//! both, so the wake path refuses to type when the box is not empty and defers
//! instead — the wake stays queued for the next idle edge, never a timer.
//!
//! Scope (approved split, bd-0uzrq.3): a captured frame answers ONE question —
//! is the operator typing? Idle vs working stays status-derived (the plane
//! status gate plus the transcript and activity readers); frames cannot tell
//! those apart for every harness (measured: opencode's working and
//! idle-after-turn frames differ only by a post-turn footer).

/// What the input line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLine {
    /// the box is empty (a placeholder, or nothing after the prompt marker)
    Empty,
    /// the operator has typed something that has not been submitted
    OperatorText,
    /// the box holds one of OUR OWN wake lines (`[doorbell] …`, `⟦toron⟧ …`).
    /// Not operator typing: the line is already queued, so a second write is
    /// both unnecessary and destructive (bd-8wdgn).
    WakeQueued,
    /// this screen's input box is not recognised (no capture for this harness)
    Unknown,
}

/// The one rule the wake path asks: may a prompt be written into this pane?
pub fn may_inject(line: InputLine) -> bool {
    matches!(line, InputLine::Empty | InputLine::Unknown)
}

/// The ONE neutral marker every agent-facing wake starts with (bd-ofa1s).
/// A box holding it is a queued wake line, not operator text (bd-8wdgn).
pub const WAKE_PREFIXES: &[&str] = &[doorbell_core::agent_text::WAKE_PREFIX];

fn is_wake_line(content: &str) -> bool {
    WAKE_PREFIXES.iter().any(|p| content.starts_with(p))
}

/// For the deferral message: our own line is queued, so say that rather than
/// accusing the operator of typing.
pub fn wake_queued(line: InputLine) -> bool {
    matches!(line, InputLine::WakeQueued)
}

fn canonical(harness: &str) -> String {
    match harness.trim().to_ascii_lowercase().as_str() {
        "oc" | "opencode" => "opencode".to_string(),
        "cc" | "claude" | "claude-code" => "claude".to_string(),
        other => other.to_string(),
    }
}

/// The trailing box/scrollbar glyphs a capture carries at the right edge.
fn trim_box_edge(text: &str) -> &str {
    text.trim_end_matches(|c: char| {
        c == '█' || c == '│' || c == '╮' || c == '╯' || c == '╰' || c.is_whitespace()
    })
}

/// A marker only counts at the left edge of a row — that is the input box's
/// border column. A marker inside a sidebar row is not a second input box.
fn marker_at(line: &str, marker: &str) -> Option<usize> {
    let at = line.find(marker)?;
    line[..at].chars().all(char::is_whitespace).then_some(at)
}

/// The right sidebar draws its own text on the same terminal rows as the input
/// box, past a wide blank gap. Its first column is the right edge of the input
/// region: cells at or past it are never typed input (bd-3p7pe). No gap means
/// no sidebar, and the whole row is input as before.
fn sidebar_column(tail: &str, marker: &str) -> Option<usize> {
    let mut bound = None;
    for line in tail.lines() {
        let Some(at) = marker_at(line, marker) else {
            continue;
        };
        // Count columns in chars, never bytes: this value is applied to other
        // rows, whose byte offsets differ when the input holds multibyte text.
        let chars: Vec<char> = line.chars().collect();
        let mut col = line[..at].chars().count() + marker.chars().count();
        while col < chars.len() {
            let start = col;
            while col < chars.len() && chars[col] == ' ' {
                col += 1;
            }
            if col - start >= 12 && col < chars.len() {
                bound = Some(bound.map_or(col, |b: usize| b.min(col)));
                break;
            }
            if col == start {
                col += 1;
            }
        }
    }
    bound
}

/// Convert a column from `sidebar_column` to a byte boundary in THIS row. A raw
/// byte offset from another row can land inside a multibyte character and make
/// the slice panic, taking the doorbell host down on a wake.
fn column_boundary(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map_or(line.len(), |(byte, _)| byte)
}

/// The content line of a marker-prefixed input box, scanning up from the tail.
///
/// `skip` drops the footer lines the same box character draws under the input
/// (opencode prints its model line there). `None` means there is no such box on
/// screen at all.
fn marker_box(tail: &str, marker: &str, skip: &dyn Fn(&str) -> bool) -> Option<String> {
    let right = sidebar_column(tail, marker);
    let mut saw_box = false;
    for line in tail.lines().rev() {
        let Some(at) = marker_at(line, marker) else {
            // The box is the contiguous marker run nearest the bottom.
            if saw_box && !line.trim().is_empty() {
                break;
            }
            continue;
        };
        let start = at + marker.len();
        let end = right.map_or(line.len(), |col| column_boundary(line, col));
        if start > end {
            continue;
        }
        let text = trim_box_edge(line[start..end].trim());
        if skip(text) {
            continue;
        }
        saw_box = true;
        // The block pads the content line with empty box lines on both sides,
        // so an empty line is not yet the answer.
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    saw_box.then(String::new)
}

/// pi draws a bare input line between two rules, above a footer carrying the
/// context percentage. No marker, so the box is positional.
fn pi_box(tail: &str) -> Option<String> {
    let lines: Vec<&str> = tail.lines().collect();
    let footer = lines.iter().rposition(|l| l.contains("%/1.0M"))?;
    // The box's rules are drawn with ─; pi also draws a spinner rule
    // ("── ⠸ Working ─…") inside the same block while a turn runs.
    let is_rule = |l: &str| l.trim().starts_with('─');
    let mut i = footer;
    while i > 0 {
        if is_rule(lines[i]) {
            let mut content: Vec<&str> = Vec::new();
            let mut j = i - 1;
            while j > 0 && !is_rule(lines[j]) {
                content.push(lines[j].trim());
                j -= 1;
            }
            return Some(content.join(" ").trim().to_string());
        }
        i -= 1;
    }
    None
}

/// A terminal wraps a long wake line; the last box row can be only its tail
/// ("... toron next"). Recognise it by structure: an earlier wake line on this
/// screen ends with exactly this fragment, and the fragment is short.
fn is_wake_continuation(content: &str, tail: &str) -> bool {
    if content.is_empty() || content.chars().count() > 40 {
        return false;
    }
    let rows: Vec<&str> = tail
        .lines()
        .map(|line| line.trim().trim_start_matches('┃').trim())
        .collect();
    rows.windows(2)
        .any(|pair| is_wake_line(pair[0]) && pair[1] == content)
}

/// A status row drawn in the input box is not operator text (bd-g4xlr).
///
/// Structural, not vendor-worded: keybinding hints, or a short dot-separated
/// `<mode/model> · <effort>` summary. A typed line containing a middle dot is
/// excluded by the short-token/length limit, so we do not hide real text.
fn is_input_footer(text: &str) -> bool {
    if text.starts_with("Build ") || text.contains("shift+tab") {
        return true;
    }
    let parts: Vec<&str> = text.split('·').map(str::trim).collect();
    parts.len() > 1
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.split_whitespace().count() <= 4)
        && text.chars().count() <= 80
}

/// An open command palette owns keystrokes; never inject into it. Its header
/// and keybind rows sit in one block directly above the bottom input box.
fn opencode_palette_open(tail: &str) -> bool {
    let lines: Vec<&str> = tail.lines().collect();
    let is_box = |line: &str| line.trim_start().starts_with('┃');
    let mut i = lines.len();
    while i > 0 && !is_box(lines[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_box(lines[i - 1]) {
        i -= 1;
    }
    let mut above: Vec<&str> = Vec::new();
    while i > 0 && !is_box(lines[i - 1]) {
        i -= 1;
        above.push(lines[i]);
    }
    let header = above
        .iter()
        .any(|line| trim_box_edge(line.trim_start()) == "Search")
        && above
            .iter()
            .any(|line| matches!(trim_box_edge(line.trim_start()), "Suggested" | "Commands"));
    let keybinds = above
        .iter()
        .filter(|line| {
            line.find("ctrl+x").is_some_and(|at| {
                line[..at]
                    .chars()
                    .rev()
                    .take_while(|c| c.is_whitespace())
                    .count()
                    >= 4
            })
        })
        .count();
    header && keybinds >= 2
}

/// Classifies a screen structurally when the registry recorded no harness.
pub fn input_line_for_unknown_harness(tail: &str, last_typed: Option<&str>) -> InputLine {
    for harness in ["opencode", "claude", "gemini", "pi"] {
        let line = input_line_for(harness, tail, last_typed);
        if line != InputLine::Unknown {
            return line;
        }
    }
    InputLine::Unknown
}

/// Classify the LAST captured screen (terminal tail) for `harness`.
///
/// `last_typed` is the line the wake path itself wrote: opencode and claude
/// keep SUBMITTED text visible in the box, so a box holding exactly that line
/// is not operator typing — measured in the corpus.
pub fn input_line_for(harness: &str, tail: &str, last_typed: Option<&str>) -> InputLine {
    let harness = canonical(harness);
    if harness.as_str() == "opencode" && opencode_palette_open(tail) {
        return InputLine::OperatorText;
    }
    let content: Option<String> = match harness.as_str() {
        "opencode" => marker_box(tail, "┃", &is_input_footer),
        "claude" => marker_box(tail, "❯", &|_| false),
        "gemini" => marker_box(tail, "│", &|text| !text.starts_with('>')),
        "pi" => pi_box(tail),
        _ => return InputLine::Unknown,
    };
    let placeholders: &[&str] = match harness.as_str() {
        "opencode" => &["Ask anything…", "Ask anything..."],
        "claude" => &["Try \""],
        "gemini" => &["Type your message"],
        _ => &[],
    };
    let Some(content) = content else {
        return InputLine::Unknown;
    };
    // gemini prefixes the content with `>` inside the box.
    let content = content.trim_start_matches('>').trim();
    if content.is_empty()
        || placeholders.iter().any(|p| content.starts_with(p))
        || last_typed.is_some_and(|typed| !typed.is_empty() && content == typed.trim())
    {
        return InputLine::Empty;
    }
    if is_wake_line(content) || is_wake_continuation(content, tail) {
        return InputLine::WakeQueued;
    }
    InputLine::OperatorText
}
