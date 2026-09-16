//! `flywheel xproj` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

use crate::config::Config;
use crate::output::print_json;

/// Cross-project swarm fan-out (bd-pattern-cross-project-swarm).
#[derive(Subcommand)]
pub(crate) enum XprojCmd {
    /// one-time contact handshake with an agent in another project
    Request {
        /// local project (sender's workspace path/label)
        project: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        to_project: String,
        #[arg(long, default_value = "cross-project swarm fan-out")]
        reason: String,
    },
    /// send a cross-project swarm request (`[xproj]` subject prefix)
    Send {
        /// local project (sender)
        project: String,
        #[arg(long)]
        to: String,
        /// absolute path or label of the receiving project
        #[arg(long)]
        to_project: String,
        /// recipe/workflow name the receiver should run
        #[arg(long)]
        recipe: String,
        /// free-form params for the receiver (strategy, asset, windows, …)
        #[arg(long, default_value = "")]
        body: String,
        /// optional product for reply search (default: trading-research)
        #[arg(long, default_value = "trading-research")]
        product: String,
        /// worker count hint for the receiver
        #[arg(long, default_value_t = 2)]
        workers: u32,
    },
    /// parse an `[xproj]` body (debug / orchestrator unit path)
    Parse {
        /// full message body or subject+body text
        text: String,
    },
}

pub(crate) fn cmd_xproj(cfg: &Config, cmd: &XprojCmd) -> Result<()> {
    match cmd {
        XprojCmd::Request {
            project,
            to,
            to_project,
            reason,
        } => {
            let mail = crate::mail_or_die(cfg)?;
            let key = crate::project_dir_str(cfg, project)?;
            let to_key = crate::project_dir_str(cfg, to_project)?;
            mail.ensure_project(&key)?;
            let from = mail.name.clone();
            let to_mail = crate::resolve_mail_target(to);
            let v = mail.request_contact(&key, &from, &to_mail, Some(&to_key), reason, None)?;
            print_json(&v);
        }
        XprojCmd::Send {
            project,
            to,
            to_project,
            recipe,
            body,
            product,
            workers,
        } => {
            let mail = crate::mail_or_die(cfg)?;
            let key = crate::project_dir_str(cfg, project)?;
            mail.ensure_project(&key)?;
            let to_mail = crate::resolve_mail_target(to);
            let ts = crate::state::now_iso().replace(['-', ':'], "");
            let thread = format!("xproj-{recipe}-{ts}");
            let subject = format!("[xproj] run {workers}-worker swarm recipe={recipe}");
            // review-fix F3: `send_message` has NO to_project param — agent-mail
            // delivers by recipient NAME across projects once the contact link is
            // approved (see `xproj request --to-project` for the handshake; the
            // receiver's loop polls ITS OWN project inbox via __xproj-inbox).
            // `to_project` is therefore routing CONTEXT, not a delivery target:
            // validate the path, carry it in the body so the receiver + logs have
            // full context, and surface it in the JSON. Never silently drop it.
            let to_project_validated: Option<String> = if to_project.is_empty() {
                None
            } else {
                let p = crate::project_dir(cfg, to_project);
                if !p.is_dir() {
                    eprintln!(
                        "flywheel: warning: --to-project {} is not a directory — delivery is name-based; the contact handshake (xproj request --to-project) is what authorizes cross-project routing",
                        p.display()
                    );
                }
                Some(to_project.clone())
            };
            let body_md = format!(
                "recipe={recipe}\nworkers={workers}\nproduct={product}\n{}\n\n{body}\n\n\
                 Reply results in this thread. Receiver: spawn workers in your project, \
                 run the recipe/workflow, reply a summary here.",
                to_project_validated
                    .as_ref()
                    .map(|tp| format!("to_project={tp}"))
                    .unwrap_or_default()
            );
            let v = mail.send(
                &key,
                &[to_mail],
                &subject,
                &body_md,
                "high",
                true,
                Some(&thread),
            )?;
            print_json(&json!({
                "ok": true,
                "thread_id": thread,
                "subject": subject,
                "product": product,
                "recipe": recipe,
                "workers": workers,
                "to_project": to_project_validated,
                "delivery": v,
            }));
        }
        XprojCmd::Parse { text } => match crate::parse_xproj_request(text) {
            Some(r) => print_json(&json!({
                "ok": true,
                "recipe": r.recipe,
                "workers": r.workers,
                "product": r.product,
                "params": r.params,
            })),
            None => {
                print_json(&json!({
                    "ok": false,
                    "error": "not an [xproj] request or missing recipe=",
                }));
            }
        },
    }
    Ok(())
}
