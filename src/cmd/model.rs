//! `flywheel model` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::output::print_json;
use std::io::IsTerminal;

/// `flywheel model` subcommands (ADR-0010 Spec D, P10.2): the `models.toml`
/// registry. Profile.model ids resolve against it for per-harness injection;
/// unknown ids warn and pass through (never block a run).
#[derive(Subcommand)]
pub(crate) enum ModelCmd {
    /// List registered model ids
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Show one registry entry (provider, cost, caps, fallback)
    Show {
        /// model id (e.g. claude-sonnet-4)
        id: String,
    },
    /// Add or update a registry entry in ~/.config/flywheel/models.toml
    Add {
        /// model id
        id: String,
        /// provider prefix (`anthropic` → opencode -m anthropic/<id>)
        #[arg(long)]
        provider: String,
        /// input cost per million tokens (advisory)
        #[arg(long, default_value_t = 0.0)]
        cost_in_per_mtok: f64,
        /// output cost per million tokens (advisory)
        #[arg(long, default_value_t = 0.0)]
        cost_out_per_mtok: f64,
        /// harnesses this model supports (comma list; empty = all)
        #[arg(long)]
        caps: Option<String>,
        /// fallback id when a harness is outside caps — ALSO the retry-on-429
        /// fallback (bd-42fs)
        #[arg(long)]
        fallback: Option<String>,
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
        /// cost tier (ADR-0007): cheap | standard | premium
        #[arg(long)]
        tier: Option<String>,
    },
    /// Set which slot a model serves as the DEFAULT for (ADR-0007 D1:
    /// "select default model from CLI"). Writes the whole SlotSpec to the
    /// user models.toml overlay — never bakes compiled-ins.
    Set {
        /// model id (provider/model or bare; bare resolves via the
        /// default provider; must be a registered id)
        id: String,
        /// target slot: cheap | standard | premium (TTY without --slot
        /// prompts; grok is closed-source — it has no slot)
        #[arg(long)]
        slot: Option<String>,
        /// never prompt (non-interactive)
        #[arg(long)]
        no_input: bool,
        /// show what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Set the operator-wide default model pair (bd-ll3k): every slot
    /// alias (@cheap/@standard/@premium) resolves through it while set.
    /// Writes `[defaults]` to the user models.toml overlay — explicit
    /// per-profile harness and concrete model ids still win (CLI >
    /// concrete id > defaults > slot).
    SetDefault {
        /// concrete model id (provider/model or bare; must be registered)
        #[arg(long)]
        model: Option<String>,
        /// harness (pi | omp | opencode | grok | ...)
        #[arg(long)]
        harness: Option<String>,
        /// never prompt (non-interactive; missing halves are an error)
        #[arg(long)]
        no_input: bool,
        /// show what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the effective default pair and which aliases it overrides.
    Defaults {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Clear the operator-wide default (back to per-slot behavior).
    ClearDefault {
        /// actually clear (skip the confirm)
        #[arg(long)]
        force: bool,
        /// never prompt (non-interactive)
        #[arg(long)]
        no_input: bool,
    },
    /// Remove registry entries. Bare ids resolve via the default provider.
    /// Compiled-in ids are refused unless --shadow (writes a user shadow).
    /// TTY with no ids → dialoguer MultiSelect. Requires --force to delete.
    Remove {
        /// model ids (bare or provider/model; repeatable)
        ids: Vec<String>,
        /// resolve bare ids against the default provider (default: on)
        #[arg(long, default_value_t = true)]
        resolve: bool,
        /// remove compiled-in ids by writing nothing — they cannot be
        /// deleted; this flag is accepted so scripts do not break
        #[arg(long)]
        shadow: bool,
        /// actually delete (skip the confirm)
        #[arg(long)]
        force: bool,
        /// never prompt (non-interactive)
        #[arg(long)]
        no_input: bool,
    },
}

/// `flywheel workflow` — the unified orchestration surface (ADR-0005 Spec 5 /
/// P3.7). Wires the dead-code modules (`crew`, `workflow_catalog`,
/// `builtin_workflows`) into the CLI: catalog listing, the fail-stop lint
/// gate, engine-backed `run`, scaffolding, history, and the auto-crew
/// preview.
/// `flywheel model` — the models.toml registry (ADR-0010 Spec D, P10.2).
pub(crate) fn cmd_models(cmd: &ModelCmd) -> Result<()> {
    match cmd {
        ModelCmd::List { json } => {
            let user = crate::models::ModelRegistry::load()?;
            let reg = crate::models::ModelRegistry::load_routing()?;
            let rows: Vec<Value> = reg
                .models
                .iter()
                .map(|(id, e)| {
                    json!({
                        "id": id,
                        "provider": crate::models::split_key(id).0,
                        "cost_in_per_mtok": e.cost_in_per_mtok,
                        "cost_out_per_mtok": e.cost_out_per_mtok,
                        "caps": e.caps,
                        "fallback": e.fallback,
                        "builtin": !user.models.contains_key(id),
                    })
                })
                .collect();
            if *json {
                print_json(&json!({ "models": rows }));
            } else if rows.is_empty() {
                println!("no models registered (flywheel model add <id> --provider <p>)");
            } else {
                for r in &rows {
                    println!(
                        "{id:24} {provider:10} ${in_cost:.2}/${out_cost:.2} caps={caps:?} fb={fb:?}",
                        id = r["id"].as_str().unwrap_or(""),
                        provider = r["provider"].as_str().unwrap_or(""),
                        in_cost = r["cost_in_per_mtok"].as_f64().unwrap_or(0.0),
                        out_cost = r["cost_out_per_mtok"].as_f64().unwrap_or(0.0),
                        caps = r["caps"]
                            .as_array()
                            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                            .unwrap_or_default(),
                        fb = r["fallback"].as_str(),
                    );
                }
            }
            Ok(())
        }
        ModelCmd::Show { id } => {
            let reg = crate::models::ModelRegistry::load_routing()?;
            match reg.get(id) {
                Some(e) => print_json(&json!({
                    "id": id,
                    "provider": reg.provider_of(id),
                    "cost_in_per_mtok": e.cost_in_per_mtok,
                    "cost_out_per_mtok": e.cost_out_per_mtok,
                    "caps": e.caps,
                    "fallback": e.fallback,
                    "effort": e.effort,
                })),
                None => bail!("no model named {id:?} (flywheel model list)"),
            }
            Ok(())
        }
        ModelCmd::Add {
            id,
            provider,
            cost_in_per_mtok,
            cost_out_per_mtok,
            caps,
            fallback,
            effort,
            tier,
        } => {
            if id.trim().is_empty() {
                bail!("model id cannot be empty");
            }
            if id.contains('/') {
                bail!(
                    "--id must be the bare model id; the registry key is \
                     '{{provider}}/{id}' — pass --provider instead"
                );
            }
            let mut reg = crate::models::ModelRegistry::load()?;
            let caps_list: Vec<String> = match caps {
                Some(c) if !c.trim().is_empty() => c
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                _ => vec![],
            };
            if provider.trim().is_empty() {
                bail!("--provider is required (registry keys are provider/model)");
            }
            let key = format!("{provider}/{id}");
            let tier_val = match tier.as_deref().map(str::trim) {
                Some("cheap") => Some(crate::models::Tier::Cheap),
                Some("standard") => Some(crate::models::Tier::Standard),
                Some("premium") => Some(crate::models::Tier::Premium),
                Some(other) => bail!("unknown tier {other:?} (cheap | standard | premium)"),
                None => None,
            };
            reg.models.insert(
                key.clone(),
                crate::models::ModelEntry {
                    cost_in_per_mtok: *cost_in_per_mtok,
                    cost_out_per_mtok: *cost_out_per_mtok,
                    caps: caps_list,
                    fallback: fallback.clone(),
                    effort: effort.clone(),
                    tier: tier_val,
                },
            );
            reg.save()?;
            println!("registered model {key}");
            Ok(())
        }
        ModelCmd::Set {
            id,
            slot,
            no_input,
            dry_run,
        } => {
            // Resolve the id first (bare → composite via default provider).
            let reg = crate::models::ModelRegistry::load_routing()?;
            let key = reg
                .key_for(id)
                .ok_or_else(|| anyhow!("unknown model {id:?} — register it first: flywheel model add {id} --provider <p>"))?;
            let entry = reg.models.get(&key).cloned();
            let entry = entry.ok_or_else(|| anyhow!("unknown model {id:?}"))?;
            // Grok is closed-source with its own proprietary model: it is
            // not a slot target and a grok-caps-only model cannot serve a
            // slot default (operator decision 2026-09-05).
            if entry.caps.len() == 1 && entry.caps[0] == "grok" {
                bail!(
                    "model {key} is grok-only — grok is closed-source with its own proprietary model and cannot serve as a slot default"
                );
            }
            // Target slot: flag, else TTY select, else error.
            let slot_name = match slot.as_deref().map(str::trim) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => {
                    if *no_input || !std::io::stdin().is_terminal() {
                        bail!("model set: pass --slot cheap|standard|premium (or run on a TTY)");
                    }
                    let idx = dialoguer::Select::new()
                        .with_prompt(format!("slot for {key}"))
                        .items(&["cheap", "standard", "premium"])
                        .interact()?;
                    ["cheap", "standard", "premium"][idx].to_string()
                }
            };
            if !matches!(slot_name.as_str(), "cheap" | "standard" | "premium") {
                bail!(
                    "unknown slot {slot_name:?} (cheap | standard | premium; never \"default\" — that is the compiled-in squad)"
                );
            }
            let routing = crate::models::ModelRegistry::load_routing()?;
            let current = routing.slot(&slot_name).cloned().unwrap_or_default();
            // Loud warn (not refusal — the operator may know something the
            // registry does not): a caps whitelist that excludes the slot's
            // harness means every spawn detours through the fallback ladder.
            if let Some(h) = (!current.harness.is_empty()).then_some(current.harness.as_str())
                && !entry.caps.is_empty()
                && !entry.caps.iter().any(|c| c == h)
            {
                eprintln!(
                    "flywheel: WARNING — model {key} caps={:?} excludes the {slot_name} slot's harness {h:?}; \
                     spawns will detour through the fallback ladder (flywheel model show {key})",
                    entry.caps
                );
            }
            let next = crate::models::SlotSpec {
                model: key.clone(),
                ..current.clone()
            };
            if *dry_run {
                print_json(&json!({
                    "dry_run": true,
                    "slot": slot_name,
                    "from": { "harness": current.harness, "model": current.model },
                    "to": { "harness": next.harness, "model": next.model },
                }));
                return Ok(());
            }
            // bd-ll3k: persist through the USER registry only — saving the
            // routing layer here baked every compiled-in entry into the
            // user file (contradicting this verb's own never-bake rule and
            // freezing future compiled-in updates out for this user).
            let mut user = crate::models::ModelRegistry::load()?;
            user.slots.insert(slot_name.clone(), next);
            user.save()?;
            println!("slot {slot_name} default model → {key}");
            Ok(())
        }
        ModelCmd::SetDefault {
            model,
            harness,
            no_input,
            dry_run,
        } => {
            let reg = crate::models::ModelRegistry::load_routing()?;
            // Resolve halves: flags, else TTY prompt, else error.
            // Non-interactive callers must pass both halves explicitly.
            let mut model_id = model.as_deref().map(str::trim).unwrap_or("").to_string();
            let mut harness_id = harness.as_deref().map(str::trim).unwrap_or("").to_string();
            if (model_id.is_empty() || harness_id.is_empty())
                && (*no_input || !std::io::stdin().is_terminal())
            {
                bail!(
                    "model set-default: pass --model <provider/id> and --harness <h> (or run on a TTY)"
                );
            }
            if model_id.is_empty() {
                model_id = dialoguer::Input::new()
                    .with_prompt("default model (provider/id, must be registered)")
                    .interact_text()?;
            }
            if harness_id.is_empty() {
                harness_id = dialoguer::Input::new()
                    .with_prompt("default harness (pi | omp | opencode | grok | ...)")
                    .interact_text()?;
            }
            let (model_id, harness_id) =
                (model_id.trim().to_string(), harness_id.trim().to_string());
            if model_id.starts_with('@') {
                bail!("defaults model must be a concrete id — defaults cannot alias slots");
            }
            let key = reg.key_for(&model_id).ok_or_else(|| {
                anyhow!("unknown model {model_id:?} — register it first: flywheel model add <id> --provider <p>")
            })?;
            if harness_id.is_empty() {
                bail!("defaults harness must not be empty");
            }
            let from = reg.defaults.clone().filter(|d| d.active());
            let next = crate::models::DefaultsSpec {
                harness: harness_id.clone(),
                model: key.clone(),
                fallback: from.as_ref().and_then(|d| d.fallback.clone()),
            };
            if *dry_run {
                print_json(&json!({
                    "dry_run": true,
                    "from": from,
                    "to": { "harness": next.harness, "model": next.model },
                    "overrides_aliases": ["@cheap", "@standard", "@premium"],
                }));
                return Ok(());
            }
            let mut user = crate::models::ModelRegistry::load()?;
            user.defaults = Some(next);
            user.save()?;
            println!(
                "default model → {key} on {harness_id} (all slot aliases resolve through it; clear with: flywheel model clear-default)"
            );
            Ok(())
        }
        ModelCmd::Defaults { json } => {
            let reg = crate::models::ModelRegistry::load_routing()?;
            match reg.defaults.clone().filter(|d| d.active()) {
                Some(d) => {
                    let out = json!({
                        "harness": d.harness,
                        "model": d.model,
                        "fallback": d.fallback,
                        "overrides_aliases": ["@cheap", "@standard", "@premium"],
                    });
                    if *json {
                        print_json(&out);
                    } else {
                        println!(
                            "default: {} on {} (aliases @cheap/@standard/@premium all resolve here)",
                            d.model, d.harness
                        );
                    }
                }
                None => {
                    if *json {
                        print_json(&json!({ "defaults": Value::Null }));
                    } else {
                        println!(
                            "no operator default set (per-slot behavior; flywheel model set-default ...)"
                        );
                    }
                }
            }
            Ok(())
        }
        ModelCmd::ClearDefault { force, no_input } => {
            let reg = crate::models::ModelRegistry::load_routing()?;
            let Some(current) = reg.defaults.clone().filter(|d| d.active()) else {
                println!("no operator default set (nothing to clear)");
                return Ok(());
            };
            if !*force {
                if *no_input || !std::io::stdin().is_terminal() {
                    bail!("model clear-default: pass --force (or run on a TTY to confirm)");
                }
                let confirm: bool = dialoguer::Confirm::new()
                    .with_prompt(format!(
                        "clear default {} on {} (back to per-slot routing)?",
                        current.model, current.harness
                    ))
                    .default(false)
                    .interact()?;
                if !confirm {
                    println!("kept");
                    return Ok(());
                }
            }
            let mut user = crate::models::ModelRegistry::load()?;
            user.defaults = None;
            user.save()?;
            println!("default cleared (back to per-slot routing)");
            Ok(())
        }
        ModelCmd::Remove {
            ids,
            resolve,
            shadow: _,
            force,
            no_input,
        } => {
            let mut picked_ids: Vec<String> = ids.clone();
            if picked_ids.is_empty() {
                if *no_input || !std::io::stdin().is_terminal() {
                    bail!("model remove: pass at least one id (flywheel model list)");
                }
                // TTY + no ids → dialoguer MultiSelect over USER models only
                // (compiled-in ids are never deletable — D4).
                let user = crate::models::ModelRegistry::load()?;
                if user.models.is_empty() {
                    bail!("no user models to remove (compiled-in ids are not deletable)");
                }
                let keys: Vec<&String> = user.models.keys().collect();
                let picked = dialoguer::MultiSelect::new()
                    .with_prompt("models to remove (Space=select, Enter=confirm)")
                    .items(&keys)
                    .interact()?;
                picked_ids.extend(picked.into_iter().map(|i| keys[i].clone()));
            }
            if picked_ids.is_empty() {
                bail!("model remove: nothing selected");
            }
            let mut reg = crate::models::ModelRegistry::load()?;
            let compiled = crate::models::compiled_in_models();
            let mut removed: Vec<String> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            for raw in &picked_ids {
                // Bare id → composite via the routing layer's default-provider
                // resolution; provider/model keys pass through verbatim.
                let key = if raw.contains('/') || !*resolve {
                    raw.clone()
                } else {
                    let routing = crate::models::ModelRegistry::load_routing()?;
                    let p = routing.provider_of(&raw);
                    if p.is_empty() {
                        raw.clone()
                    } else {
                        format!("{p}/{raw}")
                    }
                };
                if compiled.contains_key(&key) && !reg.models.contains_key(&key) {
                    skipped.push(key);
                    continue;
                }
                if reg.models.remove(&key).is_some() {
                    removed.push(key);
                } else {
                    skipped.push(key);
                }
            }
            if removed.is_empty() {
                bail!("no deletable models among {skipped:?} — compiled-in ids are never removed");
            }
            if !*force && std::io::stdin().is_terminal() {
                let ok = dialoguer::Confirm::new()
                    .with_prompt(format!("remove {} model(s)?", removed.len()))
                    .default(false)
                    .interact()?;
                if !ok {
                    bail!("model remove: aborted");
                }
            }
            reg.save()?;
            for k in &removed {
                println!("removed model {k}");
            }
            for k in &skipped {
                eprintln!("skipped {k} (not in the user file; compiled-ins are permanent)");
            }
            Ok(())
        }
    }
}
