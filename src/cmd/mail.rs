//! `flywheel mail` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::Result;
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::mail;
use crate::output::print_json;
use crate::state;

#[allow(clippy::large_enum_variant)] // clap derives one variant per verb
#[allow(clippy::large_stack_frames)] // frames come from clap's derive, not hand-written code
#[allow(unknown_lints, clippy::large_unopt_stack_frames)] // newer nightly names this derive lint differently
#[derive(Subcommand)]
pub(crate) enum MailCmd {
    /// fetch inbox (default: config identity; override with --as / FLYWHEEL_MAIL_AS / AGENT_NAME)
    Inbox {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        urgent: bool,
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value = "20")]
        limit: u32,
        /// only messages strictly newer than this RFC3339 timestamp, or the
        /// literal `last` (daemon-side crash-proof cursor, read-then-bump)
        #[arg(long)]
        since: Option<String>,
        /// keep only messages in this thread (thread id or message id)
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// bd-mail-inbox-json-a7wc: emit a STABLE machine contract — one JSON
        /// array of rows, exactly these keys per row: id, thread_id, sender,
        /// subject, importance, ack_required, read_ts_us, created_ts_us,
        /// duplicate_of, body_md (nullable: thread_id/duplicate_of/
        /// read_ts_us). Without it the human/legacy dump is unchanged.
        #[arg(long)]
        json: bool,
    },
    /// reply to a message in its thread (thread = the message id or thread id)
    Reply {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// message id to reply to (toron hex event id / thread id)
        #[arg(long = "message-id")]
        message_id: String,
        #[arg(long)]
        body: String,
        /// act as this agent (required for crew peer mail; else CoralFinch/config)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// mark a message read (read receipt; the projection honors it once
    /// read-state lands toron-side — bd-5icg)
    Read {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// message id (toron hex event id)
        #[arg(long = "message-id")]
        message_id: String,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// acknowledge a message that had ack_required=true
    Ack {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// message id (toron hex event id)
        #[arg(long = "message-id")]
        message_id: String,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// FTS5 search over the project's indexed messages
    Search {
        /// FTS5 query (phrase/prefix/boolean; hyphenated ids are quoted)
        query: String,
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long, default_value = "20")]
        limit: u32,
        /// filter by sender (adjective+noun or terminal name)
        #[arg(long)]
        sender: Option<String>,
        /// filter by importance: low|normal|high|urgent
        #[arg(long)]
        importance: Option<String>,
        /// filter to one thread (thread id or message id)
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
        /// newest first instead of relevance
        #[arg(long)]
        recency: bool,
        /// include message bodies in results
        #[arg(long)]
        bodies: bool,
    },
    /// summarize a thread (participants, key points, action items)
    Summarize {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// thread id or message id
        #[arg(long = "thread-id")]
        thread_id: String,
        /// include up to 3 sample messages
        #[arg(long)]
        examples: bool,
    },
    /// resolve an agent's profile (npub, program, model, task)
    Whois {
        /// agent (adjective+noun or terminal short name)
        agent: String,
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
    },
    /// register/refresh an agent identity (upsert). One operator is one
    /// identity (bd-register-portability-3c2b): a name is REQUIRED — bind an
    /// existing key with --from-project/--adopt-unnamed instead of minting.
    Register {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// agent name (adjective+noun pin; REQUIRED unless importing)
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "flywheel")]
        program: String,
        #[arg(long, default_value = "flywheel")]
        model: String,
        /// short task description shown in listings
        #[arg(long)]
        task: Option<String>,
        /// bind this operator's EXISTING key from another project's registry
        /// (`{data}/{slug}/agents/<name>/key.ncryptsec`) instead of minting
        /// a second npub
        #[arg(long)]
        from_project: Option<String>,
        /// adopt a prior unnamed mint (`agents/key.ncryptsec`) under this
        /// name — moves the key so register reuses its npub
        #[arg(long)]
        adopt_unnamed: Option<String>,
    },
    /// renew this identity's file reservations (extend the TTL)
    Renew {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// seconds to extend from the current expiry (default 3600)
        #[arg(long, default_value = "3600")]
        extend_seconds: u64,
        /// restrict to these path patterns (default: all of this identity's)
        #[arg(long)]
        paths: Vec<String>,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// operator force-release of a stale reservation (hc-down / abandoned)
    ForceUnlock {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// path (or glob) of the stale reservation to revoke — the daemon
        /// resolves the active lease on this path itself (bd-cumj)
        #[arg(long = "paths", value_name = "PATH")]
        path: String,
        /// holder agent name to disambiguate (optional)
        #[arg(long)]
        holder: Option<String>,
        /// optional note for the released holder
        #[arg(long)]
        note: Option<String>,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// build slots: acquire/renew/release the advisory 'cargo' slot
    Slot {
        #[command(subcommand)]
        cmd: MailSlotCmd,
    },
    /// set this agent's inbound contact policy
    Policy {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// policy: open|auto|contacts_only|block_all
        #[arg(long)]
        policy: String,
        /// agent whose policy to set (default: config identity)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// per-agent mail digest: unread/urgent counts + last sender/subject
    /// (orchestrator "who has mail" view; default = own identity)
    Digest {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// digest every registered agent (table, --json = array)
        #[arg(long)]
        all: bool,
        /// product-scoped captain digest (bd-jcqq): fan one digest per
        /// member project with loud per-member gaps; --project is ignored
        #[arg(long, value_name = "PRODUCT")]
        product: Option<String>,
    },
    /// wait until a thread gains a new message (blocks; Ctrl-C to abort)
    Watch {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// thread id or message id to watch
        #[arg(long = "thread")]
        thread_id: String,
        /// act as this registered agent (default: config identity)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// seconds to wait before giving up (default 300; 0 = forever)
        #[arg(long, default_value = "300")]
        timeout_secs: u64,
        /// poll interval seconds (default 5)
        #[arg(long, default_value = "5")]
        interval_secs: u64,
    },
    /// send a message to registered agents
    Send {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// recipients: comma-separated pins/terminal shorts; REQUIRED unless
        /// --crew expands them for you (bd-mail-crew-fanout-xq6b)
        #[arg(long)]
        to: Option<String>,
        /// bd-mail-crew-fanout-xq6b: expand the project spawn registry into
        /// recipients — every live crew pin gets one addressed copy (sender
        /// excluded, deduped, --to entries unioned on top). The broadcast-
        /// free alternative toron's rejection error names.
        #[arg(long = "crew")]
        crew: bool,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long, default_value = "normal")]
        importance: String,
        #[arg(long)]
        ack: bool,
        /// act as this agent (required for crew peer mail; else CoralFinch/config)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// thread id (use bead id for collab threads)
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
        /// GS-039 P0-1: wake tag on the gift-wrap (`standing`|`tasked`).
        /// Default `tasked` so mail deliveries are not wake:null theater.
        #[arg(long = "wake", default_value = "tasked")]
        wake: String,
        /// ADR-0010 D9 (bd-l0fz, bd-7qg6): stamp the structural done-hold
        /// (`bead=<id>` extra tag). The loop's dispatch guard matches the TAG,
        /// never the `[bd-###] done` subject — a done subject without this tag
        /// never fires the hold. In-process mailbox only (refuses loud remote).
        #[arg(long = "resolve-bead")]
        resolve_bead: Option<String>,
    },
    /// reserve / inspect file locks (advisory; subcommands: reserve, list, show)
    Locks {
        #[command(subcommand)]
        cmd: MailLockCmd,
    },
    /// release this identity's reservations
    Unlock {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// check reservation conflicts for paths
    Conflicts {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        paths: Vec<String>,
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// list registered agents in the project (active within 24h by default;
    /// --all shows the full history incl. stale registrations)
    Agents {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// include ALL registered identities (history), not just active ones
        #[arg(long)]
        all: bool,
        /// list key.ncryptsec dirs on disk that are not in the registry
        /// (whois-invisible). stdout becomes
        /// `{registered, unregistered_on_disk}` instead of a bare array.
        #[arg(long)]
        disk: bool,
    },
    /// cross-project contact (request_contact / respond / list)
    Contact {
        #[command(subcommand)]
        cmd: MailContactCmd,
    },
    /// ADR-0010 D8 (bd-d8e4): corr-preserving resend. Re-runs the send under
    /// the SAME corr — toron's idempotent claim dedupes within TTL and the
    /// receipt stays corr-keyed. Refuses resulted (resend would duplicate)
    /// and degraded (bookkeeping failed after enqueue — likely delivered).
    /// A plain re-run of `mail send` mints a NEW corr and duplicates.
    Resend {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// the corr stamped on the original send (from receipts / the
        /// transport-unknown print)
        #[arg(long)]
        corr: String,
        /// recipients: comma-separated pins (the original delivery set)
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        /// act as this agent (the original sender pin)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// product grouping for multi-project search/inbox
    Product {
        #[command(subcommand)]
        cmd: MailProductCmd,
    },
}

pub(crate) fn cmd_mail(cfg: &Config, cmd: &MailCmd) -> Result<()> {
    match cmd {
        MailCmd::Inbox {
            project,
            project_flag,
            urgent,
            unread,
            limit,
            since,
            thread_id,
            as_agent,
            json,
        } => {
            let project = mail_project(project, project_flag, "inbox")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            if mail.whois(&key, &name).is_err() {
                anyhow::bail!(
                    "mail identity {name} is not registered for {project} — register it \
                     first: flywheel mail register {project} --name <adjnoun-pin>"
                );
            }
            let v = match inbox_cli_filter(
                *urgent,
                *unread,
                since.as_deref(),
                thread_id.as_deref(),
                *limit,
            ) {
                Some(f) => mail.inbox_filtered(&key, &name, &f)?,
                None => mail.inbox(&key, &name, *urgent, *limit)?,
            };
            print_json(&if *json {
                crate::mail::inbox_json_rows(&v)
            } else {
                v
            });
        }
        MailCmd::Reply {
            project,
            project_flag,
            message_id,
            body,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "reply")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let v = mail.reply(&key, message_id, body, None)?;
            print_json(&v);
        }
        MailCmd::Read {
            project,
            project_flag,
            message_id,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "read")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.mark_read(&key, &name, message_id)?;
            print_json(&v);
        }
        MailCmd::Ack {
            project,
            project_flag,
            message_id,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "ack")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.ack(&key, &name, message_id)?;
            print_json(&v);
        }
        MailCmd::Search {
            project,
            project_flag,
            query,
            limit,
            sender,
            importance,
            thread_id,
            recency,
            bodies,
        } => {
            let project = mail_project(project, project_flag, "search")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let v = mail.search(
                &key,
                query,
                *limit,
                &crate::mail::MailSearchFilter {
                    sender: sender.clone(),
                    importance: importance.clone(),
                    thread_id: thread_id.clone(),
                    ranking: Some(if *recency { "recency" } else { "relevance" }.to_string()),
                    include_body_md: *bodies,
                    ..Default::default()
                },
            )?;
            print_json(&v);
        }
        MailCmd::Summarize {
            project,
            project_flag,
            thread_id,
            examples,
        } => {
            let project = mail_project(project, project_flag, "summarize")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let v = mail.summarize_thread(&key, thread_id, *examples)?;
            print_json(&v);
        }
        MailCmd::Whois {
            project,
            project_flag,
            agent,
        } => {
            let project = mail_project(project, project_flag, "whois")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let v = mail.whois(&key, agent)?;
            print_json(&v);
        }
        MailCmd::Register {
            project,
            project_flag,
            name,
            program,
            model,
            task,
            from_project,
            adopt_unnamed,
        } => {
            let project = mail_project(project, project_flag, "register")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let mut decided = crate::resolve_register_name(name.clone(), from_project.as_deref())?;
            if let Some(adopt) = adopt_unnamed.as_deref() {
                let (data_dir, _) = crate::mail::mailbox_roots();
                crate::adopt_unnamed_key(&data_dir, project, adopt)?;
                if decided.is_empty() {
                    decided = adopt.to_string();
                }
            }
            if decided.is_empty() {
                anyhow::bail!(
                    "--from-project needs --name <pin> — the pin selects WHICH of that \
                     project's identities binds here"
                );
            }
            if let Some(from) = from_project.as_deref()
                && from != project
            {
                let (data_dir, _) = crate::mail::mailbox_roots();
                crate::bind_source_key(&data_dir, from, project, &decided)?;
            }
            let v = mail.register_agent(
                &key,
                &decided,
                program,
                model,
                task.as_deref().unwrap_or("flywheel mail CLI actor"),
            )?;
            print_json(&v);
        }
        MailCmd::Renew {
            project,
            project_flag,
            extend_seconds,
            paths,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "renew")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.renew(&key, &name, *extend_seconds, Some(paths), None)?;
            print_json(&v);
        }
        MailCmd::ForceUnlock {
            project,
            project_flag,
            path,
            holder,
            note,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "force-unlock")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.force_release_file_reservation(
                &key,
                &name,
                path,
                holder.as_deref(),
                note.as_deref(),
                false,
            )?;
            print_json(&v);
        }
        MailCmd::Slot { cmd } => match cmd {
            MailSlotCmd::Acquire {
                project,
                project_flag,
                slot,
                exclusive,
                ttl_seconds,
                as_agent,
            } => {
                let project = mail_project(project, project_flag, "slot-acquire")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                let v = mail.acquire_slot(&key, &name, slot, *exclusive, *ttl_seconds)?;
                print_json(&v);
            }
            MailSlotCmd::Renew {
                project,
                project_flag,
                slot,
                extend_seconds,
                as_agent,
            } => {
                let project = mail_project(project, project_flag, "slot-renew")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                let v = mail.renew_slot(&key, &name, slot, *extend_seconds)?;
                print_json(&v);
            }
            MailSlotCmd::Release {
                project,
                project_flag,
                slot,
                as_agent,
            } => {
                let project = mail_project(project, project_flag, "slot-release")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                let v = mail.release_slot(&key, &name, slot)?;
                print_json(&v);
            }
        },
        MailCmd::Policy {
            project,
            project_flag,
            policy,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "policy")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.set_contact_policy(&key, &name, policy)?;
            print_json(&v);
        }
        MailCmd::Digest {
            project,
            project_flag,
            as_agent,
            all,
            product,
        } => {
            if let Some(pkey) = product {
                // governed-by: bd-jcqq — product fan-out lives in
                let probe = crate::mail_or_die(cfg)?;
                let members: Vec<String> = probe
                    .ensure_product(pkey, None)?
                    .get("member_projects")
                    .and_then(|m| m.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mail = match members.first() {
                    Some(first) => mail_as(
                        cfg,
                        &crate::project_dir_str(cfg, first)?,
                        as_agent.as_deref(),
                    )?,
                    None => match as_agent.as_deref() {
                        Some(a) if !a.trim().is_empty() => {
                            crate::mail_or_die(cfg)?.with_name(a.trim().to_string())
                        }
                        _ => crate::mail_or_die(cfg)?,
                    },
                };
                let mut dirs = Vec::new();
                for m in &members {
                    dirs.push((m.clone(), crate::project_dir_str(cfg, m)?));
                }
                let name = mail.name.clone();
                print_json(&product_digest_for(&mail, pkey, &name, &dirs));
                return Ok(());
            }
            let project = mail_project(project, project_flag, "digest")?;
            let key = crate::project_dir_str(cfg, project)?;
            if *all {
                let mail = crate::mail_or_die(cfg)?;
                mail.ensure_project(&key)?;
                let agents = mail.list_agents(&key, None)?;
                let mut out = Vec::new();
                let mut seen_identities = std::collections::HashSet::new();
                if let Some(arr) = agents.as_array() {
                    for a in arr {
                        let n = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        if let Ok(d) = digest_for(&mail, &key, n) {
                            let id = d.get("mail_identity").and_then(|v| v.as_str());
                            if id.is_some_and(|i| !seen_identities.insert(i.to_string())) {
                                continue;
                            }
                            out.push(d);
                        }
                    }
                }
                print_json(&json!(out));
            } else {
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                print_json(&digest_for(&mail, &key, &name)?);
            }
        }
        MailCmd::Watch {
            project,
            project_flag,
            thread_id,
            as_agent,
            timeout_secs,
            interval_secs,
        } => {
            let project = mail_project(project, project_flag, "watch")?;
            cmd_mail_watch(
                cfg,
                project,
                thread_id,
                as_agent.as_deref(),
                *timeout_secs,
                *interval_secs,
                None,
            )?;
        }
        MailCmd::Send {
            project,
            project_flag,
            to,
            crew,
            subject,
            body,
            importance,
            ack,
            as_agent,
            thread_id,
            wake,
            resolve_bead,
        } => {
            let project = mail_project(project, project_flag, "send")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let mut targets: Vec<String> = Vec::new();
            if *crew {
                let entries = crate::state::load_spawn_registry(project);
                if entries.is_empty() {
                    anyhow::bail!(
                        "mail send --crew: no live crew in the {project} spawn registry — \
                         spawn a swarm first (flywheel spawn) or pass explicit --to"
                    );
                }
                let sender = Some(mail.name.as_str());
                for r in crate::state::crew_recipients(&entries, sender) {
                    if !targets.contains(&r) {
                        targets.push(r);
                    }
                }
            }
            match to.as_deref() {
                Some(tos) => {
                    for s in tos.split(',') {
                        let t = crate::resolve_mail_target_registry(project, s.trim());
                        if !targets.contains(&t) {
                            targets.push(t);
                        }
                    }
                }
                None if !*crew => anyhow::bail!(
                    "mail send: pass --to <recipients> or --crew (spawn-registry fan-out)"
                ),
                None => {}
            }
            for t in &targets {
                if t.eq_ignore_ascii_case("captain") {
                    crate::state::resolve_captain_target(project, t)?;
                }
            }
            {
                let has_p = !targets.is_empty();
                if let Err(e) = crate::plane_route::check_job_tripwire(
                    body, has_p, false, false, true, None, false, false,
                ) {
                    anyhow::bail!(e);
                }
            }
            for line in crate::send_liveness_warnings(cfg, project, &targets) {
                eprintln!("{line}");
            }
            let v = mail.send_with_wake(
                &key,
                &targets,
                subject,
                body,
                importance,
                *ack,
                thread_id.as_deref(),
                Some(wake.as_str()),
                None,
                resolve_bead.as_deref(),
            )?;
            print_json(&v);
        }
        MailCmd::Resend {
            project,
            project_flag,
            corr,
            to,
            subject,
            body,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "resend")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let receipt = crate::mail::block_on(async {
                let mb = crate::mail::try_mailbox()
                    .ok_or_else(|| anyhow::anyhow!("no in-process toron mailbox"))?;
                let store = mb
                    .receipt_store()
                    .map_err(|e| anyhow::anyhow!("receipts store unavailable: {e}"))?;
                store.get(corr.trim()).await
            })?;
            match receipt {
                None => anyhow::bail!(
                    "no receipt for corr {corr} — nothing to correlate; a fresh send is a NEW corr"
                ),
                Some(r) if r.state == toron_core::mailbox::ReceiptState::Resulted => {
                    anyhow::bail!(
                        "corr {corr} already resulted — do NOT resend (a resend would duplicate)"
                    );
                }
                Some(r) if r.state == toron_core::mailbox::ReceiptState::Degraded => {
                    anyhow::bail!("{}", toron_core::mailbox::receipts::DEGRADED_NOTE);
                }
                Some(r) => {
                    let wake = r
                        .wake
                        .map(|w| w.as_str().to_string())
                        .unwrap_or_else(|| "tasked".to_string());
                    let targets: Vec<String> = to
                        .split(',')
                        .map(|s| crate::resolve_mail_target_registry(project, s.trim()))
                        .collect();
                    let v = mail.send_with_wake(
                        &key,
                        &targets,
                        subject,
                        body,
                        "normal",
                        false,
                        None,
                        Some(&wake),
                        Some(corr.trim()),
                        None,
                    )?;
                    print_json(&v);
                }
            }
        }
        MailCmd::Locks { cmd } => match cmd {
            MailLockCmd::Reserve {
                project,
                project_flag,
                paths,
                ttl_seconds,
                exclusive,
                reason,
                as_agent,
            } => {
                let project = mail_project(project, project_flag, "locks-reserve")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let agent = mail.name.clone();
                let v = mail.reserve(&key, &agent, paths, *ttl_seconds, *exclusive, reason)?;
                print_json(&v);
            }
            MailLockCmd::List {
                project,
                project_flag,
                holder,
            } => {
                let project = mail_project(project, project_flag, "locks-list")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_or_die(cfg)?;
                let v = mail.list_active(&key, holder.as_deref(), None)?;
                print_json(&v);
            }
            MailLockCmd::Show {
                project,
                project_flag,
                path,
            } => {
                let project = mail_project(project, project_flag, "locks-show")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_or_die(cfg)?;
                let v = mail.list_active(&key, None, Some(path))?;
                print_json(&v);
            }
        },
        MailCmd::Unlock {
            project,
            project_flag,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "unlock")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            let agent = mail.name.clone();
            let v = mail.release_paths(&key, &agent, &[])?;
            print_json(&v);
        }
        MailCmd::Conflicts {
            project,
            project_flag,
            paths,
            as_agent,
        } => {
            let project = mail_project(project, project_flag, "conflicts")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let agent = mail.name.clone();
            let v = mail.conflicts_for(&key, &agent, paths)?;
            print_json(&v);
        }
        MailCmd::Agents {
            project,
            project_flag,
            all,
            disk,
        } => {
            let project = mail_project(project, project_flag, "agents")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let active_within = if *all { None } else { Some(1) };
            let v = mail.list_agents(&key, active_within)?;
            let registered: std::collections::HashSet<String> = v
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let unreg = unregistered_disk_pins(&project_agent_keys_dir(project), &registered);
            if !unreg.is_empty() {
                eprintln!(
                    "flywheel: {} agent key(s) on disk are not in the registry (whois will miss them). `flywheel mail agents {project} --disk` lists names; register with `flywheel mail register` or prune stale dirs.",
                    unreg.len()
                );
            }
            if *disk {
                print_json(&json!({
                    "registered": v,
                    "unregistered_on_disk": unreg,
                }));
            } else {
                print_json(&v);
            }
        }
        MailCmd::Contact { cmd } => match cmd {
            MailContactCmd::Request {
                project,
                project_flag,
                to,
                to_project,
                reason,
            } => {
                let project = mail_project(project, project_flag, "contact-request")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                mail.ensure_project(&key)?;
                let from = mail.name.clone();
                let to_mail = crate::resolve_mail_target_registry(project, to);
                let to_proj = to_project
                    .as_ref()
                    .map(|p| crate::project_dir_str(cfg, p))
                    .transpose()?;
                let v =
                    mail.request_contact(&key, &from, &to_mail, to_proj.as_deref(), reason, None)?;
                print_json(&v);
            }
            MailContactCmd::Approve {
                project,
                project_flag,
                from,
            } => {
                let project = mail_project(project, project_flag, "contact-approve")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                let to = mail.name.clone();
                let from_mail = crate::resolve_mail_target_registry(project, from);
                let v = mail.respond_contact(&key, &to, &from_mail, true)?;
                print_json(&v);
            }
            MailContactCmd::Deny {
                project,
                project_flag,
                from,
            } => {
                let project = mail_project(project, project_flag, "contact-deny")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                let to = mail.name.clone();
                let from_mail = crate::resolve_mail_target_registry(project, from);
                let v = mail.respond_contact(&key, &to, &from_mail, false)?;
                print_json(&v);
            }
            MailContactCmd::List {
                project,
                project_flag,
            } => {
                let project = mail_project(project, project_flag, "contact-list")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                let name = mail.name.clone();
                let v = mail.list_contacts(&key, &name)?;
                print_json(&v);
            }
            MailContactCmd::Policy {
                project,
                project_flag,
                policy,
            } => {
                let project = mail_project(project, project_flag, "contact-policy")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                let name = mail.name.clone();
                let v = mail.set_contact_policy(&key, &name, policy)?;
                print_json(&v);
            }
        },
        MailCmd::Product { cmd } => match cmd {
            MailProductCmd::Ensure { name } => {
                let mail = crate::mail_or_die(cfg)?;
                let v = mail.ensure_product(name, Some(name))?;
                print_json(&v);
            }
            MailProductCmd::Link {
                product,
                project,
                project_flag,
            } => {
                let project = mail_project(project, project_flag, "product-link")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_or_die(cfg)?;
                mail.ensure_project(&key)?;
                let v = mail.products_link(product, &key)?;
                print_json(&v);
            }
            MailProductCmd::Search {
                product,
                query,
                limit,
            } => {
                let mail = crate::mail_or_die(cfg)?;
                let v = mail.search_product(product, query, *limit)?;
                print_json(&v);
            }
            MailProductCmd::Inbox { product, limit } => {
                let mail = crate::mail_or_die(cfg)?;
                let name = mail.name.clone();
                let v = mail.inbox_product(product, &name, *limit)?;
                print_json(&v);
            }
        },
    }
    Ok(())
}

