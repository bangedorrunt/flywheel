//! `flywheel peer` — structured P2P protocol directives and reactive wakes.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use std::process::Command;

use crate::config::Config;
use crate::output::print_json;
use crate::state::EventKind;

/// P2P multiplayer protocol directives.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerCmd {
    /// Declare intent to work on a path/scope for a bead.
    Intent {
        #[arg(long)]
        project: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        scope: String,
        #[arg(long)]
        bead: String,
        #[arg(long)]
        mins: Option<u32>,
        #[arg(long = "as")]
        r#as: Option<String>,
    },
    /// Request another peer release a path for a bead.
    RequestRelease {
        #[arg(long)]
        project: String,
        /// target peer pin
        pin: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        bead: String,
        #[arg(long)]
        urgency: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long = "as")]
        r#as: Option<String>,
    },
    /// Respond to a release request.
    ReleaseResponse {
        #[arg(long)]
        project: String,
        /// target peer pin
        pin: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        status: String,
        #[arg(long)]
        checkpoint: Option<String>,
        #[arg(long)]
        eta: Option<u32>,
        #[arg(long = "as")]
        r#as: Option<String>,
    },
    /// Request review from a peer.
    ReviewRequest {
        #[arg(long)]
        project: String,
        /// target peer pin
        pin: String,
        #[arg(long)]
        bead: String,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long = "as")]
        r#as: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct P2pDirective {
    pub verb: &'static str,
    pub project: String,
    pub sender: String,
    pub target_pin: Option<String>,
    pub path: String,
    pub body: Value,
}

impl P2pDirective {
    pub(crate) fn subject(&self) -> String {
        format!("p2p:{} {}", self.verb, self.path)
    }

    pub(crate) fn mail_args(&self, to: &str) -> Vec<String> {
        vec![
            "mail".to_string(),
            "send".to_string(),
            "--project".to_string(),
            self.project.clone(),
            "--as".to_string(),
            self.sender.clone(),
            "--to".to_string(),
            to.to_string(),
            "--subject".to_string(),
            self.subject(),
            "--body".to_string(),
            serde_json::to_string(&self.body).unwrap_or_else(|_| "{}".to_string()),
        ]
    }

    pub(crate) fn wake_line(&self, wm: u64) -> String {
        crate::pane_write::p2p_wake_line(self.verb, &self.sender, &self.path, wm)
    }

    pub(crate) fn confirmation(&self, wm: u64, sent: bool) -> Value {
        json!({
            "verb": self.verb,
            "sent": sent,
            "wake": self.wake_line(wm),
            "project": self.project,
            "path": self.path,
            "target": self.target_pin,
        })
    }
}

fn directive_sender(cfg: &Config, r#as: Option<&str>) -> String {
    if let Some(s) = r#as.filter(|s| !s.trim().is_empty()) {
        return s.to_string();
    }
    if let Ok(s) = std::env::var("AGENT_NAME")
        && !s.trim().is_empty()
    {
        return s;
    }
    if let Some(name) = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return name.to_string();
    }
    "agent".to_string()
}

pub(crate) fn cmd_peer(cfg: &Config, cmd: &PeerCmd) -> Result<()> {
    let directive = directive_from_cmd(cfg, cmd);
    if let Some(to) = directive.target_pin.as_deref() {
        toron_mail_send(&directive, to)?;
        let wm = recipient_wm(&directive.project, to);
        let wake = deliver_p2p_wake(cfg, &directive, to, wm)?;
        crate::log(
            &directive.project,
            EventKind::Send,
            json!({
                "op": "peer",
                "verb": directive.verb,
                "to": to,
                "path": directive.path,
                "wake": wake,
                "wm": wm,
            }),
        );
        print_json(&directive.confirmation(wm, true));
        return Ok(());
    }

    // Intent has no explicit target pin; stamp locally as a structured directive.
    crate::log(
        &directive.project,
        EventKind::Send,
        json!({
            "op": "peer",
            "verb": directive.verb,
            "path": directive.path,
            "body": directive.body,
            "broadcast": true,
        }),
    );
    print_json(&json!({
        "verb": directive.verb,
        "sent": true,
        "project": directive.project,
        "path": directive.path,
        "broadcast": true,
    }));
    Ok(())
}

