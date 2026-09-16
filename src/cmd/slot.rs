//! `flywheel slot` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::output::print_json;
use std::io::IsTerminal;

/// Routing slot subcommands (ADR-0007 D1/D4). A slot is harness + model
/// (+ fallbacks) for a cost band: cheap | standard | premium.
#[derive(Subcommand)]
pub(crate) enum SlotCmd {
    /// List slots with source (compiled-in vs user overlay)
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Show one slot (harness, model, fallbacks, source)
    Show {
        /// slot name or alias (cheap/free/smol, standard/default, premium/slow/plan)
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Set slot fields in the USER models.toml (whole-Spec overlay; the
    /// compiled-in defaults are never baked into the file — ADR-0007 D1).
    Set {
        /// slot name: cheap | standard | premium (never "default" — that is
        /// the compiled-in squad)
        name: String,
        /// harness / program (opencode | pi | omp | grok | ...)
        #[arg(long)]
        harness: Option<String>,
        /// concrete model id (provider/model or bare; NOT @alias)
        #[arg(long)]
        model: Option<String>,
        /// fallback harness when the primary is unavailable
        #[arg(long)]
        fallback_harness: Option<String>,
        /// ordered model-id fallbacks (comma-separated)
        #[arg(long = "model-fallback", value_delimiter = ',')]
        model_fallback: Vec<String>,
        /// print the resulting user-file entry without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the USER overlay for a slot (restores the compiled-in spec)
    Remove {
        /// slot name
        name: String,
        /// skip the confirm
        #[arg(long)]
        force: bool,
    },
}

/// `flywheel slot` — the three routing slots (ADR-0007 D1/D4).
pub(crate) fn cmd_slot(cmd: &SlotCmd) -> Result<()> {
    match cmd {
        SlotCmd::List { json } => {
            let reg = crate::models::ModelRegistry::load_routing()?;
            let user = crate::models::ModelRegistry::load()?;
            let rows: Vec<Value> = reg
                .slots
                .iter()
                .map(|(name, s)| {
                    json!({
                        "name": name,
                        "harness": s.harness,
                        "model": s.model,
                        "fallback_harness": s.fallback_harness,
                        "model_fallback": s.model_fallback,
                        "source": if user.slots.contains_key(name) { "user" } else { "compiled-in" },
                    })
                })
                .collect();
            if *json {
                print_json(&json!({ "slots": rows }));
            } else if rows.is_empty() {
                println!(
                    "no slots configured (flywheel slot set <name> --harness <h> --model <id>)"
                );
            } else {
                for r in &rows {
                    println!(
                        "{name:10} {harness:10} {model:36} fb_harness={fbh} source={src}",
                        name = r["name"].as_str().unwrap_or(""),
                        harness = r["harness"].as_str().unwrap_or(""),
                        model = r["model"].as_str().unwrap_or(""),
                        fbh = r["fallback_harness"].as_str().unwrap_or("-"),
                        src = r["source"].as_str().unwrap_or(""),
                    );
                }
            }
            Ok(())
        }
        SlotCmd::Show { name, json } => {
            let reg = crate::models::ModelRegistry::load_routing()?;
            match reg.slot(name) {
                Some(s) => {
                    let canonical = crate::models::slot_alias(name)
                        .or(match name.as_str() {
                            "free" | "smol" => Some(crate::models::SLOT_CHEAP),
                            "standard" => Some(crate::models::SLOT_STANDARD),
                            "slow" | "plan" | "default" => Some(crate::models::SLOT_PREMIUM),
                            _ => None,
                        })
                        .unwrap_or(name.as_str());
                    let spec = json!({
                        "name": canonical,
                        "harness": s.harness,
                        "model": s.model,
                        "fallback_harness": s.fallback_harness,
                        "model_fallback": s.model_fallback,
                    });
                    if *json {
                        print_json(&spec);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&spec)?);
                    }
                    Ok(())
                }
                None => bail!(
                    "unknown slot {name:?} (cheap | standard | premium — aliases: free/smol, default, slow/plan)"
                ),
            }
        }
        SlotCmd::Set {
            name,
            harness,
            model,
            fallback_harness,
            model_fallback,
            dry_run,
        } => {
            let canonical = match name.as_str() {
                crate::models::SLOT_CHEAP | "free" | "smol" => crate::models::SLOT_CHEAP,
                crate::models::SLOT_STANDARD | "default" => crate::models::SLOT_STANDARD,
                crate::models::SLOT_PREMIUM | "slow" | "plan" => crate::models::SLOT_PREMIUM,
                other => bail!(
                    "unknown slot {other:?} — slot names are cheap | standard | premium \
                     (aliases resolve on read; never write one). 'default' is a SQUAD name."
                ),
            };
            // Start from the ROUTING layer (compiled-in + user) so a partial
            // `slot set` keeps the other fields, then write the WHOLE spec as
            // the user overlay (D1: per-name whole-Spec).
            let routing = crate::models::ModelRegistry::load_routing()?;
            let mut spec = routing.slot(canonical).cloned().unwrap_or_default();
            if let Some(h) = harness {
                spec.harness = h.clone();
            }
            if let Some(m) = model {
                if m.starts_with('@') {
                    bail!("slot model must be a concrete id — slots cannot alias other slots");
                }
                spec.model = m.clone();
            }
            if let Some(fh) = fallback_harness {
                spec.fallback_harness = (!fh.is_empty()).then(|| fh.clone());
            }
            if !model_fallback.is_empty() {
                spec.model_fallback = model_fallback.clone();
            }
            if spec.harness.is_empty() || spec.model.is_empty() {
                bail!(
                    "slot {canonical} needs both --harness and --model (currently: harness={:?} model={:?})",
                    spec.harness,
                    spec.model
                );
            }
            let mut user = crate::models::ModelRegistry::load()?;
            user.slots.insert(canonical.to_string(), spec.clone());
            if *dry_run {
                print_json(&json!({
                    "name": canonical,
                    "spec": serde_json::to_value(&spec)?,
                    "note": "dry-run — nothing written",
                }));
                return Ok(());
            }
            user.save()?;
            println!(
                "wrote [slots.{canonical}] harness={} model={}",
                spec.harness, spec.model
            );
            Ok(())
        }
        SlotCmd::Remove { name, force } => {
            let canonical = match name.as_str() {
                crate::models::SLOT_CHEAP | "free" | "smol" => crate::models::SLOT_CHEAP,
                crate::models::SLOT_STANDARD | "default" => crate::models::SLOT_STANDARD,
                crate::models::SLOT_PREMIUM | "slow" | "plan" => crate::models::SLOT_PREMIUM,
                other => bail!("unknown slot {other:?}"),
            };
            let mut user = crate::models::ModelRegistry::load()?;
            if user.slots.remove(canonical).is_none() {
                println!("no user overlay for slot {canonical} (compiled-in default active)");
                return Ok(());
            }
            if !*force && std::io::stdin().is_terminal() {
                let ok = dialoguer::Confirm::new()
                    .with_prompt(format!("remove the user overlay for slot {canonical}?"))
                    .default(false)
                    .interact()?;
                if !ok {
                    bail!("slot remove: aborted");
                }
            }
            user.save()?;
            println!("removed user overlay for slot {canonical} (compiled-in default restored)");
            Ok(())
        }
    }
}
