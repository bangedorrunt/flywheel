// governed-by: ADR-0031 (Elm core — pure decision, effects at the edge)

/// bd-3uqz: a Jev triage of the `.unanswered` backlog, ESCALATE-ONLY.
///
/// The question this exists for is "is this pin mail-only BY DESIGN, or genuinely
/// DEAD?", and it is semantic: the project's own record is that CODE CANNOT ANSWER
/// IT. Absence of a pane is not evidence of death — 13 of 20 registry rows (65%)
/// had a pane herdr did not list — and that distinction is the whole hazard
/// bd-3uqz and bd-3uqz's sibling prunes are about. So the counting and the
/// absence-reasoning stay in Rust and Jev only supplies the semantic read.
///
/// ## WHY ESCALATE-ONLY, AND WHY THAT IS A TRUTH TABLE RATHER THAN A THRESHOLD
///
/// The action a label can cause is a LABEL, never a delete. A wrong answer costs a
/// mislabelled row. So the dangerous direction has to be fenced in the CODE, and
/// the fence is two-sided because the live docs are explicit that a Noul is a
/// probability, not a scale, and that the middle band is where a decision belongs
/// to a person:
///
///     `NO < noul < YES`  ->  not sure  ->  escalate
///
/// [`triage`] encodes exactly that, and a `Dead` verdict additionally requires
/// `mail_only_by_design` to be confidently LOW. One confident signal is never
/// enough to call something dead, because "dead" and "mail-only by design" are not
/// a partition: a pin can be mail-only and not dead, and a pin can look dead while
/// being a crew the doorbell simply cannot reach. Every other combination is
/// [`Triage::Unknown`], and Unknown is a first-class outcome, not a failure: the
/// deterministic path stands and the row is left exactly as it was.
///
/// ## WHAT THIS MUST NEVER DO
///
/// It must never delete, never rewrite, and never touch a `.unanswered` file. That
/// file is what gate P2b reads to decide whether a crew is INVISIBLE, and bd-y90n
/// already established that quieting it is the one thing that must not happen. A
/// label is a sidecar; the gate's input is not ours to annotate.
use crate::judgment::{QuestionDef, QuestionKind, QuestionPack};
use crate::stage::{Placement, Rule, Signal, Stage, StageCtx};

/// Above this, `dead` is a confident yes.
///
/// The live docs are explicit that the threshold follows the cost of being wrong:
/// "Raise it when acting on a false yes is expensive... Lower it when missing a
/// true yes is expensive." A false `dead` is the expensive direction — it is the
/// label a human might act on by pruning a crew — so this one stays high. In the
/// live run it never once exceeded 0.28, so it is a FENCE rather than a knob, and
/// it is deliberately not lowered to make any observed number look decisive.
pub const YES_DEAD: f64 = 0.80;

/// Above this, `mail_only_by_design` is a confident yes. LOWER than
/// [`YES_DEAD`]], on purpose and by consequence rather than by taste.
///
/// This module has NO WRITE PATH, so a `mail_only_by_design` label cannot delete
/// anything: it is a word in a report that a human reads. A false positive here
/// costs a mislabelled row. A false NEGATIVE costs something worse and quieter — a
/// working mail-only crew stays unlabelled and the operator does the triage by hand,
/// which is the state the tool was built to end.
///
/// HONESTY ABOUT WHERE 0.70 CAME FROM. It sits in the observed GAP between two
/// modes, not on a fitted curve: with the liveness instrument live, the eight
/// MEASURED pins read 0.71-0.75 and the eight UNMEASURED pins read 0.46-0.52, with
/// nothing in between. 0.70 is just below the lower edge of the measured mode.
///
/// THAT IS EIGHT PINS IN ONE SESSION. It is enough to show the signal is real and
/// separated; it is nowhere near enough to validate a threshold, and the first
/// honest act with more data is to move this. Recorded here so nobody later reads
/// 0.70 as a considered constant rather than a first reading.
pub const YES_MAIL_ONLY: f64 = 0.70;

/// The asymmetry is a compile-time invariant, so it is asserted at compile time.
/// If someone equalises the two bars, the cheap label silently becomes as expensive
/// as the risky one, and that must not be a thing a test run has to catch.
const _: () = assert!(
    YES_MAIL_ONLY < YES_DEAD,
    "the cheap label must keep the lower bar: mail-only is a report line, dead is      something a human may act on"
);

/// Below this, a confident no. Symmetric with [`YES`] on purpose: the band between
/// them is the escalate region, so a one-sided floor would let 0.3 read as "yes"
/// and manufacture a label out of a shrug.
pub const NO: f64 = 0.20;

/// The two propositions. NOT one question with three answers: the docs are explicit
/// that a Noul is a single yes/no proposition, and a question with two conditions
/// makes the number mean less. Two Nouls in ONE request — they are evaluated in
/// parallel, so this costs one round trip, not two.
pub const Q_MAIL_ONLY: &str = "mail_only_by_design";
pub const Q_DEAD: &str = "dead";

/// What the triage concluded. `Unknown` is a real verdict, not an error: it is
/// what "the model would not commit, or we never asked" looks like, and it is the
/// only safe answer when evidence is thin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Triage {
    /// The pin is unreachable on purpose — a mail-only crew, not a corpse.
    MailOnlyByDesign,
    /// Confidently not-by-design AND confidently gone.
    Dead,
    /// Not committed. The row is left alone.
    Unknown,
}

impl Triage {
    pub fn as_str(self) -> &'static str {
        match self {
            Triage::MailOnlyByDesign => "mail_only_by_design",
            Triage::Dead => "dead",
            Triage::Unknown => "unknown",
        }
    }
}

/// THE DECISION. Pure, total, and impossible to panic.
///
/// A `None` on either answer is `Unknown` — that is the "we never asked" path (no
/// key, transport error, timeout) and it must be indistinguishable in effect from
/// "the model declined". Both are a shrug, and a shrug never prunes a pin.
#[must_use]
pub fn triage(mail_only: Option<f64>, dead: Option<f64>) -> (Triage, String) {
    let (Some(m), Some(d)) = (mail_only, dead) else {
        return (
            Triage::Unknown,
            format!("not triaged: no answer (mail_only={mail_only:?} dead={dead:?})"),
        );
    };
    // Name both numbers in EVERY branch. A triage whose detail says only "unknown"
    // is indistinguishable from a stage that never ran, which is the failure the
    // stage registry's `detail` field exists to prevent.
    // Name both numbers in EVERY branch. A triage whose detail says only "unknown"
    // is indistinguishable from a stage that never ran, which is the failure the
    // stage registry's `detail` field exists to prevent.
    let nums = format!("mail_only={m:.2} dead={d:.2}");
    // Written as straight-line guards rather than a match on a tuple of booleans:
    // an earlier version derived `band(x) = confident(x)` and then matched on its
    // NEGATION, so every confident answer landed in the escalate branch. The
    // asymmetry between "above YES" and "below NO" is the whole rule, and it reads
    // wrong the moment it is inverted behind a helper.
    //
    // Dead requires BOTH: confidently gone AND confidently not-by-design. One
    // confident signal is never enough, because a pin that might be mail-only is
    // never a corpse however dead it looks.
    if d > YES_DEAD && m < NO {
        return (
            Triage::Dead,
            format!("{nums} — confidently dead, and confidently not a mail-only crew"),
        );
    }
    if m > YES_MAIL_ONLY && d < NO {
        return (
            Triage::MailOnlyByDesign,
            format!("{nums} — confidently mail-only by design, and not dead"),
        );
    }
    // Both confident YES is NOT a tie to break. The two propositions are not a
    // partition — a pin can be mail-only AND look dead — so this is a genuine
    // ambiguity and Unknown is the honest answer, with the reason named.
    if m > YES_MAIL_ONLY && d > YES_DEAD {
        return (
            Triage::Unknown,
            format!(
                "{nums} — both propositions read yes; they are not a partition, so not a label"
            ),
        );
    }
    (
        Triage::Unknown,
        format!(
            "{nums} — at least one answer is inside its band \
             (mail_only {NO}..{YES_MAIL_ONLY}, dead {NO}..{YES_DEAD}), so not committed"
        ),
    )
}