/// Mailbox `{data}/projects/<slug>/agents` for a project name or path.
pub(crate) fn project_agent_keys_dir(project: &str) -> std::path::PathBuf {
    let slug = std::path::Path::new(project)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(project);
    crate::mail::mailbox_roots().0.join(slug).join("agents")
}

/// Agent dirs that hold `key.ncryptsec` but are not in the registered roster.
/// Disk is not whois; this is the loud gap (141 keys vs 2 registry rows).
pub(crate) fn unregistered_disk_pins(
    agents_dir: &std::path::Path,
    registered: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(agents_dir) else {
        return out;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        if ent.path().join("key.ncryptsec").is_file() && !registered.contains(name) {
            out.push(name.to_string());
        }
    }
    out.sort();
    out
}

/// Build the filtered-inbox filter for `mail inbox` (bd-ursy): every
/// filtered arm carries the CLI limit — a default 0 limit reads as take(0)
/// downstream and yields a silent []. `None` selects the legacy path.
fn inbox_cli_filter(
    urgent: bool,
    unread: bool,
    since: Option<&str>,
    thread_id: Option<&str>,
    limit: u32,
) -> Option<mail::MailInboxFilter> {
    match (since, unread, thread_id) {
        (Some(ts), _, _) => Some(mail::MailInboxFilter {
            urgent_only: urgent,
            unread_only: unread,
            include_bodies: true,
            limit,
            since_ts: Some(ts.to_string()),
            thread_id: thread_id.map(str::to_string),
            ..Default::default()
        }),
        (None, true, _) => Some(mail::MailInboxFilter {
            unread_only: true,
            include_bodies: true,
            limit,
            thread_id: thread_id.map(str::to_string),
            ..Default::default()
        }),
        (None, false, Some(t)) => Some(mail::MailInboxFilter {
            include_bodies: true,
            limit,
            thread_id: Some(t.to_string()),
            ..Default::default()
        }),
        (None, false, None) => None,
    }
}

