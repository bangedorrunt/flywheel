// governed-by: ADR-0026
//! Interactive captain briefing for `flywheel steer` (bd-xe9r, ADR-0006) and
//! `flywheel workflow run --execute` (bd-nmzk) — same skip matrix, shared
//! prompt module.
//!
//! On a TTY (and only there — `--json`, `--yes`, and an explicit `--crew`
//! all skip it), the briefing prompts before dispatch: the inferred crew plan
//! with editable per-role counts, per-role model defaults + registry
//! alternatives (cost/caps), the squad/bare-harness choice, and the
//! resolved stage-chain preview — then go/no-go.
//!
//! The prompt logic is a PURE function of (crew plan, registry choices,
//! stage chain) over INJECTED stdin/stdout, so tests are hermetic: no real
//! TTY is ever touched. Non-TTY invocations never reach this module — the
//! dispatch output stays byte-identical to the pre-briefing binary.

// governed-by: ADR-0006 (D1 stage payloads / briefing surface) + bd-nmzk shared briefing, ADR-0026

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use crate::output::{accent, accent_lav, dim, ok, warn};

/// One role's model menu: the profile default plus every registered id that
/// could serve the role's harness, with cost/caps so the operator chooses
/// with numbers on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleModelChoice {
    /// crew role (profile name)
    pub role: String,
    /// the role harness (drives which registry ids are relevant)
    pub harness: String,
    /// default model id (profile-sourced; empty = harness default)
    pub default_model: String,
    /// (model id, cost in $/mtok (0 = unknown), caps) alternatives,
    /// default first — the menu IS the full registry slice.
    pub alternatives: Vec<(String, f64, Vec<String>)>,
}

/// Everything the briefing renders, computed BEFORE any IO. Pure data.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BriefingContext {
    pub objective: String,
    pub workflow: String,
    /// source of the briefing: "steer" or "workflow" (empty = "steer" for
    /// back-compat — bd-nmzk shared prompt module).
    pub source: String,
    /// inferred unique roles (preview of who will be packed into N)
    pub crew_plan: BTreeMap<String, u32>,
    /// ADR-0011 D2: operator cap
    pub crew_max: u32,
    /// ADR-0011 D2: default total N = min(unique, cap)
    pub default_budget: u32,
    /// per-role model menus
    pub models: Vec<RoleModelChoice>,
    /// squad recommendation from the profile catalog, if one fits
    pub squad_recommendation: Option<String>,
    /// bd-adv4: Jev profile-pick suggestion for the implement seat (the
    /// offline table when Jev is unreachable). Advisory only — the user
    /// decides via the coder_profile input or crew remix. None renders
    /// nothing.
    pub profile_suggestion: Option<String>,
    /// bd-88m4: Jev tier-pick suggestion for the model band (a slot alias:
    /// @cheap or @premium). Advisory only — the user decides via
    /// --role-model. None IS current behavior and renders nothing.
    pub tier_suggestion: Option<String>,
    /// governed-by: bd-ulzi — Jev tier-pick suggestion for the effort
    /// level (low/high/max, via the dispatch-spec effort input).
    /// Advisory only — the user decides via an explicit effort override.
    /// None IS current behavior and renders nothing.
    pub effort_suggestion: Option<String>,
    /// bare harness+provider/model override the operator may type instead of
    /// a squad/profile choice (bd-j3so surface)
    pub bare_override: Option<String>,
    /// resolved stage chain of the target workflow
    pub stage_chain: Vec<String>,
    /// funded status per role (ADR-0026 Spec 2)
    pub funded: BTreeMap<String, String>,
}

/// The operator's verdict plus the confirmed plan (post-edit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BriefingOutcome {
    /// proceed — `budget` is the confirmed total pane count (ADR-0011 D2);
    /// `role_models` is role → chosen registry id (applied via `--role-model`);
    /// `role_harnesses` is role → chosen kind (applied via `--role-harness`).
    Go {
        budget: u32,
        role_models: BTreeMap<String, String>,
        role_harnesses: BTreeMap<String, String>,
    },
    /// abort before any dispatch
    Abort,
}

