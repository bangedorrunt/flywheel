//! `flywheel stage` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;

use crate::config::Config;
use crate::output::print_json;

/// `flywheel stage` subcommands (ADR-0005 P3.3; ADR-0022 §3.8 W6): the
/// layered stage registry surface.
#[derive(Subcommand)]
pub(crate) enum StageCmd {
    /// List stages (seed + authored; `--all` kept as an alias)
    List {
        /// include seed (default: true; flag kept so old `--all` scripts still parse)
        #[arg(long)]
        all: bool,
        /// JSON rows (agent flag parity; list is JSON when this is set)
        #[arg(long)]
        json: bool,
    },
    /// Show one stage: prompt body + metadata + tier
    Show {
        /// stage name (semantic or p-code alias)
        name: String,
    },
    /// Scaffold a new stage into the project tier (.flywheel/stages.toml)
    New {
        /// stage name (must be a valid identifier)
        name: String,
    },
    /// Lint every tier: shape, size cap, role affinity, phase, principles
    /// Delete a USER-tier stage file, so the compiled-in stage shows through
    /// again. The mirror of `stage promote`: promote copies a project stage
    /// up, remove drops that copy so the builtin takes back over.
    Remove {
        /// stage name
        name: String,
    },
    Lint,
    /// Promote a stage up the §3.8 permanence ladder (project → user → repo)
    Promote {
        /// stage name (must resolve at project or user tier)
        name: String,
        /// target tier: user | repo
        #[arg(long, value_parser = ["user", "repo"])]
        to: String,
        /// repo tier only: >=2 successful run ids as evidence (distinct
        /// objectives, verified against the durable run log)
        #[arg(long, value_delimiter = ',')]
        evidence: Vec<String>,
        /// repo tier only: the review bead id witnessing the promotion
        #[arg(long)]
        review_bead: Option<String>,
        /// repo tier only: flywheel repo checkout holding src/repo-stages.toml
        /// (default: $FLYWHEEL_REPO_DIR)
        #[arg(long)]
        repo_dir: Option<String>,
    },
}