/// ONE PACK PER PIN, and that is a contract rather than a convenience.
///
/// There is deliberately no `triage_packs(rows)` helper. My first cut had one that
/// took a slice while the questions still said "this pin", so a caller handing it
/// the real six-row `flywheel-flywheel` backlog got six pins of evidence judged as
/// one subject and a single verdict covering all six. Nothing stopped it — the
/// signature accepted it.
///
/// The fix is not a better batch function, it is the ABSENCE of one: [`triage_pack`]
/// takes one objective, so the only way to get a pack is to have built the evidence
/// for exactly one pin. The live docs use the same shape for the same reason — one
/// Noul per candidate record, each question naming its own record — and they note
/// that questions run in parallel and cannot see one another's answers, so a batched
/// "is this dead" with six pins in scope is six questions about a set.
///
/// Cost is not the reason to keep them separate. The two Nouls for one pin are two
/// questions in ONE request, so a six-pin backlog is six requests, not twelve.
///
/// The evidence objective, with the LIVENESS VERDICT stated in words.
///
/// An absent pane is not evidence of death. This adds the stronger, load-bearing
/// statement: an UNMEASURED pin is
/// not evidence of anything, and only a measured age may be weighed. Without it the
/// model sees `liveness: "unmeasured"` as a bare string and is free to read it as
/// "nothing to report", which is the absence-as-evidence error one layer up.
#[must_use]
pub fn evidence_objective_with_liveness(row: &serde_json::Value) -> String {
    serde_json::json!({
        "question": "Judge the pin in `pin`. Is the doorbell's inability to reach it BY \
                     DESIGN (a mail-only crew on a harness no terminal multiplexer can \
                     inject into), or is the pin genuinely GONE (the workspace ended, or \
                     the agent was pruned and will never answer again)?",
        "pin": row,
        "note": [
            "A pin with no terminal pane is NOT evidence of death. Most pins in this \
             system have no pane and are working fine over mail.",
            "`liveness: measured` means the mail plane has a last-active stamp, and \
             `age_s` says how old it is. That is real evidence of life.",
            "`liveness: unmeasured` means NO INSTRUMENT covered this pin. It is not \
             evidence of death, not evidence of life, and not a weak yes. Say \
             uncommitted rather than guessing from silence.",
        ],
    })
    .to_string()
}

/// The workspace -> project map, read from the doorbell's armed host set.
///
/// Missing or unreadable is an EMPTY map, and every pin then reads
/// `liveness: unmeasured`. That is the right degradation: an unresolvable project
/// must not become a guessed one, because identity is per-project and a wrong guess
/// reads a DIFFERENT agent's activity and reports it as this pin's.
#[must_use]
pub fn read_host_projects(
    hosts_json: &std::path::Path,
) -> std::collections::BTreeMap<String, String> {
    let Ok(raw) = std::fs::read_to_string(hosts_json) else {
        return std::collections::BTreeMap::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return std::collections::BTreeMap::new();
    };
    let Some(arr) = v.get("hosts").and_then(serde_json::Value::as_array) else {
        return std::collections::BTreeMap::new();
    };
    let mut out = std::collections::BTreeMap::new();
    for pair in arr.iter().filter_map(serde_json::Value::as_array) {
        let [p, w] = pair.as_slice() else { continue };
        let (Some(p), Some(w)) = (p.as_str(), w.as_str()) else {
            continue;
        };
        out.insert(w.to_string(), p.to_string());
    }
    out
}

/// The pack: two Nouls, one request, each with the yes/no boundary spelled out in
/// `instructions`.
///
/// `criteria` is left empty on purpose. The local `QuestionDef.criteria` is a
/// `Vec<String>` rather than the docs' `{true, false}` object, and `crew_fit`
/// already sets the precedent of carrying the rubric in `instructions` so the
/// request never holds a second copy of anything code derived. Encoding the
/// boundary in prose is the honest fit for the type that exists; a half-used
/// `criteria` field would look like the documented shape without being it.
#[must_use]
pub fn triage_pack(objective: &str) -> QuestionPack {
    let questions = vec![
        QuestionDef {
            id: Q_MAIL_ONLY.to_string(),
            kind: QuestionKind::Noul,
            instructions: format!(
                "Is this pin unreachable BY DESIGN — a mail-only agent whose harness no \
                 terminal multiplexer can inject into? Answer yes when the pin is a \
                 working crew that simply has no injectable pane. Answer no when the \
                 pin looks abandoned rather than deliberately unreachable.\n\n\
                 Evidence: {objective}"
            ),
            options: Vec::new(),
            criteria: Vec::new(),
        },
        QuestionDef {
            id: Q_DEAD.to_string(),
            kind: QuestionKind::Noul,
            instructions: format!(
                "Is this pin genuinely GONE — the workspace ended, or the agent was \
                 pruned and will never answer again? Answer no when the pin may still be \
                 working, INCLUDING when it is mail-only: having no terminal pane is not \
                 evidence of death.\n\nEvidence: {objective}"
            ),
            options: Vec::new(),
            criteria: Vec::new(),
        },
    ];
    QuestionPack {
        id: "unanswered-triage".to_string(),
        version: 1,
        model: "jev-latest".to_string(),
        questions,
    }
}

/// The stage, under [`Placement::Inflight`] as the operator specified.
///
/// Composed with a `Code` preflight that does the part Jev cannot: refuse to spend
/// a request on an empty backlog. An empty `.unanswered` is the common case — 12 of
/// 16 live files have zero rows — and a judgment about nothing is a 5s wait for
/// nothing.
/// A stage's children, for a caller that needs a child's own report.
///
/// `Compose::run` returns a SUMMARY ("preflight=Holds, noul=Holds"), so a caller
/// that must see what the Jev child concluded has to reach the child. This is a
/// free function rather than a method so the `Stage` enum in `stage.rs` — shared
/// with `crew_fit` and CopperWren's judgment work — is not edited for one caller.
#[must_use]
pub fn children(stage: &Stage) -> &[Stage] {
    match stage {
        Stage::Compose { stages, .. } => stages,
        other => std::slice::from_ref(other),
    }
}

