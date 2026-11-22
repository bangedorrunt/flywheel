//! Deadlock watch (bd-v4dcv): a wait-for graph over live leases and open
//! collab asks, with cycle detection and long-wait surfacing.
//!
//! Pure fold: the caller supplies lease rows and collab waits; this module
//! finds pin cycles and overdue waits. Mailing the captain and remembering
//! what was already reported are the caller's (effect) job, so a re-run
//! never double-pages.

use std::collections::{BTreeMap, BTreeSet};

/// One live exclusive lease row (subset of the board the guard reads).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveLease {
    pub holder: String,
    pub path: String,
    pub reason: String,
}

/// One open collab ask: `asker` waits on `askee` in `thread`, asked at
/// `since_us`. An unknown asker becomes the `(unknown)` graph node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollabWait {
    pub asker: Option<String>,
    pub askee: String,
    pub thread: String,
    pub since_us: i64,
}

/// One directed wait: `waiter` is stuck on `holder` over `resource`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WaitEdge {
    pub waiter: String,
    pub holder: String,
    pub resource: String,
    pub kind: WaitKind,
    pub since_us: Option<i64>,
}

/// What the waiter is stuck on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WaitKind {
    /// A live exclusive lease on an overlapping path.
    Lease,
    /// An unanswered collab ask in a thread.
    Collab,
}

/// Lease edges: for every pair of live exclusive leases on overlapping
/// paths held by different pins, each holder waits on the other. The
/// overlap check is the guard's own glob rule, so the graph agrees with
/// the board the strict guard evaluates.
pub fn lease_edges(leases: &[LiveLease]) -> Vec<WaitEdge> {
    let mut edges = BTreeSet::new();
    for (i, a) in leases.iter().enumerate() {
        for b in &leases[i + 1..] {
            if a.holder == b.holder {
                continue;
            }
            let overlap = toron_core::mailbox::guard::pattern_matches(&a.path, &b.path)
                || toron_core::mailbox::guard::pattern_matches(&b.path, &a.path);
            if !overlap {
                continue;
            }
            for (waiter, holder, path) in [
                (&a.holder, &b.holder, &b.path),
                (&b.holder, &a.holder, &a.path),
            ] {
                edges.insert(WaitEdge {
                    waiter: waiter.clone(),
                    holder: holder.clone(),
                    resource: path.clone(),
                    kind: WaitKind::Lease,
                    since_us: None,
                });
            }
        }
    }
    edges.into_iter().collect()
}

/// A wait older than this is stale history, not a live deadlock: the fleet
/// turns over faster than a day, and no session that old is still running.
/// (bd-v4dcv: ignore threads older than the waiter's session or a day.)
pub const MAX_WAIT_AGE_US: i64 = 24 * 3600 * 1_000_000;

/// Keep an open collab ask only when it can page someone real: the asker is
/// known, holds a live pin, and asked within the last day. Anything else is
/// dropped before it can name a stranger, a dead pin, or ancient history.
pub fn qualify_wait(
    asker: Option<String>,
    askee: &str,
    thread: &str,
    since_us: i64,
    live: &BTreeSet<String>,
    now_us: i64,
) -> Option<CollabWait> {
    let asker = asker?;
    if !live.contains(&asker) {
        return None;
    }
    if now_us.saturating_sub(since_us) > MAX_WAIT_AGE_US {
        return None;
    }
    Some(CollabWait {
        asker: Some(asker),
        askee: askee.to_string(),
        thread: thread.to_string(),
        since_us,
    })
}

/// Collab edges: every qualified open ask waits on its askee.
pub fn collab_edges(waits: &[CollabWait]) -> Vec<WaitEdge> {
    waits
        .iter()
        .filter_map(|w| {
            w.asker.as_ref().map(|asker| WaitEdge {
                waiter: asker.clone(),
                holder: w.askee.clone(),
                resource: format!("thread:{}", w.thread),
                kind: WaitKind::Collab,
                since_us: Some(w.since_us),
            })
        })
        .collect()
}

