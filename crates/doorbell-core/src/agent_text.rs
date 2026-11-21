// governed-by: bd-ofa1s
//! The ONE owner of agent-facing wake text (bd-ofa1s). Every line a human or
//! an agent reads in a pane is rendered here, in self-contained plain English:
//! it names who the wake is for, what happened, and the exact command to run.
//! No stack jargon leaks into these strings — `no_jargon_in_agent_text` below
//! is a build-time fence. Machine identity (event ids, watermarks) is typed
//! data, never parsed out of these strings (bd-tdpix).

use crate::{EventClass, WakeKind};

/// The single neutral screen marker every agent-facing wake begins with.
/// `pane_screen::WAKE_PREFIXES` points here; there is no second constant.
pub const WAKE_PREFIX: &str = "[wake]";

/// Jargon tokens the lint refuses in any agent-facing string. The verify list
/// from bd-ofa1s is authoritative; these are the exact spellings seen in the
/// old wake lines.
pub const JARGON_TOKENS: &[&str] = &[
    "plane-P",
    "plane-S",
    "plane P",
    "plane S",
    "kind 9",
    "kind 14",
    "kind 42",
    "kind 43001",
    "kind 30031",
    "kind 20001",
    "(wm ",
    "rung",
    "corr=",
];

/// Context a renderer needs to make a wake self-contained. Callers that do
/// not yet know a field leave it empty and the renderer drops that clause.
#[derive(Debug, Default, Clone, Copy)]
pub struct WakeCtx<'a> {
    pub recipient: &'a str,
    pub project: &'a str,
    pub sender: &'a str,
    pub path: &'a str,
    pub command: &'a str,
}

fn who(ctx: &WakeCtx<'_>) -> String {
    match (ctx.recipient, ctx.project) {
        (r, p) if !r.is_empty() && !p.is_empty() => {
            format!(" for {r} in project {p}")
        }
        (r, _) if !r.is_empty() => format!(" for {r}"),
        _ => String::new(),
    }
}

fn pull(ctx: &WakeCtx<'_>) -> String {
    let base = if ctx.command.is_empty() {
        "toron next"
    } else {
        ctx.command
    };
    // Name the exact command with flags whenever the caller knows the pin
    // and project (bd-ofa1s). The generic doorbell_line wrapper omits them.
    let full =
        if !ctx.project.is_empty() && !ctx.recipient.is_empty() && !base.contains("--project") {
            format!("{base} --project {} --as {}", ctx.project, ctx.recipient)
        } else {
            base.to_string()
        };
    format!("Read it with: {full}")
}

/// Render every wake class in plain, actionable English. One match, one owner.
pub fn render(class: EventClass, ctx: &WakeCtx<'_>) -> String {
    let head = match class {
        EventClass::PlanePMail => format!("New mail{}. {}", who(ctx), pull(ctx)),
        EventClass::PlanePUrgent => {
            format!("Urgent mail{}. {}", who(ctx), pull(ctx))
        }
        EventClass::Channel => {
            let room = if ctx.project.is_empty() {
                "the team room".to_string()
            } else {
                format!("the {} team room", ctx.project)
            };
            format!("New message in {room}. {}", pull(ctx))
        }
        EventClass::Job => format!("New job assignment{}. {}", who(ctx), pull(ctx)),
        EventClass::RequestRelease => format!(
            "{} asks you to release {}. {}",
            if ctx.sender.is_empty() {
                "A teammate"
            } else {
                ctx.sender
            },
            if ctx.path.is_empty() {
                "a file"
            } else {
                ctx.path
            },
            pull(ctx),
        ),
        EventClass::ReservationConflict => format!(
            "File reservation conflict on {}. {}",
            if ctx.path.is_empty() {
                "a file"
            } else {
                ctx.path
            },
            pull(ctx),
        ),
        EventClass::P2p => {
            let verb = ctx.command;
            let action = match verb {
                "request-release" => "asks you to release",
                "release-response" => "has answered your release request for",
                "review-request" => "asks you to review",
                _ => "sent you a request about",
            };
            format!(
                "{WAKE_PREFIX} Direct message from {}: {} {}. Read it with: toron next",
                if ctx.sender.is_empty() {
                    "a teammate"
                } else {
                    ctx.sender
                },
                action,
                if ctx.path.is_empty() {
                    "a shared file"
                } else {
                    ctx.path
                },
            )
        }
    };
    if head.starts_with(WAKE_PREFIX) {
        head
    } else {
        format!("{WAKE_PREFIX} {head}")
    }
}

