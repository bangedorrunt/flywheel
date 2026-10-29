//! Replay recorded transcripts through the real host (bd-replay-fixtures-9z0u).
//!
//! The fixture (`fixtures/replay_transcript.ndjson`) is raw provider frames
//! with ids and seq ordering kept — herdr status frames, watch wire lines,
//! and toron-wire delivery frames — never pre-normalized domain events.
//! The replay replaces ONLY the transports:
//! - herdr socket → `--status pin=state` seeds on `__doorbell --once`;
//! - toron socket → [`DeliveryEvent`]s folded by the real
//!   [`toron_core::delivery::DeliveryFsm`].
//! The decider is the real `workspace::tick` fold (via `once_json`), and
//! assertions read projections (the injects output), not call counts.
//! Redaction is structural: the redacted frame keeps id/kind/tags with a
//! scrubbed body, and the ring still routes on the surviving tags.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use toron_core::delivery::{DeliveryEvent, DeliveryFsm, DeliveryState, Phase};
use toron_core::envelope::Class;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// Split the raw transcript into herdr seeds (`--status`) and watch stdin
/// lines, in seq order. Panics on any seq gap: lifecycle ordering is the
/// property under test, so a torn transcript must fail loudly, never replay
/// past the tear.
fn split_transcript() -> (Vec<String>, Vec<String>, Vec<serde_json::Value>) {
    let text = std::fs::read_to_string(repo_root().join("tests/fixtures/replay_transcript.ndjson"))
        .expect("read replay transcript fixture");
    let mut seeds = Vec::new();
    let mut watch = Vec::new();
    let mut toron = Vec::new();
    let mut expect_seq = 1u64;
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {n}: bad json: {e}"));
        let seq = v
            .get("seq")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| panic!("line {n}: frame without seq"));
        assert_eq!(
            seq, expect_seq,
            "transcript torn at line {n}: seq {seq} != {expect_seq}"
        );
        expect_seq += 1;
        match v.get("frame").and_then(serde_json::Value::as_str) {
            Some("herdr.status") => {
                let pin = v["pin"].as_str().expect("herdr frame pin");
                let status = v["agent_status"].as_str().expect("herdr frame status");
                seeds.push(format!("{pin}={status}"));
            }
            Some("watch") => {
                watch.push(serde_json::to_string(&v["line"]).expect("watch line"));
            }
            Some("toron") => toron.push(v),
            other => panic!("line {n}: unknown frame {other:?}"),
        }
    }
    (seeds, watch, toron)
}

fn run_doorbell_once(seeds: &[String], watch_lines: &[String]) -> serde_json::Value {
    let mut cmd = Command::new(flywheel());
    cmd.args([
        "__doorbell",
        "--project",
        "toron",
        "--workspace",
        "replay-fixture",
        "--once",
    ]);
    for s in seeds {
        cmd.args(["--status", s]);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn __doorbell --once");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for w in watch_lines {
            writeln!(stdin, "{w}").expect("write watch line");
        }
    }
    let out = child.wait_with_output().expect("wait --once");
    assert!(
        out.status.success(),
        "__doorbell --once failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).expect("injects json")
}

/// Map one toron transcript frame onto the real [`DeliveryEvent`]. Only the
/// socket is replaced; the event vocabulary is the FSM's own.
fn toron_event(frame: &serde_json::Value) -> DeliveryEvent {
    let e = &frame["event"];
    match e["event"].as_str().expect("toron frame event name") {
        "appended" => DeliveryEvent::Appended {
            offset: e["offset"].as_u64().expect("appended offset"),
        },
        "read" => DeliveryEvent::Read {
            reader: e["reader"].as_str().expect("read reader").to_string(),
            cursor: e["cursor"].as_u64().expect("read cursor"),
        },
        "accept" => DeliveryEvent::Accept {
            by: e["by"].as_str().expect("accept by").to_string(),
            generation: e["gen"].as_u64().expect("accept gen") as u32,
        },
        "done" => DeliveryEvent::Done {
            by: e["by"].as_str().expect("done by").to_string(),
            generation: e["gen"].as_u64().expect("done gen") as u32,
        },
        "verdict" => DeliveryEvent::Verdict {
            by: e["by"].as_str().expect("verdict by").to_string(),
            pass: e["pass"].as_bool().expect("verdict pass"),
            rework: e["rework"].as_bool().unwrap_or(false),
        },
        name => panic!("unknown toron event {name}"),
    }
}