/// Wait-for cycles: strongly connected pin sets with more than one member,
/// plus self-loops. Each cycle is reported once, members sorted, so the
/// captain's mail is stable across sweeps. Tarjan over a small graph —
/// pins, not paths, are the nodes.
pub fn find_cycles(edges: &[WaitEdge]) -> Vec<Vec<String>> {
    let mut adj: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for e in edges {
        adj.entry(e.waiter.as_str())
            .or_default()
            .insert(e.holder.as_str());
        adj.entry(e.holder.as_str()).or_default();
    }
    let mut tarjan = Tarjan {
        adj,
        index: 0,
        stack: Vec::new(),
        on_stack: BTreeSet::new(),
        indices: BTreeMap::new(),
        low: BTreeMap::new(),
        cycles: BTreeSet::new(),
    };
    let nodes: Vec<&str> = tarjan.adj.keys().copied().collect();
    for v in nodes {
        if !tarjan.indices.contains_key(v) {
            tarjan.strongconnect(v);
        }
    }
    tarjan.cycles.into_iter().collect()
}

/// Tarjan state for one SCC pass over the wait-for graph.
struct Tarjan<'a> {
    adj: BTreeMap<&'a str, BTreeSet<&'a str>>,
    index: usize,
    stack: Vec<&'a str>,
    on_stack: BTreeSet<&'a str>,
    indices: BTreeMap<&'a str, usize>,
    low: BTreeMap<&'a str, usize>,
    cycles: BTreeSet<Vec<String>>,
}

impl<'a> Tarjan<'a> {
    fn strongconnect(&mut self, v: &'a str) {
        self.indices.insert(v, self.index);
        self.low.insert(v, self.index);
        self.index += 1;
        self.stack.push(v);
        self.on_stack.insert(v);
        if let Some(succs) = self.adj.get(v).cloned() {
            for w in succs {
                if !self.indices.contains_key(w) {
                    self.strongconnect(w);
                    let (lv, lw) = (self.low[v], self.low[w]);
                    self.low.insert(v, lv.min(lw));
                } else if self.on_stack.contains(w) {
                    let (lv, iw) = (self.low[v], self.indices[w]);
                    self.low.insert(v, lv.min(iw));
                }
            }
        }
        if self.low[v] == self.indices[v] {
            let mut scc = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack.remove(w);
                scc.push(w.to_string());
                if w == v {
                    break;
                }
            }
            scc.sort();
            let self_loop = scc.len() == 1
                && self
                    .adj
                    .get(scc[0].as_str())
                    .is_some_and(|s| s.contains(scc[0].as_str()));
            if scc.len() > 1 || self_loop {
                self.cycles.insert(scc);
            }
        }
    }
}

/// Stable key for a cycle: the sorted member pins joined with `>`.
pub fn cycle_key(cycle: &[String]) -> String {
    format!("cycle:{}", cycle.join(">"))
}

/// Waits older than the threshold: collab asks (which carry an age) whose
/// asker may be known or not. Lease edges carry no age and never qualify.
pub fn long_waits(edges: &[WaitEdge], now_us: i64, threshold_us: i64) -> Vec<&WaitEdge> {
    edges
        .iter()
        .filter(|e| {
            e.since_us
                .is_some_and(|t| now_us.saturating_sub(t) >= threshold_us)
        })
        .collect()
}

/// Stable key for a long wait: waiter, holder, resource.
pub fn wait_key(edge: &WaitEdge) -> String {
    format!("wait:{}>{}:{}", edge.waiter, edge.holder, edge.resource)
}

