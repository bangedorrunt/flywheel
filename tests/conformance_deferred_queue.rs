//! bd-uawqo: record a REAL doorbell run (deferral -> watch restart -> idle
//! drain) from the ordered effect journal, convert it to `everystate conform`
//! observations, and check the recording against `DeferredQueue`. A doctored
//! copy with a second ring must be rejected with exit 20, naming the step.
//!
//! `CONFORM_TRACE_OUT` / `CONFORM_BAD_TRACE_OUT` let `verify-formal.sh` capture
//! the two JSONL files. With `FLYWHEEL_EVERYSTATE` set the test also runs the
//! checker itself, so a plain `cargo test` proves the trace end to end.

use doorbell_core::DoorbellModel;
use doorbell_rt::effect_trace::DeferredQueueTrace;
use doorbell_rt::host::CoreReducer;
use doorbell_rt::msg::BrainMsg;
use doorbell_rt::store::FileStore;
use serde_json::{Value, json};

fn effect_json(e: &doorbell_rt::Effect) -> Value {
    match e {
        doorbell_rt::Effect::Prompt { pin, line } => {
            json!({"Prompt": {"pin": pin, "line": line}})
        }
        doorbell_rt::Effect::Digest { pin, line, why } => {
            json!({"Digest": {"pin": pin, "line": line, "why": why}})
        }
        doorbell_rt::Effect::Audit { kind, pin, detail } => {
            json!({"Audit": {"kind": kind, "pin": pin, "detail": detail}})
        }
        doorbell_rt::Effect::FetchInbox => json!("FetchInbox"),
        doorbell_rt::Effect::CheckAckSla => json!("CheckAckSla"),
        doorbell_rt::Effect::RestartWatch => json!("RestartWatch"),
    }
}

fn out_path(var: &str, fallback: &str) -> std::path::PathBuf {
    std::env::var(var)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join(fallback))
}

#[test]
fn a_recorded_deferral_restart_and_boot_drain_conforms() {
    let dir = std::env::temp_dir().join(format!("dq-conform-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = FileStore::new(&dir);
    let mut reducer = CoreReducer {
        model: DoorbellModel::default(),
    };
    reducer
        .model
        .index
        .insert_pin("P".into(), Some("npub1dqp".into()));

    let line = serde_json::json!({
        "kind": 9, "id": "a".repeat(64), "pubkey": "a".repeat(64),
        "content": "", "tags": [["p", "npub1dqp"]], "created_at": 1
    })
    .to_string();

    let steps: Vec<(Value, BrainMsg)> = vec![
        (
            json!({"rt_kind":"PlaneStatus","pin":"P","status":"working"}),
            BrainMsg::PlaneStatus {
                pin: "P".into(),
                status: "working".into(),
            },
        ),
        (json!({"rt_kind":"Watch"}), BrainMsg::Watch(line.clone())),
        (json!({"rt_kind":"Watch"}), BrainMsg::Watch(line.clone())),
        (json!({"rt_kind":"Died"}), BrainMsg::Died),
        (
            json!({"rt_kind":"PlaneStatus","pin":"P","status":"idle"}),
            BrainMsg::PlaneStatus {
                pin: "P".into(),
                status: "idle".into(),
            },
        ),
    ];

    for (seq, (msg, brain)) in steps.into_iter().enumerate() {
        let effects = reducer.tick_effects(&brain);
        let actions: Vec<Value> = effects.iter().map(effect_json).collect();
        store
            .append_journal(
                "w",
                &msg,
                &json!(actions),
                json!({"stage":"decided"}),
                &["P".into()],
            )
            .expect("journal row");
        assert_eq!(seq + 1, seq + 1);
    }

    // Convert the ordered journal the same way a live observer would.
    let raw = std::fs::read_to_string(store.journal_file("w")).expect("journal");
    let mut trace = DeferredQueueTrace::new();
    for row in raw.lines() {
        let row: Value = serde_json::from_str(row).expect("journal json");
        trace.observe(&row);
    }
    let actions: Vec<&str> = trace
        .observations
        .iter()
        .skip(1)
        .filter_map(|o| o["action"].as_str())
        .collect();
    assert!(
        actions.contains(&"Arrive")
            && actions.contains(&"ReachPrompt")
            && actions.contains(&"Ring"),
        "the run must defer, restart and drain: {actions:?}"
    );
    assert_eq!(trace.observations.last().unwrap()["rings"]["brief"], 1);

    let good = out_path("CONFORM_TRACE_OUT", "dq-recorded.jsonl");
    let good_jsonl = trace.to_jsonl();
    if std::env::var("CONFORM_TRACE_OUT").is_err() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".agents/skills/verify-protocol-tla/traces/DeferredQueue.recorded.jsonl");
        assert_eq!(
            std::fs::read_to_string(&fixture).expect("checked-in recorded trace"),
            good_jsonl,
            "the recorded trace drifted from the real run; regenerate the fixture"
        );
    }
    std::fs::write(&good, &good_jsonl).expect("write trace");

    // Doctored: a second Ring after the queue is empty. The spec forbids it.
    let mut bad = trace.observations.clone();
    let mut twice = bad.last().unwrap().clone();
    twice["action"] = json!("Ring");
    twice["rings"]["brief"] = json!(2);
    bad.push(twice);
    let bad_jsonl = bad
        .iter()
        .map(|o| o.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let bad_path = out_path("CONFORM_BAD_TRACE_OUT", "dq-double-ring.jsonl");
    if std::env::var("CONFORM_BAD_TRACE_OUT").is_err() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".agents/skills/verify-protocol-tla/traces/DeferredQueue.double-ring.jsonl");
        assert_eq!(
            std::fs::read_to_string(&fixture).expect("checked-in double-ring trace"),
            bad_jsonl,
            "the doctored trace drifted; regenerate the fixture"
        );
    }
    std::fs::write(&bad_path, bad_jsonl).expect("write bad trace");

    let Some(checker) = std::env::var("FLYWHEEL_EVERYSTATE").ok() else {
        return;
    };
    let models = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".agents/skills/verify-protocol-tla/models");
    let run = |trace: &std::path::Path| {
        std::process::Command::new(&checker)
            .arg("conform")
            .arg("-trace")
            .arg(trace)
            .arg("-config")
            .arg(models.join("DeferredQueue.cfg"))
            .arg(models.join("DeferredQueue.tla"))
            .output()
            .expect("everystate conform")
    };
    let ok = run(&good);
    assert!(
        ok.status.success(),
        "recorded run must conform: {}",
        String::from_utf8_lossy(&ok.stdout)
    );
    let no = run(&bad_path);
    assert_eq!(
        no.status.code(),
        Some(20),
        "a double ring must be rejected: {}",
        String::from_utf8_lossy(&no.stdout)
    );
    assert!(
        String::from_utf8_lossy(&no.stdout).contains("Ring"),
        "the rejection must name the step"
    );
}