/// `flywheel stage list`: the stage library surface (ADR-0005 P3.3).
pub(crate) fn cmd_stage(cfg: &Config, cmd: &StageCmd) -> Result<()> {
    // The project tier lives in the default project's workspace dir
    // (<workspace-root>/flywheel/.flywheel/stages.toml) — the same project
    // `workflow compose` defaults to.
    let project_stages = crate::project_dir(cfg, "flywheel").join(".flywheel/stages.toml");
    match cmd {
        StageCmd::List {
            all: _,
            json: as_json,
        } => {
            let lib = crate::stages::StageLibrary::load_layered(Some(&project_stages))?;
            let rows: Vec<crate::stages::StageRow> = lib.list_tiered();
            if *as_json {
                print_json(&serde_json::json!({ "stages": rows }));
                return Ok(());
            }
            println!("stages (name, tier, role, phase):");
            for row in &rows {
                println!(
                    "  {:<28} [{:<7}] role={} phase={}",
                    row.name,
                    row.tier.tag(),
                    row.role.as_deref().unwrap_or("-"),
                    row.phase.as_deref().unwrap_or("-"),
                );
            }
            println!(
                "ce- loop (ADR-0009): ce-brainstorm -> ce-plan -> ce-plan-review -> ce-execute -> ce-simplify -> ce-review -> verify -> ce-compound"
            );
            println!("user override: {}", crate::override_display());
            println!("project tier: {}", project_stages.display());
            Ok(())
        }
        StageCmd::Show { name } => {
            let lib = crate::stages::StageLibrary::load_layered(Some(&project_stages))?;
            let stage = lib
                .get_stage(name)
                .ok_or_else(|| anyhow!("unknown stage {name:?} (flywheel stage list --all)"))?;
            let tier = lib.tier_of(name).unwrap_or(crate::stages::Tier::Seed);
            println!("name:     {}", stage.name);
            println!("tier:     {}", tier.tag());
            if let Some(role) = &stage.role {
                println!("role:     {role}");
            }
            if let Some(phase) = &stage.phase {
                println!("phase:    {phase}");
            }
            if !stage.principles.is_empty() {
                println!("principles: {}", stage.principles.join(", "));
            }
            if let Some(author) = &stage.author {
                println!("author:   {author}");
            }
            if let Some(run) = &stage.origin_run {
                println!("origin-run: {run}");
            }
            println!("---");
            println!("{}", stage.prompt);
            Ok(())
        }
        StageCmd::New { name } => {
            if name.trim().is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                bail!("invalid stage name {name:?} (alnum/_/-)");
            }
            let lib = crate::stages::StageLibrary::load_layered(Some(&project_stages))?;
            if lib.has(name) {
                bail!("stage {name:?} already exists (flywheel stage show {name})");
            }
            let mut stage = crate::stages::scaffold(name);
            stage.author = Some(crate::operator_pin());
            crate::stages::upsert_stage_in_file(&project_stages, &stage)?;
            println!("scaffolded [stages.{name}] in {}", project_stages.display());
            println!(
                "next: fill the prompt body + role/phase/principles, then `flywheel stage lint`"
            );
            Ok(())
        }
        StageCmd::Remove { name } => {
            let path = crate::config::config_dir()
                .join("stages")
                .join(format!("{name}.toml"));
            if !path.is_file() {
                bail!(
                    "no user-tier stage file for {name:?} at {}. Nothing to remove — the \
                     compiled-in stage already resolves.",
                    path.display()
                );
            }
            // Back up first. This deletes a file the operator may have edited,
            // and the compiled-in stage showing through afterwards is not
            // obviously the state they wanted.
            let bak = path.with_extension("toml.bak");
            std::fs::copy(&path, &bak)
                .map_err(|e| anyhow::anyhow!("backup {}: {e}", bak.display()))?;
            std::fs::remove_file(&path)
                .map_err(|e| anyhow::anyhow!("remove {}: {e}", path.display()))?;
            println!(
                "{} {} {}",
                crate::output::ok("\u{1f5d1}\u{fe0f}"),
                crate::output::accent(&format!("removed stage {name}")),
                crate::output::dim(&format!("(backup: {})", bak.display()))
            );
            println!(
                "{}",
                crate::output::dim("the compiled-in stage resolves again — flywheel stage list shows the tier")
            );
            Ok(())
        }
        StageCmd::Lint => {
            let lib = crate::stages::StageLibrary::load_layered(Some(&project_stages))?;
            let valid_roles: Vec<String> = crate::profile::ProfileCatalog::load()?.names();
            let mut issues = 0;
            for row in lib.list_tiered() {
                let stage = lib.get_stage(&row.name).expect("row source");
                for issue in crate::stages::lint_stage(stage, &valid_roles) {
                    issues += 1;
                    println!("[{:<7}] {}", row.tier.tag(), issue);
                }
            }
            if issues == 0 {
                println!("stage lint clean (all tiers)");
                Ok(())
            } else {
                bail!("{issues} stage lint issue(s)")
            }
        }
        StageCmd::Promote {
            name,
            to,
            evidence,
            review_bead,
            repo_dir,
        } => {
            let lib = crate::stages::StageLibrary::load_layered(Some(&project_stages))?;
            let stage = lib
                .get_stage(name)
                .ok_or_else(|| anyhow!("unknown stage {name:?} (flywheel stage list --all)"))?
                .clone();
            match to.as_str() {
                "user" => {
                    let tier = lib.tier_of(name).unwrap_or(crate::stages::Tier::Seed);
                    if tier == crate::stages::Tier::User {
                        bail!("stage {name:?} is already at the user tier");
                    }
                    let dest = crate::config::config_dir()
                        .join("stages")
                        .join(format!("{name}.toml"));
                    crate::stages::upsert_stage_entry(&dest, &stage)?;
                    println!("promoted {name} → user tier ({})", dest.display());
                    Ok(())
                }
                "repo" => {
                    // §3.8 evidence bar: >=2 successful runs (distinct
                    // objectives) + a review bead, then land as DATA.
                    crate::promote_to_repo(
                        name,
                        evidence,
                        review_bead.as_deref(),
                        repo_dir.as_deref(),
                    )
                }
                other => bail!("unknown promote target {other:?} (user | repo)"),
            }
        }
    }
}