/// Render the four briefing sections and read the operator's edits +
/// verdict from injected IO. Pure over `(ctx, input, output)` — hermetic
/// tests inject both ends. Blank answers keep the defaults; `n`/`no` at the
/// go/no-go aborts.
pub fn run_briefing(
    ctx: &BriefingContext,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> std::io::Result<BriefingOutcome> {
    let kind = if ctx.source.is_empty() {
        "steer"
    } else {
        ctx.source.as_str()
    };
    writeln!(
        out,
        "{}",
        accent(&format!("== captain briefing: {} {:?}", kind, ctx.workflow))
    )?;
    writeln!(out, "  {} {}", dim("objective:"), accent(&ctx.objective))?;

    // (1) total pane budget N (ADR-0011 D2). Per-role counts are packed, not edited.
    let unique = ctx.crew_plan.len() as u32;
    let default_n = if ctx.default_budget > 0 {
        ctx.default_budget
    } else {
        unique
            .max(1)
            .min(if ctx.crew_max == 0 { 5 } else { ctx.crew_max })
    };
    let cap = if ctx.crew_max == 0 { 5 } else { ctx.crew_max };
    writeln!(
        out,
        "\n{} {}",
        accent_lav("[1/4]"),
        ok("crew budget N (total panes; Enter keeps the default):")
    )?;
    writeln!(
        out,
        "  {} unique={} cap={} default={}",
        dim("roles:"),
        unique,
        cap,
        default_n
    )?;
    for role in ctx.crew_plan.keys() {
        writeln!(out, "  - {role}")?;
    }
    write!(out, "  N > ")?;
    out.flush()?;
    let line = read_line(input)?;
    let mut budget = default_n;
    if let Ok(n) = line.trim().parse::<u32>()
        && n > 0
    {
        budget = n;
        writeln!(out, "  {} -> {}", dim("N"), ok(&format!("{n}")))?;
    }

    // (2) per-role models with cost/caps + funded column (ADR-0026 Spec 2).
    writeln!(
        out,
        "\n{} {}",
        accent_lav("[2/4]"),
        ok("per-role models (Enter keeps the default):")
    )?;
    let mut role_models = BTreeMap::new();
    for m in &ctx.models {
        writeln!(
            out,
            "  {} ({}): default {}",
            accent(&m.role),
            dim(&m.harness),
            ok(&m.default_model)
        )?;
        for (i, (id, cost, caps)) in m.alternatives.iter().enumerate() {
            let cost_s = if *cost > 0.0 {
                format!("${cost:.3}/mtok")
            } else {
                "cost ?".to_string()
            };
            let caps_s = if caps.is_empty() {
                String::new()
            } else {
                format!(" caps={}", caps.join(","))
            };
            writeln!(
                out,
                "    {} {} ({}{})",
                dim(&format!("[{i}]")),
                id,
                cost_s,
                caps_s
            )?;
        }
        if let Some(funded) = ctx.funded.get(&m.role) {
            writeln!(out, "    {} {}", dim("funded:"), funded)?;
        }
        write!(out, "  {} > ", dim("select"))?;
        out.flush()?;
        let line = read_line(input)?;
        let mut chosen = m.default_model.clone();
        if let Ok(i) = line.trim().parse::<usize>()
            && let Some((id, _, _)) = m.alternatives.get(i)
        {
            chosen = id.clone();
            writeln!(out, "  {} -> {}", dim(&m.role), accent(id))?;
        }
        if !chosen.is_empty() {
            role_models.insert(m.role.clone(), chosen);
        }
    }

    // (3) squad recommendation + per-role harness remix (bound into argv).
    writeln!(out, "\n{} {}", accent_lav("[3/4]"), ok("fleet shape:"))?;
    match &ctx.squad_recommendation {
        Some(squad) => writeln!(out, "  {} {}", dim("recommended squad:"), accent(squad))?,
        None => writeln!(
            out,
            "  {} none (per-role profiles)",
            dim("recommended squad:")
        )?,
    }
    if let Some(s) = &ctx.profile_suggestion {
        writeln!(
            out,
            "  {} {} {}",
            dim("profile suggestion:"),
            accent(s),
            dim("(profile-pick; user decides — coder_profile input or keep @coder)")
        )?;
    }
    if let Some(s) = &ctx.tier_suggestion {
        writeln!(
            out,
            "  {} {} {}",
            dim("tier suggestion:"),
            accent(s),
            dim("(tier-pick; user decides — --role-model or keep default)")
        )?;
    }
    if let Some(s) = &ctx.effort_suggestion {
        writeln!(
            out,
            "  {} {} {}",
            dim("effort suggestion:"),
            accent(s),
            dim("(tier-pick; user decides — explicit effort or profile default)")
        )?;
    }
    match &ctx.bare_override {
        Some(bare) => writeln!(out, "  {} {}", dim("bare override accepted:"), accent(bare))?,
        None => writeln!(
            out,
            "  {}",
            dim("bare harness+model: available via spawn --model/--provider")
        )?,
    }
    let mut role_harnesses = BTreeMap::new();
    for m in &ctx.models {
        writeln!(
            out,
            "  {} default harness {}",
            accent(&m.role),
            ok(&m.harness)
        )?;
        write!(out, "  {} > ", dim("harness"))?;
        out.flush()?;
        let line = read_line(input)?;
        let mut chosen = m.harness.clone();
        let typed = line.trim();
        if !typed.is_empty() {
            chosen = typed.to_string();
            writeln!(out, "  {} harness -> {}", dim(&m.role), accent(&chosen))?;
        }
        if !chosen.is_empty() {
            role_harnesses.insert(m.role.clone(), chosen);
        }
    }

    // (4) resolved stage-chain preview.
    writeln!(
        out,
        "\n{} {} {:?}:",
        accent_lav("[4/4]"),
        ok("stage chain for"),
        ctx.workflow
    )?;
    for stage in &ctx.stage_chain {
        writeln!(out, "  {} {}", dim("-"), stage)?;
    }

    // go/no-go — amber gate per hacker-night warning slot
    write!(out, "\n{} > ", warn("dispatch? [Y/n]"))?;
    out.flush()?;
    let verdict = read_line(input)?;
    match verdict.trim().to_ascii_lowercase().as_str() {
        "n" | "no" => {
            writeln!(out, "{}", warn("aborted."))?;
            Ok(BriefingOutcome::Abort)
        }
        _ => {
            writeln!(out, "{}", ok("dispatching."))?;
            Ok(BriefingOutcome::Go {
                budget,
                role_models,
                role_harnesses,
            })
        }
    }
}

fn read_line(input: &mut dyn BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    let n = input.read_line(&mut line)?;
    if n == 0 {
        // EOF: treat as Enter (keep defaults) so a piped-closed stdin never
        // spins — the non-TTY path never reaches here, this is a guard.
        return Ok(String::new());
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> BriefingContext {
        BriefingContext {
            objective: "implement the auth refactor".into(),
            workflow: "goal".into(),
            source: "steer".into(),
            crew_plan: BTreeMap::from([("planner".into(), 1), ("coder".into(), 2)]),
            crew_max: 5,
            default_budget: 2,
            models: vec![RoleModelChoice {
                role: "coder".into(),
                harness: "omp".into(),
                default_model: "deepseek-v4-flash".into(),
                alternatives: vec![
                    (
                        "opencode-go/deepseek-v4-flash".into(),
                        0.001,
                        vec!["omp".into(), "opencode".into()],
                    ),
                    ("zai/glm-5.2".into(), 0.002, vec!["omp".into()]),
                    ("ghost/model".into(), 0.0, vec![]),
                ],
            }],
            squad_recommendation: Some("build".into()),
            profile_suggestion: None,
            tier_suggestion: None,
            effort_suggestion: None,
            bare_override: None,
            stage_chain: vec!["plan".into(), "implement".into(), "review".into()],
            funded: BTreeMap::new(),
        }
    }

    fn drive(answers: &[&str]) -> (BriefingOutcome, String) {
        let c = ctx();
        let mut input = answers.join("\n");
        input.push('\n');
        let mut inp = std::io::Cursor::new(input);
        let mut out: Vec<u8> = Vec::new();
        let outcome = run_briefing(&c, &mut inp, &mut out).expect("briefing");
        (outcome, String::from_utf8(out).expect("utf8"))
    }

    fn go(budget: u32, models: &[(&str, &str)], harnesses: &[(&str, &str)]) -> BriefingOutcome {
        BriefingOutcome::Go {
            budget,
            role_models: models
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            role_harnesses: harnesses
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    #[test]
    fn blank_answers_keep_defaults_and_go() {
        // N + 1 model + 1 harness + go/no-go
        let (outcome, rendered) = drive(&["", "", "", ""]);
        assert_eq!(
            outcome,
            go(2, &[("coder", "deepseek-v4-flash")], &[("coder", "omp")])
        );
        // The golden sections all render.
        assert!(rendered.contains("== captain briefing: steer \"goal\""));
        assert!(rendered.contains("crew budget N"));
        assert!(rendered.contains("default=2"));
        assert!(rendered.contains("deepseek-v4-flash"));
        assert!(rendered.contains("$0.001/mtok"));
        assert!(rendered.contains("caps=omp,opencode"));
        assert!(rendered.contains("cost ?"), "unknown cost is honest");
        assert!(rendered.contains("recommended squad: build"));
        assert!(rendered.contains("- implement"));
        assert!(rendered.contains("dispatching."));
    }

    #[test]
    fn explicit_no_aborts_before_dispatch() {
        let (outcome, rendered) = drive(&["", "", "", "n"]);
        assert_eq!(outcome, BriefingOutcome::Abort);
        assert!(rendered.contains("aborted."));
        assert!(!rendered.contains("dispatching."));
    }

    /// bd-adv4: a set profile suggestion renders its line (advisory — the
    /// prompt flow is unchanged); None renders nothing.
    #[test]
    fn profile_pick_suggestion_renders_when_set() {
        let mut c = ctx();
        c.profile_suggestion = Some("@rust-coder".into());
        let mut inp = std::io::Cursor::new("");
        let mut out: Vec<u8> = Vec::new();
        run_briefing(&c, &mut inp, &mut out).expect("briefing");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("profile suggestion:"), "{rendered}");
        assert!(rendered.contains("@rust-coder"), "{rendered}");
        let mut inp2 = std::io::Cursor::new("");
        let mut out2: Vec<u8> = Vec::new();
        run_briefing(&ctx(), &mut inp2, &mut out2).expect("briefing");
        let plain = String::from_utf8(out2).expect("utf8");
        assert!(!plain.contains("profile suggestion:"), "{plain}");
    }

    /// governed-by: bd-ulzi — a set effort suggestion renders beside the
    /// tier line; None renders nothing.
    #[test]
    fn jev_effort_suggestion_renders_beside_tier() {
        let mut c = ctx();
        c.tier_suggestion = Some("@premium".into());
        c.effort_suggestion = Some("max".into());
        let mut inp = std::io::Cursor::new("");
        let mut out: Vec<u8> = Vec::new();
        run_briefing(&c, &mut inp, &mut out).expect("briefing");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("effort suggestion:"), "{rendered}");
        assert!(rendered.contains("max"), "{rendered}");
        let mut plain = ctx();
        plain.effort_suggestion = None;
        let mut inp2 = std::io::Cursor::new("");
        let mut out2: Vec<u8> = Vec::new();
        run_briefing(&plain, &mut inp2, &mut out2).expect("briefing");
        let rendered2 = String::from_utf8(out2).expect("utf8");
        assert!(!rendered2.contains("effort suggestion:"), "{rendered2}");
    }
    #[test]
    fn tier_pick_suggestion_renders_beside_profile() {
        let mut c = ctx();
        c.profile_suggestion = Some("@rust-coder".into());
        c.tier_suggestion = Some("@premium".into());
        let mut inp = std::io::Cursor::new("");
        let mut out: Vec<u8> = Vec::new();
        run_briefing(&c, &mut inp, &mut out).expect("briefing");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("profile suggestion:"), "{rendered}");
        assert!(rendered.contains("tier suggestion:"), "{rendered}");
        assert!(rendered.contains("@premium"), "{rendered}");
    }

    #[test]
    fn edits_apply_budget() {
        let (outcome, rendered) = drive(&["4", "1", "", "y"]);
        assert_eq!(
            outcome,
            go(4, &[("coder", "zai/glm-5.2")], &[("coder", "omp")])
        );
        assert!(rendered.contains("N -> 4"), "{rendered}");
        assert!(rendered.contains("coder -> zai/glm-5.2"), "{rendered}");
    }

    #[test]
    fn eof_on_any_prompt_keeps_defaults_never_spins() {
        // Empty stdin = immediate EOF at every read.
        let mut inp = std::io::Cursor::new("");
        let mut out: Vec<u8> = Vec::new();
        let outcome = run_briefing(&ctx(), &mut inp, &mut out).expect("eof-safe");
        assert_eq!(
            outcome,
            go(2, &[("coder", "deepseek-v4-flash")], &[("coder", "omp")]),
            "EOF counts as Enter/yes"
        );
    }

    #[test]
    fn typed_harness_binds_into_outcome() {
        let (outcome, rendered) = drive(&["", "", "pi", "y"]);
        assert_eq!(
            outcome,
            go(2, &[("coder", "deepseek-v4-flash")], &[("coder", "pi")])
        );
        assert!(rendered.contains("coder harness -> pi"), "{rendered}");
    }

    #[test]
    fn no_models_and_no_squad_render_honestly() {
        let c = BriefingContext {
            workflow: "goal-direct".into(),
            ..Default::default()
        };
        let mut inp = std::io::Cursor::new("\n\n\n\n");
        let mut out: Vec<u8> = Vec::new();
        run_briefing(&c, &mut inp, &mut out).expect("briefing");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("recommended squad: none"));
        assert!(rendered.contains("bare harness+model: available via spawn --model/--provider"));
    }

    #[test]
    fn workflow_source_renders_workflow_prefix_and_shared_shape() {
        // bd-nmzk: same prompt module, same 4 sections — only the header
        // source differs (workflow vs steer). Lockstep guarantee.
        let c = BriefingContext {
            objective: "run the weekly-review flow".into(),
            workflow: "weekly-review".into(),
            source: "workflow".into(),
            crew_plan: BTreeMap::from([("coder".into(), 1)]),
            models: vec![],
            squad_recommendation: None,
            profile_suggestion: None,
            tier_suggestion: None,
            effort_suggestion: None,
            bare_override: None,
            stage_chain: vec!["review".into(), "ship".into()],
            funded: BTreeMap::new(),
            crew_max: 5,
            default_budget: 1,
        };
        let mut inp = std::io::Cursor::new("\n\n\n");
        let mut out: Vec<u8> = Vec::new();
        run_briefing(&c, &mut inp, &mut out).expect("briefing");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("== captain briefing: workflow \"weekly-review\""),
            "workflow prefix: {rendered}"
        );
        assert!(rendered.contains("objective: run the weekly-review flow"));
        assert!(rendered.contains("[1/4] crew budget N"));
        assert!(rendered.contains("[2/4] per-role models"));
        assert!(rendered.contains("[3/4] fleet shape"));
        assert!(rendered.contains("[4/4] stage chain"));
        assert!(rendered.contains("- review"));
        assert!(rendered.contains("dispatching."));
    }

    #[test]
    fn funded_column_renders_when_present() {
        let mut c = ctx();
        c.funded.insert(
            "coder".into(),
            "no funded endpoint — classifier+fallback cover it".into(),
        );
        let mut inp = std::io::Cursor::new("\n\n\n\n");
        let mut out: Vec<u8> = Vec::new();
        run_briefing(&c, &mut inp, &mut out).expect("briefing");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("funded: no funded endpoint"),
            "{rendered}"
        );
    }

    #[test]
    fn empty_source_defaults_to_steer_for_back_compat() {
        let c = BriefingContext {
            workflow: "goal".into(),
            source: String::new(),
            ..Default::default()
        };
        let mut inp = std::io::Cursor::new("\n");
        let mut out: Vec<u8> = Vec::new();
        run_briefing(&c, &mut inp, &mut out).expect("briefing");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(
            rendered.contains("== captain briefing: steer \"goal\""),
            "empty source defaults to steer: {rendered}"
        );
    }
}