#[must_use]
pub fn unanswered_triage_stage() -> Stage {
    Stage::Compose {
        id: "unanswered-triage",
        rule: Rule::All,
        stages: vec![
            Stage::Code {
                id: "unanswered:non-empty",
                placement: Placement::Preflight,
                check: |ctx: &StageCtx| {
                    // Three outcomes, not two. A MALFORMED objective is not the same
                    // as an empty one, and lumping them together is how a bug becomes
                    // a shrug: `from_str(..).ok()` turns unparseable evidence into
                    // "zero pins", which reads as a clean refusal rather than as
                    // evidence code failed to build. Jev is an enhancement, never a
                    // dependency, so refusing is always safe — but it must be
                    // REFUSING for the stated reason.
                    match serde_json::from_str::<serde_json::Value>(ctx.objective) {
                        Err(e) => (
                            Signal::Fails,
                            format!("evidence is not JSON ({e}), so no request is spent"),
                        ),
                        // The objective NESTS the evidence row under `pin`, so a
                        // top-level null check always passed and this stage could
                        // never refuse anything. Look INSIDE the row for a
                        // non-empty pin — the restructure that added liveness moved
                        // the subject one level down and silently disabled the
                        // cheap check.
                        Ok(v) => {
                            let named = v
                                .get("pin")
                                .and_then(|row| row.get("pin"))
                                .and_then(serde_json::Value::as_str)
                                .filter(|p| !p.trim().is_empty());
                            match named {
                                None => (
                                    Signal::Fails,
                                    "no pin named in the evidence, so no request is spent"
                                        .to_string(),
                                ),
                                Some(pin) => (
                                    Signal::Holds,
                                    format!(
                                        "1 pin to triage: {}",
                                        crate::judgment::cap_str(pin, 60)
                                    ),
                                ),
                            }
                        }
                    }
                },
            },
            Stage::Jev {
                id: "unanswered-triage:noul",
                placement: Placement::Inflight,
                pack: |ctx: &StageCtx| triage_pack(ctx.objective),
                read: |answers: &crate::judgment::Answers, _ctx: &mut StageCtx| {
                    let pick = |id: &str| match answers.by_question.get(id) {
                        Some(crate::judgment::Answer::Noul(a)) => Some(a.p_yes),
                        _ => None,
                    };
                    let (verdict, detail) = triage(pick(Q_MAIL_ONLY), pick(Q_DEAD));
                    // `Holds` means "the triage produced a usable label". `Unknown`
                    // is ALSO Holds, deliberately: an escalation is a successful
                    // triage, and the stage must not report a shrug as a failure —
                    // a `Fails` here would read downstream as "do not prune", which
                    // happens to be the safe direction by accident rather than by
                    // design. The LABEL is the payload; the deterministic caller
                    // decides what a label may cause.
                    (Signal::Holds, format!("{}: {detail}", verdict.as_str()))
                },
            },
        ],
    }
}

