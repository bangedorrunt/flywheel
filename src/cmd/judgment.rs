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

/// Pack-file management. A pack is `<id>.vN.yaml`; the user tier
/// (`~/.config/flywheel/judgment-packs`) is what the loader treats as truth,
/// and a project `.flywheel/judgment-packs` only fills an id the user tier
/// lacks. There was no command for any of this, so the two tiers could only be
/// compared by hand with `diff`.
#[derive(Subcommand)]
pub(crate) enum PackCmd {
    /// Compare the user-tier packs against this project's, per pack id.
    Drift {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Copy project packs UP into the user tier (the loader's source of truth).
    Seed {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// How much may be REPLACED: none (fill gaps, DEFAULT) | stale | all.
        /// A user pack at a HIGHER version is never downgraded at any scope.
        #[arg(long, value_name = "SCOPE", value_parser = ["none", "stale", "all"], default_value = "none")]
        overwrite: String,
        /// Report the plan and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Accept a destructive overwrite without prompting. Required with no TTY.
        #[arg(long)]
        yes: bool,
    },
    /// Delete a user-tier pack file, so the project tier takes over again.
    Remove {
        /// pack id (the `<id>` in `<id>.vN.yaml`)
        id: String,
        /// also delete every version of this id, not just the highest
        #[arg(long)]
        all_versions: bool,
    },
    /// List the packs that resolve, and which tier each came from.
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum JudgmentCmd {
    /// Pack-file management: the missing half of `workflow seed`.
    Pack {
        #[command(subcommand)]
        cmd: PackCmd,
    },
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

fn pack_dirs() -> (std::path::PathBuf, std::path::PathBuf) {
    let user = crate::config::config_dir().join("judgment-packs");
    let project = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(".flywheel/judgment-packs");
    (user, project)
}

fn cmd_pack(cmd: &PackCmd) -> Result<()> {
    use crate::output as out;
    let (user_dir, project_dir) = pack_dirs();
    match cmd {
        PackCmd::Drift { json } => {
            let rows = judgment::pack_drift(&user_dir, &project_dir);
            if *json {
                crate::output::print_json(&serde_json::to_value(&rows)?);
                return Ok(());
            }
            if rows.is_empty() {
                println!(
                    "{} {}",
                    out::ok("\u{2705}"),
                    out::dim("no packs on either tier")
                );
                return Ok(());
            }
            for want in ["stale", "missing", "identical", "newer-user", "user-only"] {
                let group: Vec<_> = rows.iter().filter(|r| r.status == want).collect();
                if group.is_empty() {
                    continue;
                }
                let glyph = match want {
                    "identical" => "\u{2705}",
                    "stale" => "\u{26a0}\u{fe0f}",
                    "missing" => "\u{274c}",
                    _ => "\u{1f9e9}",
                };
                println!(
                    "\n{} {}",
                    glyph,
                    out::dim(&format!("{} {}", group.len(), want))
                );
                for row in group {
                    let v = match (row.user_version, row.project_version) {
                        (Some(u), Some(p)) => format!("user v{u} / project v{p}"),
                        (Some(u), None) => format!("user v{u} / project none"),
                        (None, Some(p)) => format!("user none / project v{p}"),
                        (None, None) => "unreadable".to_string(),
                    };
                    println!("   {:<24} {}", row.id, out::dim(&v));
                }
            }
            let stale = rows.iter().filter(|r| r.status == "stale").count();
            println!();
            if stale == 0 {
                println!(
                    "{} {}",
                    out::ok("\u{2705}"),
                    out::dim("user packs match the project tier")
                );
            } else {
                println!(
                    "{} {}",
                    out::warn("\u{26a0}\u{fe0f}"),
                    out::warn(&format!("{stale} pack(s) diverged"))
                );
                println!(
                    "   {}",
                    out::accent("fix: flywheel judgment pack seed --dry-run --overwrite stale")
                );
            }
            Ok(())
        }
        PackCmd::Seed {
            json,
            overwrite,
            dry_run,
            yes,
        } => {
            let scope = judgment::PackOverwrite::parse(overwrite)?;
            let plan = judgment::pack_seed(&user_dir, &project_dir, scope, true)?;
            if !*dry_run {
                crate::output::confirm_destructive(
                    "overwrite",
                    &plan.planned_overwrites,
                    *yes,
                    *json,
                )?;
            }
            let rep = judgment::pack_seed(&user_dir, &project_dir, scope, *dry_run)?;
            if *json {
                crate::output::print_json(&serde_json::to_value(&rep)?);
                return Ok(());
            }
            println!(
                "{} {} {} pack tier  {}",
                out::ok("\u{2705}"),
                if *dry_run { "would sync" } else { "synced" },
                out::accent(&user_dir.display().to_string()),
                out::dim(&format!("(overwrite: {})", scope.as_str()))
            );
            if !rep.copied.is_empty() {
                println!(
                    "   {} {}",
                    out::ok(&rep.copied.len().to_string()),
                    out::dim("pack(s) promoted from the project tier")
                );
            }
            if !rep.overwritten.is_empty() {
                println!(
                    "   {} {}",
                    out::warn(&rep.overwritten.len().to_string()),
                    out::dim("pack(s) RESTORED from the project tier (old bytes kept as .bak)")
                );
            }
            if !rep.kept.is_empty() {
                println!(
                    "   {} {}",
                    out::dim(&rep.kept.len().to_string()),
                    out::dim("pack(s) left untouched (raise --overwrite to replace)")
                );
            }
            if *dry_run && !rep.planned_overwrites.is_empty() {
                println!(
                    "\n{} would REPLACE: {}",
                    out::warn("\u{26a0}\u{fe0f}"),
                    rep.planned_overwrites.join(", ")
                );
                println!(
                    "{}",
                    out::dim("nothing was written. Re-run without --dry-run to apply.")
                );
            }
            Ok(())
        }
        PackCmd::Remove { id, all_versions } => {
            let mut removed = Vec::new();
            for path in judgment::pack_files_for(&user_dir, id, *all_versions) {
                std::fs::remove_file(&path)
                    .map_err(|e| anyhow::anyhow!("remove {}: {e}", path.display()))?;
                removed.push(path.display().to_string());
            }
            if removed.is_empty() {
                bail!(
                    "no user-tier pack file for id {id:?} in {}",
                    user_dir.display()
                );
            }
            println!(
                "{} {}",
                out::ok("\u{1f5d1}\u{fe0f}"),
                out::dim(&format!("removed {} pack file(s) for {id}", removed.len()))
            );
            for r in &removed {
                println!("   {}", out::dim(r));
            }
            println!(
                "{}",
                out::dim("the project tier fills this id again if it has one")
            );
            Ok(())
        }
        PackCmd::List { json } => {
            let rows = judgment::pack_drift(&user_dir, &project_dir);
            if *json {
                crate::output::print_json(&serde_json::to_value(&rows)?);
                return Ok(());
            }
            for row in &rows {
                let tier = if row.user_version.is_some() {
                    "user"
                } else {
                    "project"
                };
                println!("{:<24} {:<8} {}", row.id, tier, out::dim(&row.status));
            }
            Ok(())
        }
    }
}

pub(crate) fn cmd_judgment(cmd: &JudgmentCmd) -> Result<()> {
    match cmd {
        JudgmentCmd::Pack { cmd } => cmd_pack(cmd),
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