/// A short digest line for a class that is folded, not paged immediately.
pub fn digest(label: &str) -> String {
    format!("{WAKE_PREFIX} {label}. Read it with: toron next")
}

/// Classify a rendered line by its plain-English lead, never by jargon tokens.
/// Kept beside the renderer so there is one source of truth for wake text.
pub fn classify(line: &str) -> WakeKind {
    if line.contains("Direct message from") {
        return WakeKind::Peer;
    }
    if line.contains("asks you to release") || line.contains("Urgent mail") {
        WakeKind::ActNow
    } else if line.contains("system notice") {
        WakeKind::Notice
    } else if line.contains("New mail") || line.contains("New message") {
        WakeKind::AtIdle
    } else {
        WakeKind::Digest
    }
}

/// Every canonical golden string. The lint test scans these plus the source
/// of this module, so a jargon token cannot enter agent-facing text unnoticed.
pub fn golden_strings() -> Vec<String> {
    let ctx = WakeCtx {
        recipient: "SlateHarbor",
        project: "flywheel",
        sender: "BoldCliff",
        path: "src/peer.rs",
        command: "toron next --project flywheel --as SlateHarbor",
    };
    [
        EventClass::PlanePMail,
        EventClass::PlanePUrgent,
        EventClass::Channel,
        EventClass::Job,
        EventClass::RequestRelease,
        EventClass::ReservationConflict,
        EventClass::P2p,
    ]
    .into_iter()
    .map(|c| render(c, &ctx))
    .chain([digest("system notice")])
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wake_class_is_plain_self_contained_english() {
        let lines = golden_strings();
        assert_eq!(lines.len(), 8, "one golden per wake class + digest");
        for line in &lines {
            assert!(
                line.starts_with(WAKE_PREFIX),
                "single neutral prefix: {line}"
            );
            assert!(
                line.to_ascii_lowercase().contains("read it with"),
                "actionable: {line}"
            );
            assert!(
                !line.contains("toron next.") || line.contains("--project"),
                "flags: {line}"
            );
        }
        assert!(lines[0].contains("for SlateHarbor in project flywheel"));
        assert!(lines[0].contains("--project flywheel --as SlateHarbor"));
        assert!(lines[1].starts_with(&format!("{WAKE_PREFIX} Urgent mail")));
        assert!(lines[2].contains("the flywheel team room"));
        assert!(lines[4].contains("BoldCliff asks you to release src/peer.rs"));
        assert!(lines[5].contains("File reservation conflict on src/peer.rs"));
        assert!(
            lines[6].contains("Direct message from BoldCliff") && lines[6].contains("src/peer.rs")
        );
    }

    #[test]
    fn no_jargon_in_agent_text() {
        let src = include_str!("agent_text.rs");
        let start = src.find("pub const JARGON_TOKENS").expect("token list");
        let end = src[start..].find("];").expect("token list end") + start + 2;
        let body = format!("{}{}", &src[..start], &src[end..]);
        for token in JARGON_TOKENS {
            assert!(
                !body.contains(token),
                "agent-facing text must not contain {token:?}",
            );
        }
        for line in golden_strings() {
            for token in JARGON_TOKENS {
                assert!(!line.contains(token), "golden line has {token:?}: {line}");
            }
        }
    }

    #[test]
    fn classify_uses_plain_english_not_jargon() {
        assert_eq!(
            classify(&render(EventClass::PlanePUrgent, &WakeCtx::default())),
            WakeKind::ActNow
        );
        assert_eq!(
            classify(&render(EventClass::PlanePMail, &WakeCtx::default())),
            WakeKind::AtIdle
        );
        assert_eq!(
            classify(&render(EventClass::P2p, &WakeCtx::default())),
            WakeKind::Peer
        );
    }
}