#[cfg(test)]
mod inbox_filter_tests {
    use super::*;

    /// bd-ursy: unread-only must carry the CLI limit (default 0 = take(0)).
    #[test]
    fn unread_only_carries_limit() {
        let f = inbox_cli_filter(false, true, None, None, 10).expect("filtered");
        assert!(f.unread_only);
        assert_eq!(f.limit, 10, "limit rides every filtered arm");
        assert!(inbox_cli_filter(false, false, None, None, 10).is_none());
        let s = inbox_cli_filter(true, false, Some("x"), None, 7).expect("since");
        assert_eq!(s.limit, 7);
        assert!(s.urgent_only);
    }

    /// bd-44hw: a bare --thread-id rides the filtered surface too.
    #[test]
    fn thread_only_rides_filtered_surface() {
        let f = inbox_cli_filter(false, false, None, Some("bd-1"), 20).expect("filtered");
        assert_eq!(f.thread_id.as_deref(), Some("bd-1"));
        assert_eq!(f.limit, 20);
        let g = inbox_cli_filter(true, true, Some("2026-01-01T00:00:00Z"), Some("bd-2"), 5)
            .expect("filtered");
        assert_eq!(g.thread_id.as_deref(), Some("bd-2"));
        assert!(g.urgent_only && g.unread_only);
    }
}

