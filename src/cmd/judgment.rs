//! `flywheel judgment` — host-side judgment surfaces (bd-lpn6, ADR-0014).
//!
//! Annotate-only doors over the judgment adapter: the miss ledger
//! (reclassify expands + resolutions + needle mining), on-demand
//! coordination annotators, and the verify prescreen. Nothing here
//! dispatches, holds, assigns, or closes — table/Tick/beads authority is
//! untouched. Every network ask fails loud on deny (no key/pack); the
//! ledger commands are local and always work.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::judgment;

#[derive(Subcommand)]
pub(crate) enum JudgmentCmd {
    /// Record a resolution for a miss-ledger row (confirm|deny|raw|compose).
    /// Fails loud on an unknown effect key or a bad verdict.
    MissResolve {
        /// parent effect key of the expand row (see `miss-list`)
        effect_key: String,
        /// confirm|deny closes a none-path advisory; raw|compose records
        /// how an undecided goal was actually dispatched
        #[arg(long)]
        resolution: String,
        /// optional note (why this verdict)
        #[arg(long)]
        note: Option<String>,
    },
    /// List miss-ledger rows (default: unresolved only) or needle
    /// candidates — recurring unresolved `none` goals that want new routing
    /// table needles.
    MissList {
        /// include resolved rows
        #[arg(long)]
        all: bool,
        /// group recurring unresolved `none` goals with counts
        #[arg(long)]
        needles: bool,
        /// machine JSON
        #[arg(long)]
        json: bool,
    },
    /// Coordination annotator: advisory text for one surface over a goal.
    /// Never dispatches — the captain acts on the text.
    Annotate {
        /// coalition-intent|wave-readiness|reviewer-rank|profile-pick|tier-pick
        #[arg(long)]
        surface: String,
        /// goal text to judge
        #[arg(long)]
        goal: String,
        /// extra state fields as JSON object (e.g. held/blockers/candidate)
        #[arg(long)]
        state_json: Option<String>,
        /// project for pack discovery (default: current directory walk)
        #[arg(long)]
        project: Option<String>,
        /// machine JSON
        #[arg(long)]
        json: bool,
    },
    /// Verify prescreen: advisory input to ce-evaluate over a VERIFY command
    /// (shape + destructive risk). Never a verdict — candidates stay
    /// uncommitted and `workflow run --execute` remains authoritative.
    Prescreen {
        /// VERIFY command text to prescreen
        #[arg(long)]
        cmd: String,
        /// project for pack discovery (default: current directory walk)
        #[arg(long)]
        project: Option<String>,
        /// machine JSON
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn cmd_judgment(cmd: &JudgmentCmd) -> Result<()> {
    match cmd {
        JudgmentCmd::MissResolve {
            effect_key,
            resolution,
            note,
        } => {
            let row = judgment::miss_resolve(effect_key, resolution, note.as_deref())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "resolved {} as {} ({})",
                row.effect_key,
                row.resolution.as_deref().unwrap_or("?"),
                row.resolved_ts.as_deref().unwrap_or("?")
            );
            Ok(())
        }
        JudgmentCmd::MissList { all, needles, json } => {
            if *needles {
                let cands = judgment::miss_needle_candidates();
                if *json {
                    crate::output::print_json(&serde_json::json!(
                        cands
                            .iter()
                            .map(
                                |(goal, count)| serde_json::json!({ "goal": goal, "count": count })
                            )
                            .collect::<Vec<_>>()
                    ));
                } else if cands.is_empty() {
                    println!("no recurring misses — no needle candidates");
                } else {
                    for (goal, count) in &cands {
                        println!("{count:>4}x  {goal}");
                    }
                }
                return Ok(());
            }
            let rows = if *all {
                let mut rows = judgment::miss_read();
                rows.sort_by(|a, b| a.ts.cmp(&b.ts));
                rows
            } else {
                judgment::miss_unresolved()
            };
            if *json {
                crate::output::print_json(&serde_json::json!(rows));
            } else if rows.is_empty() {
                println!("miss ledger empty (flag off = no expands recorded)");
            } else {
                for r in &rows {
                    println!(
                        "{} {} {} -> {} res={} {}",
                        r.ts,
                        r.effect_key,
                        truncate(&r.goal, 60),
                        r.table_primitive,
                        r.resolution.as_deref().unwrap_or("-"),
                        if r.escalated { "[escalated]" } else { "" }
                    );
                }
            }
            Ok(())
        }
        JudgmentCmd::Annotate {
            surface,
            goal,
            state_json,
            project,
            json,
        } => {
            let surface = judgment::AnnotateSurface::parse(surface)
                .ok_or_else(|| anyhow::anyhow!("unknown surface {surface:?}; want coalition-intent|wave-readiness|reviewer-rank|profile-pick|tier-pick"))?;
            let mut extra = BTreeMap::new();
            if let Some(s) = state_json {
                let v: serde_json::Value = serde_json::from_str(s)
                    .map_err(|e| anyhow::anyhow!("--state-json is not an object: {e}"))?;
                let obj = v
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("--state-json must be a JSON object"))?;
                for (k, val) in obj {
                    extra.insert(
                        k.clone(),
                        val.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| val.to_string()),
                    );
                }
            }
            let project = project.as_deref().unwrap_or(".");
            let (advisory, conf, latency_ms) = judgment::annotate(
                project,
                surface,
                goal,
                &extra,
                &crate::authority::RuntimeAuthority::resolve(),
            )
            .map_err(|e| anyhow::anyhow!("annotate denied: {e}"))?;
            if *json {
                crate::output::print_json(&serde_json::json!({
                    "surface": format!("{surface:?}"),
                    "advisory": advisory,
                    "confidence": conf,
                    "latency_ms": latency_ms,
                }));
            } else {
                println!("{advisory}");
            }
            Ok(())
        }
        JudgmentCmd::Prescreen { cmd, project, json } => {
            let project = project.as_deref().unwrap_or(".");
            let (advisory, destructive, conf, latency_ms) = judgment::prescreen_verify(
                project,
                cmd,
                &crate::authority::RuntimeAuthority::resolve(),
            )
            .map_err(|e| anyhow::anyhow!("prescreen denied: {e}"))?;
            if *json {
                crate::output::print_json(&serde_json::json!({
                    "advisory": advisory,
                    "destructive": destructive,
                    "confidence": conf,
                    "latency_ms": latency_ms,
                    "verdict": false,
                }));
            } else {
                println!("{advisory} (advisory only — never a verdict)");
            }
            if destructive {
                bail!("prescreen flags destructive content — human review required");
            }
            Ok(())
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
