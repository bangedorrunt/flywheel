//! `flywheel profile` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::cmd::workflow::cmd_workflow;
use crate::config::Config;
use crate::output::print_json;
use crate::state;
use crate::{
    ProfilesFile, bind_pane_mail_identity, bind_spawn_role, is_builtin_profile,
    pinned_mail_identity, prime_profile_identity, profile_launch_args, profile_workflow_invoke,
    record_spawn_entry, refuse_unpinned_spawn, resolved_spec_workspace,
};
use crate::{backend, config, models, profile};

/// `flywheel profile` subcommands (ADR-0005 Spec 1/1b) — the agent catalog.
/// `list`/`show` are read-only; `create`/`edit` write `profiles.toml`; `run`
/// resolves `@name` → `Backend::spawn` (host scope, operator identity) and
/// optionally invokes a workflow.
#[derive(Subcommand)]
pub(crate) enum ProfileCmd {
    /// List catalog profiles (compiled-in defaults + locals)
    List,
    /// Show a profile (full behavior preamble + skills)
    Show {
        /// profile name (e.g. housekeeper, or a local [profiles.foo])
        name: String,
        /// machine output with the ADR-0007 D1 resolved route
        /// (harness/model after slot-inherit + fallback ladder)
        #[arg(long)]
        json: bool,
    },
    /// Add or update a profile in ~/.config/flywheel/profiles.toml
    Create {
        /// profile name
        name: String,
        /// harness / program (pi | oc | grok | hermes | prime-agent | ...)
        #[arg(long)]
        harness: Option<String>,
        /// model id (e.g. claude-sonnet-4)
        #[arg(long)]
        model: Option<String>,
        /// role label (free-form prompt-shaping)
        #[arg(long)]
        role: Option<String>,
        /// behavior preamble (system-prompt discipline)
        #[arg(long)]
        behavior: Option<String>,
        /// short task hint
        #[arg(long)]
        task_hint: Option<String>,
        /// backend trait impl (default hc)
        #[arg(long)]
        backend: Option<String>,
        /// default workflow name
        #[arg(long)]
        default_workflow: Option<String>,
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
        /// omp role model for lightweight tasks (--smol=, bd-gsvn.2)
        #[arg(long)]
        smol: Option<String>,
        /// omp role model for slow/thorough reasoning (--slow=, bd-gsvn.2)
        #[arg(long)]
        slow: Option<String>,
        /// omp role model for architectural planning (--plan=, bd-gsvn.2)
        #[arg(long)]
        plan: Option<String>,
    },
    /// Alias for `create` (same fields)
    Edit {
        /// profile name
        name: String,
        /// harness / program
        #[arg(long)]
        harness: Option<String>,
        /// model id
        #[arg(long)]
        model: Option<String>,
        /// role label
        #[arg(long)]
        role: Option<String>,
        /// behavior preamble
        #[arg(long)]
        behavior: Option<String>,
        /// short task hint
        #[arg(long)]
        task_hint: Option<String>,
        /// backend trait impl
        #[arg(long)]
        backend: Option<String>,
        /// default workflow name
        #[arg(long)]
        default_workflow: Option<String>,
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
        /// omp role model for lightweight tasks (--smol=, bd-gsvn.2)
        #[arg(long)]
        smol: Option<String>,
        /// omp role model for slow/thorough reasoning (--slow=, bd-gsvn.2)
        #[arg(long)]
        slow: Option<String>,
        /// omp role model for architectural planning (--plan=, bd-gsvn.2)
        #[arg(long)]
        plan: Option<String>,
    },
    /// Resolve @name (host scope) and spawn the agent via the configured
    /// backend. Optionally invoke a workflow after spawn.
    Run {
        /// profile name (e.g. housekeeper)
        name: String,
        /// workflow to invoke after spawn (resolves via the workflow catalog)
        #[arg(long)]
        workflow: Option<String>,
        /// JSON inputs for the workflow invoke
        #[arg(long)]
        inputs: Option<String>,
    },
    /// Remove a local profile from ~/.config/flywheel/profiles.toml (built-ins
    /// cannot be removed — override them via `create`/`edit` instead).
    Remove {
        /// profile name
        name: String,
    },
    /// Set routing fields on a profile (ADR-0007 D4 — the missing verb).
    /// Persists the merged profile to the user profiles.toml (bd-2dao rule:
    /// write the RAW user file merged over the built-in, never the catalog
    /// rebuild).
    Set {
        /// profile name (built-in or local)
        name: String,
        /// model id or slot alias (@cheap/@standard/@premium)
        #[arg(long)]
        model: Option<String>,
        /// provider prefix for a concrete model id
        #[arg(long)]
        provider: Option<String>,
        /// slot alias override for model (same as --model @slot)
        #[arg(long)]
        slot: Option<String>,
        /// ordered model-id fallbacks (comma-separated; NOT Profile.fallback —
        /// those are other profile names, ADR-0026)
        #[arg(long = "model-fallback", value_delimiter = ',')]
        model_fallback: Vec<String>,
        /// ordered profile-name fallbacks for billing/auth/provider reroute
        /// (ADR-0026 Spec 3). Distinct from --model-fallback.
        #[arg(long = "fallback", value_delimiter = ',')]
        fallback: Vec<String>,
        /// harness / program (empty on a slot-aliased profile inherits the
        /// slot's harness)
        #[arg(long)]
        harness: Option<String>,
        /// thinking/effort level: minimal|low|medium|high|xhigh|max
        #[arg(long)]
        effort: Option<String>,
        /// omp --smol= role model (id or @slot)
        #[arg(long)]
        smol: Option<String>,
        /// omp --slow= role model (id or @slot)
        #[arg(long)]
        slow: Option<String>,
        /// omp --plan= role model (id or @slot)
        #[arg(long)]
        plan: Option<String>,
        /// comma-separated skills list (catalog only)
        #[arg(long, value_delimiter = ',')]
        skills: Vec<String>,
        /// print the resulting user-file entry without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Export a profile as the versioned portable doc (ADR-0013 P2-11) to
    /// stdout, or a file with --out
    Export {
        /// profile name (built-in or local; exports the merged definition)
        name: String,
        /// write the doc to this path instead of stdout
        #[arg(long)]
        out: Option<String>,
    },
    /// Import a portable profile doc: upsert into
    /// ~/.config/flywheel/profiles.toml (an import of a built-in name lands
    /// as a deliberate local override)
    Import {
        /// path to the portable doc, or - for stdin
        path: String,
        /// import under this name instead of the doc's own
        #[arg(long)]
        name: Option<String>,
        /// print the resulting user-file entry without writing
        #[arg(long)]
        dry_run: bool,
    },
}

/// `flywheel profile` — the agent profile catalog (ADR-0005 Spec 1/1b).
///
/// `list` shows compiled-in defaults + local `profiles.toml` entries; `show`
/// prints the full profile (behavior preamble + skills); `create`/`edit`
/// write `~/.config/flywheel/profiles.toml`; `run @name` resolves the profile
/// (host scope, operator identity) and spawns the agent via the configured
/// multiplexer backend, optionally invoking a workflow after spawn.
///
/// governed-by: ADR-0005 (bd-p2-cli-wn8)
/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
pub(crate) fn cmd_profile(cfg: &Config, cmd: &ProfileCmd) -> Result<()> {
    use profile::{Profile, ProfileCatalog};

    match cmd {
        ProfileCmd::List => {
            let cat = ProfileCatalog::load()?;
            let mut rows: Vec<Value> = Vec::new();
            for name in cat.names() {
                let p = cat.get(&name).expect("names from catalog");
                rows.push(json!({
                    "name": name,
                    "harness": p.harness,
                    "model": p.model,
                    "effort": p.effort,
                    "smol": p.smol,
                    "slow": p.slow,
                    "plan": p.plan,
                    "role": p.role,
                    "backend": cat.backend(&name),
                    "default_workflow": p.default_workflow,
                    "builtin": is_builtin_profile(&name),
                    "fallback": p.fallback,
                    "model_fallback": p.model_fallback,
                }));
            }
            print_json(&json!({ "profiles": rows }));
        }
        ProfileCmd::Show { name, json } => {
            let cat = ProfileCatalog::load()?;
            let p = cat
                .get(name)
                .ok_or_else(|| anyhow!("unknown profile '@{name}' (flywheel profile list)"))?;
            if *json {
                let resolved = models::ModelRegistry::load_routing().ok().and_then(|r| {
                    r.resolve_profile_route_full(&p.harness, &p.model, &p.model_fallback)
                        .ok()
                        .map(|(h, m, route)| {
                            json!({
                                "harness": h, "model": m, "route": route,
                            })
                        })
                });
                print_json(&json!({
                    "name": name,
                    "harness": p.harness,
                    "model": p.model,
                    "fallback": p.fallback,
                    "model_fallback": p.model_fallback,
                    "resolved": resolved,
                    "effort": p.effort,
                    "smol": p.smol,
                    "slow": p.slow,
                    "plan": p.plan,
                    "role": p.role,
                    "backend": cat.backend(name),
                    "default_workflow": p.default_workflow,
                    "behavior": p.behavior,
                    "task_hint": p.task_hint,
                    "skills": p.skills,
                }));
                return Ok(());
            }
            print_json(&json!({
                "name": name,
                "harness": p.harness,
                "fallback": p.fallback,
                "model_fallback": p.model_fallback,
                "effort": p.effort,
                "smol": p.smol,
                "slow": p.slow,
                "plan": p.plan,
                "role": p.role,
                "backend": cat.backend(name),
                "default_workflow": p.default_workflow,
                "behavior": p.behavior,
                "task_hint": p.task_hint,
                "skills": p.skills,
            }));
        }
        ProfileCmd::Create {
            name,
            harness,
            model,
            role,
            behavior,
            task_hint,
            backend,
            default_workflow,
            effort,
            smol,
            slow,
            plan,
        }
        | ProfileCmd::Edit {
            name,
            harness,
            model,
            role,
            behavior,
            task_hint,
            backend,
            default_workflow,
            effort,
            smol,
            slow,
            plan,
        } => {
            let path = config::config_dir().join("profiles.toml");
            let cat = ProfileCatalog::load()?;
            let base = cat.get(name).cloned().unwrap_or_default();
            let mut merged = Profile {
                harness: harness.clone().unwrap_or(base.harness),
                model: model.clone().unwrap_or(base.model),
                effort: effort.clone().unwrap_or(base.effort),
                smol: smol.clone().unwrap_or(base.smol),
                slow: slow.clone().unwrap_or(base.slow),
                plan: plan.clone().unwrap_or(base.plan),
                role: role.clone().unwrap_or(base.role),
                behavior: behavior.clone().unwrap_or(base.behavior),
                task_hint: task_hint.clone().unwrap_or(base.task_hint),
                backend: backend.clone().unwrap_or(base.backend),
                default_workflow: default_workflow.clone().unwrap_or(base.default_workflow),
                skills: base.skills,
                provider: base.provider,
                fallback: base.fallback,
                when: base.when,
                model_fallback: base.model_fallback,
            };
            if merged.model.is_empty() {
                bail!("profile {name} needs --model (id or @cheap/@standard/@premium)");
            }
            if merged.harness.is_empty() && models::slot_alias(&merged.model).is_none() {
                bail!(
                    "profile {name} needs --harness, or a slot alias (@cheap/@standard/@premium)"
                );
            }
            if !merged.harness.is_empty() {
                match cat.canonical_harness(&merged.harness) {
                    Some(kind) => merged.harness = kind,
                    None => bail!(
                        "profile {name}: unknown harness {:?} — spawnable harnesses: {} \
                         (short aliases oc/gk are normalized). For a custom harness add \
                         `[harness.{}] spawn_cmd = \"...\"` to profiles.toml.",
                        merged.harness,
                        cat.known_harnesses().join(", "),
                        merged.harness
                    ),
                }
            }
            let mut on_disk: BTreeMap<String, profile::Profile> =
                profile::ProfileCatalog::load_user_file(Some(&path));
            on_disk.insert(name.clone(), merged);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let raw = toml::to_string_pretty(&ProfilesFile { profiles: on_disk })?;
            std::fs::write(&path, raw)?;
            println!("wrote {} [profiles.{name}]", path.display());
        }
        ProfileCmd::Run {
            name,
            workflow,
            inputs,
        } => {
            let cat = ProfileCatalog::load()?;
            let _inputs: Value = match inputs {
                Some(s) if !s.is_empty() => {
                    serde_json::from_str(s).map_err(|e| anyhow!("--inputs must be JSON: {e}"))?
                }
                _ => json!({}),
            };
            let operator_npub =
                std::env::var("TORON_OPERATOR_NPUB").unwrap_or_else(|_| "operator".to_string());
            let resolved = cat.resolve(name, None, &operator_npub, &|_, _| None)?;
            let spec = &resolved.spec;

            let backend = backend::multiplexer_backend(cfg)?;
            let agent_name = format!("profile-{name}");
            let profile = cat.get(name).expect("resolve validated the profile above");
            let args = profile_launch_args(&spec.harness, profile, None)?;
            let workspace = resolved_spec_workspace(cfg, backend.as_ref(), &spec.harness)?;
            let host_project = "flywheel";
            let mail_identity = pinned_mail_identity(cfg, host_project, &agent_name, &spec.harness);
            if mail_identity.is_none() {
                refuse_unpinned_spawn(&agent_name, Some(name), backend::mail_backend(cfg).is_ok())?;
            }
            let info = backend.agent_start(&agent_name, &spec.harness, &workspace, &args)?;
            println!(
                "profile {name} spawned: agent={} pane={} backend={}",
                info.name.as_deref().unwrap_or(&agent_name),
                info.pane_id,
                spec.backend
            );
            if let Some(ref pin) = mail_identity {
                prime_profile_identity(backend.as_ref(), &info.pane_id, &agent_name, pin);
                bind_pane_mail_identity(cfg, host_project, &agent_name, &info.pane_id, Some(pin));
            }
            bind_spawn_role(
                cfg,
                host_project,
                Some(name),
                name,
                mail_identity.as_deref(),
            )?;
            record_spawn_entry(
                cfg,
                host_project,
                state::SpawnEntry::profiled(
                    &info.pane_id,
                    &agent_name,
                    name,
                    &spec.harness,
                    mail_identity,
                ),
            );
            if let Some(wf) = workflow {
                let invoke = profile_workflow_invoke(
                    wf.as_str(),
                    inputs.as_deref(),
                    workspace.as_str(),
                    host_project,
                );
                cmd_workflow(cfg, &invoke)?;
            }
        }
        ProfileCmd::Remove { name } => {
            if is_builtin_profile(name) {
                bail!(
                    "cannot remove built-in profile {name:?} — override it via `flywheel profile create {name} ...` instead"
                );
            }
            let path = config::config_dir().join("profiles.toml");
            let cat = ProfileCatalog::load()?;
            if cat.get(name).is_none() {
                bail!("no profile named {name:?}");
            }
            let mut on_disk: BTreeMap<String, profile::Profile> =
                profile::ProfileCatalog::load_user_file(Some(&path));
            on_disk.remove(name);
            if on_disk.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                let raw = toml::to_string_pretty(&ProfilesFile { profiles: on_disk })?;
                std::fs::write(&path, raw)?;
            }
            println!("removed profile {name}");
        }
        ProfileCmd::Set {
            name,
            model,
            provider,
            slot,
            model_fallback,
            fallback,
            harness,
            effort,
            smol,
            slow,
            plan,
            skills,
            dry_run,
        } => {
            use models::slot_alias;
            let path = config::config_dir().join("profiles.toml");
            let cat = ProfileCatalog::load()?;
            if cat.get(name).is_none() {
                bail!("no profile named {name:?} (flywheel profile list)");
            }
            let base = cat.get(name).cloned().unwrap_or_default();
            let model = match slot {
                Some(s) => Some(format!("@{}", s.trim_start_matches('@'))),
                None => model.clone(),
            };
            let merged = Profile {
                model: model.unwrap_or(base.model),
                provider: provider.clone().unwrap_or(base.provider),
                model_fallback: if model_fallback.is_empty() {
                    base.model_fallback
                } else {
                    model_fallback.clone()
                },
                fallback: if fallback.is_empty() {
                    base.fallback
                } else {
                    fallback.clone()
                },
                harness: harness.clone().unwrap_or(base.harness),
                effort: effort.clone().unwrap_or(base.effort),
                smol: smol.clone().unwrap_or(base.smol),
                slow: slow.clone().unwrap_or(base.slow),
                plan: plan.clone().unwrap_or(base.plan),
                skills: if skills.is_empty() {
                    base.skills
                } else {
                    skills.clone()
                },
                ..base
            };
            if merged.model.is_empty() {
                bail!("profile {name} needs --model (id or @cheap/@standard/@premium)");
            }
            if merged.harness.is_empty() && slot_alias(&merged.model).is_none() {
                bail!(
                    "profile {name} needs --harness, or a slot alias (@cheap/@standard/@premium)"
                );
            }
            let mut on_disk: BTreeMap<String, profile::Profile> =
                profile::ProfileCatalog::load_user_file(Some(&path));
            on_disk.insert(name.clone(), merged.clone());
            if *dry_run {
                print_json(&serde_json::to_value(&merged)?);
                return Ok(());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let raw = toml::to_string_pretty(&ProfilesFile { profiles: on_disk })?;
            std::fs::write(&path, raw)?;
            println!("wrote {} [profiles.{name}]", path.display());
        }
        ProfileCmd::Export { name, out } => {
            let cat = ProfileCatalog::load()?;
            let profile = cat
                .get(name)
                .with_context(|| format!("no profile named {name:?} (flywheel profile list)"))?;
            let doc = profile::profile_to_portable(name, profile);
            let rendered = serde_json::to_string_pretty(&doc)?;
            match out {
                Some(out_path) => {
                    std::fs::write(out_path, rendered + "\n")?;
                    println!("exported profile {name} -> {out_path}");
                }
                None => println!("{rendered}"),
            }
        }
        ProfileCmd::Import {
            path,
            name,
            dry_run,
        } => {
            let raw = if path == "-" {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            } else {
                std::fs::read_to_string(path)?
            };
            let doc: serde_json::Value = serde_json::from_str(raw.trim())?;
            let (doc_name, imported) = profile::profile_from_portable(&doc)?;
            let target = name.clone().unwrap_or(doc_name);
            if *dry_run {
                let mut one: BTreeMap<String, profile::Profile> = BTreeMap::new();
                one.insert(target.clone(), imported);
                print_json(&serde_json::to_value(&one)?);
                return Ok(());
            }
            let file_path = config::config_dir().join("profiles.toml");
            let mut on_disk: BTreeMap<String, profile::Profile> =
                profile::ProfileCatalog::load_user_file(Some(&file_path));
            on_disk.insert(target.clone(), imported);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let raw_toml = toml::to_string_pretty(&ProfilesFile { profiles: on_disk })?;
            std::fs::write(&file_path, raw_toml)?;
            println!("imported profile {target} -> {}", file_path.display());
        }
    }
    Ok(())
}

/// --- moved-in test mod (bd-wave5-main-eviction-d5hht) ---
#[cfg(test)]
mod profile_cli_tests {
    use super::*;
    use crate::cmd::squad::resolve_squad_member;
    use crate::is_builtin_profile;
    use crate::snapshot::snapshot_runs;
    #[test]
    fn is_builtin_profile_classifies_the_six() {
        for name in [
            "oracle",
            "planner",
            "reviewer",
            "coder",
            "scout",
            "housekeeper",
        ] {
            assert!(is_builtin_profile(name), "{name} is a built-in");
        }
        assert!(!is_builtin_profile("mybot"));
        assert!(!is_builtin_profile(""));
    }

