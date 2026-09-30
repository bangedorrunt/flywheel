//! `flywheel mail` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

use crate::config::Config;
use crate::output::print_json;

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
        /// act as this registered agent (adjective+noun or herdr short name)
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
        /// act as this registered agent (adjective+noun or herdr short name)
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
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// FTS5 search over the project's indexed messages
    Search {
        // bd-g5q4: required positionals must precede optional ones — the old
        // `[PROJECT] <QUERY>` order panicked in clap's debug asserts, making
        // this subcommand unusable in any arg order. Search is project-scoped
        // FTS (no identity in the query contract), so there is no --as here.
        /// FTS5 query (phrase/prefix/boolean; hyphenated ids are quoted)
        query: String,
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long, default_value = "20")]
        limit: u32,
        /// filter by sender (adjective+noun or herdr name)
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
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// agent (adjective+noun or herdr short name)
        agent: String,
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
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// operator force-release of a stale reservation (herdr-down / abandoned)
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
        /// act as this registered agent (adjective+noun or herdr short name)
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
        /// act as this registered agent (adjective+noun or herdr short name)
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
        /// recipients: comma-separated pins/herdr shorts; REQUIRED unless
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
            let project = crate::mail_project(project, project_flag, "inbox")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            // 2026-08-15: an UNREGISTERED --as identity previously made the
            // inbox projection scan without an agent scope (multi-second
            // hang). Fail fast with the registration command instead.
            if mail.whois(&key, &name).is_err() {
                anyhow::bail!(
                    "mail identity {name} is not registered for {project} — register it \
                     first: flywheel mail register {project} --name <adjnoun-pin>"
                );
            }
            // --unread needs the filtered surface; --urgent rides the same
            // inbox() call, which honors the flag with id-set parity to
            // inbox_filtered/MCP urgent_only (bd-mail-surface-drift-7r2o).
            // bd-zd2v: --since (RFC3339 or the `last` cursor) rides the
            // filtered surface with the other flags passed through; without
            // it the legacy path below is byte-identical to before.
            // bd-44hw: --thread-id rides it too (post-unwrap server-side).
            let v = match crate::inbox_cli_filter(
                *urgent,
                *unread,
                since.as_deref(),
                thread_id.as_deref(),
                *limit,
            ) {
                Some(f) => mail.inbox_filtered(&key, &name, &f)?,
                None => mail.inbox(&key, &name, *urgent, *limit)?,
            };
            // bd-mail-inbox-json-a7wc: --json is the DOCUMENTED machine
            // contract (stable keys + types); the default dump stays
            // byte-identical to before.
            print_json(&if *json {
                crate::mail_shim::inbox_json_rows(&v)
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
            let project = crate::mail_project(project, project_flag, "reply")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
            let project = crate::mail_project(project, project_flag, "read")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
            let project = crate::mail_project(project, project_flag, "ack")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
            let project = crate::mail_project(project, project_flag, "search")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let v = mail.search(
                &key,
                query,
                *limit,
                &crate::mail_shim::MailSearchFilter {
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
            let project = crate::mail_project(project, project_flag, "summarize")?;
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
            let project = crate::mail_project(project, project_flag, "whois")?;
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
            let project = crate::mail_project(project, project_flag, "register")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            // bd-register-portability-3c2b: never mint an unnamed identity.
            let mut decided = crate::resolve_register_name(name.clone(), from_project.as_deref())?;
            if let Some(adopt) = adopt_unnamed.as_deref() {
                let (data_dir, _) = crate::mail_shim::mailbox_roots();
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
            // Import: copy the source project's key into this project's
            // named slot BEFORE registering — register_agent then REUSES
            // that npub instead of minting a second identity. bd-zugo: a
            // missing source key is a LOUD refusal, never a silent fresh
            // mint (the ghost holder that could lease but never commit).
            if let Some(from) = from_project.as_deref()
                && from != project
            {
                let (data_dir, _) = crate::mail_shim::mailbox_roots();
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
            let project = crate::mail_project(project, project_flag, "renew")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
            let project = crate::mail_project(project, project_flag, "force-unlock")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
                let project = crate::mail_project(project, project_flag, "slot-acquire")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
                let project = crate::mail_project(project, project_flag, "slot-renew")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
                let project = crate::mail_project(project, project_flag, "slot-release")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
            let project = crate::mail_project(project, project_flag, "policy")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
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
                // product_digest_for; the CLI only lists members, anchors
                // the actor, and resolves member dirs.
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
                // Actor anchor: first member (ensure+register there, like
                // the single-project path). No members → bare facade, no
                // writes; product_digest_for renders the gap.
                let mail = match members.first() {
                    Some(first) => crate::mail_as(
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
                print_json(&crate::product_digest_for(&mail, pkey, &name, &dirs));
                return Ok(());
            }
            let project = crate::mail_project(project, project_flag, "digest")?;
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
                        if let Ok(d) = crate::digest_for(&mail, &key, n) {
                            // bd-dbre: name-alias identities resolve to the
                            // same pin inbox — emit one row per identity.
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
                let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                print_json(&crate::digest_for(&mail, &key, &name)?);
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
            let project = crate::mail_project(project, project_flag, "watch")?;
            crate::cmd_mail_watch(
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
            let project = crate::mail_project(project, project_flag, "send")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            // bd-mail-crew-fanout-xq6b: --crew expands the project spawn
            // registry into recipients (sender excluded, deduped); explicit
            // --to entries union on top. toron rejects broadcast=true (AM
            // invariant) — this is the supported per-recipient alternative.
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
            // ADR-0025 Spec 4: alias cleared under continue/adopt — --to captain fails loud until rebound
            for t in &targets {
                if t.eq_ignore_ascii_case("captain") {
                    crate::state::resolve_captain_target(project, t)?;
                }
            }
            // GS-006 plane tripwire: mail is gift-wrap private. Job-assign via private gift-wrap is the wrong plane
            // (should be channel/doorbell/job). Secret→public does not fire here (mail is private, safe), but job→private does.
            {
                let has_p = !targets.is_empty();
                if let Err(e) = crate::plane_route::check_job_tripwire(
                    body, has_p, false, false, true, None, false, false,
                ) {
                    anyhow::bail!(e);
                }
            }
            // bd-8pis: dead-pin hardening — loud advisory warn per
            // recipient with no live pane and no heartbeat. Mail stays
            // async: the send below runs regardless.
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
            let project = crate::mail_project(project, project_flag, "resend")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            // ADR-0010 D8 (bd-d8e4): look the receipt up FIRST — the resend is
            // only safe while the corr is genuinely outstanding.
            let receipt = crate::mail_shim::block_on(async {
                let mb = crate::mail_shim::try_mailbox()
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
                    // Preserve the original wake tag; default tasked (the
                    // send default) when the receipt carries none.
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
                let project = crate::mail_project(project, project_flag, "locks-reserve")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                // bd-reservation-identity-tlnf: the lease holder is the
                // RESOLVED actor (mail.name), never the raw `--as` spelling —
                // the guard attributes commits to the resolved identity, so a
                // slug-held lease and its pin-attributed commit were two
                // different holders.
                let agent = mail.name.clone();
                let v = mail.reserve(&key, &agent, paths, *ttl_seconds, *exclusive, reason)?;
                print_json(&v);
            }
            MailLockCmd::List {
                project,
                project_flag,
                holder,
            } => {
                let project = crate::mail_project(project, project_flag, "locks-list")?;
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
                let project = crate::mail_project(project, project_flag, "locks-show")?;
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
            let project = crate::mail_project(project, project_flag, "unlock")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
            // bd-reservation-identity-tlnf: resolved actor, never the raw slug.
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
            let project = crate::mail_project(project, project_flag, "conflicts")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            // bd-reservation-identity-tlnf: resolved actor, never the raw slug.
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
            let project = crate::mail_project(project, project_flag, "agents")?;
            let key = crate::project_dir_str(cfg, project)?;
            let mail = crate::mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            // Roster = CURRENT agents by default (active <24h); --all shows
            // the full registration history (ADR-0018 keeps legacy
            // identities registered for attribution).
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
                let project = crate::mail_project(project, project_flag, "contact-request")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, None)?;
                mail.ensure_project(&key)?;
                let from = mail.name.clone();
                // bd-aseq: registry pin first (same-project peers); a
                // cross-project target misses and falls back to the hash.
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
                let project = crate::mail_project(project, project_flag, "contact-approve")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, None)?;
                let to = mail.name.clone();
                // bd-aseq: registry pin first (same-project peers); a
                // cross-project requester misses and falls back to the hash.
                let from_mail = crate::resolve_mail_target_registry(project, from);
                let v = mail.respond_contact(&key, &to, &from_mail, true)?;
                print_json(&v);
            }
            MailContactCmd::Deny {
                project,
                project_flag,
                from,
            } => {
                let project = crate::mail_project(project, project_flag, "contact-deny")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, None)?;
                let to = mail.name.clone();
                let from_mail = crate::resolve_mail_target_registry(project, from);
                let v = mail.respond_contact(&key, &to, &from_mail, false)?;
                print_json(&v);
            }
            MailContactCmd::List {
                project,
                project_flag,
            } => {
                let project = crate::mail_project(project, project_flag, "contact-list")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, None)?;
                let name = mail.name.clone();
                let v = mail.list_contacts(&key, &name)?;
                print_json(&v);
            }
            MailContactCmd::Policy {
                project,
                project_flag,
                policy,
            } => {
                let project = crate::mail_project(project, project_flag, "contact-policy")?;
                let key = crate::project_dir_str(cfg, project)?;
                let mail = crate::mail_as(cfg, &key, None)?;
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
                let project = crate::mail_project(project, project_flag, "product-link")?;
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
    crate::mail_shim::mailbox_roots()
        .0
        .join(slug)
        .join("agents")
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
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// path or glob whose holders to show
        path: String,
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
        /// act as this registered agent (adjective+noun or herdr short name)
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
        /// act as this registered agent (adjective+noun or herdr short name)
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
        /// act as this registered agent (adjective+noun or herdr short name)
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