pub(crate) fn mail_or_die(cfg: &Config) -> Result<mail::Mail> {
    mail::Mail::from_config(Some(&cfg.toron_mail_config()))?.ok_or_else(|| {
        anyhow::anyhow!(
            "toron mail not configured — set [mail.toron] url in {}",
            crate::config::config_file().display()
        )
    })
}

/// Who this CLI mail invocation acts as — the ONE selection+resolution
/// ladder (bd-mail-actor-identity-nze, bd-reservation-identity-tlnf,
/// bd-2mdo).
///
/// The selection half lives in [`crate::authority::select_agent_identity`] —
/// explicit `--as` → `TORON_AGENT` → `FLYWHEEL_MAIL_AS` → `AGENT_NAME` →
/// config `[agent_mail].name`, the env ladder mirroring toron's
/// `guard_agent` attribution order exactly (TORON_AGENT > FLYWHEEL_MAIL_AS >
/// AGENT_NAME). One ladder, one implementation: two orders was the seam where
/// leases landed under one identity and commits were attributed to another.
/// A selected token naming a REGISTERED mailbox identity passes through
/// verbatim; only unregistered tokens defer to the spawn-registry → hash
/// resolver.
pub(crate) fn resolve_mail_actor_roster(
    project: &str,
    explicit: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    roster: &[String],
) -> Option<String> {
    let token = crate::authority::select_agent_identity(explicit, env)?;
    if roster.iter().any(|n| n == &token) {
        return Some(token);
    }
    Some(crate::resolve_mail_target_registry(project, &token))
}

/// Mail facade acting as resolved actor (registers identity best-effort).
pub(crate) fn mail_as(
    cfg: &Config,
    project_key: &str,
    as_agent: Option<&str>,
) -> Result<mail::Mail> {
    let mail = mail_or_die(cfg)?;
    let base = crate::project_base(project_key);
    let roster: Vec<String> = mail
        .list_agents(project_key, None)
        .ok()
        .map(|v| {
            v.as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|a| a.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mail = match resolve_mail_actor_roster(&base, as_agent, &|k| std::env::var(k).ok(), &roster)
    {
        Some(name) => mail.with_name(name),
        None => mail,
    };
    let name = mail.name.clone();
    if let Err(e) = mail.register_agent(
        project_key,
        &name,
        "flywheel",
        "flywheel",
        "flywheel mail CLI actor",
    ) {
        eprintln!("flywheel: mail self-register as {name} failed (non-fatal): {e:#}");
    }
    Ok(mail)
}

/// Flag wins when both are given; neither fails loud naming the subcommand.
/// Total over all four cases — a missing project is never an empty string.
/// bd-empty-project-silently-retargets-workspace-bkr3: a blank WINNING
/// spelling ("" or whitespace) fails loud with the same actionable message
/// — it used to slip through and `project_dir(cfg, "")` aliased it onto the
/// workspace root, silently retargeting the door to a different project.
/// No silent fallback to the other spelling either: that would be a second
/// silent correction of operator-shaped input.
pub(crate) fn mail_project<'a>(
    positional: &'a Option<String>,
    flag: &'a Option<String>,
    sub: &str,
) -> Result<&'a str> {
    match (positional, flag) {
        (_, Some(f)) if !f.trim().is_empty() => Ok(f.as_str()),
        (Some(p), None) if !p.trim().is_empty() => Ok(p.as_str()),
        _ => anyhow::bail!(
            "mail {sub} requires a project: pass PROJECT positionally or --project <slug>"
        ),
    }
}

/// Per-agent digest from the live mailbox (mirror of the `__mail-digest`
/// leaf, surfaced for the orchestrator). Reads `sender`/`from` aliases and
/// counts high|urgent as urgent.
///
/// NOTE on `unread`: toron's projection HAS per-recipient read-state since
/// bd-5icg (`read_ts_us` on every row; `unread_only` filters). The digest
/// still reports the RECENT inbox (limit 10) with an honest label:
/// `recent` = messages in the projection window, `urgent` = the
/// [`wake_urgent`]-filtered high|urgent count among them (S8.2 + bd-dbre:
/// read rows — assign or not — do not wake). The wake trigger uses
/// `urgent > 0`, which IS meaningful (importance is a real message
/// attribute).
pub(crate) fn digest_for(mail: &mail::Mail, key: &str, name: &str) -> Result<Value> {
    let identity = {
        let reg = state::load_spawn_registry(key);
        let names = state::load_agent_names(std::path::Path::new(key));
        wake_identity_pin(&reg, &names, name)
    };
    let v = mail.inbox_filtered(
        key,
        &identity,
        &mail::MailInboxFilter {
            unread_only: false,
            include_bodies: true,
            limit: 10,
            ..Default::default()
        },
    )?;
    let arr = v.as_array().cloned().unwrap_or_default();
    let recent = arr.len();
    let urgent_rows = wake_urgent(&arr);
    let urgent = urgent_rows.len();
    let last = arr.first();
    let chosen = newest_urgent(&urgent_rows, last);
    let last_from = last.and_then(|m| {
        m.get("sender")
            .or_else(|| m.get("from"))
            .and_then(|f| f.as_str())
    });
    Ok(json!({
        "agent": name,
        "mail_identity": identity,
        "recent": recent,
        "urgent": urgent,
        "last_id": chosen.and_then(|m| {
            m.get("id").or_else(|| m.get("message_id")).and_then(|i| i.as_str())
        }),
        "last_from": last_from,
        "last_subject": chosen.and_then(|m| m.get("subject").and_then(|s| s.as_str())),
        "thread_id": chosen.and_then(|m| m.get("thread_id").and_then(|t| t.as_str())),
    }))
}

