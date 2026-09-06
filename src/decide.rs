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

fn dec(primitive: &'static str, reason: &str, argv: &[&str]) -> Decision {
    Decision {
        primitive,
        reason: reason.to_string(),
        argv: argv.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// Classify `target` + `snap` into one primitive. No harness special case.
/// Table order (first match wins):
///   1 audit|read-only|analyze (not implement/fix) → captain-audit
///   2 repro|regression|bug-fix, or implement/fix with a bug-shaped needle
///     (bug/crash/panic/leak/broken/failing/error) → bug-fix
///   3 refactor → refactoring
///   4 prototype|sketch|mock → prototype
///   5 arena → arena
///   6 perf|p95|slow (not implement/fix) → perf
///   7 hillclimb → hillclimb
///   8 forensic|leak|idle cpu|spindump (not implement/fix) → forensics
///   9 babysit + bead-ish → babysit-bead
///  10 pickup|resume session → pickup
///  11 pause|going offline → pause
///  12 interrogate|tear this apart|adversarial review → interrogate
///  13 figure it out|no playbook → figure-it-out
///  14 ship beads|land the beads → ship-beads
///  15 ready beads + empty|work|loop|beads (not implement/fix) → loop-work
///  16 implement/fix + beads|tracker|gate (not "no tracker") → steer
///  17 implement/fix + no tracker|direct → steer-direct
///  18 compose|catalog miss|parts: → compose-parts (`--parts` always has a value)
///  19 shape unknown|improvise|author workflow → compose-agent
///  20 contention|coalition|shared-file (target or snap) → coalition-form
///  21 one pane|watch me|single agent → spawn-send
///  22 cron|schedule|nightly → cron-start
///  23 crew-steer|mention peer|peer mention → steer (plane S: the steer
///     target runs `toron channel post --mention` / `toron job claim`; no new
///     primitive — first-match keeps row 16/fallback)
/// Implement-bearing targets never fall to a diagnosis-only workflow
/// (Spec 8): `fix the memory leak` → bug-fix, not forensics; `perf`/`slow`
/// and `forensic`/`leak`/`idle cpu`/`spindump` rows carry the !implement
/// guard.
/// Unmatched targets fall back to steer (the catalog loudly rejects unknown
/// shapes — never a silent no-op).
pub fn decide(target: &str, snap: &Snapshot) -> Decision {
    let t = target.to_ascii_lowercase();
    let implement = has(&t, &["implement", "fix"]);

    // 1. Read-only analysis wins over everything (never for implement/fix).
    if has(&t, &["audit", "read-only", "analyze"]) && !implement {
        return dec(
            "captain-audit",
            "read-only analysis — captain-audit headless",
            &[
                "flywheel",
                "workflow",
                "run",
                "captain-audit",
                "--execute",
                "--mode",
                "headless",
            ],
        );
    }
    // 2. repro/regression/bug-fix — plus any implement/fix target carrying a
    //    bug-shaped needle -> bug-fix. The repro step IS the forensics;
    //    implement-bearing targets never fall to forensics/perf (Spec 8
    //    pre-acceptance guard: `fix the memory leak` contains no bug/repro
    //    token, yet must reach bug-fix, not forensics). Placed before
    //    loop-work so a repro never hides behind ready-bead dispatch.
    if has(&t, &["repro", "regression", "bug-fix"])
        || (implement
            && has(
                &t,
                &[
                    "bug", "crash", "panic", "leak", "broken", "failing", "error",
                ],
            ))
    {
        return dec(
            "bug-fix",
            "repro/regression/bug-shaped fix — bug-fix workflow (repro first)",
            &[
                "flywheel",
                "workflow",
                "run",
                "bug-fix",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 3. refactor -> refactoring (baseline -> subtract -> reshape -> prove).
    if has(&t, &["refactor"]) {
        return dec(
            "refactoring",
            "refactor — refactoring workflow (invariant stays green)",
            &[
                "flywheel",
                "workflow",
                "run",
                "refactoring",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 4. prototype/sketch/mock it up -> prototype (n >= 2 arms).
    if has(&t, &["prototype", "sketch", "mock"]) {
        return dec(
            "prototype",
            "prototype/sketch — prototype workflow (2-3 arms, oracle picks)",
            &[
                "flywheel",
                "workflow",
                "run",
                "prototype",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 5. arena -> arena (same brief on different models).
    if has(&t, &["arena"]) {
        return dec(
            "arena",
            "arena — model race on one brief (declared selection rule)",
            &[
                "flywheel",
                "workflow",
                "run",
                "arena",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 6. perf/p95/slow, NOT implement/fix -> perf.
    if !implement && has(&t, &["perf", "p95", "slow"]) {
        return dec(
            "perf",
            "perf/p95/slow — perf workflow (metric + budget)",
            &[
                "flywheel",
                "workflow",
                "run",
                "perf",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 7. hillclimb -> hillclimb (hypothesize -> smallest change -> measure).
    if has(&t, &["hillclimb"]) {
        return dec(
            "hillclimb",
            "hillclimb — hillclimb workflow (one commit per accepted win)",
            &[
                "flywheel",
                "workflow",
                "run",
                "hillclimb",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 8. forensic/leak/idle cpu/spindump, NOT implement/fix -> forensics
    //    (diagnosis is the artifact; NO fix step — a fix is a separate
    //    bug-fix after the captain accepts the diagnosis).
    if !implement && has(&t, &["forensic", "leak", "idle cpu", "spindump"]) {
        return dec(
            "forensics",
            "forensics — diagnosis only, no fix step (bug-fix consumes Act-on)",
            &[
                "flywheel",
                "workflow",
                "run",
                "forensics",
                "--execute",
                "--mode",
                "headless",
            ],
        );
    }
    // 9. babysit + bead-ish -> babysit-bead (no new beads).
    if has(&t, &["babysit"]) && has(&t, &["bead", "tracker", "issue"]) {
        return dec(
            "babysit-bead",
            "babysit a bead — babysit-bead workflow (no new beads)",
            &[
                "flywheel",
                "workflow",
                "run",
                "babysit-bead",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 10. pickup / resume session -> pickup.
    if has(&t, &["pickup", "resume session"]) {
        return dec(
            "pickup",
            "pickup / resume session — pickup workflow",
            &[
                "flywheel",
                "workflow",
                "run",
                "pickup",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 11. pause / going offline -> pause (checkpoint + handoff; no --mode).
    if has(&t, &["pause", "going offline"]) {
        return dec(
            "pause",
            "pause / going offline — checkpoint + standing handoff",
            &["flywheel", "workflow", "run", "pause", "--execute"],
        );
    }
    // 12. interrogate / tear this apart / adversarial review -> interrogate
    //     (N reviewers, different families, same rubric, read-only).
    if has(
        &t,
        &["interrogate", "tear this apart", "adversarial review"],
    ) {
        return dec(
            "interrogate",
            "interrogate — N-model adversarial review (read-only)",
            &[
                "flywheel",
                "workflow",
                "run",
                "interrogate",
                "--execute",
                "--mode",
                "headless",
            ],
        );
    }
    // 13. figure it out / no playbook -> figure-it-out (planner authors
    //     YAML, lint is the gate).
    if has(&t, &["figure it out", "no playbook"]) {
        return dec(
            "figure-it-out",
            "no matching playbook — planner compose, lint is the gate",
            &["flywheel", "workflow", "compose", target, "--agent"],
        );
    }
    // 14. ship beads / land the beads -> ship-beads (ledger-green then
    //     housekeeper commit).
    if has(&t, &["ship beads", "land the beads"]) {
        return dec(
            "ship-beads",
            "ship beads — ledger-green then housekeeper commit",
            &[
                "flywheel",
                "workflow",
                "run",
                "ship-beads",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 15. feature -> feature (bd-x6x4: shape the domain then TDD build).
    if has(&t, &["feature"]) {
        return dec(
            "feature",
            "feature — feature workflow (shape -> build -> prove)",
            &[
                "flywheel",
                "workflow",
                "run",
                "feature",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 16. goal -> goal (meta: ce-brainstorm -> plan -> beads -> build -> review -> ship).
    if has(&t, &["goal"]) {
        return dec(
            "goal",
            "goal — meta-workflow (ce-brainstorm -> plan -> beads -> build)",
            &[
                "flywheel",
                "workflow",
                "run",
                "goal",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 17. triage-deep before triage (longer phrase wins).
    if has(&t, &["triage-deep", "triage deep", "deep triage"]) {
        return dec(
            "triage-deep",
            "triage-deep — deep backlog triage (bv robot-triage -> spawn crew)",
            &[
                "flywheel",
                "workflow",
                "run",
                "triage-deep",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 18. triage -> triage (canonical reactive workflow).
    if has(&t, &["triage"]) {
        return dec(
            "triage",
            "triage — inbound message triage (ack + dispatch)",
            &[
                "flywheel",
                "workflow",
                "run",
                "triage",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 19. weekly-review -> weekly-review (schedule: scout fanout 3 -> oracle).
    if has(&t, &["weekly-review", "weekly review"]) {
        return dec(
            "weekly-review",
            "weekly-review — weekly repo review (scout fanout -> oracle idea)",
            &[
                "flywheel",
                "workflow",
                "run",
                "weekly-review",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 20. autonomous-work -> autonomous-work (real loop then ship-beads).
    if has(&t, &["autonomous-work", "autonomous work"]) {
        return dec(
            "autonomous-work",
            "autonomous-work — autonomous overnight work (loop run --work then ship)",
            &[
                "flywheel",
                "workflow",
                "run",
                "autonomous-work",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 21. reflect -> reflect (corrections become structure).
    if has(&t, &["reflect"]) {
        return dec(
            "reflect",
            "reflect — corrections become structure (hook bead, never a paragraph)",
            &[
                "flywheel",
                "workflow",
                "run",
                "reflect",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    //     learning after verified work — any topology).
    if has(
        &t,
        &[
            "compound the learning",
            "capture the learning",
            "capture what we learned",
            "compound this",
        ],
    ) {
        return dec(
            "ce-compound",
            "ce-compound — run the compound gate over verified work (docs/learnings/)",
            &[
                "flywheel",
                "workflow",
                "run",
                "ce-compound",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 23. sweep the learnings -> ce-sweep (ADR-0009 corpus maintenance).
    if has(
        &t,
        &["sweep the learnings", "learning sweep", "compound-refresh"],
    ) {
        return dec(
            "ce-sweep",
            "ce-sweep — maintain docs/learnings/ (keep/update/consolidate/delete)",
            &[
                "flywheel",
                "workflow",
                "run",
                "ce-sweep",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 22. awareness-digest -> awareness-digest (read-only digest, headless).
    if has(&t, &["awareness-digest", "awareness digest"]) {
        return dec(
            "awareness-digest",
            "awareness-digest — captain digest (presence + reservations + guard)",
            &[
                "flywheel",
                "workflow",
                "run",
                "awareness-digest",
                "--execute",
                "--mode",
                "headless",
            ],
        );
    }
    // 23. onboard -> onboard (cold-start map + seed).
    if has(&t, &["onboard"]) {
        return dec(
            "onboard",
            "onboard — cold-start newcomer (map repo -> seed first task)",
            &[
                "flywheel",
                "workflow",
                "run",
                "onboard",
                "--execute",
                "--mode",
                "pane",
            ],
        );
    }
    // 24. wave-swarm -> compose --parts wave-swarm (ADR-0006 D3).
    // Do not require the recipe name in the objective (GS-013: kill the easter egg).
    let wave_shaped = has(&t, &["wave-swarm", "wave swarm", "lanes", "waves"])
        || t.split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .any(|w| w.starts_with("wave-") && w.len() > 5);
    if wave_shaped {
        return dec(
            "wave-swarm",
            "wave-swarm — composed wave swarm (GATE -> DISPATCH -> OBSERVE -> VERIFY -> SIGNOFF)",
            &[
                "flywheel",
                "workflow",
                "compose",
                target,
                "--parts",
                "wave-swarm",
                "--run",
            ],
        );
    }
    // 24b. repair-until-verified recipe (not the bug-fix wrapper).
    if has(&t, &["repair-until-verified", "repair until verified"]) {
        return dec(
            "repair-until-verified",
            "repair-until-verified — compose --parts repair-until-verified --run",
            &[
                "flywheel",
                "workflow",
                "compose",
                target,
                "--parts",
                "repair-until-verified",
                "--run",
            ],
        );
    }
    // 25. Ready beads + work-ish intent → run the loop, not hand-rolled sends.
    if snap.ready_bead_count > 0
        && !implement
        && (t.trim().is_empty() || has(&t, &["work", "loop", "beads"]))
    {
        return dec(
            "loop-work",
            &format!(
                "{} ready beads — loop-mode run <project> --work",
                snap.ready_bead_count
            ),
            &["flywheel", "loop-mode", "run", "<project>", "--work"],
        );
    }
    // 3. Implement/fix with a tracker/gate → steer (plan gate).
    if implement && has(&t, &["beads", "tracker", "gate"]) && !has(&t, &["no tracker"]) {
        return dec(
            "steer",
            "implement + tracker/gate — steer",
            &["flywheel", "steer", target],
        );
    }
    // 4. Implement/fix, no tracker / direct → steer --direct.
    if implement && has(&t, &["no tracker", "direct"]) {
        return dec(
            "steer-direct",
            "implement, no tracker — steer --direct",
            &["flywheel", "steer", target, "--direct"],
        );
    }
    // 5. Catalog cannot express it → compose --parts.
    if has(&t, &["compose", "catalog miss", "parts:"]) {
        let parts = compose_parts_value(target);
        return dec(
            "compose-parts",
            "catalog cannot express the named parts — compose --parts",
            &["flywheel", "workflow", "compose", target, "--parts", &parts],
        );
    }
    // 6. Shape unknown → planner arm (lint is the gate).
    if has(&t, &["shape unknown", "improvise", "author workflow"]) {
        return dec(
            "compose-agent",
            "shape unknown — planner arm, lint is the gate",
            &["flywheel", "workflow", "compose", target, "--agent"],
        );
    }
    // 7. Shared-file contention → coalition (pane-crew-only).
    if snap.contention || has(&t, &["contention", "coalition", "shared-file"]) {
        return dec(
            "coalition-form",
            "file contention — coalition, not a solo dispatch",
            &["flywheel", "coalition", "form"],
        );
    }
    // 8. One pane, captain watches → spawn --profile; NEVER --oc; --to full name.
    if has(&t, &["one pane", "watch me", "single agent"]) {
        return dec(
            "spawn-send",
            "one pane — spawn --profile <role> --count 1, then send --to <full-name> --wait (never --oc)",
            &[
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
            ],
        );
    }
    // 9. Scheduled → cron YAML + cron start (cron run is dry-run).
    if has(&t, &["cron", "schedule", "nightly"]) {
        return dec(
            "cron-start",
            "scheduled work — cron YAML + cron start",
            &["flywheel", "cron", "start"],
        );
    }
    // Fallback: forward to the catalog; it rejects unknown shapes loudly.
    dec(
        "steer",
        "default — steer from catalog",
        &["flywheel", "steer", target],
    )
}

// ---- bd-x6x4: decide trigger phrase per builtin (workflow list --json) ----
/// The decide keyword phrase that routes to a builtin via `flywheel decide`.
/// Returns the primary trigger phrase (the first `has()` needle) for each
/// routed builtin, or None for command-adjacent / recipe / phase-chain
/// builtins that are deliberately NOT keyword-routed.
pub fn decide_trigger_for(workflow: &str) -> Option<&'static str> {
    match workflow {
        // Existing 13
        "captain-audit" => Some("audit"),
        "bug-fix" => Some("repro"),
        "refactoring" => Some("refactor"),
        "prototype" => Some("prototype"),
        "arena" => Some("arena"),
        "perf" => Some("perf"),
        "hillclimb" => Some("hillclimb"),
        "forensics" => Some("forensic"),
        "babysit-bead" => Some("babysit"),
        "pickup" => Some("pickup"),
        "pause" => Some("pause"),
        "interrogate" => Some("interrogate"),
        "figure-it-out" => Some("figure it out"),
        "ship-beads" => Some("ship beads"),
        // New 9 (bd-x6x4)
        "feature" => Some("feature"),
        "goal" => Some("goal"),
        "triage-deep" => Some("triage-deep"),
        "triage" => Some("triage"),
        "weekly-review" => Some("weekly-review"),
        "autonomous-work" => Some("autonomous-work"),
        "reflect" => Some("reflect"),
        "awareness-digest" => Some("awareness-digest"),
        "onboard" => Some("onboard"),
        "wave-swarm" => Some("wave-swarm"),
        "repair-until-verified" => Some("repair-until-verified"),
        "ce-compound" => Some("compound the learning"),
        "ce-sweep" => Some("sweep the learnings"),
        _ => None,
    }
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
            Archetype::ScoutFanout => "scout-fanout",
            Archetype::VerifyFence => "verify-fence",
            Archetype::Debate => "debate",
            Archetype::Hillclimb => "hillclimb",
            Archetype::Review => "review",
        }
    }

    /// The stage-first parts this archetype composes from (real seed stages;
    /// W6's affinity resolution dispatches each to its default role).
    pub fn default_parts(self) -> &'static [&'static str] {
        match self {
            Archetype::ScoutFanout => &["fresh-eyes-explore"],
            Archetype::VerifyFence => &["repro-baseline", "tdd-red", "minimal-fix", "prove-verify"],
            Archetype::Debate => &["judge-arms"],
            Archetype::Hillclimb => &["baseline-measure", "measure-keep-or-discard"],
            Archetype::Review => &["code-review"],
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
            Archetype::Debate
        } else if has(
            &t,
            &["hillclimb", "keep-or-discard", "measure against baseline"],
        ) {
            Archetype::Hillclimb
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
            Archetype::VerifyFence
        } else if has(&t, &["review", "audit", "fresh eyes", "code check"]) {
            Archetype::Review
        } else {
            Archetype::ScoutFanout
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
                    .map(|s| s.to_string())
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

    #[test]
    fn wave_label_routes_to_wave_swarm_without_recipe_token() {
        let d = decide("implement the parser for wave-1", &with_beads(3));
        assert_eq!(d.primitive, "wave-swarm");
        assert!(d.argv.contains(&"compose".to_string()));
        assert!(d.argv.contains(&"wave-swarm".to_string()));
    }

    #[test]
    fn lanes_keyword_routes_to_wave_swarm() {
        let d = decide("run the lanes", &snap());
        assert_eq!(d.primitive, "wave-swarm");
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
    // run inside the steer target; no new primitive — first-match preserved).
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

    // Fallback — unmatched targets forward to steer (catalog rejects loudly).
    #[test]
    fn unmatched_target_falls_back_to_steer() {
        let d = decide("zzz something odd", &snap());
        assert_eq!(d.primitive, "steer");
        assert!(d.argv.contains(&"zzz something odd".to_string()));
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

    // Row 6 guard — implement + slow is NOT perf (falls through to steer).
    #[test]
    fn row6_implement_slow_is_not_perf() {
        let d = decide("fix the slow dispatch", &snap());
        assert_ne!(d.primitive, "perf");
        assert_eq!(
            d.primitive, "steer",
            "implement falls through to steer: {:?}",
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
    fn decide_trigger_for_maps_routed_and_unrouted() {
        assert_eq!(decide_trigger_for("feature"), Some("feature"));
        assert_eq!(decide_trigger_for("triage-deep"), Some("triage-deep"));
        assert_eq!(decide_trigger_for("weekly-review"), Some("weekly-review"));
        assert_eq!(
            decide_trigger_for("awareness-digest"),
            Some("awareness-digest")
        );
        // deliberately unrouted
        assert_eq!(decide_trigger_for("debate"), None);
        assert_eq!(decide_trigger_for("plan-review"), None);
        assert_eq!(decide_trigger_for("phase-resolve"), None);
        assert_eq!(decide_trigger_for("ship"), None);
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
        assert_eq!(parts.join(","), "code-review", "review archetype parts");
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