fn directive_from_cmd(cfg: &Config, cmd: &PeerCmd) -> P2pDirective {
    match cmd {
        PeerCmd::Intent {
            project,
            path,
            scope,
            bead,
            mins,
            r#as,
        } => P2pDirective {
            verb: "intent",
            project: project.clone(),
            sender: directive_sender(cfg, r#as.as_deref()),
            target_pin: None,
            path: path.clone(),
            body: json!({
                "verb": "intent",
                "project": project,
                "path": path,
                "scope": scope,
                "bead": bead,
                "mins": mins,
            }),
        },
        PeerCmd::RequestRelease {
            project,
            pin,
            path,
            bead,
            urgency,
            reason,
            r#as,
        } => P2pDirective {
            verb: "request-release",
            project: project.clone(),
            sender: directive_sender(cfg, r#as.as_deref()),
            target_pin: Some(pin.clone()),
            path: path.clone(),
            body: json!({
                "verb": "request-release",
                "project": project,
                "path": path,
                "bead": bead,
                "urgency": urgency,
                "reason": reason,
            }),
        },
        PeerCmd::ReleaseResponse {
            project,
            pin,
            path,
            status,
            checkpoint,
            eta,
            r#as,
        } => P2pDirective {
            verb: "release-response",
            project: project.clone(),
            sender: directive_sender(cfg, r#as.as_deref()),
            target_pin: Some(pin.clone()),
            path: path.clone(),
            body: json!({
                "verb": "release-response",
                "project": project,
                "path": path,
                "status": status,
                "checkpoint": checkpoint,
                "eta": eta,
            }),
        },
        PeerCmd::ReviewRequest {
            project,
            pin,
            bead,
            commit,
            summary,
            r#as,
        } => {
            // no path in spec; use bead-derived scope token for wake/subject context.
            let path = format!("bead:{bead}");
            P2pDirective {
                verb: "review-request",
                project: project.clone(),
                sender: directive_sender(cfg, r#as.as_deref()),
                target_pin: Some(pin.clone()),
                path: path.clone(),
                body: json!({
                    "verb": "review-request",
                    "project": project,
                    "bead": bead,
                    "commit": commit,
                    "summary": summary,
                }),
            }
        }
    }
}

fn toron_mail_send(directive: &P2pDirective, to: &str) -> Result<()> {
    let in_process = (|| -> Result<()> {
        let mb = crate::mail::comms_mailbox()?;
        let send = toron_core::mailbox::messaging::SendMessage {
            project_slug: directive.project.clone(),
            sender_name: directive.sender.clone(),
            to: vec![to.to_string()],
            subject: directive.subject(),
            body_md: serde_json::to_string(&directive.body).unwrap_or_else(|_| "{}".to_string()),
            ..Default::default()
        };
        crate::mail::block_on(mb.send_message(&send)).map(|_| ())
    })();
    if in_process.is_ok() {
        return Ok(());
    }

    let args = directive.mail_args(to);
    let out = Command::new("toron")
        .args(&args)
        .output()
        .with_context(|| "run toron mail send")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "toron mail send failed for {}: {}{}",
            to,
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" {}", stdout.trim())
            }
        );
    }
    Ok(())
}

fn recipient_wm(project: &str, pin: &str) -> u64 {
    let Ok(mb) = crate::mail::comms_mailbox() else {
        return 0;
    };
    let Ok(store) = crate::comms_send::store() else {
        return 0;
    };
    crate::mail::block_on(async {
        let Ok(npub) = mb.comms_npub(project, pin).await else {
            return 0;
        };
        store.head(&npub).await.unwrap_or(0)
    })
}

fn harness_kind_for_target(project: &str, pane_id: &str, name: Option<&str>) -> String {
    let reg = crate::state::load_spawn_registry(project);
    reg.into_iter()
        .find(|e| e.pane_id == pane_id || name.is_some_and(|n| e.name == n))
        .and_then(|e| e.harness)
        .unwrap_or_default()
}