/// governed-by: bd-jcqq — product-scoped captain digest. Fans one digest
/// per member project (read-only aggregation, one high-water per member
/// via each member's own inbox water), with unreadable members as loud
/// gap rows — never a silent skip, never an abort that hides the readable
/// members. Unknown/empty products render a gap, not an empty success.
///
/// Identity fold: before reading member M as `name`, bind the pin's
/// EXISTING key into M's slot from the first member slot that holds it
/// ([`bind_source_key`], never a mint). A pin known nowhere renders gaps
/// everywhere and mints nothing.
///
/// `member_dirs`: (slug, registry dir-key) pairs; the CLI resolves dirs
/// via `project_dir_str`, tests pass slugs (no registry → name fallback).
pub(crate) fn product_digest_for(
    mail: &mail::Mail,
    product_key: &str,
    name: &str,
    member_dirs: &[(String, String)],
) -> Value {
    let mut members_out = Vec::new();
    let mut gaps_out = Vec::new();
    let gap = |project: &str, reason: String| json!({ "project": project, "gap": reason });
    let slugs: Vec<String> = match mail.ensure_product(product_key, None) {
        Ok(v) => v
            .get("member_projects")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        Err(e) => {
            gaps_out.push(gap(product_key, format!("product unreadable: {e:#}")));
            return json!({ "product": product_key, "agent": name, "members": members_out, "gaps": gaps_out });
        }
    };
    if slugs.is_empty() {
        gaps_out.push(gap(
            product_key,
            "unknown product or no member projects — link members with `toron products-link` first"
                .to_string(),
        ));
        return json!({ "product": product_key, "agent": name, "members": members_out, "gaps": gaps_out });
    }
    let dirs: std::collections::BTreeMap<&str, &str> = member_dirs
        .iter()
        .map(|(s, d)| (s.as_str(), d.as_str()))
        .collect();
    let (data_dir, _) = mail::built_mailbox_roots();
    for slug in &slugs {
        let Some(dir) = dirs.get(slug.as_str()) else {
            gaps_out.push(gap(slug, "no project dir supplied for member".to_string()));
            continue;
        };
        if mail_key_present(&data_dir, slug, name).is_none() {
            let donor = slugs
                .iter()
                .find(|s| mail_key_present(&data_dir, s, name).is_some());
            match donor {
                Some(from) => {
                    if let Err(e) = crate::bind_source_key(&data_dir, from, slug, name) {
                        gaps_out.push(gap(slug, format!("identity fold failed: {e:#}")));
                        continue;
                    }
                }
                None => {
                    gaps_out.push(gap(
                        slug,
                        format!(
                            "no key for `{name}` in any member — register it in one member first \
                             (`flywheel mail register <member> --name {name}`); refusing to mint"
                        ),
                    ));
                    continue;
                }
            }
        }
        match digest_for(mail, dir, name) {
            Ok(mut row) => {
                row["project"] = json!(slug);
                members_out.push(row);
            }
            Err(e) => gaps_out.push(gap(slug, format!("digest failed: {e:#}"))),
        }
    }
    json!({ "product": product_key, "agent": name, "members": members_out, "gaps": gaps_out })
}

/// Key-file presence probe for the identity fold (bd-jcqq): `Some` when
/// `{data}/{slug}/agents/{name}/key.ncryptsec` (or the legacy layout)
/// exists. Read-only — never creates.
pub(crate) fn mail_key_present(
    data_dir: &std::path::Path,
    slug: &str,
    name: &str,
) -> Option<std::path::PathBuf> {
    crate::import_source_key(data_dir, slug, name)
}

/// S8.1: the wake identity for an hc agent name. The spawn-registry /
/// agent-names pin (pane → name → mail identity) wins; otherwise the
/// canonical name itself — the identity flywheel registers at spawn and the
/// wake backstop registers idempotently BEFORE instructing fetch. Never the
/// legacy adjective+noun hash: a wake must name an identity that exists (or
/// is about to be registered), or the "fetch as X" instruction 404s
/// (debrief D1/D2 — "fetch as flywheel-toron-oc4" for a never-registered
/// name).
pub(crate) fn wake_identity_pin(
    reg: &[state::SpawnEntry],
    names: &[state::SpawnEntry],
    name: &str,
) -> String {
    for e in reg.iter().chain(names.iter()) {
        if e.name == name {
            if let Some(mid) = e.mail_identity.as_deref().filter(|m| !m.is_empty()) {
                return mid.to_string();
            }
            break;
        }
    }
    name.to_string()
}

/// S8.2 + bd-dbre: which urgent inbox rows legitimately wake an agent.
///
/// ANY row the agent already read (`read_ts_us` set — bd-5icg projection)
/// does not wake; per-recipient read state is the once-only signal.
/// Assign messages (`[assign]` subjects) additionally wake only when
/// not-yet-executed (no in-thread reply newer than the assign). Identical
/// (sender, subject, body) assigns within the window collapse to the first
/// — the duplicate-assign double-wake (debrief D1: two identical assigns
/// both woke the agent).
///
/// Pure + deterministic; the digest paths (loop wake + operator `mail
/// digest`) share this one filter.
/// bd-vbf2: the wake key is the NEWEST urgent row by `created_ts_us` —
/// the wire may deliver rows oldest-first, so `.first()` cited stale ids
/// and misdirected the mandated reply to the wrong thread. Deterministic
/// fallback: the first raw row when nothing urgent survives the filter.
///
/// bd-9w95: the timestamp alone leaves a tie to iteration order, and the point
/// of this function is that the wire order is NOT trusted. A burst can land two
/// urgent rows in the same microsecond; the event id breaks it, the same way a
/// Nostr timestamp breaks a same-second tie.
pub(crate) fn newest_urgent<'a>(
    urgent: &[&'a serde_json::Value],
    fallback: Option<&'a serde_json::Value>,
) -> Option<&'a serde_json::Value> {
    urgent
        .iter()
        .copied()
        .max_by_key(|m| {
            (
                m.get("created_ts_us")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                m.get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
            )
        })
        .or(fallback)
}

pub(crate) fn wake_urgent(rows: &[Value]) -> Vec<&Value> {
    let urgent = |m: &Value| {
        m.get("importance")
            .and_then(|i| i.as_str())
            .map(|i| i == "high" || i == "urgent")
            .unwrap_or(false)
    };
    let is_transport_noise = |m: &Value| {
        let subj_empty = m
            .get("subject")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim()
            .is_empty();
        let body = m.get("body_md").and_then(|b| b.as_str()).unwrap_or("");
        subj_empty && body.trim_start().starts_with("reservation ") && body.contains("released")
    };
    let is_read = |m: &Value| m.get("read_ts_us").map(|v| !v.is_null()).unwrap_or(false);
    let is_assign = |m: &Value| {
        m.get("subject")
            .and_then(|s| s.as_str())
            .map(|s| s.starts_with("[assign]"))
            .unwrap_or(false)
    };
    let key_of = |m: &Value| {
        (
            m.get("sender")
                .or_else(|| m.get("from"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            m.get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            m.get("body_md")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        )
    };
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    let mut duplicate_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut deduped: Vec<&Value> = Vec::new();
    for m in rows {
        if is_transport_noise(m) {
            continue;
        }
        if !urgent(m) {
            continue;
        }
        if is_read(m) {
            continue;
        }
        if is_assign(m) && !seen.insert(key_of(m)) {
            if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                duplicate_ids.insert(id);
            }
            continue;
        }
        deduped.push(m);
    }
    let mut out = Vec::new();
    for m in &deduped {
        if !is_assign(m) {
            out.push(*m);
            continue;
        }
        if m.get("read_ts_us").map(|v| !v.is_null()).unwrap_or(false) {
            continue;
        }
        if thread_has_reply(rows, &duplicate_ids, m) {
            continue;
        }
        out.push(*m);
    }
    out
}

/// A later, non-duplicate message exists on the same thread as `m` (the
/// thread moved). `duplicate_ids` are the collapsed assign copies — they do
/// not count as replies.
fn thread_has_reply(
    rows: &[Value],
    duplicate_ids: &std::collections::HashSet<&str>,
    m: &Value,
) -> bool {
    let Some(tid) = m.get("thread_id").and_then(|t| t.as_str()) else {
        return false;
    };
    let Some(ts) = m.get("created_ts_us").and_then(serde_json::Value::as_i64) else {
        return false;
    };
    rows.iter().any(|r| {
        !std::ptr::eq(r, m)
            && !r
                .get("id")
                .and_then(|i| i.as_str())
                .map(|i| duplicate_ids.contains(i))
                .unwrap_or(false)
            && r.get("thread_id").and_then(|t| t.as_str()) == Some(tid)
            && r.get("created_ts_us")
                .and_then(serde_json::Value::as_i64)
                .map(|t| t > ts)
                .unwrap_or(false)
    })
}

/// Block until a thread gains a message newer than the current latest
/// (thread = a message id or a free-form thread id; search by thread_id).
/// Polls the inbox every `interval_secs`; prints the newest message and exits
/// 0 on the first NEW arrival, 1 on timeout.
///
/// `after_id` is the caller's own outbound message, when it has one. It is
/// then the baseline, because reading the thread head at startup would make a
/// reply that landed between the send and this watch into the starting point
/// and the wait would time out on a thread that was answered (bd-7hdb).
/// Standalone `mail watch` passes `None` and keeps head-at-start.
pub(crate) fn cmd_mail_watch(
    cfg: &Config,
    project: &str,
    thread_id: &str,
    as_agent: Option<&str>,
    timeout_secs: u64,
    interval_secs: u64,
    after_id: Option<&str>,
) -> Result<()> {
    let key = crate::project_dir_str(cfg, project)?;
    let mail = mail_as(cfg, &key, as_agent)?;
    mail.ensure_project(&key)?;
    let rows = |m: &mail::Mail| -> Result<Vec<Value>> {
        let v = m.inbox_filtered(&key, &m.name, &watch_filter(thread_id))?;
        Ok(v.as_array().cloned().unwrap_or_default())
    };
    let start = std::time::Instant::now();
    let (seen, _) = watch_progress(&rows(&mail)?, thread_id, after_id);
    let mut empty_noted = false;
    eprintln!(
        "flywheel: watching thread {thread_id} (baseline {}); Ctrl-C to abort",
        seen.as_deref().unwrap_or("<none>")
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(interval_secs));
        if timeout_secs > 0 && start.elapsed().as_secs() >= timeout_secs {
            eprintln!("flywheel: no new message in thread {thread_id} within {timeout_secs}s");
            std::process::exit(1);
        }
        let current = rows(&mail)?;
        let (_, arrival) = watch_progress(&current, thread_id, after_id);
        if let Some(m) = arrival {
            print_json(&json!({ "thread": thread_id, "new_message": m }));
            return Ok(());
        }
        if !current.iter().any(|row| in_thread(row, thread_id)) && !empty_noted {
            eprintln!("flywheel: thread {thread_id} has no messages yet (waiting for reply…)");
            empty_noted = true;
        }
    }
}