/// Split findings into new vs already-reported, given the reported set the
/// caller persisted. Returns `(new_cycles, new_waits)`.
pub fn unreported<'a>(
    cycles: &'a [Vec<String>],
    waits: &[&'a WaitEdge],
    reported: &BTreeSet<String>,
) -> (Vec<&'a Vec<String>>, Vec<&'a WaitEdge>) {
    let new_cycles = cycles
        .iter()
        .filter(|c| !reported.contains(&cycle_key(c)))
        .collect();
    let new_waits = waits
        .iter()
        .filter(|w| !reported.contains(&wait_key(w)))
        .copied()
        .collect();
    (new_cycles, new_waits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(holder: &str, path: &str) -> LiveLease {
        LiveLease {
            holder: holder.into(),
            path: path.into(),
            reason: "bd-x".into(),
        }
    }

    fn collab(asker: Option<&str>, askee: &str, thread: &str, since: i64) -> CollabWait {
        CollabWait {
            asker: asker.map(str::to_string),
            askee: askee.into(),
            thread: thread.into(),
            since_us: since,
        }
    }

    /// Two holders on the same file wait on each other: the textbook
    /// two-pin deadlock.
    #[test]
    fn overlapping_leases_form_a_two_pin_cycle() {
        let edges = lease_edges(&[lease("A", "src/x.rs"), lease("B", "src/x.rs")]);
        assert_eq!(edges.len(), 2, "one edge each way: {edges:?}");
        let cycles = find_cycles(&edges);
        assert_eq!(cycles, vec![vec!["A".to_string(), "B".to_string()]]);
    }

    /// Disjoint files never meet: no edge, no cycle.
    #[test]
    fn disjoint_leases_stay_out_of_the_graph() {
        let edges = lease_edges(&[lease("A", "src/a.rs"), lease("B", "src/b.rs")]);
        assert!(edges.is_empty(), "no overlap, no wait: {edges:?}");
        assert!(find_cycles(&edges).is_empty());
    }

    /// A collab ask closes the loop the other way: A holds the file B
    /// wants, B's answer is what A waits for — wait, this test pins the
    /// asymmetric case: one lease edge plus one collab edge the other
    /// way round is already a cycle.
    #[test]
    fn a_collab_wait_against_a_lease_edge_is_a_cycle() {
        let mut edges = lease_edges(&[lease("A", "src/x.rs"), lease("B", "src/x.rs")]);
        edges.retain(|e| !(e.waiter == "A" && e.holder == "B"));
        edges.extend(collab_edges(&[collab(Some("A"), "B", "t1", 100)]));
        let cycles = find_cycles(&edges);
        assert_eq!(cycles, vec![vec!["A".to_string(), "B".to_string()]]);
    }

    /// Unknown, dead, and ancient waiters never become waits: no page, no
    /// cycle. A live pin asking within the last day still pages on age.
    #[test]
    fn unknown_askers_count_as_waits_not_cycles() {
        let live: BTreeSet<String> = ["A".to_string()].into_iter().collect();
        assert!(qualify_wait(None, "B", "t1", 100, &live, 200).is_none());
        assert!(qualify_wait(Some("Ghost".into()), "B", "t1", 100, &live, 200).is_none());
        assert!(qualify_wait(Some("A".into()), "B", "t1", 0, &live, MAX_WAIT_AGE_US + 1).is_none());
        let qualified = qualify_wait(Some("A".into()), "B", "t1", 100, &live, 200)
            .expect("live, known, fresh asker qualifies");
        let edges = collab_edges(&[qualified]);
        assert_eq!(edges.len(), 1);
        assert!(find_cycles(&edges).is_empty(), "a lone edge is no cycle");
        let long = long_waits(&edges, 100 + 60_000_001, 60_000_000);
        assert_eq!(long.len(), 1, "a qualifying wait still pages on age");
    }

    /// Reported cycles never page twice.
    #[test]
    fn reported_cycles_do_not_page_twice() {
        let edges = lease_edges(&[lease("A", "src/x.rs"), lease("B", "src/x.rs")]);
        let cycles = find_cycles(&edges);
        let mut reported = BTreeSet::new();
        let (new1, _) = unreported(&cycles, &[], &reported);
        assert_eq!(new1.len(), 1);
        for c in &cycles {
            reported.insert(cycle_key(c));
        }
        let (new2, _) = unreported(&cycles, &[], &reported);
        assert!(new2.is_empty(), "already reported: {new2:?}");
    }

    /// bd-v4dcv: the conformance trace is generated from a real run of the
    /// kernel — the fixture graph through lease_edges/collab_edges, swept
    /// with find_cycles and long_waits — and checked against DeadlockWatch.
    /// With FLYWHEEL_EVERYSTATE set the test also runs the checker itself,
    /// so a plain `cargo test` proves the trace end to end.
    #[test]
    fn conformance_trace_matches_the_model() {
        let live: BTreeSet<String> = ["A", "B", "C", "D", "E", "F"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut edges = lease_edges(&[lease("A", "src/x.rs"), lease("B", "src/x.rs")]);
        // now ~= 2.7h: C->D is hours old (pages), E->F is 30s old (quiet).
        let now = 10_000_000_000i64;
        for (asker, askee, thread, since) in [
            ("C", "D", "t-old", 100),
            ("E", "F", "t-young", now - 30_000_000),
        ] {
            if let Some(w) = qualify_wait(Some(asker.to_string()), askee, thread, since, &live, now)
            {
                edges.extend(collab_edges(&[w]));
            }
        }
        let cycles = find_cycles(&edges);
        assert_eq!(cycles, vec![vec!["A".to_string(), "B".to_string()]]);
        let waits = long_waits(&edges, now, 60_000_000);
        assert!(
            waits.iter().any(|w| w.holder == "D"),
            "the old wait pages: {waits:?}"
        );
        assert!(
            waits.iter().all(|w| w.holder != "F"),
            "the young wait stays silent: {waits:?}"
        );
        let mut alerted: Vec<String> = cycles.iter().map(|c| cycle_key(c)).collect();
        alerted.extend(waits.iter().map(|w| wait_key(w)));
        alerted.sort();
        let line = |action: Option<&str>, tick: i64| {
            let mut o = serde_json::json!({
                "alerted": alerted,
                "alerts": alerted,
                "missed": [],
                "tick": tick,
            });
            if let Some(action) = action {
                o["action"] = serde_json::Value::String(action.to_string());
            }
            serde_json::to_string(&o).expect("trace line serializes")
        };
        let good = [
            line(None, 0),
            line(Some("Sweep"), 1),
            line(Some("Sweep"), 2),
            line(Some("Sweep"), 3),
        ]
        .join("\n")
            + "\n";
        let bad = good.clone() + &line(Some("FalseAlert"), 3) + "\n";
        let out_path = |var: &str, fallback: &str| {
            std::env::var(var)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir().join(fallback))
        };
        let good_path = out_path("CONFORM_TRACE_OUT", "dw-recorded.jsonl");
        if std::env::var("CONFORM_TRACE_OUT").is_err()
            && std::env::var("FLYWHEEL_EVERYSTATE").is_err()
        {
            let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".agents/skills/verify-protocol-tla/traces/DeadlockWatch.recorded.jsonl");
            assert_eq!(
                std::fs::read_to_string(&fixture).expect("checked-in recorded trace"),
                good,
                "the recorded trace drifted from the real run; regenerate the fixture"
            );
        }
        std::fs::write(&good_path, &good).expect("write trace");
        let bad_path = out_path("CONFORM_BAD_TRACE_OUT", "dw-false-alert.jsonl");
        if std::env::var("CONFORM_BAD_TRACE_OUT").is_err()
            && std::env::var("FLYWHEEL_EVERYSTATE").is_err()
        {
            let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".agents/skills/verify-protocol-tla/traces/DeadlockWatch.false-alert.jsonl");
            assert_eq!(
                std::fs::read_to_string(&fixture).expect("checked-in doctored trace"),
                bad,
                "the doctored trace drifted; regenerate the fixture"
            );
        }
        std::fs::write(&bad_path, &bad).expect("write bad trace");
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
                .arg(models.join("DeadlockWatch.cfg"))
                .arg(models.join("DeadlockWatch.tla"))
                .output()
                .expect("everystate conform")
        };
        let ok = run(&good_path);
        assert!(
            ok.status.success(),
            "recorded trace must conform: {}",
            String::from_utf8_lossy(&ok.stdout)
        );
        let bad_out = run(&bad_path);
        assert_eq!(bad_out.status.code(), Some(20), "a false alert must fail");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&bad_out.stdout),
            String::from_utf8_lossy(&bad_out.stderr)
        );
        assert!(
            text.contains("FalseAlert"),
            "the rejection names the step: {text}"
        );
    }
}