    /// bd-2nib: the builtin set must track compiled_in_defaults exactly —
    /// a hardcoded name list drifted and missed `pa`, so profile
    /// create/remove baked the pa compiled-in into the user's
    /// profiles.toml (the merged-catalog leak).
    #[test]
    fn is_builtin_profile_tracks_compiled_in_defaults() {
        assert!(
            !is_builtin_profile("pa"),
            "pa is a harness recipe, not a compiled-in profile (ADR-0007 D2)"
        );
        for name in profile::compiled_in_defaults().keys() {
            assert!(
                is_builtin_profile(name),
                "every compiled-in profile is builtin: {name}"
            );
        }
    }

    #[test]
    fn profile_catalog_lists_nine_builtins() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        assert_eq!(cat.names().len(), 9, "9 job profiles (ADR-0007 D2)");
        for name in [
            "oracle",
            "planner",
            "reviewer",
            "coder",
            "rust-coder",
            "ts-coder",
            "zig-coder",
            "scout",
            "housekeeper",
        ] {
            let p = cat.get(name).expect(name);
            assert!(p.behavior.len() > 200, "{name} preamble missing");
        }
    }

    /// Project-scope resolution of an UNREGISTERED profile falls back to
    /// host-scope (operator npub) — the same effective behavior as
    /// `spawn --profile`. Registration stays the strict path for workflow
    /// runs that sign as project agents (engine resolution, untouched).
    #[test]
    fn squad_resolve_unregistered_profile_falls_back_host_scope() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        let (r, fell_back) = resolve_squad_member(&cat, "coder", Some("demo"), "npub-operator")
            .expect("fallback resolves host-scope");
        assert!(fell_back, "unregistered profile must fall back");
        assert_eq!(r.npub, "npub-operator");
        assert!(matches!(r.signing, profile::SigningContext::Operator(_)));
    }

    /// No project flag → host-scope resolve directly, no fallback signal.
    #[test]
    fn squad_resolve_host_scope_without_project_no_fallback() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        let (r, fell_back) =
            resolve_squad_member(&cat, "coder", None, "npub-operator").expect("host-scope resolve");
        assert!(!fell_back, "no project -> no fallback path");
        assert_eq!(r.npub, "npub-operator");
        assert!(matches!(r.signing, profile::SigningContext::Operator(_)));
    }

    /// Unknown profile still fails fast in both scopes (never a silent
    /// fallback for a typo).
    #[test]
    fn squad_resolve_unknown_profile_fails_fast() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        let err = resolve_squad_member(&cat, "ghost", Some("demo"), "npub-operator").unwrap_err();
        assert!(err.to_string().contains("unknown profile"), "{err}");
        let err = resolve_squad_member(&cat, "ghost", None, "npub-operator").unwrap_err();
        assert!(err.to_string().contains("unknown profile"), "{err}");
    }

    #[test]
    fn profile_run_resolves_host_scope_and_spec() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        let r = cat
            .resolve("housekeeper", None, "npub-op", &|_, _| None)
            .expect("resolve");
        assert_eq!(r.spec.harness, "pi");
        assert_eq!(r.spec.model, "opencode-go/muse-spark-1.3-contributor");
        assert_eq!(r.spec.backend, "hc");
        assert_eq!(
            r.signing,
            profile::SigningContext::Operator("npub-op".into())
        );
    }

    #[test]
    fn squad_show_resolves_recipe_alias_to_members() {
        let r = config::Recipe {
            oc: Some(2),
            pi: Some(1),
            gk: None,
            extra: Default::default(),
        };
        let squad = profile::Squad::from_recipe(&r);
        let members = squad.members();
        assert_eq!(members, vec![("oc", 2), ("pi", 1)]);
    }

    #[test]
    fn squad_list_lists_builtin_profiles_as_singles() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        let recipes = config::Recipes::default().recipes;
        let names: Vec<String> = cat.names();
        for name in &names {
            assert!(
                !recipes.contains_key(name),
                "no user recipe named {name} in a fresh install"
            );
        }
        assert_eq!(names.len(), 9, "9 job profiles (ADR-0007 D2)");
    }

    #[test]
    fn snapshot_runs_returns_well_formed_entries() {
        let runs = snapshot_runs();
        for r in &runs {
            assert!(r.get("id").and_then(Value::as_str).is_some(), "id");
            assert!(
                r.get("project").and_then(Value::as_str).is_some(),
                "project"
            );
            assert!(r.get("kind").and_then(Value::as_str).is_some(), "kind");
            assert!(r.get("owner").and_then(Value::as_str).is_some(), "owner");
            assert!(r.get("state").and_then(Value::as_str).is_some(), "state");
            assert!(r.get("ts").and_then(Value::as_str).is_some(), "ts");
        }
    }
}