/// The inbox read a thread watch performs. Scoped to the thread so the store's
/// `limit` (which keeps the OLDEST rows of an ascending list) cannot hide the
/// very reply the watch exists to detect. Pure: one thread in, one filter out.
fn watch_filter(thread_id: &str) -> mail::MailInboxFilter {
    mail::MailInboxFilter {
        unread_only: false,
        include_bodies: true,
        limit: 200,
        thread_id: Some(thread_id.to_owned()),
        ..Default::default()
    }
}

/// Exact thread membership of one inbox row. FTS5 tokenizes on hyphens, so the
/// projection's `thread_id` is matched verbatim rather than phrase-searched.
fn in_thread(row: &Value, thread_id: &str) -> bool {
    row.get("thread_id")
        .and_then(|t| t.as_str())
        .map(|t| t == thread_id)
        .unwrap_or(false)
}

/// The baseline a watch starts from, and the message that already counts as a
/// new arrival. Pure: `rows` is the inbox projection, the thread match is
/// exact, and the newest matching row is the head.
///
/// `after` is the baseline when the caller supplies one (collab's outbound
/// ask). The head is then an arrival BY CONSTRUCTION — including a reply that
/// landed between the send and this watch, which the old head-at-startup
/// baseline swallowed and then timed out on (bd-7hdb). With `None` the head IS
/// the baseline, which is the standalone `mail watch` contract: messages that
/// were already there are not reported as new.
fn watch_progress(
    rows: &[Value],
    thread_id: &str,
    after: Option<&str>,
) -> (Option<String>, Option<Value>) {
    let head = rows
        .iter()
        .rev()
        .find(|row| in_thread(row, thread_id))
        .cloned();
    let head_id = head
        .as_ref()
        .and_then(|row| row.get("id"))
        .and_then(|i| i.as_str())
        .map(str::to_owned);
    match after {
        Some(ask) => {
            let arrival = head.filter(|row| {
                row.get("id")
                    .and_then(|i| i.as_str())
                    .is_some_and(|i| i != ask)
            });
            (Some(ask.to_owned()), arrival)
        }
        None => (head_id, None),
    }
}

/// Real-time collab primitive: send an urgent ask to a peer, then block for
/// the in-thread reply and print it. Combines `mail send` (urgent, ack) +
/// `mail watch` — the A-asks-B pattern as one command. Exit 0 on reply,
/// 1 on timeout.
pub(crate) fn cmd_collab(
    cfg: &Config,
    project: &str,
    to: &str,
    subject: &str,
    body: &str,
    timeout_secs: u64,
    as_agent: Option<&str>,
) -> Result<()> {
    let key = crate::project_dir_str(cfg, project)?;
    let mail = mail_as(cfg, &key, as_agent)?;
    mail.ensure_project(&key)?;
    let thread_id = format!(
        "collab-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%3f"),
        std::process::id()
    );
    let targets: Vec<String> = to
        .split(',')
        .map(|s| crate::resolve_mail_target_registry(project, s.trim()))
        .collect();
    let sent = mail.send(
        &key,
        &targets,
        subject,
        body,
        "urgent",
        true,
        Some(&thread_id),
    )?;
    let msg_id = sent
        .get("id")
        .or_else(|| sent.get("message_id"))
        .and_then(|i| i.as_str())
        .unwrap_or(&thread_id)
        .to_string();
    {
        let mut entries = state::load_expected_replies(project);
        let ts = crate::state::now_iso();
        let flag = crate::comms_inbox_on();
        for t in &targets {
            state::upsert_expected_reply(
                flag,
                &mut entries,
                state::ExpectedReply {
                    agent: t.clone(),
                    thread: thread_id.clone(),
                    head: Some(msg_id.clone()),
                    created_ts: ts.clone(),
                },
            );
        }
        if let Err(e) = state::save_expected_replies(project, &entries) {
            eprintln!("flywheel: cannot save expected-reply registry: {e:#}");
        }
    }
    eprintln!(
        "flywheel: asked {} (thread {thread_id}, msg {msg_id}); waiting up to {timeout_secs}s for the reply…",
        targets.join(", ")
    );
    cmd_mail_watch(
        cfg,
        project,
        &thread_id,
        as_agent,
        timeout_secs,
        5,
        Some(&msg_id),
    )
}

/// Verify the §3.8 repo-tier evidence bar against the durable run log +
/// the beads ledger, shared by `stage promote --to repo` and
/// `workflow promote --to repo`. Evidence = >=2 cited run ids that exist,
/// carry status=success, and cover >=2 distinct objectives (prompt hashes);
/// the review bead must exist. Returns the verified run count.
pub(crate) fn repo_evidence_verified(
    what: &str,
    evidence: &[String],
    review_bead: Option<&str>,
) -> Result<usize> {
    if evidence.len() < 2 {
        anyhow::bail!(
            "{what}: repo promotion needs --evidence <run-id1>,<run-id2> (>=2 successful runs, \
             distinct objectives) — the §3.8 evidence bar refuses unevidenced promotion"
        );
    }
    let runs = crate::run_log::read_runs();
    let mut hashes = std::collections::HashSet::new();
    for id in evidence {
        let entry = runs.iter().find(|r| &r.run_id == id).ok_or_else(|| {
            anyhow::anyhow!("{what}: evidence run {id:?} not found in the run log")
        })?;
        if entry.status != "success" {
            anyhow::bail!(
                "{what}: evidence run {id:?} status is {:?} — only success runs count",
                entry.status
            );
        }
        hashes.insert(entry.prompt_hash.clone());
    }
    if hashes.len() < 2 {
        anyhow::bail!(
            "{what}: the {} evidence runs cover {} objective(s) — need >=2 distinct objectives",
            evidence.len(),
            hashes.len()
        );
    }
    let bead = review_bead
        .ok_or_else(|| anyhow::anyhow!("{what}: repo promotion needs --review-bead <id>"))?;
    let ok = crate::br::Bead::exists(None, bead);
    if !ok {
        anyhow::bail!("{what}: review bead {bead:?} not found (br show {bead})");
    }
    if !crate::stages::check_repo_evidence(hashes.len(), true) {
        anyhow::bail!("{what}: evidence bar not met");
    }
    Ok(hashes.len())
}