fn deliver_p2p_wake_hc(
    cfg: &Config,
    directive: &P2pDirective,
    pin: &str,
    wm: u64,
) -> Result<String> {
    let mut c = crate::hc::Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = crate::find_workspace(&mut c, &directive.project)?;
    let targets = crate::resolve_agents(&mut c, &ws_id, &directive.project, pin)?;
    if targets.len() != 1 {
        bail!(
            "peer directive targets ONE pane — '{pin}' resolved {} ({})",
            targets.len(),
            targets
                .iter()
                .filter_map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let a = &targets[0];
    let target = crate::agent_target(a).to_string();
    let mut kind = harness_kind_for_target(&directive.project, &a.pane_id, a.name.as_deref());
    if kind.is_empty() {
        kind = a.agent.as_deref().unwrap_or("").to_string();
    }
    let route = crate::harness_caps::wake_route(&kind, crate::harness_caps::installed_from_env());
    let line = directive.wake_line(wm);
    crate::pane_write::write_p2p_wake(&crate::pane_write::HcPanes(&c), &target, &line, route)?;
    Ok(line)
}

#[cfg(feature = "backend-superlogical")]
fn deliver_p2p_wake_rex(
    cfg: &Config,
    directive: &P2pDirective,
    pin: &str,
    wm: u64,
) -> Result<String> {
    let backend = crate::backend::multiplexer_backend(cfg)?;
    let ws_id = crate::find_workspace_dyn(&*backend, &directive.project)?;
    let targets = crate::resolve_agents_dyn(&*backend, &ws_id, &directive.project, pin)?;
    if targets.len() != 1 {
        bail!(
            "peer directive targets ONE pane — '{pin}' resolved {} ({})",
            targets.len(),
            targets
                .iter()
                .filter_map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let a = &targets[0];
    let target = a.pane_id.clone();
    let mut kind = harness_kind_for_target(&directive.project, &a.pane_id, a.name.as_deref());
    if kind.is_empty() {
        kind = a.agent.as_deref().unwrap_or("").to_string();
    }
    let route = crate::harness_caps::wake_route(&kind, crate::harness_caps::installed_from_env());
    let line = directive.wake_line(wm);
    crate::pane_write::write_p2p_wake(
        &crate::pane_write::RexPanes(&*backend),
        &target,
        &line,
        route,
    )?;
    Ok(line)
}

fn deliver_p2p_wake(cfg: &Config, directive: &P2pDirective, pin: &str, wm: u64) -> Result<String> {
    if crate::pane::rex_door_selected(cfg) {
        #[cfg(feature = "backend-superlogical")]
        {
            return deliver_p2p_wake_rex(cfg, directive, pin, wm);
        }
        #[cfg(not(feature = "backend-superlogical"))]
        {
            let _ = (cfg, directive, pin, wm);
            bail!("rex backend selected but backend-superlogical feature is not enabled");
        }
    }
    deliver_p2p_wake_hc(cfg, directive, pin, wm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        cmd: PeerCmd,
    }

    #[test]
    fn parses_intent_request_release_release_response_and_review_request() {
        let intent = Cli::try_parse_from([
            "flywheel",
            "intent",
            "--project",
            "flywheel",
            "--path",
            "src/pane.rs",
            "--scope",
            "dispatch",
            "--bead",
            "bd-1",
            "--mins",
            "15",
            "--as",
            "BlackGorge",
        ])
        .expect("intent parses");
        match intent.cmd {
            PeerCmd::Intent {
                project,
                path,
                scope,
                bead,
                mins,
                r#as,
            } => {
                assert_eq!(project, "flywheel");
                assert_eq!(path, "src/pane.rs");
                assert_eq!(scope, "dispatch");
                assert_eq!(bead, "bd-1");
                assert_eq!(mins, Some(15));
                assert_eq!(r#as.as_deref(), Some("BlackGorge"));
            }
            other => panic!("expected Intent, got {other:?}"),
        }

        let req = Cli::try_parse_from([
            "flywheel",
            "request-release",
            "--project",
            "flywheel",
            "AmberHarbor",
            "--path",
            "src/pane.rs",
            "--bead",
            "bd-1",
            "--urgency",
            "high",
            "--reason",
            "blocked",
            "--as",
            "BlackGorge",
        ])
        .expect("request-release parses");
        match req.cmd {
            PeerCmd::RequestRelease {
                project,
                pin,
                path,
                bead,
                urgency,
                reason,
                r#as,
            } => {
                assert_eq!(project, "flywheel");
                assert_eq!(pin, "AmberHarbor");
                assert_eq!(path, "src/pane.rs");
                assert_eq!(bead, "bd-1");
                assert_eq!(urgency.as_deref(), Some("high"));
                assert_eq!(reason.as_deref(), Some("blocked"));
                assert_eq!(r#as.as_deref(), Some("BlackGorge"));
            }
            other => panic!("expected RequestRelease, got {other:?}"),
        }

        let resp = Cli::try_parse_from([
            "flywheel",
            "release-response",
            "--project",
            "flywheel",
            "AmberHarbor",
            "--path",
            "src/pane.rs",
            "--status",
            "released",
            "--checkpoint",
            "abc",
            "--eta",
            "5",
            "--as",
            "BlackGorge",
        ])
        .expect("release-response parses");
        match resp.cmd {
            PeerCmd::ReleaseResponse {
                project,
                pin,
                path,
                status,
                checkpoint,
                eta,
                r#as,
            } => {
                assert_eq!(project, "flywheel");
                assert_eq!(pin, "AmberHarbor");
                assert_eq!(path, "src/pane.rs");
                assert_eq!(status, "released");
                assert_eq!(checkpoint.as_deref(), Some("abc"));
                assert_eq!(eta, Some(5));
                assert_eq!(r#as.as_deref(), Some("BlackGorge"));
            }
            other => panic!("expected ReleaseResponse, got {other:?}"),
        }

        let review = Cli::try_parse_from([
            "flywheel",
            "review-request",
            "--project",
            "flywheel",
            "AmberHarbor",
            "--bead",
            "bd-1",
            "--commit",
            "abc123",
            "--summary",
            "ready",
            "--as",
            "BlackGorge",
        ])
        .expect("review-request parses");
        match review.cmd {
            PeerCmd::ReviewRequest {
                project,
                pin,
                bead,
                commit,
                summary,
                r#as,
            } => {
                assert_eq!(project, "flywheel");
                assert_eq!(pin, "AmberHarbor");
                assert_eq!(bead, "bd-1");
                assert_eq!(commit, "abc123");
                assert_eq!(summary.as_deref(), Some("ready"));
                assert_eq!(r#as.as_deref(), Some("BlackGorge"));
            }
            other => panic!("expected ReviewRequest, got {other:?}"),
        }
    }

    #[test]
    fn directive_builds_p2p_wake_line_and_mail_structure() {
        let d = P2pDirective {
            verb: "request-release",
            project: "flywheel".to_string(),
            sender: "BlackGorge".to_string(),
            target_pin: Some("AmberHarbor".to_string()),
            path: "src/pane.rs".to_string(),
            body: json!({"verb":"request-release","path":"src/pane.rs"}),
        };
        let wake = d.wake_line(0x7f);
        assert_eq!(
            wake,
            "⟦p2p:request-release⟧ from BlackGorge for src/pane.rs (wm 7f) — run: toron next"
        );
        let mail = d.mail_args("AmberHarbor");
        assert_eq!(mail[0], "mail");
        assert_eq!(mail[1], "send");
        assert!(mail.windows(2).any(|w| w == ["--project", "flywheel"]));
        assert!(mail.windows(2).any(|w| w == ["--as", "BlackGorge"]));
        assert!(mail.windows(2).any(|w| w == ["--to", "AmberHarbor"]));
        assert!(
            mail.windows(2)
                .any(|w| w == ["--subject", "p2p:request-release src/pane.rs"])
        );
        let confirm = d.confirmation(0x7f, true);
        assert_eq!(confirm["wake"], wake);
        assert_eq!(confirm["verb"], "request-release");
        assert_eq!(confirm["sent"], true);
    }
}