/// bd-3uqz: the command handler, and the module's only caller.
///
/// It drives [`unanswered_triage_stage`] rather than calling [`triage`] directly,
/// because the stage is the design: the cheap `Code` preflight and the `Jev` child
/// are what an operator reviews, and a command that bypassed them would be a second
/// path with the same answers and none of the fences.
///
/// READ-ONLY by construction. There is no write path in this file at all, which is
/// the point: the `.unanswered` files are what gate P2b reads to decide whether a
/// crew is invisible, bd-y90n established that quieting them is the one thing that
/// must not happen, and the cheapest guarantee is a triage that cannot write. A
/// `dead` label is a review-queue entry a human acts on.
///
/// The output distinguishes three things a single `label` field would conflate:
///  - `label: null` + `triage: "unknown"` — we asked, and declined. `null` is
///    visibly the absence of a judgment rather than a finding.
///  - `asked: false` + a transport reason — we never spent a request. Different
///    follow-up: fix the key, not the pin.
///  - `detail` carries both Noul numbers, so any label is auditable later without
///    re-asking.
pub fn run(
    state_dir: &std::path::Path,
    doorbell_state_dir: &std::path::Path,
    transport: &dyn crate::judgment::Transport,
    dry_run: bool,
    now_unix: u64,
    toron_bin: &str,
) -> anyhow::Result<serde_json::Value> {
    // The `.unanswered` files live under the FLYWHEEL state dir, not the doorbell
    // state dir: `$XDG_STATE_HOME/flywheel/doorbell/<workspace>.unanswered`. I
    // looked in the doorbell dir first and got "No such file or directory", which
    // is a useful failure and still a wrong default.
    let dir = state_dir.join("doorbell");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", dir.display()))?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".unanswered"))
        })
        .collect();
    files.sort();

    // The workspace -> project map, read from the doorbell's OWN armed host set
    // rather than guessed. The `.unanswered` file is named by WORKSPACE
    // (`flywheel-toron.unanswered`) and says nothing about the project, and the
    // project decides the npub: `whois flywheel EmberOtter` and
    // `whois toron EmberOtter` return two different keys for the same pin. A
    // guessed project would read a different agent's activity and report it as
    // this pin's liveness.
    // hosts.json is in the DOORBELL state dir, not the flywheel one. I read
    // `flywheel/hosts.json` first — which does not exist, so the map was silently
    // EMPTY and every pin read `unmeasured`. That is the third time in this
    // session a wrong base directory produced a plausible empty result rather than
    // an error, which is the reason both directories are now explicit parameters.
    let project_of = read_host_projects(&doorbell_state_dir.join("hosts.json"));

    let mut workspaces = Vec::new();
    let mut pins_total = 0usize;
    let mut unmeasured = 0usize;
    let mut labels: std::collections::BTreeMap<&str, usize> = Default::default();
    let stage = unanswered_triage_stage();

    for file in &files {
        let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let workspace = name.trim_end_matches(".unanswered").to_string();
        let Ok(raw) = std::fs::read_to_string(file) else {
            continue;
        };
        let pins: Vec<String> = raw
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        pins_total += pins.len();
        let mut rows = Vec::new();
        for pin in &pins {
            // The evidence is what CODE knows. Jev is only asked to read the
            // semantics; an absent pane is stated as absence, never as death.
            let liveness = liveness_from_whois(
                project_of.get(&workspace).map(String::as_str),
                pin,
                toron_bin,
            );
            let row = evidence_with_liveness(pin, name, liveness, now_unix);
            if liveness == Liveness::Unmeasured {
                unmeasured += 1;
            }
            if dry_run {
                rows.push(serde_json::json!({
                    "pin": pin, "label": null, "triage": "not_asked", "asked": false,
                    "detail": "dry run: no request spent",
                }));
                *labels.entry("not_asked").or_default() += 1;
                continue;
            }
            let objective = evidence_objective_with_liveness(&row);
            // Drive the stage's OWN CHILDREN rather than the Compose wrapper, and
            // ask ONCE.
            //
            // `Compose::run` renders a summary ("preflight=Holds, noul=Holds"), so
            // the label the Jev child produced does not survive composition. My
            // first version ran the Compose and then asked AGAIN to recover the
            // numbers — 32 requests for a 16-pin backlog, and a second call that
            // could disagree with the first. Running the cheap preflight and the
            // Jev child directly keeps every fence AND spends one request.
            let mut menu = crate::crew_menu::HarnessMenu {
                crew: 1,
                options: Vec::new(),
                registered_unconfigured: Vec::new(),
            };
            let mut ctx = StageCtx {
                objective: &objective,
                menu: &mut menu,
            };
            let preflight = children(&stage)
                .iter()
                .find(|c| matches!(c, Stage::Code { id, .. } if *id == "unanswered:non-empty"))
                .expect("the preflight child");
            let pre = preflight.run(&mut ctx, transport);
            if pre.signal != Signal::Holds {
                rows.push(serde_json::json!({
                    "pin": pin, "label": null, "triage": "not_asked", "asked": false,
                    "detail": pre.detail,
                }));
                *labels.entry("not_asked").or_default() += 1;
                continue;
            }
            let jev = children(&stage)
                .iter()
                .find(|c| matches!(c, Stage::Jev { id, .. } if *id == "unanswered-triage:noul"))
                .expect("the Jev child");
            let (triage_verdict, detail) = match jev.run(&mut ctx, transport) {
                r if r.signal == Signal::Unknown => (
                    Triage::Unknown,
                    // The child's detail is "unknown: no answer (...)" on a declined
                    // or unreachable ask, which is exactly the report to keep.
                    r.detail,
                ),
                r => {
                    // The child prefixes its detail with the label, so the label is
                    // recoverable without a second request. The format is pinned by
                    // a test, because a silent change here would make every pin read
                    // as "unknown" while still spending the request.
                    let label = r
                        .detail
                        .split_once(':')
                        .map(|(head, _)| head.trim().to_string())
                        .unwrap_or_default();
                    match label.as_str() {
                        "dead" => (Triage::Dead, r.detail),
                        "mail_only_by_design" => (Triage::MailOnlyByDesign, r.detail),
                        "unknown" => (Triage::Unknown, r.detail),
                        other => (
                            Triage::Unknown,
                            format!("unrecognised label {other:?} from the stage: {}", r.detail),
                        ),
                    }
                }
            };
            *labels.entry(triage_verdict.as_str()).or_default() += 1;
            rows.push(serde_json::json!({
                "pin": pin,
                "label": match triage_verdict {
                    Triage::Unknown => serde_json::Value::Null,
                    other => serde_json::Value::String(other.as_str().to_string()),
                },
                "triage": triage_verdict.as_str(),
                // `Unknown` means the judgment did not happen, which is what
                // `asked` must report — not merely that the label is null.
                "asked": triage_verdict != Triage::Unknown || !detail.starts_with("not triaged"),
                "detail": detail,
            }));
        }
        workspaces.push(serde_json::json!({
            "workspace": workspace,
            "file": name,
            "pins": pins.len(),
            "rows": rows,
        }));
    }

    // Computed here rather than inside the `json!` macro: a macro invocation
    // takes expressions, not statements, and a `for` loop in one is a syntax error.
    let unresolvable = files
        .iter()
        .filter_map(|f| f.file_name().and_then(|x| x.to_str()))
        .filter(|n| !project_of.contains_key(n.trim_end_matches(".unanswered")))
        .count();

    Ok(serde_json::json!({
        "verdict": "unanswered-triage.v1",
        // Stated so nobody reads this output as permission to prune.
        "wrote_anything": false,
        "dry_run": dry_run,
        "scanned_dir": dir.display().to_string(),
        "armed_hosts": project_of.len(),
        "workspaces_without_project": unresolvable,
        "workspaces_scanned": workspaces.len(),
        "pins_total": pins_total,
        "pins_unmeasured": unmeasured,
        // Named so a wall of `unknown` is legible as a missing INSTRUMENT rather
        // than as sixteen inconclusive model opinions.
        "instrument_gap": if unmeasured == 0 {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "pins_without_liveness": unmeasured,
                "why": "no `last_active_ts_us` for this pin, or no project could be \
                        resolved for its workspace. UNMEASURED, not dead.",
            })
        },
        "tally": labels,
        "workspaces": workspaces,
    }))
}

// ---------------------------------------------------------------------------
// Liveness: the instrument, and its absence
// ---------------------------------------------------------------------------

/// bd-3uqz: what the doorbell can actually PROVE about a pin, before any judgment.
///
/// This exists because the first wired run returned `unknown` for all sixteen real
/// pins, and the reason was not a bad question. The evidence the code could gather
/// was `{pin, injects: false, pane_known_to_herdr: false}` — pure absence — so Jev
/// correctly answered "confidently not dead" (0.12) and declined to say "mail-only
/// by design", for which there was no evidence of design at all.
///
/// I went looking for a liveness source and the survey is worth recording, because
/// three of the four candidates are traps:
///
///  - `toron presence list` is EMPTY for both live projects. These agents do not
///    publish heartbeats, so presence is not an instrument here at all.
///  - the journal's per-pin attribution exists but covers only 4 of 16 pins: it is
///    written on `HerdrStatus` drains (`msg.pin`), while sweep rows carry no pin.
///    The top-level `pins` ARRAY is present in the schema and empty in every one of
///    4065 rows of the flywheel-toron journal, so a reader that trusted the field
///    name rather than the data would conclude every pin is dead.
///  - the doorbell audit carries 936 pin-bearing rows and every one is NEGATIVE:
///    760 `inbox_error`, 174 `registry_pin_stale`, 2 `inbox_pin_miss`.
///    `registry_pin_stale` is exactly the pane-absent signal bd-oa4u established
///    must never be read as death, so feeding it as liveness would resurrect the
///    precise error this whole bead exists to prevent.
///  - `toron mail whois <project> <pin> --json` returns `last_active_ts_us`. This
///    is the only real instrument, and it is one subprocess per pin with no bulk
///    form (`toron agent` has no `list`/`roster`).
///
/// So the honest shape is: a liveness source that can be ABSENT, reported as
/// absent. `None` here means UNMEASURED, and it is deliberately not rendered as a
/// large age, because "we did not look" and "we looked and it has been quiet for a
/// month" must not produce the same evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The mail plane has an activity stamp for this pin.
    Active { last_active_unix: u64 },
    /// No instrument covers this pin. NOT "quiet", NOT "dead".
    Unmeasured,
}

impl Liveness {
    /// Age in seconds against a caller-supplied clock. The core never reads the
    /// clock itself; the host passes `now` in, per the effect law.
    #[must_use]
    pub fn age_s(self, now_unix: u64) -> Option<u64> {
        match self {
            Liveness::Active { last_active_unix } => {
                Some(now_unix.saturating_sub(last_active_unix))
            }
            Liveness::Unmeasured => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Liveness::Active { .. } => "measured",
            Liveness::Unmeasured => "unmeasured",
        }
    }
}

