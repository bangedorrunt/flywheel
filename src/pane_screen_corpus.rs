//! The per-harness pane-state corpus (bd-0uzrq.3).
//!
//! Every row is a real capture from a throwaway rex session
//! (`scripts/capture-pane-corpus.sh`). The detectors must classify each one the
//! way the wake path needs: a box holding operator text defers, an empty box
//! (or opencode's echo of the line we ourselves typed) does not.

use crate::pane_screen::{InputLine, may_inject};

fn fixture(harness: &str, file: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/pane-screens/{harness}/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("capture {harness}/{file}: {e}"))
}

struct Row {
    file: String,
    expect: InputLine,
    last_typed: Option<String>,
}

fn rows(harness: &str) -> Vec<Row> {
    let index = format!(
        "{}/tests/fixtures/pane-screens/{harness}/index.toml",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&index).unwrap_or_else(|e| panic!("{index}: {e}"));
    let doc: toml::Value = toml::from_str(&text).unwrap_or_else(|e| panic!("{index}: {e}"));
    doc.get("screen")
        .and_then(|v| v.as_array())
        .expect("an index with [[screen]] rows")
        .iter()
        .map(|row| Row {
            file: row["file"].as_str().expect("file").to_string(),
            expect: match row["expect"].as_str().expect("expect") {
                "empty" => InputLine::Empty,
                "operator-text" => InputLine::OperatorText,
                other => panic!("unknown expect {other:?}"),
            },
            last_typed: row
                .get("last_typed")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
        .collect()
}

/// Every captured screen classifies the way the corpus recorded it. Six per
/// harness minimum, because one frame cannot show both an empty and a typed
/// box, and at least one must be a typed one.
#[test]
fn every_captured_screen_classifies() {
    let root = format!("{}/tests/fixtures/pane-screens", env!("CARGO_MANIFEST_DIR"));
    let mut harnesses = 0;
    for entry in std::fs::read_dir(&root).expect("corpus dir") {
        let path = entry.expect("dir entry").path();
        if !path.is_dir() {
            continue;
        }
        let harness = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("harness name");
        if !path.join("index.toml").is_file() {
            assert!(
                path.join("GAP.md").is_file(),
                "{harness}: a harness directory without index.toml must document its gap in GAP.md"
            );
            continue;
        }
        let rows = rows(harness);
        assert!(
            rows.len() >= 6,
            "{harness}: a corpus needs >= 6 captured screens, has {}",
            rows.len()
        );
        assert!(
            rows.iter().any(|r| r.expect == InputLine::OperatorText),
            "{harness}: the corpus must contain the typed-box screen that the guard exists for"
        );
        for row in &rows {
            let tail = fixture(harness, &row.file);
            let got = crate::pane_screen::input_line_for(harness, &tail, row.last_typed.as_deref());
            assert_eq!(
                got, row.expect,
                "{harness}/{}: expected {:?}, detector said {:?}",
                row.file, row.expect, got
            );
            assert_eq!(
                may_inject(got),
                row.expect != InputLine::OperatorText,
                "{harness}/{}: only operator text defers a wake",
                row.file
            );
        }
        harnesses += 1;
    }
    assert!(
        harnesses >= 4,
        "expected at least four captured harnesses (opencode, claude, gemini, pi), found {harnesses}"
    );
}

/// A harness with no capture is `Unknown`, and `Unknown` never blocks a wake on
/// its own: the plane status gate still owns idle/working.
#[test]
fn an_uncaptured_harness_is_unknown_and_does_not_block() {
    assert_eq!(
        crate::pane_screen::input_line_for("codex", "anything at all", None),
        InputLine::Unknown
    );
    assert!(may_inject(InputLine::Unknown));
}
