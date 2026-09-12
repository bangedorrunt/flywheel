//! Captain classifier (pre-invoke). Pure: no IO.
//!
//! Picks ONE primitive from `target` + `snap` using the captain-selector
//! table (docs/agent-instructions/orchestration.md), evaluated in table
//! order — first match wins. Engine control-flow and the durable loop stay
//! untouched. `flywheel decide` prints the Decision; the captain runs argv.
//!
//! governed-by: ADR-0005/0010/0011/0012, bd-5inr

/// Live facts the classifier may read. Absent facts stay zero/false.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub ready_bead_count: u32,
    pub profiled_panes: u32,
    pub launch_pending: u32,
    pub contention: bool,
}

/// One classified command. `primitive` is the stable id; `argv` is what to run.
/// Angle-bracket argv slots (`<role>`, `<full-name>`) are filled by the caller
/// (captain / `flywheel decide`); the classifier cannot know them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub primitive: &'static str,
    pub reason: String,
    pub argv: Vec<String>,
    /// ADR-0011 D1: topology is a briefing axis, never collapsed into the
    /// workflow primitive. Join is decided by the briefing world, not here.
    pub topology: &'static str,
}

fn has(t: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| t.contains(n))
}

/// Pull a `--parts` value out of free text (`parts: a,b`). Empty / missing
/// yields the angle-bracket slot so clap never sees a bare `--parts`.
fn compose_parts_value(target: &str) -> String {
    const MARK: &str = "parts:";
    let lower = target.to_ascii_lowercase();
    if let Some(idx) = lower.find(MARK) {
        let rest = target[idx + MARK.len()..].trim();
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    "<role:stage,...>".to_string()
}

/// Topology is declared per routing row (ADR-0032 Spec 9): the one catalog
/// table below is the only map. Unknown primitives are explicit `"none"` —
/// the silent `_ => "swarm"` bucket is deleted.
pub fn topology_for_primitive(primitive: &str) -> &'static str {
    ROUTING_TABLE
        .iter()
        .find(|e| e.primitive == primitive)
        .map(|e| e.topology)
        .unwrap_or("none")
}

/// Total pane budget N from free text. `"2 crews"` / `"3 crew"` / `"coder:2"`.
/// Captain cut: N is total panes, not coder-N.
pub fn parse_budget_n(target: &str) -> Option<u32> {
    let lower = target.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != ':')
        .filter(|w| !w.is_empty())
        .collect();
    for pair in words.windows(2) {
        if matches!(pair[1], "crew" | "crews")
            && let Ok(n) = pair[0].parse::<u32>()
            && n >= 1
        {
            return Some(n);
        }
    }
    for w in &words {
        for prefix in ["coder:", "n:", "crew:"] {
            if let Some(rest) = w.strip_prefix(prefix)
                && let Ok(n) = rest.parse::<u32>()
                && n >= 1
            {
                return Some(n);
            }
        }
    }
    None
}

fn apply_crew_budget(d: &mut Decision, target: &str) {
    let Some(n) = parse_budget_n(target) else {
        return;
    };
    if d.argv.iter().any(|a| a == "--crew") {
        return;
    }
    if matches!(
        d.primitive,
        "wave-swarm" | "repair-until-verified" | "beads-lifecycle"
    ) {
        d.argv.push("--crew".into());
        d.argv.push(n.to_string());
    }
}

// ---- ADR-0032 Spec 9 (bd-himk): one catalog-driven routing table ----
//
// The flywheel-owned static routing catalog. `decide` and `briefing`
// consume this one compiled table: matchers are declarative data,
// overlapping claims fail `compile_routing_table` loud, and table order
// never decides (most-specific needle wins; ties are explicit none).
//
// governed-by: ADR-0032 Spec 9

/// Which snapshots/words admit a routing row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteGate {
    /// Eligible whenever a matcher hits.
    Always,
    /// Diagnosis-only rows: never for implement/fix targets.
    NoImplement,
    /// implement/fix + beads|tracker|gate, minus "no tracker".
    ImplementTracker,
    /// implement/fix + no tracker|direct.
    ImplementDirect,
    /// Ready beads + work-ish intent, never implement/fix.
    ReadyBeadsWork,
    /// snap.contention admits even with no keyword; otherwise keywords.
    ContentionOr,
    /// babysit AND a bead-ish word must both appear.
    Babysit,
    /// implement/fix AND a bug-shaped word must both appear.
    BugFixShaped,
}

/// How a routed row builds its argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteArgv {
    /// flywheel workflow run <primitive> --execute --mode <mode>.
    WorkflowRun {
        mode: &'static str,
    },
    /// flywheel workflow run <primitive> --execute (pause: no --mode).
    WorkflowRunNoMode,
    /// flywheel workflow compose <target> --agent.
    ComposeAgent,
    /// flywheel workflow compose <target> --parts <extracted value>.
    ComposeParts,
    /// flywheel workflow compose <target> --parts <token> --run.
    ComposeToken {
        token: &'static str,
    },
    Steer,
    SteerDirect,
    LoopWork,
    Join,
    SpawnSend,
    CoalitionForm,
    CronStart,
}

/// One declarative routing row: matchers + gate + argv shape + topology.
/// A workflow's `decide_trigger_for` phrase is always `matchers[0]`.
#[derive(Debug, Clone, Copy)]
pub struct RouteEntry {
    pub primitive: &'static str,
    pub matchers: &'static [&'static str],
    pub topology: &'static str,
    pub gate: RouteGate,
    pub argv: RouteArgv,
    pub reason: &'static str,
}