/// The one instrument, isolated so it is the ONLY place a subprocess happens.
///
/// `project` is a parameter and is never inferred. Identity is PER-PROJECT: measured
/// today, `toron mail whois flywheel EmberOtter` and `toron mail whois toron
/// EmberOtter` return two different npubs, so a guessed project silently reads a
/// DIFFERENT AGENT's activity. Guessing here would be the same identity collision
/// bd-xroute describes, wearing a liveness hat.
pub fn liveness_from_whois(project: Option<&str>, pin: &str, toron_bin: &str) -> Liveness {
    let Some(project) = project else {
        return Liveness::Unmeasured;
    };
    let out = std::process::Command::new(toron_bin)
        .args(["mail", "whois", project, pin, "--json"])
        .output();
    let Ok(out) = out else {
        return Liveness::Unmeasured;
    };
    if !out.status.success() {
        return Liveness::Unmeasured;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Liveness::Unmeasured;
    };
    match v
        .get("last_active_ts_us")
        .and_then(serde_json::Value::as_u64)
    {
        Some(us) => Liveness::Active {
            // A zero stamp means "never seen", which is UNMEASURED, not "active at
            // the epoch" — reading it as the latter would make a never-seen pin look
            // maximally dead, which is precisely the error being fenced.
            last_active_unix: if us == 0 {
                return Liveness::Unmeasured;
            } else {
                us / 1_000_000
            },
        },
        None => Liveness::Unmeasured,
    }
}

/// The evidence row, with liveness folded in and the absence stated in words.
#[must_use]
pub fn evidence_with_liveness(
    pin: &str,
    file: &str,
    liveness: Liveness,
    now_unix: u64,
) -> serde_json::Value {
    let mut row = serde_json::json!({
        "pin": pin,
        "injects": false,
        "pane_known_to_herdr": false,
        "file": file,
        "liveness": liveness.as_str(),
    });
    if let Some(age) = liveness.age_s(now_unix) {
        row["age_s"] = serde_json::json!(age);
    }
    row
}

/// Wall clock, owned by the host so the core stays pure.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// bd-3uqz: the liveness instrument. Every one of these encodes a way the
/// "is it alive" question goes wrong, and three of them are ways it went wrong
/// live during this work.
///
/// Its own `mod` rather than an addition to `mod tests`, because it was written
/// after that module and appending across the boundary is how a stray brace ends
/// up closing the wrong thing.
#[cfg(test)]
mod liveness_tests {
    use super::*;

    /// "We did not look" and "we looked and it has been quiet for a month" must
    /// not produce the same evidence, or the model weighs silence as death.
    #[test]
    fn unmeasured_liveness_is_never_rendered_as_a_large_age() {
        assert_eq!(Liveness::Unmeasured.age_s(1_000_000), None);
        assert_eq!(Liveness::Unmeasured.as_str(), "unmeasured");
        let row = evidence_with_liveness("Pin", "ws.unanswered", Liveness::Unmeasured, 1_000);
        assert_eq!(row["liveness"], "unmeasured");
        assert!(
            row.get("age_s").is_none(),
            "no age at all when unmeasured, not age 0 and not a big number: {row}"
        );
    }

    #[test]
    fn a_measured_liveness_reports_its_age_and_saturates_rather_than_wrapping() {
        let live = Liveness::Active {
            last_active_unix: 900,
        };
        assert_eq!(live.age_s(1_000), Some(100));
        // A clock that went backwards must not produce a huge age, which would read
        // as "dead for a very long time" — the worst possible direction to be wrong.
        assert_eq!(live.age_s(800), Some(0), "saturating, not wrapping");
        assert_eq!(live.as_str(), "measured");
        let row = evidence_with_liveness("Pin", "ws.unanswered", live, 1_000);
        assert_eq!(row["liveness"], "measured");
        assert_eq!(row["age_s"], 100);
    }

