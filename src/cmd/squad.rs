//! `flywheel squad` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;
use crate::profile::ProfileCatalog;
use crate::state;
use crate::{backend, config, profile};
use crate::{
    bind_pane_mail_identity, bind_spawn_role, pinned_mail_identity, prime_profile_identity,
    profile_launch_args, project_door, record_spawn_entry, refuse_unpinned_spawn,
    resolved_spec_workspace,
};

/// Squad commands (ADR-0005 P6): a squad is a named group of profile counts —
/// the generalization of the legacy `Recipe`. `squad spawn <recipe>` resolves
/// each member profile via `Profile::resolve` and spawns it on the backend.
#[derive(Subcommand)]
pub(crate) enum SquadCmd {
    /// List squads (compiled-in + from recipes.toml as aliases)
    List,
    /// Show a squad's member profiles + counts
    Show {
        /// squad / recipe name
        name: String,
    },
    /// Spawn every profile in the squad (resolves via Profile::resolve)
    Spawn {
        /// squad / recipe name (a legacy recipe maps via recipe_to_squad)
        name: String,
        /// project scope for resolve (default: host scope / operator)
        #[arg(long)]
        project: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Remove a squad/recipe from ~/.config/flywheel/recipes.toml.
    Remove {
        /// squad / recipe name
        name: String,
    },
    /// Create/overwrite a squad in ~/.config/flywheel/recipes.toml
    /// (ADR-0007 D3: user file overrides compiled-in squads per name).
    Create {
        /// squad name
        name: String,
        /// member counts: --profile coder=3 (repeatable)
        #[arg(long = "profile", value_name = "PROFILE=COUNT", value_parser = crate::parse_kind_count)]
        members: Vec<(String, u32)>,
        /// print the resulting user-file entry without writing
        #[arg(long)]
        dry_run: bool,
    },
}

/// bd-brie: resolve a squad member profile for spawn with the
/// project-scope → host-scope fallback. Project-scope resolution that fails
/// with "not registered in project" falls back to host-scope (operator
/// npub) — profiles are host-scoped in practice; `spawn --profile` resolves
/// the same way. Returns `(resolved, fell_back)`.
pub(crate) fn resolve_squad_member(
    cat: &ProfileCatalog,
    name: &str,
    project: Option<&str>,
    operator_npub: &str,
) -> Result<(profile::ResolvedProfile, bool)> {
    match cat.resolve(name, project, operator_npub, &|_, _| None) {
        Ok(r) => Ok((r, false)),
        Err(e) if project.is_some() && e.to_string().contains("not registered in project") => {
            let r = cat.resolve(name, None, operator_npub, &|_, _| None)?;
            Ok((r, true))
        }
        Err(e) => Err(e),
    }
}

/// `flywheel squad` — named groups of profiles (ADR-0005 P6). Legacy recipes
/// map onto squads via [`profile::Squad::from_recipe`]; `recipes list` stays
/// an alias (the `recipes`/`template` asset commands).
/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
pub(crate) fn cmd_squad(cfg: &Config, cmd: &SquadCmd) -> Result<()> {
    use profile::{ProfileCatalog, Squad};

    match cmd {
        SquadCmd::List => {
            let cat = ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let mut rows: Vec<Value> = Vec::new();
            let compiled = profile::compiled_in_squads();
            for (name, squad) in &compiled {
                if !recipes.contains_key(name) {
                    rows.push(json!({
                        "name": name,
                        "members": squad.counts,
                        "source": "compiled-in",
                    }));
                }
            }
            for (name, r) in &recipes {
                let squad = Squad::from_recipe(r);
                rows.push(json!({
                    "name": name,
                    "members": squad.counts,
                }));
            }
            for name in cat.names() {
                if !recipes.contains_key(&name) && !compiled.contains_key(&name) {
                    rows.push(json!({
                        "name": name,
                        "members": { name: 1 },
                    }));
                }
            }
            if rows.is_empty() {
                println!(
                    "no squads defined (edit {})",
                    config::config_dir().join("recipes.toml").display()
                );
            } else {
                for row in &rows {
                    println!("{}", serde_json::to_string_pretty(row)?);
                }
            }
            Ok(())
        }
        SquadCmd::Show { name } => {
            let cat = ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let squad = profile::resolve_squad(name, &recipes, cat.get(name).is_some())
                .ok_or_else(|| anyhow!("unknown squad '@{name}' (flywheel squad list)"))?;
            let members: Vec<Value> = squad
                .members()
                .into_iter()
                .map(|(p, n)| json!({ "profile": p, "count": n }))
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "name": name, "members": members }))?
            );
            Ok(())
        }
        SquadCmd::Spawn {
            name,
            project,
            json,
        } => {
            let cat = ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let squad = profile::resolve_squad(name, &recipes, cat.get(name).is_some())
                .ok_or_else(|| anyhow!("unknown squad '@{name}' (flywheel squad list)"))?;
            if squad.counts.is_empty() {
                bail!("squad {name} has no members");
            }
            let operator_npub =
                std::env::var("TORON_OPERATOR_NPUB").unwrap_or_else(|_| "operator".to_string());
            let backend = backend::multiplexer_backend(cfg)?;
            let mut spawned: Vec<Value> = Vec::new();
            let project_key = project_door(project.as_deref(), "flywheel", "squad")?;
            for (profile_name, n) in squad.members() {
                let (resolved, fell_back) =
                    resolve_squad_member(&cat, profile_name, project.as_deref(), &operator_npub)?;
                if fell_back {
                    eprintln!(
                        "flywheel: notice: profile '@{profile_name}' not registered in project '{}' — falling back to host-scope resolve (operator {operator_npub})",
                        project.as_deref().unwrap_or("?")
                    );
                }
                let spec = &resolved.spec;
                let profile = cat
                    .get(profile_name)
                    .expect("resolve validated the profile above");
                for i in 0..n {
                    let agent_name = format!("squad-{name}-{profile_name}-{}", i + 1);
                    let workspace = resolved_spec_workspace(cfg, backend.as_ref(), &spec.harness)?;
                    let args = profile_launch_args(&spec.harness, profile, None)?;
                    let mail_identity =
                        pinned_mail_identity(cfg, project_key, &agent_name, &spec.harness);
                    if mail_identity.is_none() {
                        refuse_unpinned_spawn(
                            &agent_name,
                            Some(profile_name),
                            backend::mail_backend(cfg).is_ok(),
                        )?;
                    }
                    let info =
                        backend.agent_start(&agent_name, &spec.harness, &workspace, &args)?;
                    if let Some(ref pin) = mail_identity {
                        prime_profile_identity(backend.as_ref(), &info.pane_id, &agent_name, pin);
                        bind_pane_mail_identity(
                            cfg,
                            project_key,
                            &agent_name,
                            &info.pane_id,
                            Some(pin),
                        );
                    }
                    bind_spawn_role(
                        cfg,
                        project_key,
                        Some(profile_name),
                        profile_name,
                        mail_identity.as_deref(),
                    )?;
                    record_spawn_entry(
                        cfg,
                        project_key,
                        state::SpawnEntry::profiled(
                            &info.pane_id,
                            &agent_name,
                            profile_name,
                            &spec.harness,
                            mail_identity,
                        ),
                    );
                    spawned.push(json!({
                        "squad": name,
                        "profile": profile_name,
                        "agent": info.name.as_deref().unwrap_or(&agent_name),
                        "pane": info.pane_id,
                        "backend": spec.backend,
                    }));
                    if !*json {
                        println!(
                            "spawned {profile_name}#{i} → agent={} pane={} backend={}",
                            info.name.as_deref().unwrap_or(&agent_name),
                            info.pane_id,
                            spec.backend
                        );
                    }
                }
            }
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "spawned": spawned }))?
                );
            }
            Ok(())
        }
        SquadCmd::Remove { name } => {
            let mut recipes = config::Recipes::load()?;
            if !recipes.recipes.contains_key(name) {
                bail!("no squad/recipe named {name:?}");
            }
            recipes.recipes.remove(name);
            let path = config::config_dir().join("recipes.toml");
            if recipes.recipes.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                let raw = toml::to_string_pretty(&recipes)?;
                std::fs::write(&path, raw)?;
            }
            println!("removed squad/recipe {name}");
            Ok(())
        }
        SquadCmd::Create {
            name,
            members,
            dry_run,
        } => {
            if members.is_empty() {
                bail!("squad create {name}: pass at least one --profile <profile>=<count>");
            }
            let mut recipes = config::Recipes::load()?;
            let mut recipe = config::Recipe::default();
            let cat = ProfileCatalog::load()?;
            for (profile, count) in members {
                if cat.get(profile).is_none() {
                    bail!("unknown profile @{profile} (flywheel profile list)");
                }
                recipe.extra.insert(profile.clone(), *count);
            }
            if *dry_run {
                print_json(&json!({ "name": name, "members": recipe.extra }));
                return Ok(());
            }
            recipes.recipes.insert(name.clone(), recipe);
            let path = config::config_dir().join("recipes.toml");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, toml::to_string_pretty(&recipes)?)?;
            println!("wrote squad {name} → {}", path.display());
            Ok(())
        }
    }
}