#[test]
fn replay_transcript_drives_real_host() {
    let (seeds, watch, toron) = split_transcript();
    assert_eq!(seeds, vec!["BlueRidge=idle", "LilacChapel=working"]);
    assert_eq!(watch.len(), 6, "six watch wire lines, got {watch:?}");

    // Pane/wake half: the real tick fold decides; read the injects.
    let injects = run_doorbell_once(&seeds, &watch);
    let rows = injects.as_array().expect("injects array");
    let pins: Vec<&str> = rows
        .iter()
        .map(|r| r["pin"].as_str().expect("inject pin"))
        .collect();
    // Both urgent rings address idle BlueRidge (plain + redacted) — the wake
    // fired. The Ghost ring names a pin the fold never saw (no prompt: the
    // wrong pane never answers) and the LilacChapel ring hits a working pin
    // (queued, never injected: the pane never woke mid-work).
    assert_eq!(
        pins,
        vec!["BlueRidge", "BlueRidge"],
        "only the idle addressed pin prompts, once per urgent ring, got {injects}"
    );
    for r in rows {
        assert!(
            r["line"]
                .as_str()
                .unwrap_or_default()
                .contains("[doorbell]"),
            "urgent ring must render a doorbell pull line, got {r}"
        );
    }

    // Redaction half: the scrubbed frame keeps id/kind/tags; routing never
    // reads the body. Structural check on the fixture, behavioral check above
    // (second BlueRidge prompt came from the redacted ring).
    let redacted: serde_json::Value = serde_json::from_str(&watch[5]).expect("redacted line");
    assert_eq!(redacted["id"], "r9");
    assert_eq!(redacted["kind"], 9);
    assert_eq!(redacted["content"], "[redacted]");
    assert!(
        redacted["tags"].as_array().is_some_and(|t| t
            .iter()
            .any(|tag| tag == &serde_json::json!(["p", "BlueRidge"]))),
        "redaction must preserve the address tags, got {redacted}"
    );

    // Toron-wire half: send→delivery→read→verdict through the real FSM.
    assert_eq!(toron.len(), 5, "five toron frames, got {toron:?}");
    let first = &toron[0];
    assert_eq!(first["op"], "send");
    assert_eq!(first["task_class"], "task");
    let mut state = DeliveryState::intent(
        Class::Task,
        first["recipient"].as_str().expect("recipient").to_string(),
        1,
    );
    assert_eq!(state.phase, Phase::Intent);
    for f in &toron {
        let ev = toron_event(f);
        state = DeliveryFsm::step(&state, &ev)
            .unwrap_or_else(|e| panic!("op {} rejected at {:?}: {e}", f["op"], state.phase));
    }
    assert_eq!(state.phase, Phase::Proved, "task must prove out");
    assert!(state.is_terminal());
    assert_eq!(state.holder.as_deref(), Some("BlueRidge"));
}

/// Ordering is load-bearing: a verdict before the read is a refusal, and a
/// read from a stranger is a refusal — the replay must surface both instead
/// of folding past them.
#[test]
fn replay_transcript_out_of_order_is_refused() {
    let mut state = DeliveryState::intent(Class::Task, "BlueRidge".to_string(), 1);
    state = DeliveryFsm::step(&state, &DeliveryEvent::Appended { offset: 41 }).expect("append");
    assert_eq!(state.phase, Phase::Sent);
    let early = DeliveryEvent::Verdict {
        by: "BlueRidge".to_string(),
        pass: true,
        rework: false,
    };
    assert!(
        DeliveryFsm::step(&state, &early).is_err(),
        "verdict at Sent must be refused"
    );
    let stranger = DeliveryEvent::Read {
        reader: "Ghost".to_string(),
        cursor: 41,
    };
    assert!(
        DeliveryFsm::step(&state, &stranger).is_err(),
        "read by a non-recipient must be refused"
    );
}