/// The one routing table. Order is cosmetic: resolution is earliest-match
/// position, then longest needle — never first-match-wins.
pub static ROUTING_TABLE: &[RouteEntry] = &[
    RouteEntry {
        primitive: "captain-audit",
        matchers: &["audit", "read-only", "analyze"],
        topology: "open",
        gate: RouteGate::NoImplement,
        argv: RouteArgv::WorkflowRun { mode: "headless" },
        reason: "read-only analysis — captain-audit headless",
    },
    RouteEntry {
        primitive: "bug-fix",
        matchers: &["repro", "regression", "bug-fix"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "repro/regression/bug-shaped fix — bug-fix workflow (repro first)",
    },
    RouteEntry {
        primitive: "bug-fix",
        matchers: &[
            "bug", "crash", "panic", "leak", "broken", "failing", "error",
        ],
        topology: "swarm",
        gate: RouteGate::BugFixShaped,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "repro/regression/bug-shaped fix — bug-fix workflow (repro first)",
    },
    RouteEntry {
        primitive: "refactoring",
        matchers: &["refactor"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "refactor — refactoring workflow (invariant stays green)",
    },
    RouteEntry {
        primitive: "prototype",
        matchers: &["prototype", "sketch", "mock"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "prototype/sketch — prototype workflow (2-3 arms, oracle picks)",
    },
    RouteEntry {
        primitive: "arena",
        matchers: &["arena"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "arena — model race on one brief (declared selection rule)",
    },
    RouteEntry {
        primitive: "perf",
        matchers: &["perf", "p95", "slow"],
        topology: "swarm",
        gate: RouteGate::NoImplement,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "perf/p95/slow — perf workflow (metric + budget)",
    },
    RouteEntry {
        primitive: "hillclimb",
        matchers: &["hillclimb"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "hillclimb — hillclimb workflow (one commit per accepted win)",
    },
    RouteEntry {
        primitive: "forensics",
        matchers: &["forensic", "leak", "idle cpu", "spindump"],
        topology: "open",
        gate: RouteGate::NoImplement,
        argv: RouteArgv::WorkflowRun { mode: "headless" },
        reason: "forensics — diagnosis only, no fix step (bug-fix consumes Act-on)",
    },
    RouteEntry {
        primitive: "babysit-bead",
        matchers: &["babysit"],
        topology: "open",
        gate: RouteGate::Babysit,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "babysit a bead — babysit-bead workflow (no new beads)",
    },
    RouteEntry {
        primitive: "pickup",
        matchers: &["pickup", "resume session"],
        topology: "loop",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "pickup / resume session — pickup workflow",
    },
    RouteEntry {
        primitive: "pause",
        matchers: &["pause", "going offline"],
        topology: "loop",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRunNoMode,
        reason: "pause / going offline — checkpoint + standing handoff",
    },
    RouteEntry {
        primitive: "interrogate",
        matchers: &["interrogate", "tear this apart", "adversarial review"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "headless" },
        reason: "interrogate — N-model adversarial review (read-only)",
    },
    RouteEntry {
        primitive: "figure-it-out",
        matchers: &["figure it out", "no playbook"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::ComposeAgent,
        reason: "no matching playbook — planner compose, lint is the gate",
    },
    RouteEntry {
        primitive: "ship-beads",
        matchers: &["ship beads", "land the beads"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "ship beads — ledger-green then housekeeper commit",
    },
    RouteEntry {
        primitive: "feature",
        matchers: &["feature"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "feature — feature workflow (shape -> build -> prove)",
    },
    RouteEntry {
        primitive: "goal",
        matchers: &["goal"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "goal — meta-workflow (ce-brainstorm -> plan -> beads -> build)",
    },
    RouteEntry {
        primitive: "triage-deep",
        matchers: &["triage-deep", "triage deep", "deep triage"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "triage-deep — deep backlog triage (bv robot-triage -> spawn crew)",
    },
    RouteEntry {
        primitive: "triage",
        matchers: &["triage"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "triage — inbound message triage (ack + dispatch)",
    },
    RouteEntry {
        primitive: "weekly-review",
        matchers: &["weekly-review", "weekly review"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "weekly-review — weekly repo review (scout fanout -> oracle idea)",
    },
    RouteEntry {
        primitive: "autonomous-work",
        matchers: &["autonomous-work", "autonomous work"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "autonomous-work — autonomous overnight work (loop run --work then ship)",
    },
    RouteEntry {
        primitive: "reflect",
        matchers: &["reflect"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "reflect — corrections become structure (hook bead, never a paragraph)",
    },
    RouteEntry {
        primitive: "ce-compound",
        matchers: &[
            "compound the learning",
            "capture the learning",
            "capture what we learned",
            "compound this",
        ],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "ce-compound — run the compound gate over verified work (docs/learnings/)",
    },
    RouteEntry {
        primitive: "ce-sweep",
        matchers: &["sweep the learnings", "learning sweep", "compound-refresh"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "ce-sweep — maintain docs/learnings/ (keep/update/consolidate/delete)",
    },
    RouteEntry {
        primitive: "awareness-digest",
        matchers: &["awareness-digest", "awareness digest"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "headless" },
        reason: "awareness-digest — captain digest (presence + reservations + guard)",
    },
    RouteEntry {
        primitive: "onboard",
        matchers: &["onboard"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "onboard — cold-start newcomer (map repo -> seed first task)",
    },
    RouteEntry {
        primitive: "join",
        matchers: &["join the fleet", "join fleet", "joining"],
        topology: "join",
        gate: RouteGate::Always,
        argv: RouteArgv::Join,
        reason: "join the live fleet — arrive, do not spawn a second swarm",
    },
    RouteEntry {
        primitive: "spawn-send",
        matchers: &[
            "one pane",
            "watch me",
            "single agent",
            "typo",
            "one-line",
            "one line",
        ],
        topology: "open",
        gate: RouteGate::Always,
        argv: RouteArgv::SpawnSend,
        reason: "one pane — spawn --profile <role> --count 1, then send --to <full-name> --wait (never --oc)",
    },
    // Declared wave trigger only: bare swarm/lanes/waves substrings and lone
    // budgets never route here (the old wave_shaped sniffing is deleted).
    RouteEntry {
        primitive: "wave-swarm",
        matchers: &["wave-swarm", "wave swarm"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::ComposeToken {
            token: "wave-swarm",
        },
        reason: "wave-swarm — composed wave swarm (GATE -> DISPATCH -> OBSERVE -> VERIFY -> SIGNOFF)",
    },
    RouteEntry {
        primitive: "repair-until-verified",
        matchers: &["repair-until-verified", "repair until verified"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::ComposeToken {
            token: "repair-until-verified",
        },
        reason: "repair-until-verified — compose --parts repair-until-verified --run",
    },
    RouteEntry {
        primitive: "debate",
        matchers: &[
            "argue both sides",
            "devil's advocate",
            "debate the",
            "steelman",
        ],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "debate — adversarial briefs, oracle decides",
    },
    RouteEntry {
        primitive: "ce-brainstorm",
        matchers: &[
            "brainstorm",
            "sharpen the requirements",
            "requirements-only",
            "ideate",
        ],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "ce-brainstorm — requirements-first ideation (oracles diverge, planner reduces)",
    },
    RouteEntry {
        primitive: "ce-plan",
        matchers: &[
            "break this down",
            "bead-ready plan",
            "decompose this goal",
            "make a bead-ready plan",
        ],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "ce-plan — goal to bead-ready plan, approval gate",
    },
    RouteEntry {
        primitive: "ship",
        matchers: &[
            "ship the chain",
            "run the ship",
            "ship chain",
            "resolve to verify",
        ],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "ship — resolve to verify gated chain",
    },
    RouteEntry {
        primitive: "beads",
        matchers: &[
            "convert to beads",
            "file these as beads",
            "track this plan",
            "beads from this plan",
        ],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::WorkflowRun { mode: "pane" },
        reason: "beads — plan to tracker (planner files the beads)",
    },
    RouteEntry {
        primitive: "loop-work",
        matchers: &["work", "loop", "beads"],
        topology: "loop",
        gate: RouteGate::ReadyBeadsWork,
        argv: RouteArgv::LoopWork,
        reason: "ready beads — loop-mode run <project> --work",
    },
    RouteEntry {
        primitive: "steer",
        matchers: &["beads", "tracker", "gate"],
        topology: "swarm",
        gate: RouteGate::ImplementTracker,
        argv: RouteArgv::Steer,
        reason: "implement + tracker/gate — steer",
    },
    RouteEntry {
        primitive: "steer-direct",
        matchers: &["no tracker", "direct"],
        topology: "swarm",
        gate: RouteGate::ImplementDirect,
        argv: RouteArgv::SteerDirect,
        reason: "implement, no tracker — steer --direct",
    },
    // Plane-S steering is an explicit route, never the silent fallback.
    RouteEntry {
        primitive: "steer",
        matchers: &["crew-steer", "mention peer", "peer mention"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::Steer,
        reason: "crew steer / peer mention — steer on plane S (channel post --mention / job claim)",
    },
    RouteEntry {
        primitive: "compose-parts",
        matchers: &["compose", "catalog miss", "parts:"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::ComposeParts,
        reason: "catalog cannot express the named parts — compose --parts",
    },
    RouteEntry {
        primitive: "compose-agent",
        matchers: &["shape unknown", "improvise", "author workflow"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::ComposeAgent,
        reason: "shape unknown — planner arm, lint is the gate",
    },
    RouteEntry {
        primitive: "coalition-form",
        matchers: &["contention", "coalition", "shared-file"],
        topology: "swarm",
        gate: RouteGate::ContentionOr,
        argv: RouteArgv::CoalitionForm,
        reason: "file contention — coalition, not a solo dispatch",
    },
    RouteEntry {
        primitive: "cron-start",
        matchers: &["cron", "schedule", "nightly"],
        topology: "swarm",
        gate: RouteGate::Always,
        argv: RouteArgv::CronStart,
        reason: "scheduled work — cron YAML + cron start",
    },
];

/// Earliest match position, longest needle on ties: among the needles of
/// one row that hit, the smallest index wins and the longest needle breaks
/// index ties. Returns (pos, len).
fn earliest_hit(matchers: &[&str], t: &str) -> Option<(usize, usize)> {
    matchers
        .iter()
        .filter_map(|m| t.find(m).map(|pos| (pos, m.len())))
        .min_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
}

impl RouteEntry {
    /// Match key when this row admits the target. Ambient-only matches
    /// (empty goal, contention with no keyword) sort last so an explicit
    /// mention always beats world state.
    fn match_key(&self, t: &str, implement: bool, snap: &Snapshot) -> Option<(usize, usize)> {
        match self.gate {
            RouteGate::Always => earliest_hit(self.matchers, t),
            RouteGate::NoImplement => (!implement)
                .then(|| earliest_hit(self.matchers, t))
                .flatten(),
            RouteGate::ImplementTracker => (implement && !has(t, &["no tracker"]))
                .then(|| earliest_hit(self.matchers, t))
                .flatten(),
            RouteGate::ImplementDirect => {
                implement.then(|| earliest_hit(self.matchers, t)).flatten()
            }
            RouteGate::ReadyBeadsWork => {
                if snap.ready_bead_count == 0 || implement {
                    return None;
                }
                if t.trim().is_empty() {
                    return Some((usize::MAX, 0));
                }
                earliest_hit(self.matchers, t)
            }
            RouteGate::ContentionOr => {
                if snap.contention {
                    return Some(earliest_hit(self.matchers, t).unwrap_or((usize::MAX, 0)));
                }
                earliest_hit(self.matchers, t)
            }
            RouteGate::Babysit => (has(t, &["babysit"]) && has(t, &["bead", "tracker", "issue"]))
                .then(|| earliest_hit(self.matchers, t))
                .flatten(),
            RouteGate::BugFixShaped => implement.then(|| earliest_hit(self.matchers, t)).flatten(),
        }
    }
}

/// Loud load check: one needle claimed by two different routes with
/// compatible gates fails. Same-primitive rows share one route and never
/// conflict; disjoint gates (implement vs diagnosis-only) share needles
/// honestly (e.g. "beads" on steer vs loop-work, "leak" on bug-fix vs
/// forensics).
pub fn check_routing_table(table: &[RouteEntry]) -> Result<(), String> {
    for (i, a) in table.iter().enumerate() {
        for b in &table[i + 1..] {
            if a.primitive == b.primitive {
                continue;
            }
            for m in a.matchers {
                if b.matchers.contains(m) && gates_compatible(a.gate, b.gate) {
                    return Err(format!(
                        "routing overlap: needle {m:?} claimed by both {:?} and {:?}",
                        a.primitive, b.primitive
                    ));
                }
            }
        }
    }
    Ok(())
}

fn gates_compatible(a: RouteGate, b: RouteGate) -> bool {
    // 0 = diagnosis-only, 1 = implement-only, 2 = either.
    fn implement_req(g: RouteGate) -> u8 {
        match g {
            RouteGate::NoImplement | RouteGate::ReadyBeadsWork => 0,
            RouteGate::ImplementTracker | RouteGate::ImplementDirect | RouteGate::BugFixShaped => 1,
            RouteGate::Always | RouteGate::ContentionOr | RouteGate::Babysit => 2,
        }
    }
    let (x, y) = (implement_req(a), implement_req(b));
    x == 2 || y == 2 || x == y
}

/// Table-wide invariants that only make sense for the REAL table (a synthetic
/// table in a test carries names there is no catalog to compare against).
///
/// ADR-0032 Spec 9: one table is the single source for primitive, matcher,
/// argv shape, and topology — so its load must be loud on every way the table
/// could produce two routes or a dangling claim.
pub fn check_catalog(table: &[RouteEntry]) -> Result<(), String> {
    for e in table {
        if e.matchers.is_empty() {
            return Err(format!(
                "catalog: primitive {:?} declares no matchers — a route needs a trigger",
                e.primitive
            ));
        }
    }
    for (i, a) in table.iter().enumerate() {
        for b in &table[i + 1..] {
            if a.primitive == b.primitive {
                continue;
            }
            if a.matchers[0] == b.matchers[0] {
                return Err(format!(
                    "catalog: trigger {:?} claims both {:?} and {:?} — \
                     decide_trigger_for would be ambiguous",
                    a.matchers[0], a.primitive, b.primitive
                ));
            }
        }
    }
    for name in UNROUTED_PRIMITIVES {
        if !table.iter().any(|e| e.primitive == *name) {
            return Err(format!(
                "catalog: {name:?} is declared unrouted but has no routing row (stale entry)"
            ));
        }
    }
    Ok(())
}

/// Spec 9 cross-check: every row that dispatches `workflow run <primitive>`
/// must name a workflow the catalog actually resolves. `known` is the merged
/// catalog's names (builtin < user < project); decide.rs stays pure by taking
/// them as an argument. Loud, naming the orphaned route.
pub fn check_workflow_routes(table: &[RouteEntry], known: &[String]) -> Result<(), String> {
    for e in table {
        if !matches!(
            e.argv,
            RouteArgv::WorkflowRun { .. } | RouteArgv::WorkflowRunNoMode
        ) {
            continue;
        }
        if !known.iter().any(|k| k == e.primitive) {
            return Err(format!(
                "catalog: route {:?} dispatches `workflow run` but no such workflow exists \
                 (flywheel workflow list)",
                e.primitive
            ));
        }
    }
    Ok(())
}

/// Compile the one routing table. Loud on overlap, on an ambiguous trigger, and
/// on a dangling claim — callers fail closed.
pub fn compile_routing_table() -> Result<(), String> {
    check_routing_table(ROUTING_TABLE)?;
    check_catalog(ROUTING_TABLE)
}

fn none_decision(target: &str, why: String) -> Decision {
    Decision {
        primitive: "none",
        reason: format!("no catalog route for {target:?} — {why}"),
        argv: Vec::new(),
        topology: "none",
    }
}

fn build_decision(entry: &RouteEntry, target: &str, snap: &Snapshot) -> Decision {
    let p = entry.primitive;
    let argv: Vec<String> = match entry.argv {
        RouteArgv::WorkflowRun { mode } => [
            "flywheel",
            "workflow",
            "run",
            p,
            "--execute",
            "--mode",
            mode,
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect(),
        RouteArgv::WorkflowRunNoMode => ["flywheel", "workflow", "run", p, "--execute"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        RouteArgv::ComposeAgent => ["flywheel", "workflow", "compose", target, "--agent"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        RouteArgv::ComposeParts => {
            let parts = compose_parts_value(target);
            ["flywheel", "workflow", "compose", target, "--parts", &parts]
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        }
        RouteArgv::ComposeToken { token } => [
            "flywheel", "workflow", "compose", target, "--parts", token, "--run",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect(),
        RouteArgv::Steer => ["flywheel", "steer", target]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        RouteArgv::SteerDirect => ["flywheel", "steer", target, "--direct"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        RouteArgv::LoopWork => ["flywheel", "loop-mode", "run", "<project>", "--work"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        RouteArgv::Join => ["flywheel", "arrive"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        RouteArgv::SpawnSend => [
            "flywheel",
            "spawn",
            "--profile",
            "<role>",
            "--count",
            "1",
            "send",
            "--to",
            "<full-name>",
            "--wait",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect(),
        RouteArgv::CoalitionForm => ["flywheel", "coalition", "form"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        RouteArgv::CronStart => ["flywheel", "cron", "start"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    };
    let reason = if p == "loop-work" {
        format!(
            "{} ready beads — loop-mode run <project> --work",
            snap.ready_bead_count
        )
    } else {
        entry.reason.to_string()
    };
    Decision {
        primitive: p,
        reason,
        argv,
        topology: entry.topology,
    }
}

/// Classify `target` + `snap` into one primitive. No harness special case.
///
/// `decide` is the classifier leaf: it resolves the one routing table and
/// applies the crew budget. Briefing consumes its output directly.
pub fn decide(target: &str, snap: &Snapshot) -> Decision {
    let mut d = classify(target, snap);
    apply_crew_budget(&mut d, target);
    d
}

/// Classify `target` + `snap` against the one routing table.
///
/// Resolution: earliest match position wins (fronted intent beats an
/// incidental later mention — "goal: ship the onboarding flow" is goal, not
/// onboard); same-position contention resolves to the longest needle
/// (triage-deep beats triage); exact ties across different primitives and
/// zero matches are explicit `none`. Table order never decides.
/// Implement-bearing targets never fall to a diagnosis-only workflow
/// (Spec 8 guards live in the row gates).
fn classify(target: &str, snap: &Snapshot) -> Decision {
    if let Err(e) = compile_routing_table() {
        return none_decision(target, format!("routing table invalid: {e}"));
    }
    let t = target.to_ascii_lowercase();
    let implement = has(&t, &["implement", "fix"]);
    let hits: Vec<((usize, usize), &RouteEntry)> = ROUTING_TABLE
        .iter()
        .filter_map(|e| e.match_key(&t, implement, snap).map(|key| (key, e)))
        .collect();
    pick(hits, target, snap)
}

/// Resolve collected row hits: earliest position, then longest needle, then
/// explicit none on exact cross-primitive ties. Pure and order-free.
fn pick(mut hits: Vec<((usize, usize), &RouteEntry)>, target: &str, snap: &Snapshot) -> Decision {
    if hits.is_empty() {
        return none_decision(target, "declare a trigger or compose --agent".to_string());
    }
    hits.sort_by(|a, b| a.0.0.cmp(&b.0.0).then(b.0.1.cmp(&a.0.1)));
    let best = hits[0].0;
    let top: Vec<&RouteEntry> = hits
        .into_iter()
        .filter(|(key, _)| *key == best)
        .map(|(_, e)| e)
        .collect();
    let mut prims: Vec<&str> = top.iter().map(|e| e.primitive).collect();
    prims.sort_unstable();
    prims.dedup();
    if prims.len() == 1 {
        let entry = top.into_iter().find(|e| e.primitive == prims[0]).unwrap();
        return build_decision(entry, target, snap);
    }
    none_decision(
        target,
        format!(
            "ambiguous goal — {} both claim it; narrow the wording",
            prims.join(" and ")
        ),
    )
}

// ---- bd-x6x4: decide trigger phrase per builtin (workflow list --json) ----

/// Primitives that are deliberately NOT keyword-routed: the command-adjacent
/// shapes the captain invokes by name (`steer`), the lifecycle fan-out
/// (`join` / `spawn-send` / `loop-work`), and the compose / cron forms.
///
/// This list is the ONLY place "not routed" is stated. Everything outside it
/// is keyword-routed, and its trigger is derived from the table below.
pub const UNROUTED_PRIMITIVES: &[&str] = &[
    "coalition-form",
    "compose-agent",
    "compose-parts",
    "cron-start",
    "join",
    "loop-work",
    "spawn-send",
    "steer",
    "steer-direct",
];

/// The decide keyword phrase that routes to a builtin via `flywheel decide`.
///
/// ADR-0032 Spec 9: this is DERIVED from the one routing table — the phrase IS
/// the row's first matcher, never a second hand-kept list that can drift from
/// what `decide` actually matches. `None` means the primitive is declared
/// unrouted ([`UNROUTED_PRIMITIVES`]) or has no row at all (command-adjacent
/// / recipe / phase-chain names).
pub fn decide_trigger_for(workflow: &str) -> Option<&'static str> {
    if UNROUTED_PRIMITIVES.contains(&workflow) {
        return None;
    }
    ROUTING_TABLE
        .iter()
        .find(|e| e.primitive == workflow)
        .and_then(|e| e.matchers.first().copied())
}

// ---- ADR-0022 §3.7 T4 / W7: delegated composition coverage (bd-adr22-w7) ----

/// The compose-request archetype (§3.7 T4): the shape of work a mailed
/// objective asks for, driving which stage-first parts a candidate carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archetype {
    /// explore/map/survey — fan a scout across the target
    ScoutFanout,
    /// verify/prove/fence — repro → red → fix → prove
    VerifyFence,
    /// debate/argue/compare arms — judge competing approaches
    Debate,
    /// hillclimb/measure/optimize — baseline → keep-or-discard
    Hillclimb,
    /// review/audit — adversarial read of existing work
    Review,
}

impl Archetype {
    /// The tag surfaced in candidate rendering.
    pub fn tag(self) -> &'static str {
        match self {
            Self::ScoutFanout => "scout-fanout",
            Self::VerifyFence => "verify-fence",
            Self::Debate => "debate",
            Self::Hillclimb => "hillclimb",
            Self::Review => "review",
        }
    }

    /// The stage-first parts this archetype composes from (real seed stages;
    /// W6's affinity resolution dispatches each to its default role).
    pub fn default_parts(self) -> &'static [&'static str] {
        match self {
            Self::ScoutFanout => &["fresh-eyes-explore"],
            Self::VerifyFence => &["repro-baseline", "tdd-red", "minimal-fix", "prove-verify"],
            Self::Debate => &["judge-arms"],
            Self::Hillclimb => &["baseline-measure", "measure-keep-or-discard"],
            Self::Review => &["ce-review"],
        }
    }

    fn classify(goal: &str) -> Self {
        let t = goal.to_lowercase();
        if has(
            &t,
            &[
                "debate",
                "argue",
                "compare arms",
                "judge arms",
                "two approaches",
            ],
        ) {
            Self::Debate
        } else if has(
            &t,
            &["hillclimb", "keep-or-discard", "measure against baseline"],
        ) {
            Self::Hillclimb
        } else if has(
            &t,
            &[
                "verify",
                "fence",
                "repro",
                "regression",
                "prove",
                "red-green",
                "failing test",
            ],
        ) {
            Self::VerifyFence
        } else if has(&t, &["review", "audit", "fresh eyes", "code check"]) {
            Self::Review
        } else {
            Self::ScoutFanout
        }
    }
}

/// One catalog entry as (name, description?) for coverage matching.
pub type CatalogEntry<'a> = (&'a str, Option<&'a str>);

/// The §3.7 T4 coverage report: does ANY catalog workflow cover `goal`?
/// A hit names the workflow; a miss renders the archetype-matched compose
/// candidate FOR APPROVAL (W7 never auto-runs it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    pub covered: bool,
    /// The covering workflow name when covered.
    pub matched: Option<String>,
    pub archetype: Archetype,
    /// Some(...) only on a miss — the rendered-for-approval candidate.
    pub candidate_parts: Option<Vec<String>>,
}

/// Significant-token coverage: a goal token counts when >=4 chars and not a
/// stopword; the entry covers when every such token appears in its name or
/// description. Pure + deterministic.
fn goal_tokens(goal: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "this", "that", "then", "into", "from", "make", "make",
        "want", "need", "please", "should", "would", "work", "workflow",
    ];
    goal.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| w.len() >= 4 && !STOP.contains(&w.as_str()))
        .collect()
}

pub fn coverage_report(goal: &str, catalog: &[CatalogEntry]) -> CoverageReport {
    let tokens = goal_tokens(goal);
    let archetype = Archetype::classify(goal);
    let mut matched = None;
    if !tokens.is_empty() {
        for (name, desc) in catalog {
            let hay = format!("{name} {}", desc.unwrap_or("")).to_lowercase();
            if tokens.iter().all(|t| hay.contains(t.as_str())) {
                matched = Some((*name).to_string());
                break;
            }
        }
    }
    let covered = matched.is_some();
    CoverageReport {
        covered,
        matched,
        archetype,
        candidate_parts: if covered {
            None
        } else {
            Some(
                archetype
                    .default_parts()
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
            )
        },
    }
}

/// Render a coverage-miss candidate FOR APPROVAL. The text is the contract:
/// it names the objective, archetype, proposed parts, and says explicitly
/// that nothing runs until an operator approves (`workflow compose --run`).
pub fn render_compose_candidate_for_approval(
    requester: &str,
    objective: &str,
    archetype: Archetype,
    parts: &[String],
) -> String {
    format!(
        "COMPOSE CANDIDATE (approval required — W7 never auto-runs): requester {requester} \
         mailed a compose-request with no catalog coverage.\n  objective: {objective}\n  \
         archetype: {}\n  parts: {}\nApprove with: flywheel workflow compose '{objective}' \
         --parts '{}' --run",
        archetype.tag(),
        parts.join(","),
        parts.join(",")
    )
}

/// The contact/policy allowlist gate (§3.7 T4): a compose-request dispatches
/// ONLY from an allowlisted sender. Exact pin match after trim — deny is the
/// default, and the deny case NEVER dispatches (the loop drops it with a log).
pub fn compose_request_allowed(sender: &str, allowlist: &[String]) -> bool {
    let s = sender.trim();
    !s.is_empty() && allowlist.iter().any(|a| a.trim() == s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot::default()
    }

    fn with_beads(n: u32) -> Snapshot {
        Snapshot {
            ready_bead_count: n,
            ..Snapshot::default()
        }
    }

    // Row 1 — audit/read-only/analyze (not implement/fix) → captain-audit.
    #[test]
    fn row1_audit_is_captain_audit() {
        for t in [
            "audit the mailbox layer",
            "read-only review of the loop",
            "analyze the spawn backlog",
        ] {
            // Beads ready must not win over a read-only audit.
            let d = decide(t, &with_beads(5));
            assert_eq!(d.primitive, "captain-audit", "target: {t}");
            assert!(
                d.argv.contains(&"captain-audit".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(
                d.argv.contains(&"--mode".to_string()) && d.argv.contains(&"headless".to_string()),
                "headless workflow run: {:?}",
                d.argv
            );
        }
    }

    // Row 1 boundary — audit + implement/fix must NOT classify as audit.
    #[test]
    fn row1_audit_plus_implement_is_steer_not_audit() {
        let d = decide("audit then fix the tracker", &snap());
        assert_ne!(d.primitive, "captain-audit");
        assert_eq!(d.primitive, "steer");
    }

    // Row 2 — ready beads + empty|work|loop|beads → loop-work.
    #[test]
    fn row2_ready_beads_is_loop_work() {
        for t in ["", "work the ready beads", "loop", "beads"] {
            let d = decide(t, &with_beads(3));
            assert_eq!(d.primitive, "loop-work", "target: {t:?}");
            assert!(
                d.argv.windows(2).any(|w| w == ["loop-mode", "run"]),
                "argv: {:?}",
                d.argv
            );
            assert!(
                d.argv.contains(&"<project>".to_string()),
                "loop-work argv must name the project slot: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"--work".to_string()));
        }
    }

    // Row 2 boundary — no ready beads → not loop-work even for "work".
    #[test]
    fn row2_no_ready_beads_is_not_loop_work() {
        let d = decide("work", &snap());
        assert_ne!(d.primitive, "loop-work");
    }

    // Row 3 — implement/fix + beads|tracker|gate → steer.
    #[test]
    fn row3_implement_with_tracker_is_steer() {
        // Beads ready must not hijack an implement request into loop-work.
        let d = decide("implement the parser with a plan gate", &with_beads(3));
        assert_eq!(d.primitive, "steer");
        assert!(d.argv.contains(&"steer".to_string()));
        assert!(
            d.argv
                .contains(&"implement the parser with a plan gate".to_string())
        );
    }

    // Wave-label sniffing deleted: a wave- label is not a declared trigger.
    #[test]
    fn wave_label_without_trigger_is_none() {
        let d = decide("implement the parser for wave-1", &with_beads(3));
        assert_eq!(d.primitive, "none");
        assert_eq!(d.topology, "none");
    }

    // Wave sniffing deleted: bare lanes/swarm never route to wave-swarm.
    #[test]
    fn lanes_keyword_is_not_wave_swarm() {
        let d = decide("run the lanes", &snap());
        assert_eq!(d.primitive, "none", "lanes alone is not a declared trigger");
    }

    #[test]
    fn wave_swarm_with_2_crews_is_wave_swarm_with_crew_2() {
        let d = decide("wave-swarm with 2 crews", &snap());
        assert_eq!(d.primitive, "wave-swarm");
        assert_eq!(d.topology, "swarm");
        assert!(
            d.argv.windows(2).any(|w| w == ["--crew", "2"]),
            "argv: {:?}",
            d.argv
        );
    }

    #[test]
    fn wave_swarm_3_crews_honors_n() {
        let d = decide("wave-swarm 3 crews", &snap());
        assert_eq!(d.primitive, "wave-swarm");
        assert!(
            d.argv.windows(2).any(|w| w == ["--crew", "3"]),
            "{:?}",
            d.argv
        );
    }

    #[test]
    fn join_the_fleet_is_join_topology() {
        let d = decide("join the fleet", &snap());
        assert_eq!(d.primitive, "join");
        assert_eq!(d.topology, "join");
        assert!(d.argv.contains(&"arrive".to_string()));
    }

    #[test]
    fn one_line_typo_is_open_spawn_send() {
        let d = decide("fix a one-line typo", &snap());
        assert_eq!(d.primitive, "spawn-send");
        assert_eq!(d.topology, "open");
    }

    #[test]
    fn parse_budget_n_examples() {
        assert_eq!(parse_budget_n("swarm with 2 crews"), Some(2));
        assert_eq!(parse_budget_n("wave-swarm 3 crews"), Some(3));
        assert_eq!(parse_budget_n("coder:4"), Some(4));
        assert_eq!(parse_budget_n("work the ready beads"), None);
    }

    // Row 4 — implement/fix + no tracker|direct → steer-direct.
    #[test]
    fn row4_implement_direct_is_steer_direct() {
        for t in ["implement this direct", "fix the widget no tracker"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "steer-direct", "target: {t}");
            assert!(
                d.argv.contains(&"--direct".to_string()),
                "argv: {:?}",
                d.argv
            );
        }
    }

    // Row 5 — compose|catalog miss|parts: → compose-parts.
    #[test]
    fn row5_compose_parts_is_compose_parts() {
        for t in [
            "compose a pipeline",
            "catalog miss on this",
            "compose parts: scout:fresh-eyes",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "compose-parts", "target: {t}");
            assert!(
                d.argv.contains(&"--parts".to_string()),
                "argv: {:?}",
                d.argv
            );
            let parts_idx = d.argv.iter().position(|a| a == "--parts").expect("--parts");
            let value = d.argv.get(parts_idx + 1).expect("--parts needs a value");
            assert!(
                !value.is_empty() && value != "--parts",
                "bare --parts is unrunnable clap: {:?}",
                d.argv
            );
        }
        let extracted = decide(
            "compose parts: scout:fresh-eyes-explore,coder:systematic-execute",
            &snap(),
        );
        assert_eq!(extracted.primitive, "compose-parts");
        let idx = extracted.argv.iter().position(|a| a == "--parts").unwrap();
        assert_eq!(
            extracted.argv.get(idx + 1).map(String::as_str),
            Some("scout:fresh-eyes-explore,coder:systematic-execute")
        );
    }

    #[test]
    fn repair_until_verified_routes_to_recipe() {
        for t in ["repair until verified", "repair-until-verified the panic"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "repair-until-verified", "target: {t}");
            assert!(
                d.argv.contains(&"compose".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"repair-until-verified".to_string()));
            assert!(d.argv.contains(&"--run".to_string()));
        }
    }

    // Row 6 — shape unknown|improvise|author workflow → compose-agent.
    #[test]
    fn row6_shape_unknown_is_compose_agent() {
        for t in [
            "shape unknown here",
            "improvise it",
            "author workflow for ingest",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "compose-agent", "target: {t}");
            assert!(
                d.argv.contains(&"--agent".to_string()),
                "argv: {:?}",
                d.argv
            );
        }
    }

    // Row 7 — contention|coalition|shared-file (target or snap) → coalition-form.
    #[test]
    fn row7_contention_is_coalition_form() {
        for t in [
            "shared-file contention across crews",
            "coalition for the migration",
            "contention on state.rs",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "coalition-form", "target: {t}");
        }
        let s = Snapshot {
            contention: true,
            ..snap()
        };
        assert_eq!(decide("ship it", &s).primitive, "coalition-form");
    }

    // Row 8 — one pane|watch me|single agent → spawn-send.
    #[test]
    fn row8_one_pane_is_spawn_send() {
        for t in ["one pane for the oracle", "watch me", "single agent job"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "spawn-send", "target: {t}");
            // MUST be --profile; NEVER add --oc; --to carries a full name.
            assert!(
                d.argv.contains(&"--profile".to_string()),
                "--profile MUST be present: {:?}",
                d.argv
            );
            assert!(
                !d.argv.iter().any(|a| a.contains("--oc")),
                "NEVER add --oc: {:?}",
                d.argv
            );
            let to = d
                .argv
                .iter()
                .position(|a| a == "--to")
                .expect("--to full name present");
            let name = &d.argv[to + 1];
            assert!(
                name.starts_with('<') && name.ends_with('>'),
                "--to must carry a full-name slot: {name}"
            );
        }
    }

    // Row 9 — cron|schedule|nightly → cron-start.
    #[test]
    fn row9_scheduled_is_cron_start() {
        for t in [
            "cron the weekly report",
            "schedule the nightly sync",
            "nightly housekeeping",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "cron-start", "target: {t}");
            assert_eq!(d.argv, vec!["flywheel", "cron", "start"]);
        }
    }

    // Row 10 — crew-steer|mention peer|peer mention → steer (plane S verbs
    // run inside the steer target; no new primitive — an explicit row, never
    // the silent fallback).
    #[test]
    fn row10_crew_steer_is_steer_not_new_primitive() {
        for t in [
            "crew-steer this to the peer",
            "mention peer on the channel",
            "peer mention for the handoff",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "steer", "target: {t}");
            // No flywheel channel subcommand (ADR-0020 wave 1) — the steer
            // target drives `toron channel post --mention` / `toron job claim`.
            assert!(
                !d.argv.iter().any(|a| a == "channel"),
                "no flywheel channel subcommand: {:?}",
                d.argv
            );
        }
    }

    // Fallback — unmatched targets are explicit none, never silent steer.
    #[test]
    fn unmatched_target_is_explicit_none() {
        let d = decide("zzz something odd", &snap());
        assert_eq!(d.primitive, "none");
        assert_eq!(d.topology, "none");
        assert!(d.argv.is_empty(), "none carries no dispatch: {:?}", d.argv);
    }

    // ===== ADR-0019 Spec 8 rows (bd-w2-decide-drgi) =====

    // Row 2 — repro/regression/bug-fix, and bug-shaped implement targets.
    #[test]
    fn row2_repro_and_bug_shaped_fix_go_to_bug_fix() {
        for t in [
            "repro the scroll drift",
            "fix the repro",
            "regression in dispatch",
            "fix the memory leak",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "bug-fix", "target: {t}");
            assert!(
                d.argv.contains(&"bug-fix".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(
                d.argv.contains(&"pane".to_string()),
                "pane mode: {:?}",
                d.argv
            );
        }
    }

    // Row 3 — refactor -> refactoring.
    #[test]
    fn row3_refactor_goes_to_refactoring() {
        for t in ["refactor the auth layer", "refactoring the loop dispatch"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "refactoring", "target: {t}");
            assert!(
                d.argv.contains(&"refactoring".to_string()),
                "argv: {:?}",
                d.argv
            );
        }
    }

    // Row 4 — prototype/sketch/mock -> prototype.
    #[test]
    fn row4_prototype_goes_to_prototype() {
        for t in [
            "prototype the data shape",
            "sketch the state machine",
            "mock it up before we build",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "prototype", "target: {t}");
            assert!(
                d.argv.contains(&"prototype".to_string()),
                "argv: {:?}",
                d.argv
            );
        }
    }

    // Row 5 — arena -> arena.
    #[test]
    fn row5_arena_goes_to_arena() {
        let d = decide("arena two models on this brief", &snap());
        assert_eq!(d.primitive, "arena");
        assert!(d.argv.contains(&"arena".to_string()), "argv: {:?}", d.argv);
    }

    // Row 6 — perf/p95/slow (not fix/implement) -> perf.
    #[test]
    fn row6_perf_goes_to_perf() {
        for t in [
            "profile the p95 latency",
            "dispatch is slow",
            "perf the ingest path",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "perf", "target: {t}");
            assert!(d.argv.contains(&"perf".to_string()), "argv: {:?}", d.argv);
        }
    }

    // Row 6 guard — implement + slow matches no diagnosis row and no
    // explicit route: explicit none, never silent steer.
    #[test]
    fn row6_implement_slow_is_not_perf() {
        let d = decide("fix the slow dispatch", &snap());
        assert_ne!(d.primitive, "perf");
        assert_eq!(
            d.primitive, "none",
            "implement without a declared trigger is explicit none: {:?}",
            d
        );
    }

    // Row 7 — hillclimb -> hillclimb.
    #[test]
    fn row7_hillclimb_goes_to_hillclimb() {
        let d = decide("hillclimb the sharpe ratio", &snap());
        assert_eq!(d.primitive, "hillclimb");
        assert!(
            d.argv.contains(&"hillclimb".to_string()),
            "argv: {:?}",
            d.argv
        );
    }

    // Row 8 — forensic/leak/idle cpu/spindump (not fix/implement) -> forensics.
    #[test]
    fn row8_forensics_goes_to_forensics() {
        for t in [
            "profile the idle cpu spin",
            "forensic trace of the crash",
            "investigate the memory leak",
            "spindump the hung pane",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "forensics", "target: {t}");
            assert!(
                d.argv.contains(&"forensics".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(
                d.argv.contains(&"headless".to_string()),
                "forensics is headless: {:?}",
                d.argv
            );
        }
    }

    // Row 8 guard — implement + leak -> bug-fix, NOT forensics (the
    // pre-acceptance fix: `fix the memory leak` contains no bug/repro token,
    // yet the captain said FIX).
    #[test]
    fn row8_fix_memory_leak_is_bug_fix_not_forensics() {
        let d = decide("fix the memory leak", &snap());
        assert_ne!(d.primitive, "forensics");
        assert_eq!(d.primitive, "bug-fix", "the repro step IS the forensics");
    }

    // Row 9 — babysit + bead-ish -> babysit-bead.
    #[test]
    fn row9_babysit_bead_goes_to_babysit_bead() {
        let d = decide("babysit the bd-w2-ops-szky bead", &snap());
        assert_eq!(d.primitive, "babysit-bead");
        assert!(
            d.argv.contains(&"babysit-bead".to_string()),
            "argv: {:?}",
            d.argv
        );
        // not bead-ish -> no match.
        let d2 = decide("babysit the server", &snap());
        assert_ne!(d2.primitive, "babysit-bead");
    }

    // Row 10 — pickup / resume session -> pickup.
    #[test]
    fn row10_pickup_goes_to_pickup() {
        for t in [
            "pickup the stranded session",
            "resume session after the reboot",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "pickup", "target: {t}");
            assert!(d.argv.contains(&"pickup".to_string()), "argv: {:?}", d.argv);
        }
    }

    // Row 11 — pause / going offline -> pause (argv has NO --mode).
    #[test]
    fn row11_pause_goes_to_pause() {
        for t in ["pause the swarm", "going offline for the night"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "pause", "target: {t}");
            assert!(d.argv.contains(&"pause".to_string()), "argv: {:?}", d.argv);
            assert!(
                !d.argv.contains(&"--mode".to_string()),
                "pause takes no --mode: {:?}",
                d.argv
            );
        }
    }

    // Row 12 — interrogate / tear this apart / adversarial review -> interrogate.
    #[test]
    fn row12_interrogate_goes_to_interrogate() {
        for t in [
            "interrogate this diff",
            "tear this apart",
            "adversarial review of the patch",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "interrogate", "target: {t}");
            assert!(
                d.argv.contains(&"interrogate".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(
                d.argv.contains(&"headless".to_string()),
                "headless: {:?}",
                d.argv
            );
        }
    }

    // Row 13 — figure it out / no playbook -> figure-it-out (planner arm).
    #[test]
    fn row13_figure_it_out_goes_to_figure_it_out() {
        for t in ["figure it out", "there's no playbook for this"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "figure-it-out", "target: {t}");
            assert!(
                d.argv.contains(&"compose".to_string()),
                "compose: {:?}",
                d.argv
            );
            assert!(
                d.argv.contains(&"--agent".to_string()),
                "planner arm: {:?}",
                d.argv
            );
        }
    }

    // Row 14 — ship beads / land the beads -> ship-beads.
    #[test]
    fn row14_ship_beads_goes_to_ship_beads() {
        for t in ["ship beads now", "land the beads"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "ship-beads", "target: {t}");
            assert!(
                d.argv.contains(&"ship-beads".to_string()),
                "argv: {:?}",
                d.argv
            );
        }
    }

    // ---- bd-x6x4: high-value unrouted builtins (feature, goal, triage, triage-deep,
    //      weekly-review, autonomous-work, reflect, awareness-digest, onboard) ----

    #[test]
    fn row15_feature_goes_to_feature() {
        for t in [
            "feature: add domain type for ingest",
            "implement the feature",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "feature", "target: {t}");
            assert!(
                d.argv.contains(&"feature".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
    }

    #[test]
    fn row16_goal_goes_to_goal() {
        for t in ["goal: ship the onboarding flow", "run the goal workflow"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "goal", "target: {t}");
            assert!(d.argv.contains(&"goal".to_string()), "argv: {:?}", d.argv);
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
    }

    #[test]
    fn row17_triage_deep_goes_to_triage_deep() {
        for t in [
            "triage-deep the backlog",
            "triage deep across 50 beads",
            "deep triage for the repo",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "triage-deep", "target: {t}");
            assert!(
                d.argv.contains(&"triage-deep".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
        // triage-deep must win over plain triage
        let d = decide("triage-deep backlog", &snap());
        assert_eq!(d.primitive, "triage-deep");
    }

    #[test]
    fn row18_triage_goes_to_triage() {
        for t in ["triage the support inbox", "triage inbound messages"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "triage", "target: {t}");
            assert!(d.argv.contains(&"triage".to_string()), "argv: {:?}", d.argv);
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
    }

    #[test]
    fn row19_weekly_review_goes_to_weekly_review() {
        for t in ["weekly-review the repo", "weekly review on Sunday"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "weekly-review", "target: {t}");
            assert!(
                d.argv.contains(&"weekly-review".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
    }

    #[test]
    fn row20_autonomous_work_goes_to_autonomous_work() {
        for t in ["autonomous-work tonight", "autonomous work the backlog"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "autonomous-work", "target: {t}");
            assert!(
                d.argv.contains(&"autonomous-work".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
    }

    #[test]
    fn row21_reflect_goes_to_reflect() {
        for t in ["reflect on standing orders", "run reflect for the project"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "reflect", "target: {t}");
            assert!(
                d.argv.contains(&"reflect".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
    }

    #[test]
    fn row22_awareness_digest_goes_to_awareness_digest() {
        for t in ["awareness-digest for flywheel", "awareness digest check"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "awareness-digest", "target: {t}");
            assert!(
                d.argv.contains(&"awareness-digest".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(
                d.argv.contains(&"headless".to_string()),
                "headless: {:?}",
                d.argv
            );
        }
    }

    #[test]
    fn row23_onboard_goes_to_onboard() {
        for t in ["onboard the newcomer", "run onboard for the repo"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "onboard", "target: {t}");
            assert!(
                d.argv.contains(&"onboard".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
    }
    #[test]
    fn row24c_debate_goes_to_debate() {
        for t in ["argue both sides of this", "play devil's advocate here"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "debate", "target: {t}");
            assert!(d.argv.contains(&"debate".to_string()), "argv: {:?}", d.argv);
            assert!(d.argv.contains(&"pane".to_string()), "pane: {:?}", d.argv);
        }
    }

    #[test]
    fn row24d_brainstorm_goes_to_ce_brainstorm() {
        for t in [
            "brainstorm the retry design",
            "sharpen the requirements first",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "ce-brainstorm", "target: {t}");
            assert!(
                d.argv.contains(&"ce-brainstorm".to_string()),
                "argv: {:?}",
                d.argv
            );
        }
    }

    #[test]
    fn row24e_breakdown_goes_to_ce_plan() {
        for t in ["break this down into beads", "make a bead-ready plan"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "ce-plan", "target: {t}");
            assert!(
                d.argv.contains(&"ce-plan".to_string()),
                "argv: {:?}",
                d.argv
            );
        }
    }

    #[test]
    fn row24f_ship_chain_goes_to_ship() {
        for t in ["ship the chain", "run the ship workflow"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "ship", "target: {t}");
            assert!(d.argv.contains(&"ship".to_string()), "argv: {:?}", d.argv);
        }
    }

    #[test]
    fn row24g_convert_goes_to_beads() {
        for t in ["convert to beads", "file these as beads"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "beads", "target: {t}");
            assert!(d.argv.contains(&"beads".to_string()), "argv: {:?}", d.argv);
        }
    }

    #[test]
    fn decide_trigger_for_maps_routed_and_unrouted() {
        assert_eq!(decide_trigger_for("feature"), Some("feature"));
        assert_eq!(decide_trigger_for("triage-deep"), Some("triage-deep"));
        assert_eq!(decide_trigger_for("weekly-review"), Some("weekly-review"));
        assert_eq!(
            decide_trigger_for("awareness-digest"),
            Some("awareness-digest")
        );
        // routed since ADR-0009 (agent-native pickup)
        assert_eq!(decide_trigger_for("debate"), Some("argue both sides"));
        assert_eq!(decide_trigger_for("ce-brainstorm"), Some("brainstorm"));
        assert_eq!(decide_trigger_for("ce-plan"), Some("break this down"));
        assert_eq!(decide_trigger_for("ship"), Some("ship the chain"));
        assert_eq!(decide_trigger_for("beads"), Some("convert to beads"));
        // deliberately unrouted
        assert_eq!(decide_trigger_for("plan-review"), None);
        assert_eq!(decide_trigger_for("phase-resolve"), None);
        assert_eq!(decide_trigger_for("wave-swarm"), Some("wave-swarm"));
        assert_eq!(
            decide_trigger_for("repair-until-verified"),
            Some("repair-until-verified")
        );
    }

    // Existing rows still win where they should (regression anchors).
    #[test]
    fn rows_keep_audit_loop_work_and_steer_anchors() {
        // audit + implement still steers (no bug-shaped needle).
        let d = decide("audit then fix the tracker", &snap());
        assert_eq!(d.primitive, "steer"); // plain implement without a playbook needle still steers.
        let d2 = decide("fix the tracker", &snap());
        assert_eq!(d2.primitive, "steer");
        // ready beads + work intent still loop-work (no new-row needle hits).
        let d3 = decide("work the ready beads", &with_beads(3));
        assert_eq!(d3.primitive, "loop-work");
    }

    // ---- W7 (§3.7 T4): coverage report + archetype + allowlist ----

    #[test]
    fn w7_coverage_hit_names_workflow_no_candidate() {
        let cat = vec![
            ("weekly-review", Some("review the week and land beads")),
            ("nightly-fence", Some("run the verify fence nightly")),
        ];
        let r = coverage_report("run the verify fence for me", &cat);
        assert!(r.covered, "token-covered entry matches");
        assert_eq!(r.matched.as_deref(), Some("nightly-fence"));
        assert!(r.candidate_parts.is_none(), "no candidate on a hit");
    }

    #[test]
    fn w7_coverage_miss_renders_candidate_not_autorun() {
        let cat = vec![("totally-unrelated", Some("ships widgets"))];
        let r = coverage_report("adversarial review of the parser module", &cat);
        assert!(!r.covered);
        assert_eq!(r.matched, None);
        let parts = r.candidate_parts.expect("miss renders candidate parts");
        assert_eq!(parts.join(","), "ce-review", "review archetype parts");
        let rendered = render_compose_candidate_for_approval(
            "SandySparrow",
            "adversarial review of the parser module",
            r.archetype,
            &parts,
        );
        assert!(
            rendered.contains("approval required") && rendered.contains("--parts"),
            "candidate is for APPROVAL with the exact command: {rendered}"
        );
        assert!(
            !rendered.contains("--run --keep") || rendered.contains("Approve with"),
            "never auto-run in W7"
        );
    }

    #[test]
    fn w7_archetype_classification_table() {
        use Archetype::*;
        assert_eq!(Archetype::classify("debate the two approaches"), Debate);
        assert_eq!(Archetype::classify("hillclimb the cache policy"), Hillclimb);
        assert_eq!(
            Archetype::classify("verify the fence around flaky repro"),
            VerifyFence
        );
        assert_eq!(Archetype::classify("fresh-eyes review of cron.rs"), Review);
        assert_eq!(
            Archetype::classify("explore and map this unknown crate"),
            ScoutFanout,
            "fallback archetype"
        );
    }

    #[test]
    fn w7_allowlist_gate_deny_never_dispatches() {
        let allow = vec!["SandySparrow".to_string(), "TanFern".to_string()];
        assert!(compose_request_allowed("SandySparrow", &allow));
        assert!(
            !compose_request_allowed("SomeImpostor", &allow),
            "deny default"
        );
        assert!(!compose_request_allowed("", &allow), "empty sender denies");
        assert!(!compose_request_allowed("  ", &allow));
        // Case-exact pins: near-miss casing denies (pins are CamelCase).
        assert!(!compose_request_allowed("sandysparrow", &allow));
    }
}

// ---- ADR-0032 Spec 9 (bd-himk): catalog-driven router contract ----
//
// RED batch A: assertions against the existing API. These encode the new
// truth (explicit none, no wave sniffing, declared topology, ce-review)
// and MUST fail until the routing table lands.
#[cfg(test)]
mod decide_router_tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot::default()
    }

    // No match returns explicit none, never silent steer.
    #[test]
    fn decide_router_no_match_is_explicit_none() {
        let d = decide("zzz something odd", &snap());
        assert_eq!(d.primitive, "none", "no-match must be explicit none");
        assert_eq!(d.topology, "none");
        assert!(d.argv.is_empty(), "none carries no dispatch: {:?}", d.argv);
        assert!(!d.reason.is_empty(), "none names the reason");
    }

    // wave_shaped substring sniffing is deleted: bare swarm/lanes/waves and
    // a lone budget never route to wave-swarm. Only the declared trigger does.
    #[test]
    fn decide_router_bare_swarm_lanes_budget_do_not_route_wave() {
        for t in [
            "swarm the backlog",
            "run the lanes",
            "coder:2 tune the loop",
            "waves of review",
        ] {
            let d = decide(t, &snap());
            assert_ne!(d.primitive, "wave-swarm", "target: {t}");
        }
        let d = decide("wave-swarm with 2 crews", &snap());
        assert_eq!(d.primitive, "wave-swarm", "declared trigger still routes");
        assert!(
            d.argv.windows(2).any(|w| w == ["--crew", "2"]),
            "budget still honored on the declared trigger: {:?}",
            d.argv
        );
    }

    // Topology is declared per workflow: babysit-bead is open, and unknown
    // primitives are none — the `_ => swarm` bucket is gone.
    #[test]
    fn decide_router_topology_declared_no_silent_swarm() {
        assert_eq!(topology_for_primitive("babysit-bead"), "open");
        assert_eq!(
            topology_for_primitive("bogus-primitive-xyz"),
            "none",
            "unknown primitives never silently become swarm"
        );
    }

    // Fronted intent wins over an incidental later mention: pause@0 beats
    // arena@6, goal@0 beats onboard@14. Table order never decides.
    #[test]
    fn decide_router_earliest_match_wins() {
        let d = decide("pause the arena", &snap());
        assert_eq!(d.primitive, "pause", "fronted verb is the intent: {d:?}");
        let d = decide("goal: ship the onboarding flow", &snap());
        assert_eq!(d.primitive, "goal", "fronted topic is the intent: {d:?}");
    }

    // Exact same-position contention across primitives is explicit none.
    #[test]
    fn decide_router_same_position_tie_is_explicit_none() {
        static TIE_A: RouteEntry = RouteEntry {
            primitive: "aaa",
            matchers: &["x"],
            topology: "swarm",
            gate: RouteGate::Always,
            argv: RouteArgv::CronStart,
            reason: "a",
        };
        static TIE_B: RouteEntry = RouteEntry {
            primitive: "bbb",
            matchers: &["x"],
            topology: "open",
            gate: RouteGate::Always,
            argv: RouteArgv::CronStart,
            reason: "b",
        };
        let d = pick(vec![((0, 5), &TIE_A), ((0, 5), &TIE_B)], "t", &snap());
        assert_eq!(d.primitive, "none", "tie must not silently pick: {d:?}");
        assert!(
            d.reason.contains("aaa") && d.reason.contains("bbb"),
            "tie names the contenders: {}",
            d.reason
        );
    }

    // Stale stage reference: the review archetype composes ce-review.
    #[test]
    fn decide_router_review_archetype_is_ce_review() {
        assert_eq!(
            Archetype::Review.default_parts(),
            &["ce-review"],
            "no such stage as code-review — only ce-review exists"
        );
    }

    // The live table loads loud-clean.
    #[test]
    fn decide_router_table_compiles_clean() {
        compile_routing_table().expect("live routing table must load clean");
        assert!(!ROUTING_TABLE.is_empty(), "one routing truth exists");
    }

    // Overlapping matcher claims fail load, naming the needle + claimants.
    #[test]
    fn decide_router_overlap_fails_load() {
        let dup = [
            RouteEntry {
                primitive: "aaa",
                matchers: &["same-needle"],
                topology: "swarm",
                gate: RouteGate::Always,
                argv: RouteArgv::CronStart,
                reason: "a",
            },
            RouteEntry {
                primitive: "bbb",
                matchers: &["same-needle"],
                topology: "open",
                gate: RouteGate::Always,
                argv: RouteArgv::CronStart,
                reason: "b",
            },
        ];
        let err = check_routing_table(&dup).expect_err("duplicate needle must fail load");
        assert!(err.contains("same-needle"), "names the needle: {err}");
        assert!(
            err.contains("aaa") && err.contains("bbb"),
            "names claimants: {err}"
        );
    }

    // Spec 9: one trigger per phrase — two primitives sharing a first matcher
    // would make decide_trigger_for ambiguous, so the catalog refuses to load.
    #[test]
    fn decide_router_shared_trigger_fails_load() {
        let dup = [
            RouteEntry {
                primitive: "aaa",
                matchers: &["same-trigger"],
                topology: "swarm",
                gate: RouteGate::Always,
                argv: RouteArgv::CronStart,
                reason: "a",
            },
            RouteEntry {
                primitive: "bbb",
                matchers: &["same-trigger", "other"],
                topology: "open",
                gate: RouteGate::Always,
                argv: RouteArgv::CronStart,
                reason: "b",
            },
        ];
        let err = check_catalog(&dup).expect_err("shared trigger must fail load");
        assert!(err.contains("same-trigger"), "names the phrase: {err}");
        assert!(
            err.contains("aaa") && err.contains("bbb"),
            "names claimants: {err}"
        );
    }

    // Spec 9: a row that runs a workflow must name a workflow that resolves;
    // the live table + live catalog are checked together in main.rs.
    #[test]
    fn decide_router_orphan_workflow_route_fails_load() {
        let table = [RouteEntry {
            primitive: "ghost-wf",
            matchers: &["ghost"],
            topology: "swarm",
            gate: RouteGate::Always,
            argv: RouteArgv::WorkflowRun { mode: "pane" },
            reason: "g",
        }];
        let err = check_workflow_routes(&table, &["real-wf".to_string()])
            .expect_err("orphan route must fail load");
        assert!(err.contains("ghost-wf"), "names the orphan: {err}");
        check_workflow_routes(&table, &["ghost-wf".to_string()])
            .expect("a resolvable workflow admits the route");
        // A non-workflow argv shape is never subject to the cross-check.
        let non_wf = [RouteEntry {
            primitive: "join",
            matchers: &["join it"],
            topology: "swarm",
            gate: RouteGate::Always,
            argv: RouteArgv::Join,
            reason: "j",
        }];
        check_workflow_routes(&non_wf, &[]).expect("Join is not a `workflow run` route");
    }

    // The LIVE catalog proves the table: every `workflow run` route resolves to
    // a real workflow. This is the "one catalog" proof — a rename in the
    // catalog that leaves a route behind turns this red instead of shipping a
    // decide argv that cannot run.
    #[test]
    fn decide_router_live_table_resolves_against_the_real_catalog() {
        let known: Vec<String> = crate::builtin_workflows::builtin_workflows()
            .into_keys()
            .collect();
        assert!(
            known.len() > 20,
            "the builtin catalog is non-trivial: {known:?}"
        );
        check_workflow_routes(ROUTING_TABLE, &known)
            .expect("every routed `workflow run` primitive must exist in the catalog");
    }

    // The unrouted declaration is one place, and every name in it is real.
    #[test]
    fn decide_router_unrouted_list_is_real_and_not_keyword_routed() {
        for name in UNROUTED_PRIMITIVES {
            assert!(
                ROUTING_TABLE.iter().any(|e| e.primitive == *name),
                "{name} is declared unrouted but has no routing row"
            );
            assert_eq!(
                decide_trigger_for(name),
                None,
                "{name} is not keyword-routed"
            );
        }
        // Every OTHER primitive is keyword-routed, and its trigger is the FIRST
        // row's first matcher. A multi-row primitive (bug-fix: repro…, then
        // bug…; steer: beads…, then crew-steer…) keeps its secondary needles
        // out of the trigger position — the phrase is per-primitive, so the
        // first row owns it.
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for e in ROUTING_TABLE {
            if UNROUTED_PRIMITIVES.contains(&e.primitive) || !seen.insert(e.primitive) {
                continue;
            }
            assert_eq!(
                decide_trigger_for(e.primitive),
                Some(e.matchers[0]),
                "{} must trigger on its first row's first matcher",
                e.primitive
            );
        }
        assert!(
            seen.len() >= 30,
            "the table carries a real keyword-routed set: {}",
            seen.len()
        );
    }

    // One truth: every routed workflow's decide_trigger phrase is its row's
    // first matcher, so `workflow list --json` and `decide` cannot diverge.
    #[test]
    fn decide_router_trigger_phrase_is_first_matcher() {
        let pairs = [
            ("captain-audit", "audit"),
            ("bug-fix", "repro"),
            ("refactoring", "refactor"),
            ("prototype", "prototype"),
            ("arena", "arena"),
            ("perf", "perf"),
            ("hillclimb", "hillclimb"),
            ("forensics", "forensic"),
            ("babysit-bead", "babysit"),
            ("pickup", "pickup"),
            ("pause", "pause"),
            ("interrogate", "interrogate"),
            ("figure-it-out", "figure it out"),
            ("ship-beads", "ship beads"),
            ("feature", "feature"),
            ("goal", "goal"),
            ("triage-deep", "triage-deep"),
            ("triage", "triage"),
            ("weekly-review", "weekly-review"),
            ("autonomous-work", "autonomous-work"),
            ("reflect", "reflect"),
            ("awareness-digest", "awareness-digest"),
            ("debate", "argue both sides"),
            ("ce-brainstorm", "brainstorm"),
            ("ce-plan", "break this down"),
            ("ship", "ship the chain"),
            ("beads", "convert to beads"),
            ("onboard", "onboard"),
            ("wave-swarm", "wave-swarm"),
            ("repair-until-verified", "repair-until-verified"),
            ("ce-compound", "compound the learning"),
            ("ce-sweep", "sweep the learnings"),
        ];
        for (workflow, trigger) in pairs {
            assert_eq!(
                decide_trigger_for(workflow),
                Some(trigger),
                "trigger surface for {workflow}"
            );
            assert!(
                ROUTING_TABLE
                    .iter()
                    .any(|e| e.primitive == workflow && e.matchers.first() == Some(&trigger)),
                "{workflow} row leads with its trigger {trigger:?}"
            );
        }
    }
}
