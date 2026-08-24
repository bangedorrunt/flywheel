//! Interactive captain briefing for `flywheel steer` (bd-xe9r, ADR-0006).
//!
//! On a TTY (and only there — `--json`, `--yes`, and an explicit `--crew`
//! all skip it), steer prompts before dispatch: the inferred crew plan with
//! editable per-role counts, per-role model defaults + registry
//! alternatives (cost/caps), the squad/bare-harness choice, and the
//! resolved stage-chain preview — then go/no-go.
//!
//! The prompt logic is a PURE function of (crew plan, registry choices,
//! stage chain) over INJECTED stdin/stdout, so tests are hermetic: no real
//! TTY is ever touched. Non-TTY invocations never reach this module — the
//! dispatch output stays byte-identical to the pre-briefing binary.

// governed-by: ADR-0006 (D1 stage payloads / briefing surface)

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

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
    /// inferred crew plan: role -> count (editable)
    pub crew_plan: BTreeMap<String, u32>,
    /// per-role model menus
    pub models: Vec<RoleModelChoice>,
    /// squad recommendation from the profile catalog, if one fits
    pub squad_recommendation: Option<String>,
    /// bare harness+provider/model override the operator may type instead of
    /// a squad/profile choice (bd-j3so surface)
    pub bare_override: Option<String>,
    /// resolved stage chain of the target workflow
    pub stage_chain: Vec<String>,
}

/// The operator's verdict plus the confirmed plan (post-edit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BriefingOutcome {
    /// proceed — dispatch exactly as briefed (edits applied to the preview;
    /// dispatch itself is downstream machinery)
    Go,
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
    writeln!(out, "== captain briefing: steer {:?}", ctx.workflow)?;
    writeln!(out, "objective: {}", ctx.objective)?;

    // (1) inferred crew plan with editable counts.
    writeln!(out, "\n[1/4] inferred crew plan (Enter keeps the count):")?;
    for (role, count) in &ctx.crew_plan {
        write!(out, "  {role} x{count} > ")?;
        out.flush()?;
        let line = read_line(input)?;
        if let Ok(n) = line.trim().parse::<u32>()
            && n > 0
        {
            writeln!(out, "  {role} -> x{n}")?;
        }
    }

    // (2) per-role models with cost/caps.
    writeln!(out, "\n[2/4] per-role models (Enter keeps the default):")?;
    for m in &ctx.models {
        writeln!(
            out,
            "  {} ({}): default {}",
            m.role, m.harness, m.default_model
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
            writeln!(out, "    [{i}] {id} ({cost_s}{caps_s})")?;
        }
        write!(out, "  select > ")?;
        out.flush()?;
        let line = read_line(input)?;
        if let Ok(i) = line.trim().parse::<usize>()
            && let Some((id, _, _)) = m.alternatives.get(i)
        {
            writeln!(out, "  {} -> {id}", m.role)?;
        }
    }

    // (3) squad recommendation or explicit bare harness+model.
    writeln!(out, "\n[3/4] fleet shape:")?;
    match &ctx.squad_recommendation {
        Some(squad) => writeln!(out, "  recommended squad: {squad}")?,
        None => writeln!(out, "  recommended squad: none (per-role profiles)")?,
    }
    match &ctx.bare_override {
        Some(bare) => writeln!(out, "  bare override accepted: {bare}")?,
        None => writeln!(out, "  bare harness+model: available via spawn --model/--provider")?,
    }

    // (4) resolved stage-chain preview.
    writeln!(out, "\n[4/4] stage chain for {:?}:", ctx.workflow)?;
    for stage in &ctx.stage_chain {
        writeln!(out, "  - {stage}")?;
    }

    // go/no-go.
    write!(out, "\ndispatch? [Y/n] > ")?;
    out.flush()?;
    let verdict = read_line(input)?;
    match verdict.trim().to_ascii_lowercase().as_str() {
        "n" | "no" => {
            writeln!(out, "aborted.")?;
            Ok(BriefingOutcome::Abort)
        }
        _ => {
            writeln!(out, "dispatching.")?;
            Ok(BriefingOutcome::Go)
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
            crew_plan: BTreeMap::from([("planner".into(), 1), ("coder".into(), 2)]),
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
            bare_override: None,
            stage_chain: vec!["plan".into(), "implement".into(), "review".into()],
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

    #[test]
    fn blank_answers_keep_defaults_and_go() {
        // 2 count prompts + 1 model prompt + go/no-go = blank,blank,blank,y
        let (outcome, rendered) = drive(&["", "", "", ""]);
        assert_eq!(outcome, BriefingOutcome::Go);
        // The golden sections all render.
        assert!(rendered.contains("== captain briefing: steer \"goal\""));
        assert!(rendered.contains("planner x1"));
        assert!(rendered.contains("coder x2"));
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

    #[test]
    fn edits_change_the_preview_not_the_context() {
        // coder count 1 -> x5; keep planner; model pick index 1 (zai/glm-5.2).
        let (_, rendered) = drive(&["5", "", "1", "y"]);
        assert!(rendered.contains("coder -> x5"), "{rendered}");
        assert!(rendered.contains("coder -> zai/glm-5.2"), "{rendered}");
    }

    #[test]
    fn eof_on_any_prompt_keeps_defaults_never_spins() {
        // Empty stdin = immediate EOF at every read.
        let mut inp = std::io::Cursor::new("");
        let mut out: Vec<u8> = Vec::new();
        let outcome = run_briefing(&ctx(), &mut inp, &mut out).expect("eof-safe");
        assert_eq!(outcome, BriefingOutcome::Go, "EOF counts as Enter/yes");
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
}
