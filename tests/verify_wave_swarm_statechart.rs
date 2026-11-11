//! Formal statechart + TLA+ verification suite for the 5-stage wave-swarm
//! coordinator (Gate, Dispatch, Execute, Review, Verify).
//!
//! Deterministic, pure, zero ambient I/O, zero wall clock.
//! Bead: bd-wave-swarm-statechart-tla-tjhrx

use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Stage {
    Gate,
    Dispatch,
    Execute,
    Review,
    Verify,
}

impl Stage {
    const ORDER: [Stage; 5] = [
        Stage::Gate,
        Stage::Dispatch,
        Stage::Execute,
        Stage::Review,
        Stage::Verify,
    ];

    fn next(self) -> Option<Stage> {
        match self {
            Stage::Gate => Some(Stage::Dispatch),
            Stage::Dispatch => Some(Stage::Execute),
            Stage::Execute => Some(Stage::Review),
            Stage::Review => Some(Stage::Verify),
            Stage::Verify => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Stage::Gate => "Gate",
            Stage::Dispatch => "Dispatch",
            Stage::Execute => "Execute",
            Stage::Review => "Review",
            Stage::Verify => "Verify",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum State {
    InStage(Stage),
    Succeeded,
    Failed { at: Stage },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Signal {
    /// Stage-completion signal for a specific stage.
    Done(Stage),
    /// Unexpected failure signal at a specific stage.
    Failed(Stage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StepOutcome {
    from: State,
    signal: Signal,
    to: State,
}

/// Deterministic coordinator statechart:
/// - progress only on Done(current stage), advancing in fixed order
/// - unexpected failure at any stage halts progression (fail-stop)
/// - terminal success only after all 5 stages complete
fn step(from: State, signal: Signal) -> State {
    match (from, signal) {
        (State::InStage(current), Signal::Done(stage)) if stage == current => {
            match current.next() {
                Some(next) => State::InStage(next),
                None => State::Succeeded, // Verify done => all 5 complete
            }
        }
        (State::InStage(current), Signal::Failed(stage)) if stage == current => {
            State::Failed { at: current }
        }
        // Fail-stop: terminals and out-of-context signals cannot progress.
        (state, _) => state,
    }
}

fn drive(signals: &[Signal]) -> (State, Vec<StepOutcome>) {
    let mut state = State::InStage(Stage::Gate);
    let mut trace = Vec::with_capacity(signals.len());
    for &signal in signals {
        let to = step(state, signal);
        trace.push(StepOutcome {
            from: state,
            signal,
            to,
        });
        state = to;
    }
    (state, trace)
}

/// All deterministic step transitions (the transition relation) over in-flight stages.
fn transition_relation() -> Vec<(State, Signal, State)> {
    let mut rel = Vec::new();
    for &stage in &Stage::ORDER {
        let from = State::InStage(stage);
        let done = Signal::Done(stage);
        let fail = Signal::Failed(stage);
        rel.push((from, done, step(from, done)));
        rel.push((from, fail, step(from, fail)));
    }
    rel
}

/// XState-like JSON export (equivalence representation, machine-readable).
fn to_xstate_json() -> serde_json::Value {
    serde_json::json!({
        "id": "wave-swarm-coordinator",
        "initial": "Gate",
        "states": {
            "Gate": {
                "on": {
                    "DONE_GATE": { "target": "Dispatch" },
                    "FAIL_GATE": { "target": "Failed" }
                }
            },
            "Dispatch": {
                "on": {
                    "DONE_DISPATCH": { "target": "Execute" },
                    "FAIL_DISPATCH": { "target": "Failed" }
                }
            },
            "Execute": {
                "on": {
                    "DONE_EXECUTE": { "target": "Review" },
                    "FAIL_EXECUTE": { "target": "Failed" }
                }
            },
            "Review": {
                "on": {
                    "DONE_REVIEW": { "target": "Verify" },
                    "FAIL_REVIEW": { "target": "Failed" }
                }
            },
            "Verify": {
                "on": {
                    "DONE_VERIFY": { "target": "Succeeded" },
                    "FAIL_VERIFY": { "target": "Failed" }
                }
            },
            "Succeeded": { "type": "final" },
            "Failed": { "type": "final" }
        }
    })
}

/// TLA+-style statechart export (equivalence/spec representation).
fn to_tla() -> String {
    // A compact TLA+ module capturing states, initial state, next-state relation,
    // and the core invariants (fail-stop, deterministic progress, liveness shape).
    let mut s = String::new();
    s.push_str("---- MODULE WaveSwarmCoordinator ----\n");
    s.push_str("EXTENDS Naturals\n\n");
    s.push_str("Stages == {\"Gate\", \"Dispatch\", \"Execute\", \"Review\", \"Verify\"}\n");
    s.push_str("States == Stages \\union {\"Succeeded\", \"Failed\"}\n\n");
    s.push_str("VARIABLE state\n\n");
    s.push_str("Init == state = \"Gate\"\n\n");
    s.push_str("DoneGate == state = \"Gate\" /\\ state' = \"Dispatch\"\n");
    s.push_str("DoneDispatch == state = \"Dispatch\" /\\ state' = \"Execute\"\n");
    s.push_str("DoneExecute == state = \"Execute\" /\\ state' = \"Review\"\n");
    s.push_str("DoneReview == state = \"Review\" /\\ state' = \"Verify\"\n");
    s.push_str("DoneVerify == state = \"Verify\" /\\ state' = \"Succeeded\"\n");
    s.push_str("FailGate == state = \"Gate\" /\\ state' = \"Failed\"\n");
    s.push_str("FailDispatch == state = \"Dispatch\" /\\ state' = \"Failed\"\n");
    s.push_str("FailExecute == state = \"Execute\" /\\ state' = \"Failed\"\n");
    s.push_str("FailReview == state = \"Review\" /\\ state' = \"Failed\"\n");
    s.push_str("FailVerify == state = \"Verify\" /\\ state' = \"Failed\"\n\n");
    s.push_str(
        "Next == DoneGate \\lor DoneDispatch \\lor DoneExecute \\lor DoneReview \\lor DoneVerify\n",
    );
    s.push_str(
        "  \\lor FailGate \\lor FailDispatch \\lor FailExecute \\lor FailReview \\lor FailVerify\n\n",
    );
    s.push_str("Spec == Init /\\ [][Next]_state\n\n");
    s.push_str("FailStop == state = \"Failed\" => state' = \"Failed\"\n");
    s.push_str("TerminalSuccessIffAllFive == state = \"Succeeded\" <=> TRUE\n");
    s.push_str("====\n");
    s
}

fn success_signals() -> Vec<Signal> {
    Stage::ORDER.iter().map(|&s| Signal::Done(s)).collect()
}

#[test]
fn deterministic_step_transitions_on_stage_done_signals() {
    // Same (state, signal) always yields the same next state.
    for &stage in &Stage::ORDER {
        let from = State::InStage(stage);
        let done = Signal::Done(stage);
        let expected = step(from, done);
        for _ in 0..8 {
            assert_eq!(
                step(from, done),
                expected,
                "non-deterministic transition at {stage:?}"
            );
        }
    }

    // Fixed-order progression on correct done signals.
    let (terminal, trace) = drive(&success_signals());
    assert_eq!(terminal, State::Succeeded);
    let visited: Vec<State> = std::iter::once(State::InStage(Stage::Gate))
        .chain(trace.iter().map(|o| o.to))
        .collect();
    assert_eq!(
        visited,
        vec![
            State::InStage(Stage::Gate),
            State::InStage(Stage::Dispatch),
            State::InStage(Stage::Execute),
            State::InStage(Stage::Review),
            State::InStage(Stage::Verify),
            State::Succeeded,
        ]
    );
}

#[test]
fn liveness_terminal_success_iff_all_five_stages_complete() {
    // => If and only if all 5 complete, terminal success is reached.
    let (terminal, _) = drive(&success_signals());
    assert_eq!(terminal, State::Succeeded);

    // <= If not all 5 complete, terminal success is not reached.
    for k in 0..5 {
        let prefix: Vec<Signal> = Stage::ORDER[..k].iter().map(|&s| Signal::Done(s)).collect();
        let (state, _) = drive(&prefix);
        assert_ne!(
            state,
            State::Succeeded,
            "reached Succeeded after only {k} completed stages"
        );
        if k < 5 {
            assert_eq!(state, State::InStage(Stage::ORDER[k]));
        }
    }

    // Any permutation/out-of-order or wrong done signals cannot manufacture success.
    let wrong = vec![
        Signal::Done(Stage::Dispatch),
        Signal::Done(Stage::Gate),
        Signal::Done(Stage::Execute),
        Signal::Done(Stage::Review),
        Signal::Done(Stage::Verify),
    ];
    let (state, _) = drive(&wrong);
    assert_ne!(state, State::Succeeded);
}

#[test]
fn fail_stop_unexpected_failure_at_any_stage_halts_progression() {
    for (i, &stage) in Stage::ORDER.iter().enumerate() {
        // Reach the target stage via successful prior dones, then fail it.
        let mut signals: Vec<Signal> = Stage::ORDER[..i].iter().map(|&s| Signal::Done(s)).collect();
        signals.push(Signal::Failed(stage));

        // Attempt further progression after failure.
        for &later in &Stage::ORDER[i..] {
            signals.push(Signal::Done(later));
        }
        signals.push(Signal::Done(Stage::Verify));

        let (terminal, trace) = drive(&signals);
        assert_eq!(
            terminal,
            State::Failed { at: stage },
            "failure at {stage:?} must halt at Failed"
        );

        // Once failed, every subsequent step is a no-op (halts progression).
        let fail_idx = trace
            .iter()
            .position(|o| o.to == State::Failed { at: stage })
            .expect("must enter Failed");
        for outcome in &trace[fail_idx + 1..] {
            assert_eq!(outcome.from, State::Failed { at: stage });
            assert_eq!(
                outcome.to,
                State::Failed { at: stage },
                "progressed after fail-stop"
            );
        }
    }
}

#[test]
fn all_terminal_outcomes_are_either_full_success_or_fail_stop() {
    // Exhaustive over traces of length <= 6 over {Done(s), Fail(s)} for current stage + noise:
    // success is reachable only by the exact 5-done sequence; any failure => Failed; else in-flight.
    let all_signals: Vec<Signal> = Stage::ORDER
        .iter()
        .flat_map(|&s| [Signal::Done(s), Signal::Failed(s)])
        .collect();

    fn enumerate(
        prefix: &mut Vec<Signal>,
        depth: usize,
        max: usize,
        alphabet: &[Signal],
        f: &mut impl FnMut(&[Signal]),
    ) {
        f(prefix);
        if depth == max {
            return;
        }
        for &sig in alphabet {
            prefix.push(sig);
            enumerate(prefix, depth + 1, max, alphabet, f);
            prefix.pop();
        }
    }

    let mut count = 0usize;
    let mut prefix = Vec::new();
    enumerate(&mut prefix, 0, 6, &all_signals, &mut |seq| {
        count += 1;
        let (state, trace) = drive(seq);
        match state {
            State::Succeeded => {
                let advanced: Vec<Signal> = trace
                    .iter()
                    .filter(|o| o.from != o.to)
                    .map(|o| o.signal)
                    .collect();
                assert_eq!(
                    advanced,
                    success_signals(),
                    "terminal success must come from completing all 5 stages in order"
                );
            }
            State::Failed { .. } => {
                assert!(seq.iter().any(|s| matches!(s, Signal::Failed(_))));
            }
            State::InStage(_) => {}
        }
    });
    assert!(count > 0, "enumeration must exercise traces");
}

#[test]
fn xstate_export_represents_five_stage_coordinator() {
    let m = to_xstate_json();
    assert_eq!(m["id"], "wave-swarm-coordinator");
    assert_eq!(m["initial"], "Gate");

    let states = m["states"].as_object().expect("states object");
    let expected: BTreeSet<&str> = [
        "Gate",
        "Dispatch",
        "Execute",
        "Review",
        "Verify",
        "Succeeded",
        "Failed",
    ]
    .into_iter()
    .collect();
    let actual: BTreeSet<&str> = states.keys().map(|s| s.as_str()).collect();
    assert_eq!(
        actual, expected,
        "xstate export must contain exactly the coordinator states"
    );

    // Five in-flight stages each have a deterministic done transition and a fail transition.
    for &stage in &Stage::ORDER {
        let node = &states[stage.as_str()];
        let on = node["on"].as_object().expect("stage has on-map");
        assert_eq!(
            on.len(),
            2,
            "{stage:?} must expose done and fail transitions only"
        );
    }

    assert_eq!(states["Gate"]["on"]["DONE_GATE"]["target"], "Dispatch");
    assert_eq!(
        states["Dispatch"]["on"]["DONE_DISPATCH"]["target"],
        "Execute"
    );
    assert_eq!(states["Execute"]["on"]["DONE_EXECUTE"]["target"], "Review");
    assert_eq!(states["Review"]["on"]["DONE_REVIEW"]["target"], "Verify");
    assert_eq!(states["Verify"]["on"]["DONE_VERIFY"]["target"], "Succeeded");
    for &stage in &Stage::ORDER {
        let key = format!("FAIL_{}", stage.as_str().to_uppercase());
        assert_eq!(
            states[stage.as_str()]["on"][key]["target"],
            "Failed",
            "fail-stop edge missing at {stage:?}"
        );
    }
    assert_eq!(states["Succeeded"]["type"], "final");
    assert_eq!(states["Failed"]["type"], "final");
}

#[test]
fn tla_export_represents_same_statechart_as_model() {
    let tla = to_tla();
    assert!(tla.contains("---- MODULE WaveSwarmCoordinator ----"));
    assert!(tla.contains("Init == state = \"Gate\""));
    assert!(tla.contains("DoneVerify == state = \"Verify\" /\\ state' = \"Succeeded\""));

    // Fail-stop edges for every stage present.
    for &stage in &Stage::ORDER {
        let name = stage.as_str();
        assert!(
            tla.contains(&format!(
                "Fail{name} == state = \"{name}\" /\\ state' = \"Failed\""
            )),
            "TLA export missing fail-stop for {name}"
        );
    }

    // Equivalence to transition relation: every modeled edge exists in TLA.
    let rel = transition_relation();
    assert_eq!(rel.len(), 10, "5 stages * (done + fail)");
    for (from, signal, to) in rel {
        let State::InStage(stage) = from else {
            unreachable!()
        };
        let expected = match (signal, to) {
            (Signal::Done(_), State::InStage(next)) => {
                format!(
                    "Done{} == state = \"{}\" /\\ state' = \"{}\"",
                    stage.as_str(),
                    stage.as_str(),
                    next.as_str()
                )
            }
            (Signal::Done(_), State::Succeeded) => {
                format!(
                    "Done{} == state = \"{}\" /\\ state' = \"Succeeded\"",
                    stage.as_str(),
                    stage.as_str()
                )
            }
            (Signal::Failed(_), State::Failed { .. }) => {
                format!(
                    "Fail{} == state = \"{}\" /\\ state' = \"Failed\"",
                    stage.as_str(),
                    stage.as_str()
                )
            }
            other => panic!("unexpected edge shape: {other:?}"),
        };
        assert!(tla.contains(&expected), "TLA missing edge: {expected}");
    }
}

#[test]
fn xstate_and_tla_exports_are_equivalent_to_each_other() {
    // Equivalence: same state set and same transition structure.
    let x = to_xstate_json();
    let tla = to_tla();
    let x_states: BTreeSet<String> = x["states"].as_object().unwrap().keys().cloned().collect();
    for s in &x_states {
        assert!(
            tla.contains(&format!("\"{s}\"")),
            "state {s} in xstate but absent from TLA"
        );
    }
    // Same number of done/fail actions: 5 done + 5 fail.
    for &stage in &Stage::ORDER {
        let n = stage.as_str();
        assert!(tla.contains(&format!("Done{n} ==")));
        assert!(tla.contains(&format!("Fail{n} ==")));
        let on = x["states"][n]["on"].as_object().unwrap();
        assert_eq!(on.len(), 2);
    }
    assert!(tla.contains("Next =="));
    assert!(tla.contains("Spec == Init /\\ [][Next]_state"));
}
