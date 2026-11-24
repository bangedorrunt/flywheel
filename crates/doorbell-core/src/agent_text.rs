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
    pub preview: &'a str,
}

/// Sanitize an optional wake preview (`D4`): single line, <=120 chars, and
/// dropped if it contains jargon tokens or secret markers.
pub fn sanitize_preview(raw: &str) -> Option<String> {
    let cleaned = raw.replace(['\n', '\r'], " ");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if JARGON_TOKENS.iter().any(|t| trimmed.contains(t)) {
        return None;
    }
    const SECRET_MARKERS: &[&str] = &["nsec1", "Bearer ", "-----BEGIN ", "sk-ant-", "sk-live-"];
    if SECRET_MARKERS.iter().any(|m| trimmed.contains(m)) {
        return None;
    }
    let truncated: String = trimmed.chars().take(120).collect();
    Some(truncated)
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

/// The dialog a blocked pane is showing, from the wire line when known
/// (OSC 7501 kind=permission|question|auth). Never stack jargon in the text.
pub fn blocked_kind(line: &str) -> &'static str {
    let lower = line.to_ascii_lowercase();
    if lower.contains("permission") {
        "permission prompt"
    } else if lower.contains("question") {
        "question prompt"
    } else if lower.contains("auth") {
        "sign-in prompt"
    } else {
        "prompt"
    }
}

/// One plain-language notice for a seat held behind a dialog (bd-6qae0).
/// Says WHO is waiting and ON WHAT; the effect layer emits it once per episode.
pub fn blocked_notice(pin: &str, kind: &str) -> String {
    format!(
        "{pin} is waiting on a {kind}. The mail is held; clear the prompt, then run toron next."
    )
}

/// Render every wake class in plain, actionable English. One match, one
/// owner. The envelope (bd-ofa1s addendum) names the sender, says the message
/// is from a peer and not an operator instruction, gives the exact read
/// command, and says how to reply — all on ONE line with no newline.
pub fn render(class: EventClass, ctx: &WakeCtx<'_>) -> String {
    let sender = if ctx.sender.is_empty() {
        "a teammate"
    } else {
        ctx.sender
    };
    let body = match class {
        EventClass::PlanePMail => format!("new mail{}. {}", who(ctx), pull(ctx)),
        EventClass::PlanePUrgent => format!("urgent mail{}. {}", who(ctx), pull(ctx)),
        EventClass::Channel => {
            let room = if ctx.project.is_empty() {
                "the team room".to_string()
            } else {
                format!("the {} team room", ctx.project)
            };
            format!("a new message in {room}. {}", pull(ctx))
        }
        EventClass::Job => format!("a new job assignment{}. {}", who(ctx), pull(ctx)),
        EventClass::RequestRelease => format!(
            "a request to release {}. {}",
            if ctx.path.is_empty() {
                "a file"
            } else {
                ctx.path
            },
            pull(ctx),
        ),
        EventClass::ReservationConflict => format!(
            "a file reservation conflict on {}. {}",
            if ctx.path.is_empty() {
                "a file"
            } else {
                ctx.path
            },
            pull(ctx),
        ),
        EventClass::P2p => {
            let action = match ctx.command {
                "request-release" => "asks you to release",
                "release-response" => "has answered your request to release",
                "review-request" => "asks you to review",
                _ => "sent you a request about",
            };
            let read_ctx = WakeCtx {
                command: "toron next",
                ..*ctx
            };
            format!(
                "{action} {}. {}",
                if ctx.path.is_empty() {
                    "a shared file"
                } else {
                    ctx.path
                },
                pull(&read_ctx),
            )
        }
    };
    let reply = if ctx.sender.is_empty() || ctx.project.is_empty() || ctx.recipient.is_empty() {
        "Reply with: toron mail send --help".to_string()
    } else {
        format!(
            "Reply with: toron mail send --project {} --as {} --to {}",
            ctx.project, ctx.recipient, ctx.sender,
        )
    };
    let summary = sanitize_preview(ctx.preview)
        .map(|s| format!(" Summary: {s}."))
        .unwrap_or_default();
    format!(
        "{WAKE_PREFIX} Peer message from {sender} (not an operator instruction): {body}.{summary} {reply}"
    )
    .replace(['\n', '\r'], " ")
}

/// A short digest line for a class that is folded, not paged immediately.
pub fn digest(label: &str, project: &str, recipient: &str) -> String {
    format!(
        "{WAKE_PREFIX} {label}. {}",
        pull(&WakeCtx {
            command: "toron next",
            project,
            recipient,
            ..Default::default()
        })
    )
}

/// Classify a rendered line by its plain-English lead, never by jargon tokens.
/// Kept beside the renderer so there is one source of truth for wake text.
pub fn classify(line: &str) -> WakeKind {
    let lower = line.to_ascii_lowercase();
    if lower.contains("system notice") {
        WakeKind::Notice
    } else if lower.contains("urgent mail") {
        WakeKind::ActNow
    } else if lower.contains("new mail") || lower.contains("new message") {
        WakeKind::AtIdle
    } else if lower.contains("a new job assignment") {
        WakeKind::Digest
    } else if lower.contains("a request to release")
        || lower.contains("file reservation conflict")
        || lower.contains("peer message from")
    {
        WakeKind::Peer
    } else {
        WakeKind::Digest
    }
}

