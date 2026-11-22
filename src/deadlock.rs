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

/// Collab edges: every open ask waits on its askee. An unknown asker is
/// the `(unknown)` node: out-edges only, so it can never close a cycle it
/// does not belong to, but its age still pages.
pub fn collab_edges(waits: &[CollabWait]) -> Vec<WaitEdge> {
    waits
        .iter()
        .map(|w| WaitEdge {
            waiter: w.asker.clone().unwrap_or_else(|| "(unknown)".to_string()),
            holder: w.askee.clone(),
            resource: format!("thread:{}", w.thread),
            kind: WaitKind::Collab,
            since_us: Some(w.since_us),
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

    /// Unknown askers never close a cycle (the `(unknown)` node has no
    /// in-edges), but their age still pages.
    #[test]
    fn unknown_askers_count_as_waits_not_cycles() {
        let edges = collab_edges(&[collab(None, "B", "t1", 100)]);
        assert_eq!(edges.len(), 1);
        assert!(find_cycles(&edges).is_empty(), "no in-edge, no cycle");
        let long = long_waits(&edges, 100 + 60_000_001, 60_000_000);
        assert_eq!(long.len(), 1, "the unknown wait still pages on age");
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
}