    /// The ZERO-STAMP case. A registered identity that has never been active reports
    /// `last_active_ts_us: 0`, and reading that as a unix time makes the pin look
    /// dead since the epoch — maximally dead, from a field that means "never seen".
    #[test]
    fn a_zero_stamp_is_unmeasured_not_active_at_the_epoch() {
        let dir = std::env::temp_dir().join(format!("armstate-zero-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dirs");
        let script = dir.join("toron");
        std::fs::write(&script, "#!/bin/sh\necho '{\"last_active_ts_us\": 0}'\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let l = liveness_from_whois(Some("flywheel"), "Pin", script.to_str().unwrap());
        assert_eq!(
            l,
            Liveness::Unmeasured,
            "a zero stamp means never-seen, not dead since 1970"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Identity is PER-PROJECT, so an unresolvable project must not become a
    /// guessed one. Measured today: `whois flywheel EmberOtter` and
    /// `whois toron EmberOtter` return two DIFFERENT npubs, so a guess reads a
    /// different agent's activity and reports it as this pin's liveness.
    #[test]
    fn no_project_means_unmeasured_and_never_a_guess() {
        assert_eq!(
            liveness_from_whois(None, "EmberOtter", "definitely-not-a-binary"),
            Liveness::Unmeasured,
            "with no project the lookup must not run at all"
        );
        // And a missing binary is unmeasured, not a panic.
        assert_eq!(
            liveness_from_whois(Some("flywheel"), "EmberOtter", "definitely-not-a-binary"),
            Liveness::Unmeasured
        );
    }

    /// The workspace -> project map, and its degradation. Both state dirs are
    /// explicit parameters precisely because I read the wrong one first and got a
    /// silently EMPTY map, which made all sixteen pins read `unmeasured`.
    #[test]
    fn the_project_map_comes_from_the_armed_hosts_and_degrades_to_empty() {
        let dir = std::env::temp_dir().join(format!("armstate-hosts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dirs");
        let path = dir.join("hosts.json");
        assert!(
            read_host_projects(&path).is_empty(),
            "a missing file is an empty map, not a panic"
        );
        std::fs::write(&path, "not json").expect("write");
        assert!(read_host_projects(&path).is_empty(), "corrupt is empty too");
        std::fs::write(
            &path,
            r#"{"hosts":[["flywheel","flywheel-flywheel"],["toron","flywheel-toron"]],"v":1}"#,
        )
        .expect("write");
        let m = read_host_projects(&path);
        assert_eq!(m.get("flywheel-toron").map(String::as_str), Some("toron"));
        assert_eq!(
            m.get("flywheel-flywheel").map(String::as_str),
            Some("flywheel")
        );
        assert!(
            !m.contains_key("adr22"),
            "an unhosted workspace has NO project and must not inherit one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The note the model reads. It is the load-bearing sentence: without it,
    /// `liveness: unmeasured` is a bare string the model is free to read as
    /// "nothing to report", which is the absence-as-evidence error one layer up.
    #[test]
    fn the_evidence_states_that_unmeasured_is_not_death_in_words() {
        let row = evidence_with_liveness("Pin", "ws.unanswered", Liveness::Unmeasured, 1_000);
        let obj = evidence_objective_with_liveness(&row);
        let v: serde_json::Value = serde_json::from_str(&obj).expect("json");
        let notes = v["note"].as_array().expect("array of notes");
        let blob = notes
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            blob.contains("NOT evidence of death"),
            "the absent-pane rule travels with the evidence: {blob}"
        );
        assert!(
            blob.contains("unmeasured") && blob.contains("NO INSTRUMENT"),
            "and unmeasured is named as a missing INSTRUMENT, not as quiet: {blob}"
        );
        assert!(
            blob.contains("not evidence of death"),
            "and it is denied death explicitly: {blob}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew_menu::HarnessMenu;

    /// A menu is irrelevant to this stage; the triage never reads it. Built in one
    /// place so a future field on `HarnessMenu` is a one-line change here rather
    /// than three.
    fn menu() -> HarnessMenu {
        HarnessMenu {
            crew: 1,
            options: Vec::new(),
            registered_unconfigured: Vec::new(),
        }
    }

    /// The case this fence exists for: `dead` is confident and `mail_only` has NOT
    /// committed. One confident signal is never enough.
    ///
    /// My first version of this test used mail_only=0.10 and asserted Unknown. That
    /// was wrong about my own rule: 0.10 is below NO=0.20, so it is a confident LOW
    /// and `Dead` was correct. A shrug is the middle of the band, so the value that
    /// actually exercises the fence is 0.45.
    #[test]
    fn a_confident_dead_still_needs_mail_only_to_be_confidently_low() {
        let (v, d) = triage(Some(0.45), Some(0.95));
        assert_eq!(v, Triage::Unknown, "{d}");
        assert!(d.contains("mail_only=0.45"), "the shrug is named: {d}");
        assert!(
            d.contains("band"),
            "and the reason is the band, not silence: {d}"
        );
        // And the symmetric case: mail_only confident, dead a shrug. Same rule,
        // other side — and again the shrug has to be IN the band, not merely low.
        let (v2, d2) = triage(Some(0.90), Some(0.45));
        assert_eq!(v2, Triage::Unknown, "{d2}");
        // A confident LOW on the other proposition is NOT a shrug and does commit.
        let (v3, d3) = triage(Some(0.90), Some(0.10));
        assert_eq!(v3, Triage::MailOnlyByDesign, "{d3}");
    }

    #[test]
    fn dead_requires_mail_only_to_be_confidently_low() {
        let (v, d) = triage(Some(0.02), Some(0.97));
        assert_eq!(v, Triage::Dead, "{d}");
        let (v2, d2) = triage(Some(0.05), Some(0.99));
        assert_eq!(v2, Triage::Dead, "{d2}");
    }

    #[test]
    fn a_confident_mail_only_crew_is_not_a_corpse() {
        let (v, d) = triage(Some(0.93), Some(0.04));
        assert_eq!(v, Triage::MailOnlyByDesign, "{d}");
    }

    /// The middle band is the escalate region, and it must escalate in BOTH
    /// directions. A one-sided floor would read 0.3 as a yes.
    #[test]
    fn the_middle_band_escalates_rather_than_committing() {
        for (m, d) in [(0.5, 0.9), (0.9, 0.5), (0.5, 0.5), (0.3, 0.95), (0.95, 0.3)] {
            let (v, detail) = triage(Some(m), Some(d));
            assert_eq!(
                v,
                Triage::Unknown,
                "mail_only={m} dead={d} must not commit: {detail}"
            );
            assert!(
                detail.contains(&format!("mail_only={m:.2}"))
                    && detail.contains(&format!("dead={d:.2}")),
                "both numbers ride along so a shrug is distinguishable from a stage \
                 that never ran: {detail}"
            );
        }
    }

    /// Both propositions confident is NOT a tie to break. They are not a partition:
    /// a pin can be mail-only AND look dead. Unknown, with the reason named.
    #[test]
    fn both_propositions_confident_is_an_ambiguity_not_a_label() {
        let (v, d) = triage(Some(0.88), Some(0.91));
        assert_eq!(v, Triage::Unknown, "{d}");
        assert!(d.contains("not a partition"), "the reason is named: {d}");
    }

    /// The "we never asked" path. No key, transport error, timeout — all of it is a
    /// shrug, and a shrug never prunes a pin.
    #[test]
    fn a_missing_answer_is_unknown_and_names_the_gap() {
        for (m, d) in [(None, Some(0.99)), (Some(0.99), None), (None, None)] {
            let (v, detail) = triage(m, d);
            assert_eq!(
                v,
                Triage::Unknown,
                "m={m:?} d={d:?} must not commit: {detail}"
            );
            assert!(detail.contains("no answer"), "{detail}");
        }
    }

    #[test]
    fn the_pack_asks_two_nouls_in_one_request() {
        let pack = triage_pack("{}");
        assert_eq!(
            pack.questions.len(),
            2,
            "one proposition per Noul: {pack:?}"
        );
        assert!(
            pack.questions.iter().all(|q| q.kind == QuestionKind::Noul),
            "both are Nouls, not a Choice over labels"
        );
        let ids: Vec<&str> = pack.questions.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids, vec![Q_MAIL_ONLY, Q_DEAD]);
        // The boundary has to be in the text, because the local `criteria` is a
        // Vec<String> and the docs' {true,false} shape does not exist here.
        let dead = &pack.questions[1].instructions;
        assert!(
            dead.contains("not evidence of death"),
            "the mail-only-is-not-dead rule must be IN the question: {dead}"
        );
    }

    #[test]
    fn the_evidence_carries_the_rows_as_named_json() {
        let row = serde_json::json!({"pin": "EmberOtter", "panes_missing": true});
        let obj = evidence_objective_with_liveness(&row);
        let v: serde_json::Value = serde_json::from_str(&obj).expect("valid JSON");
        assert!(v["pin"].is_object(), "the subject is ONE pin: {v}");
        // The note is an ARRAY of sentences, not one string: the two rules are
        // independent claims and a reader must be able to see them separately.
        let blob = v["note"]
            .as_array()
            .expect("note is an array of sentences")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            blob.contains("NOT evidence of death"),
            "the absent-pane warning travels WITH the evidence: {blob}"
        );
    }

    /// The preflight must refuse to spend a request on nothing. 12 of 16 live
    /// `.unanswered` files have zero rows, so the empty case is the COMMON one, and
    /// a judgment about nothing is a 5s wait for nothing.
    ///
    /// The assertion is on the child's SIGNAL, not on prose. `Compose` renders its
    /// own summary detail, so the child's "no pins to triage" string does not appear
    /// in `report.detail` — my first version asserted on that string and failed
    /// against correct code.
    #[test]
    fn the_preflight_refuses_an_empty_backlog_without_asking_anything() {
        let mut menu = menu();
        let empty = evidence_objective_with_liveness(&serde_json::json!({ "pin": null }));
        let mut ctx = StageCtx {
            objective: &empty,
            menu: &mut menu,
        };
        let report = unanswered_triage_stage().run(&mut ctx, &crate::judgment::OfflineTransport);
        assert_eq!(report.signal, Signal::Fails, "{}", report.detail);
        assert!(
            report.detail.contains("unanswered:non-empty=Fails"),
            "the CHEAP check is the one that stopped it, by name: {}",
            report.detail
        );
    }

    /// And the honest counterpart, which is the property that matters most.
    ///
    /// `Stage::Jev::run` turns a transport error into [`Signal::Unknown`] BEFORE the
    /// `read` closure is called, so an unreachable Jev can never reach the label
    /// logic at all. I had written this test expecting `Holds` ("an escalation is a
    /// successful triage") and it failed — correctly, because the framework already
    /// does the safer thing. The contract worth pinning is the one the framework
    /// gives for free: no key, a transport error, and a timeout all land on
    /// `Unknown`, which is the deterministic path standing.
    #[test]
    fn an_unreachable_jev_is_unknown_which_is_the_deterministic_path_standing() {
        let mut menu = menu();
        let obj = evidence_objective_with_liveness(&serde_json::json!({"pin": "TealMarlin"}));
        let mut ctx = StageCtx {
            objective: &obj,
            menu: &mut menu,
        };
        let report = unanswered_triage_stage().run(&mut ctx, &crate::judgment::OfflineTransport);
        assert_eq!(
            report.signal,
            Signal::Unknown,
            "a shrug must never read as a verdict: {}",
            report.detail
        );
        assert!(
            report.detail.contains("unanswered:non-empty=Holds"),
            "the preflight DID pass, so this is an unavailable judgment and not a \
             refused backlog: {}",
            report.detail
        );
    }

    /// A live-ish transport that answers both questions, so the `read` closure is
    /// actually reached. This is the only test that exercises the label path end to
    /// end, and it pins that `read` never reports a shrug as a failure.
    #[test]
    fn answers_are_read_into_a_label_and_never_into_a_failure() {
        use crate::judgment::{Answer, Answers, JevError, NoulAnswer, StateSlice, Transport};
        struct Fixed(f64, f64);
        impl Transport for Fixed {
            fn ask(&self, _s: &StateSlice, pack: &QuestionPack) -> Result<Answers, JevError> {
                let mut by_question = std::collections::BTreeMap::new();
                for q in &pack.questions {
                    let p = if q.id == Q_MAIL_ONLY { self.0 } else { self.1 };
                    by_question.insert(q.id.clone(), Answer::Noul(NoulAnswer { p_yes: p }));
                }
                Ok(Answers {
                    model_id: "test".into(),
                    pack_id: pack.id.clone(),
                    pack_version: pack.version,
                    by_question,
                    usage: "test".into(),
                })
            }
        }
        for (m, d, want) in [
            (0.02, 0.97, Triage::Dead),
            (0.93, 0.04, Triage::MailOnlyByDesign),
            (0.50, 0.50, Triage::Unknown),
            (0.88, 0.91, Triage::Unknown),
        ] {
            let mut menu = menu();
            let obj = evidence_objective_with_liveness(&serde_json::json!({"pin": "EmberOtter"}));
            let mut ctx = StageCtx {
                objective: &obj,
                menu: &mut menu,
            };
            // The COMPOSED report renders a summary ("child=Holds, child=Holds"), so
            // the label is asserted on the JEV CHILD, which is where `read` actually
            // puts it. Reaching into the Compose is deliberate: the label is the
            // payload of this whole stage and nothing else exposes it.
            let stage = unanswered_triage_stage();
            let Stage::Compose { stages, .. } = &stage else {
                panic!("the triage must stay a Compose so the cheap check runs first");
            };
            let jev = stages
                .iter()
                .find(|s| matches!(s, Stage::Jev { id, .. } if *id == "unanswered-triage:noul"))
                .expect("the Jev child");
            let report = jev.run(&mut ctx, &Fixed(m, d));
            assert_eq!(
                report.signal,
                Signal::Holds,
                "a triage that produced a label is a SUCCESS whatever the label: {}",
                report.detail
            );
            assert!(
                report.detail.contains(want.as_str()),
                "mail_only={m} dead={d} should read {}: {}",
                want.as_str(),
                report.detail
            );
            // The numbers must ride along: a label with no numbers is not auditable.
            assert!(
                report.detail.contains(&format!("{m:.2}"))
                    && report.detail.contains(&format!("{d:.2}")),
                "both Noul values are named beside the label: {}",
                report.detail
            );
        }
    }

    /// bd-3uqz: the LIVE check. `#[ignore]`d so CI never spends a request, and run
    /// explicitly with `--ignored`. This is the only proof that the two questions
    /// are ANSWERABLE — a pack can satisfy every structural test and still ask a
    /// question the model cannot resolve, and the numbers coming back flat around
    /// 0.5 on both would be the signature.
    #[test]
    #[ignore = "live Jev ask; run with --ignored and TYPESAFE_API_KEY set"]
    fn a_live_ask_separates_a_mail_only_crew_from_a_dead_pin() {
        use crate::judgment::{Answer, SYSTEMONE_URL, StateSlice, Transport, UreqTransport};
        let Ok(key) = std::env::var("TYPESAFE_API_KEY") else {
            eprintln!("skipping: TYPESAFE_API_KEY unset");
            return;
        };
        let transport = UreqTransport {
            api_key: key,
            url: SYSTEMONE_URL.to_string(),
        };
        // Two deliberately CONTRASTED cases, because a pack that returns the same
        // number for both has measured nothing.
        let cases: Vec<(&str, serde_json::Value)> = vec![
            (
                "a pin on a harness herdr cannot inject, still posting to the room",
                serde_json::json!({
                    "pin": "IvoryRidge",
                    "harness": "freebuff",
                    "pane_known_to_herdr": false,
                    "injects": false,
                    "recent_signal": "posted to the channel 4 minutes ago",
                }),
            ),
            (
                "a pin whose workspace was explicitly torn down and whose agent was pruned",
                serde_json::json!({
                    "pin": "adr22",
                    "harness": "opencode",
                    "pane_known_to_herdr": false,
                    "injects": false,
                    "workspace_closed": true,
                    "agent_pruned": true,
                    "recent_signal": "the workspace was deleted and the pin deregistered",
                }),
            ),
            (
                "a pin whose workspace ended and which has not answered in weeks",
                serde_json::json!({
                    "pin": "adr22",
                    "harness": "opencode",
                    "pane_known_to_herdr": false,
                    "injects": false,
                    "recent_signal": "no journal rows, no room posts, no receipt",
                }),
            ),
        ];
        for (label, row) in cases {
            let obj = evidence_objective_with_liveness(&row);
            let pack = triage_pack(&obj);
            let answers = transport
                .ask(
                    &StateSlice {
                        fields: std::collections::BTreeMap::new(),
                        truncated: false,
                    },
                    &pack,
                )
                .expect("live ask");
            let n = |id: &str| match answers.by_question.get(id) {
                Some(Answer::Noul(a)) => a.p_yes,
                other => panic!("{id} came back {other:?}"),
            };
            let (verdict, detail) = triage(Some(n(Q_MAIL_ONLY)), Some(n(Q_DEAD)));
            eprintln!(
                "  {label}\n    -> {detail}\n    verdict={}",
                verdict.as_str()
            );
        }
    }

    /// bd-3uqz: the cases a first pass would have missed.
    ///
    /// 1. A WRONG-TYPED answer. If the model returns a `Choice` where a `Noul` was
    ///    asked, the value must read as `Unknown`, not as a missing key and not as
    ///    some other question's number. This is the "absent rather than wrong"
    ///    contract at the boundary where it is easiest to get wrong.
    #[test]
    fn a_wrong_typed_answer_reads_as_unknown_not_as_a_number() {
        use crate::judgment::{Answer, Answers, ChoiceAnswer, JevError, StateSlice, Transport};
        struct WrongType;
        impl Transport for WrongType {
            fn ask(&self, _s: &StateSlice, pack: &QuestionPack) -> Result<Answers, JevError> {
                let mut by_question = std::collections::BTreeMap::new();
                for q in &pack.questions {
                    // A Choice where a Noul was asked: the shape is wrong, and the
                    // only safe reading is that we do not have the number.
                    by_question.insert(
                        q.id.clone(),
                        Answer::Choice(ChoiceAnswer {
                            choice: "yes".into(),
                            confidence: 0.99,
                            probs: Default::default(),
                        }),
                    );
                }
                Ok(Answers {
                    model_id: "test".into(),
                    pack_id: pack.id.clone(),
                    pack_version: pack.version,
                    by_question,
                    usage: "test".into(),
                })
            }
        }
        let mut menu = menu();
        let obj = evidence_objective_with_liveness(&serde_json::json!({"pin": "EmberOtter"}));
        let mut ctx = StageCtx {
            objective: &obj,
            menu: &mut menu,
        };
        let stage = unanswered_triage_stage();
        let Stage::Compose { stages, .. } = &stage else {
            panic!("compose");
        };
        let jev = stages
            .iter()
            .find(|s| matches!(s, Stage::Jev { id, .. } if *id == "unanswered-triage:noul"))
            .expect("jev child");
        let report = jev.run(&mut ctx, &WrongType);
        assert!(
            report.detail.contains("unknown"),
            "a shape we did not ask for must not become a verdict: {}",
            report.detail
        );
    }

    /// 2. The THRESHOLD BOUNDARY. `dead` must be `> YES` and `mail_only` `< NO`,
    ///    strictly. Landing exactly ON a boundary is not confidence, and an
    ///    off-by-one here is the difference between labelling a pin and escalating.
    #[test]
    fn the_threshold_boundaries_are_strict_in_both_directions() {
        // Exactly on YES is not "confidently dead".
        let (v, d) = triage(Some(0.05), Some(YES_DEAD));
        assert_eq!(
            v,
            Triage::Unknown,
            "dead={YES_DEAD} is the boundary, not a yes: {d}"
        );
        // Exactly on NO is not "confidently not-by-design".
        let (v2, d2) = triage(Some(NO), Some(0.95));
        assert_eq!(v2, Triage::Unknown, "mail_only={NO} is the boundary: {d2}");
        // A hair past each one does commit.
        let (v3, _) = triage(Some(0.05), Some(YES_DEAD + 0.001));
        assert_eq!(v3, Triage::Dead);
        let (v4, _) = triage(Some(NO - 0.001), Some(0.95));
        assert_eq!(v4, Triage::Dead);
    }

    /// 3. ONE PACK PER PIN, which is the defect this refactor fixed. There is no
    ///    batch helper to test — the contract is that each pack's evidence and each
    ///    question name the SAME single pin, so a multi-pin backlog cannot be
    ///    collapsed into one subject.
    #[test]
    fn each_pins_pack_names_that_pin_and_no_other() {
        for want in ["EmberOtter", "TealMarlin", "IvoryRidge"] {
            let obj = evidence_objective_with_liveness(&serde_json::json!({ "pin": want }));
            let pack = triage_pack(&obj);
            for other in ["EmberOtter", "TealMarlin", "IvoryRidge"]
                .iter()
                .filter(|o| **o != want)
            {
                assert!(
                    !pack
                        .questions
                        .iter()
                        .any(|q| q.instructions.contains(other)),
                    "the pack for {want} leaked {other}: {:?}",
                    pack.questions[0].instructions
                );
            }
            assert!(
                pack.questions.iter().all(|q| q.instructions.contains(want)),
                "the question must NAME its subject, or the model is judging a set: {:?}",
                pack.questions[0].instructions
            );
            // And the subject is a single `pin`, never a `pins` array.
            let v: serde_json::Value = serde_json::from_str(&obj).expect("json");
            assert!(v["pin"].is_object() && v.get("pins").is_none(), "{v}");
        }
    }

    /// The ASYMMETRY is a decision and must stay visible: `mail_only_by_design` is a
    /// label with no automated consequence, so its bar is lower than `dead`'s, which
    /// a human might act on by pruning. If these ever converge, the reasoning is
    /// gone and the cheap label has silently become as expensive as the risky one.
    #[test]
    fn the_two_labels_have_different_bars_and_the_reasoning_is_recorded() {
        // A `const` assertion, not a runtime one: clippy is right that a `#[test]`
        // comparing two consts is a value known at compile time, and this project
        // already converts those (bd-3uqz, toron). The pairing assertion below
        // covers the runtime behaviour.
        // `mail_only` above ITS bar and `dead` confidently low is the label. My
        // first version used dead=0.74 — merely under the dead BAR, which is not
        // "confidently not dead" and correctly does not commit.
        let (v, d) = triage(Some(0.74), Some(0.10));
        assert_eq!(v, Triage::MailOnlyByDesign, "{d}");
        // And dead=0.74, in the gap between the two bars, still escalates.
        let (vg, dg) = triage(Some(0.74), Some(0.74));
        assert_eq!(vg, Triage::Unknown, "the gap is not a yes: {dg}");
        // And a `dead` at the same value stays far from committed.
        let (v2, d2) = triage(Some(0.10), Some(0.74));
        assert_eq!(v2, Triage::Unknown, "dead must not move for this: {d2}");
    }

    /// 4. MALFORMED evidence is its own refusal reason. Folding it into "no pins"
    ///    is how a bug becomes a shrug: the log would say the backlog was empty
    ///    when the truth is that code failed to build the evidence.
    #[test]
    fn malformed_evidence_refuses_for_its_own_stated_reason() {
        let mut menu = menu();
        let mut ctx = StageCtx {
            objective: "this is not json",
            menu: &mut menu,
        };
        let stage = unanswered_triage_stage();
        let Stage::Compose { stages, .. } = &stage else {
            panic!("compose");
        };
        // On the CHILD, for the same reason as the label test: `Compose` renders a
        // summary and the reason lives in the child that produced it.
        let preflight = stages
            .iter()
            .find(|s| matches!(s, Stage::Code { id, .. } if *id == "unanswered:non-empty"))
            .expect("the preflight");
        let report = preflight.run(&mut ctx, &crate::judgment::OfflineTransport);
        assert_eq!(report.signal, Signal::Fails, "{}", report.detail);
        assert!(
            report.detail.contains("not JSON"),
            "the refusal must say WHY, not imply an empty backlog: {}",
            report.detail
        );
        assert!(
            !report.detail.contains("no pin to triage"),
            "and must NOT read as an empty backlog: {}",
            report.detail
        );
    }
}