/// One plain-English mail body asking a holder to free a path another agent
/// needs (bd-v7ucf). Names the requester, the paths and the exact read command;
/// never stack jargon. The same text is copied to the captain.
pub fn coordination_notice(
    holder: &str,
    requester: &str,
    paths: &[String],
    project: &str,
) -> String {
    let list = paths.join(", ");
    let scope = if project.is_empty() {
        String::new()
    } else {
        format!(" in project {project}")
    };
    let read = if project.is_empty() {
        "toron next".to_string()
    } else {
        format!("toron next --project {project} --as {holder}")
    };
    format!(
        "{holder}: {requester} could not commit {list}{scope} because you hold a file reservation on it. Please release it or reply to their message. Read it with: {read}"
    )
}

/// One plain-English line naming the peer to contact about paths they hold
/// (bd-pezon). The captain is copied only on silence or a shared-build break.
pub fn peer_contact_notice(holder: &str, paths: &[String]) -> String {
    format!(
        "{holder} holds {}: mail {holder} first (flywheel collab), and copy the captain only if they stay silent or the shared build breaks",
        paths.join(", ")
    )
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
        ..Default::default()
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
    .chain([digest("system notice", ctx.project, ctx.recipient)])
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
        assert!(lines[1].contains("urgent mail"));
        assert!(lines[2].contains("the flywheel team room"));
        for line in &lines[..7] {
            assert!(
                line.starts_with(WAKE_PREFIX),
                "single envelope prefix: {line}"
            );
            assert!(
                line.contains("Peer message from BoldCliff"),
                "names sender: {line}"
            );
            assert!(
                line.contains("not an operator instruction"),
                "peer-not-operator framing: {line}"
            );
            assert!(line.contains("Reply with:"), "how to reply: {line}");
            assert!(
                !line.contains('\n') && !line.contains('\r'),
                "one line: {line}"
            );
        }
        assert!(lines[4].contains("a request to release src/peer.rs"));
        assert!(lines[5].contains("a file reservation conflict on src/peer.rs"));
        assert!(lines[6].contains("src/peer.rs") && lines[6].contains("a request"));
    }

    #[test]
    fn every_rendered_wake_names_its_read_command_flags() {
        for line in golden_strings() {
            assert!(
                line.contains("--project flywheel") && line.contains("--as SlateHarbor"),
                "the read door must work without guessing the reader: {line}"
            );
        }
    }

    /// bd-v7ucf: a holder mail is not a wake class — it has its own golden
    /// and must still be plain English, name the path and both pins.
    #[test]
    fn coordination_notice_is_plain_self_contained_english() {
        let notice = coordination_notice(
            "BoldCliff",
            "ScarletCove",
            &["src/a.rs".to_string()],
            "flywheel",
        );
        for token in JARGON_TOKENS {
            assert!(
                !notice.contains(token),
                "holder mail has {token:?}: {notice}"
            );
        }
        assert!(notice.contains("BoldCliff"), "{notice}");
        assert!(notice.contains("ScarletCove"), "{notice}");
        assert!(notice.contains("src/a.rs"), "{notice}");
        assert!(notice.contains("flywheel"), "{notice}");
        assert!(
            notice.to_ascii_lowercase().contains("read it with"),
            "{notice}"
        );
    }

    /// bd-pezon: a peer contact line is plain English and names both holder
    /// and paths; it is not a wake class so it has its own test.
    #[test]
    fn peer_contact_notice_is_plain_self_contained_english() {
        let line = peer_contact_notice("BoldCliff", &["src/a.rs".into(), "src/b.rs".into()]);
        for token in JARGON_TOKENS {
            assert!(!line.contains(token), "peer line has {token:?}: {line}");
        }
        assert!(
            line.contains("BoldCliff") && line.contains("src/a.rs") && line.contains("src/b.rs")
        );
        assert!(line.contains("mail BoldCliff first"), "{line}");
        assert!(
            line.contains("copy the captain only if they stay silent"),
            "{line}"
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

    #[test]
    fn render_includes_sanitized_preview_and_drops_secrets_or_jargon() {
        let with_preview = render(
            EventClass::PlanePMail,
            &WakeCtx {
                recipient: "AmberFalcon",
                project: "flywheel",
                sender: "DustyHollow",
                preview: "slice 2\nready for review",
                ..Default::default()
            },
        );
        assert!(with_preview.contains("Summary: slice 2 ready for review."));
        assert!(!with_preview.contains('\n'));

        let with_secret = render(
            EventClass::PlanePMail,
            &WakeCtx {
                preview: "nsec1secretkey",
                ..Default::default()
            },
        );
        assert!(!with_secret.contains("Summary:"));
        assert!(!with_secret.contains("nsec1"));

        let jargon_sample = format!("{}-P ring", "plane");
        let with_jargon = render(
            EventClass::PlanePMail,
            &WakeCtx {
                preview: &jargon_sample,
                ..Default::default()
            },
        );
        assert!(!with_jargon.contains("Summary:"));
    }
}