fn run_workflow(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let workflows = crate::config::Workflows::load()?;
    let steps = workflows
        .workflows
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("workflow not found: {name} (flywheel workflows list)"))?;
    let mux = crate::backend::multiplexer_backend(cfg)?;
    let ws_id = crate::find_workspace_dyn(&*mux, project)?;
    for (i, step) in steps.iter().enumerate() {
        println!(
            "[{}/{}] {}",
            i + 1,
            steps.len(),
            crate::output::dim(&step_label(step))
        );
        match step {
            crate::config::WorkflowStep::Send { to, stage, msg } => {
                let text = match (stage.as_deref(), msg.as_deref()) {
                    (Some(s), _) => crate::stage_prompt(s)?,
                    (None, Some(m)) => m.to_string(),
                    (None, None) => anyhow::bail!("workflow step {i}: send needs stage or msg"),
                };
                let targets = crate::resolve_agents_dyn(&*mux, &ws_id, project, to)?;
                for a in &targets {
                    mux.prompt(crate::agent_target(a), &text, None)?;
                    println!("  {} sent", crate::output::dim(crate::agent_target(a)));
                }
            }
            crate::config::WorkflowStep::Wait { until, timeout_ms } => {
                let st = crate::hc::parse_agent_status(until)?;
                let targets = crate::resolve_agents_dyn(&*mux, &ws_id, project, "all")?;
                for a in &targets {
                    mux.agent_wait(
                        crate::agent_target(a),
                        &[st],
                        timeout_ms.or(Some(cfg.timeout_ms())),
                    )?;
                    println!(
                        "  {} {}",
                        crate::output::dim(crate::agent_target(a)),
                        crate::output::status_paint(st)
                    );
                }
            }
        }
    }
    crate::log(
        project,
        crate::state::EventKind::Workflow,
        json!({ "workflow": name, "status": "completed" }),
    );
    println!("workflow {name} complete");
    Ok(())
}

fn step_label(step: &crate::config::WorkflowStep) -> String {
    match step {
        crate::config::WorkflowStep::Send { to, stage, msg } => format!(
            "send → {to} {}",
            stage
                .as_deref()
                .map(|s| format!("[{s}]"))
                .unwrap_or_else(|| msg.as_deref().unwrap_or("").chars().take(40).collect())
        ),
        crate::config::WorkflowStep::Wait { until, .. } => format!("wait {until}"),
    }
}

pub(crate) fn cmd_ensemble(
    cfg: &Config,
    project: &str,
    recipe: &str,
    workflow: &str,
) -> Result<()> {
    crate::cmd::spawn::cmd_spawn(
        cfg,
        project,
        crate::SpawnOpts {
            recipe: Some(recipe.to_string()),
            ..Default::default()
        },
    )?;
    run_workflow(cfg, project, workflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;

    #[test]
    fn unregistered_disk_pins_skips_registered_and_keyless() {
        let dir = std::env::temp_dir().join(format!(
            "fw-agent-keys-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("QuietHeron")).unwrap();
        fs::write(dir.join("QuietHeron").join("key.ncryptsec"), b"k").unwrap();
        fs::create_dir_all(dir.join("AmberBeacon")).unwrap();
        fs::write(dir.join("AmberBeacon").join("key.ncryptsec"), b"k").unwrap();
        fs::create_dir_all(dir.join("no-key")).unwrap();
        let registered = HashSet::from(["QuietHeron".into()]);
        let out = unregistered_disk_pins(&dir, &registered);
        assert_eq!(out, vec!["AmberBeacon".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }
}

#[derive(Subcommand)]
pub(crate) enum MailLockCmd {
    /// reserve files (advisory locks)
    Reserve {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        paths: Vec<String>,
        #[arg(long, default_value = "7200")]
        ttl_seconds: u64,
        #[arg(long)]
        exclusive: bool,
        #[arg(long, default_value = "flywheel")]
        reason: String,
        /// hold locks as this agent (crew must pass --as $AGENT_NAME)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// dump the active lock board (path, holder, exclusive, reason, expires, id)
    List {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// filter to one holder's leases
        #[arg(long)]
        holder: Option<String>,
    },
    /// show the holders of one path/glob (glob-overlap match)
    Show {
        /// path or glob whose holders to show
        path: String,
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum MailContactCmd {
    /// request contact with an agent (optionally in another project)
    Request {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        to: String,
        /// target project path for cross-project contact
        #[arg(long)]
        to_project: Option<String>,
        #[arg(long, default_value = "cross-project coordination")]
        reason: String,
    },
    /// accept a pending contact request
    Approve {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        from: String,
    },
    /// deny a pending contact request
    Deny {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        from: String,
    },
    /// list contact links for the flywheel identity
    List {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
    },
    /// set contact policy: open|auto|contacts_only|block_all
    Policy {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        policy: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum MailSlotCmd {
    /// acquire the advisory build slot (exclusive recommended)
    Acquire {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long, default_value = "cargo")]
        slot: String,
        #[arg(long)]
        exclusive: bool,
        #[arg(long, default_value = "1800")]
        ttl_seconds: u64,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// extend an active slot lease
    Renew {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long, default_value = "cargo")]
        slot: String,
        #[arg(long, default_value = "1800")]
        extend_seconds: u64,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// release an active slot lease (non-destructive)
    Release {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long, default_value = "cargo")]
        slot: String,
        /// act as this registered agent (adjective+noun or terminal short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum MailProductCmd {
    /// ensure a product exists (e.g. trading-research, knowledge)
    Ensure {
        #[arg(long)]
        name: String,
    },
    /// link a project into a product
    Link {
        #[arg(long)]
        product: String,
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
    },
    /// search messages across a product
    Search {
        #[arg(long)]
        product: String,
        query: String,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// product-scoped inbox for the flywheel identity
    Inbox {
        #[arg(long)]
        product: String,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
}

#[cfg(test)]
mod product_digest_tests {
    use super::*;

    fn pd_mail() -> mail::Mail {
        mail::Mail::from_config(None)
            .expect("mail ok")
            .expect("mail Some")
    }

    fn pd_slug(tag: &str) -> String {
        format!("pd-{tag}-{}", std::process::id())
    }

    fn pd_register(mail: &mail::Mail, project: &str, pin: &str) {
        mail.ensure_project(project).expect("ensure member");
        mail.register_fixture(project, pin, "flywheel", "m", "t")
            .expect("register pin");
    }

    fn pd_seed(mail: &mail::Mail, project: &str, pin: &str, subject: &str) {
        let sender = mail.clone().with_name(pin.to_string());
        sender
            .send(
                project,
                &[pin.to_string()],
                subject,
                "seed body",
                "normal",
                false,
                None,
            )
            .expect("seed mail");
    }

    fn pd_product(mail: &mail::Mail, key: &str, members: &[String]) {
        mail.ensure_product(key, None).expect("ensure product");
        for m in members {
            mail.products_link(key, m).expect("link member");
        }
    }

    #[test]
    fn product_digest_surfaces_seeded_mail_per_member() {
        let mail = pd_mail();
        let (a, b) = (pd_slug("mema"), pd_slug("memb"));
        let key = format!("pd-key-{}", std::process::id());
        let pin = "DigestFalcon";
        pd_register(&mail, &a, pin);
        pd_register(&mail, &b, pin);
        pd_seed(&mail, &a, pin, "report from A");
        pd_seed(&mail, &b, pin, "report from B");
        pd_product(&mail, &key, &[a.clone(), b.clone()]);
        let dirs = vec![(a.clone(), a.clone()), (b.clone(), b.clone())];
        let v = product_digest_for(&mail, &key, pin, &dirs);
        let members = v
            .get("members")
            .and_then(|m| m.as_array())
            .expect("members");
        assert_eq!(members.len(), 2, "both members surface: {v}");
        for m in members {
            let recent = m.get("recent").and_then(|r| r.as_u64()).unwrap_or(0);
            assert!(
                recent > 0,
                "seeded mail surfaces for {}: {m}",
                m.get("project").unwrap_or(&Value::Null)
            );
        }
        assert!(
            v.get("gaps")
                .and_then(|g| g.as_array())
                .map(|g| g.is_empty())
                .unwrap_or(false),
            "no gaps when all members read: {v}"
        );
    }

    #[test]
    fn product_digest_unreadable_member_is_loud_gap() {
        let mail = pd_mail();
        let (a, ghost) = (pd_slug("ok"), pd_slug("ghost"));
        let key = format!("pd-gap-{}", std::process::id());
        let pin = "GapFalcon";
        pd_register(&mail, &a, pin);
        pd_seed(&mail, &a, pin, "report from A");
        pd_product(&mail, &key, &[a.clone(), ghost.clone()]);
        let dirs = vec![(a.clone(), a.clone()), (ghost.clone(), ghost.clone())];
        let v = product_digest_for(&mail, &key, "NowherePin", &dirs);
        let gaps = v.get("gaps").and_then(|g| g.as_array()).expect("gaps");
        assert!(!gaps.is_empty(), "unreadable pin renders loud gaps: {v}");
        assert!(
            mail.whois(&a, "NowherePin").is_err(),
            "gap path never mints a key"
        );
    }

    #[test]
    fn product_digest_unknown_product_is_gap_not_empty() {
        let mail = pd_mail();
        let v = product_digest_for(
            &mail,
            &format!("pd-nope-{}", std::process::id()),
            "DigestFalcon",
            &[],
        );
        let gaps = v.get("gaps").and_then(|g| g.as_array()).expect("gaps");
        assert!(!gaps.is_empty(), "unknown product renders a gap: {v}");
        assert_eq!(
            v.get("members").and_then(|m| m.as_array()).map(|m| m.len()),
            Some(0)
        );
    }

    #[test]
    fn product_digest_binds_key_across_members_without_mint() {
        let mail = pd_mail();
        let (a, b) = (pd_slug("folda"), pd_slug("foldb"));
        let key = format!("pd-fold-{}", std::process::id());
        let pin = "FoldFalcon";
        pd_register(&mail, &a, pin);
        mail.ensure_project(&b).expect("ensure B");
        pd_seed(&mail, &a, pin, "report from A");
        pd_product(&mail, &key, &[a.clone(), b.clone()]);
        let dirs = vec![(a.clone(), a.clone()), (b.clone(), b.clone())];
        let v = product_digest_for(&mail, &key, pin, &dirs);
        let members = v
            .get("members")
            .and_then(|m| m.as_array())
            .expect("members");
        assert_eq!(members.len(), 2, "fold makes B readable: {v}");
        let (data_dir, _) = mail::built_mailbox_roots();
        let key_of = |p: &str| {
            std::fs::read(
                data_dir
                    .join(p)
                    .join("agents")
                    .join(pin)
                    .join("key.ncryptsec"),
            )
            .expect("fold-bound key file")
        };
        assert_eq!(key_of(&a), key_of(&b), "same key bytes in both members");
    }

    /// bd-jcqq: the fold probes the root the LIVE mailbox was built with.
    /// Proves it the way the suite poisons roots — clear the TORON_*
    /// overrides under the env lock, call the fold, restore — because before
    /// the fix the probe re-resolved env, landed on the home fallback, found
    /// no key for the pin it had just registered, and gapped both members
    /// with "refusing to mint".
    #[test]
    fn product_digest_fold_probes_the_built_root_not_a_fresh_env_resolution() {
        let _g = crate::config::lock_env();
        let mail = pd_mail();
        let (a, b) = (pd_slug("proba"), pd_slug("probb"));
        let key = format!("pd-probe-{}", std::process::id());
        let pin = "ProbeFalcon";
        pd_register(&mail, &a, pin);
        mail.ensure_project(&b).expect("ensure B");
        pd_seed(&mail, &a, pin, "report from A");
        pd_product(&mail, &key, &[a.clone(), b.clone()]);
        let dirs = vec![(a.clone(), a.clone()), (b.clone(), b.clone())];

        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            ["TORON_DATA_DIR", "TORON_MCP_DATA_DIR"]
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect();
        unsafe {
            std::env::remove_var("TORON_DATA_DIR");
            std::env::remove_var("TORON_MCP_DATA_DIR");
        }
        let v = product_digest_for(&mail, &key, pin, &dirs);
        for (name, value) in saved {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }

        let members = v
            .get("members")
            .and_then(|m| m.as_array())
            .expect("members");
        assert_eq!(
            members.len(),
            2,
            "the fold must probe the built mail root, not a fresh env resolution: {v}"
        );
    }
}

#[cfg(test)]
mod watch_progress_tests {
    use super::*;

    /// bd-7hdb: a reply that lands between the collab send and the watcher
    /// startup must still be reported. The watcher used to take the thread
    /// head as its baseline, so exactly that reply became the baseline and the
    /// wait timed out on a thread that was answered.
    #[test]
    fn a_reply_present_before_the_watch_starts_is_still_an_arrival() {
        let row = |id: &str, thread: &str| json!({ "id": id, "thread_id": thread });
        let rows = vec![row("ask-1", "collab-x"), row("reply-1", "collab-x")];

        let (baseline, arrival) = watch_progress(&rows, "collab-x", Some("ask-1"));
        assert_eq!(baseline.as_deref(), Some("ask-1"));
        assert_eq!(
            arrival.as_ref().and_then(|r| r["id"].as_str()),
            Some("reply-1"),
            "a reply that landed before the watcher started must not be swallowed"
        );

        let (baseline, arrival) = watch_progress(&rows, "collab-x", None);
        assert_eq!(baseline.as_deref(), Some("reply-1"));
        assert!(
            arrival.is_none(),
            "standalone watch keeps head-at-start: existing messages are not new"
        );

        let other = vec![row("ask-1", "collab-x"), row("elsewhere", "collab-y")];
        let (_, arrival) = watch_progress(&other, "collab-x", Some("ask-1"));
        assert!(arrival.is_none(), "thread match is exact: {arrival:?}");
    }

    /// bd-7hdb: the thread-scoping is only worth anything if the watch actually
    /// scopes its read, so the call site is pinned, not just the helper.
    /// `mail::a_thread_scoped_read_survives_a_limit_smaller_than_the_inbox`
    /// proves the mechanism against the real store; this proves the watch uses
    /// it. Without this the fix could be reverted to a hand-built filter and
    /// the mail test would stay green over a watch that no longer scopes.
    #[test]
    fn cmd_mail_watch_scopes_its_read_to_the_watched_thread() {
        let call = ["watch_filter", "(thread_id)"].concat();
        let start = ["fn cmd_mail_", "watch("].concat();
        let end = ["fn watch_", "filter("].concat();
        let src = include_str!("mail.rs");
        let body = src
            .split_once(&start)
            .and_then(|(_, rest)| rest.split_once(&end))
            .map(|(body, _)| body)
            .expect("cmd_mail_watch body is locatable in cmd/mail.rs");
        assert!(
            body.contains(&call),
            "cmd_mail_watch must read through watch_filter(thread_id): an \
             un-scoped read is truncated to the OLDEST rows, so the reply it \
             waits for would never appear"
        );
    }

    /// bd-7hdb: the scoping the tripwire above checks is real only if the
    /// filter carries the thread id.
    #[test]
    fn the_watch_filter_is_scoped_to_the_thread() {
        let f = watch_filter("collab-x");
        assert_eq!(
            f.thread_id.as_deref(),
            Some("collab-x"),
            "the watch reads exactly one thread"
        );
        assert!(f.include_bodies, "a reply is useless without its body");
    }

    /// bd-0qsc1: collab sends the question ONCE and waits. The old wait re-rang
    /// the same full question every 15s, so a reply filed in the wrong thread
    /// turned one ask into eleven inbox copies. The wait is `mail watch`, which
    /// only reads; nothing in this path writes a second time.
    #[test]
    fn collab_sends_once_and_waits_without_rerings() {
        let src = include_str!("mail.rs");
        let body = src
            .split_once("fn cmd_collab(")
            .and_then(|(_, rest)| rest.split_once("fn repo_evidence_verified("))
            .map(|(body, _)| body)
            .expect("cmd_collab body must sit before the next fn");
        let sends = body.matches("mail.send(").count();
        assert_eq!(
            sends, 1,
            "one send per invocation: a retry or re-ring duplicates the question, got {sends}"
        );
        assert!(
            body.contains("cmd_mail_watch("),
            "the wait is the read-only mail watch, not a bespoke loop"
        );
        assert!(
            !body.contains("thread::sleep"),
            "nothing in this path sleeps and resends"
        );
    }
}

#[cfg(test)]
mod plane_tests {
    /// `mail workflow run` sends and waits through live agents. It is a
    /// project door, so it must resolve the serving plane; a hard-dialed hc
    /// client makes every rex workflow fail at the socket.
    #[test]
    fn mail_workflow_resolves_the_active_plane() {
        let src = include_str!("mail.rs");
        let start = concat!("fn ", "run_workflow");
        let end = concat!("fn ", "step_label");
        let body = src
            .split(start)
            .nth(1)
            .expect("run_workflow must exist")
            .split(end)
            .next()
            .expect("step_label follows run_workflow");
        let resolved = concat!("multiplexer_backend", "(cfg)");
        let hard_dial = concat!("Client", "::connect");
        assert!(
            body.contains(resolved),
            "mail workflow must build the active-plane backend"
        );
        assert!(
            !body.contains(hard_dial),
            "mail workflow must not hard-dial the hc socket"
        );
        assert!(
            body.contains(concat!("resolve_agents", "_dyn")),
            "mail workflow must resolve targets through the backend"
        );
    }
}
