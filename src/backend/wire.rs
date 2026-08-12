//! Canonical agent-mail wire-argument builders (c3b.5 — single source of
//! truth for the MCP tool argument JSON).
//!
//! BOTH remote backends (`AgentMailBackend` and the `Mail` facade's remote
//! path) call the same MCP server wire via `McpClient::call_tool`, so the
//! argument JSON must be byte-identical between them. Before this module each
//! backend kept an independent copy of the arg-mapping code — a drift
//! compiles fine and silently breaks one backend. Here the mapping lives ONCE;
//! the golden-JSON unit tests at the bottom pin every shape so a contract
//! change is a one-place edit + one test update.
//!
//! Every builder is pure: `(params...) -> Value` with NO I/O. Callers add
//! per-backend coercions (e.g. decimal-vs-hex message ids) BEFORE calling.
//!
//! governed-by: ADR-0003

use serde_json::{Map, Value, json};

/// `health_check` — no args (agent-mail takes none; toron's tool ignores the
/// `project_key` it is called with, so the caller passes it through).
pub fn health_check_args() -> Value {
    json!({})
}

/// `ensure_project` — the project key IS the human key on the wire.
pub fn ensure_project_args(project_key: &str) -> Value {
    json!({ "human_key": project_key })
}

/// `register_agent`.
pub fn register_agent_args(
    project_key: &str,
    name: &str,
    program: &str,
    model: &str,
    task: &str,
) -> Value {
    json!({
        "project_key": project_key,
        "name": name,
        "program": program,
        "model": model,
        "task_description": task,
    })
}

/// `list_agents`.
pub fn list_agents_args(project_key: &str) -> Value {
    json!({ "project_key": project_key })
}

/// `whois`.
pub fn whois_args(project_key: &str, name: &str) -> Value {
    // the daemon's WhoisArgs field is `agent_name` (not `name` — wire drift
    // caught by live test 2026-08-09)
    json!({ "project_key": project_key, "agent_name": name })
}

/// `send_message`.
#[allow(clippy::too_many_arguments)] // mirrors the MailBackend::send trait surface
pub fn send_args(
    project_key: &str,
    sender_name: &str,
    to: &[String],
    subject: &str,
    body: &str,
    importance: &str,
    ack_required: bool,
    thread_id: Option<&str>,
) -> Value {
    let mut args = json!({
        "project_key": project_key,
        "sender_name": sender_name,
        "to": to,
        "subject": subject,
        "body_md": body,
        "importance": importance,
        "ack_required": ack_required,
    });
    if let Some(tid) = thread_id {
        args["thread_id"] = json!(tid);
    }
    args
}

/// `reply_message`. The subject is caller-resolved (`Re: <parent subject>`).
///
/// ADR-0003 V1 divergence (documented, kept in ONE place): agent-mail anchors
/// replies on the numeric `message_id`; the toron daemon anchors on the
/// string `thread_id` (NIP-10 `e` reference). The id KEY is therefore a
/// parameter — the common fields (project/sender/to/subject/body) map once
/// here, and the golden tests pin BOTH final shapes.
pub fn reply_args(
    project_key: &str,
    sender_name: &str,
    to: &[String],
    subject: &str,
    body: &str,
    id_key: &str,
    id: Value,
) -> Value {
    let mut args = json!({
        "project_key": project_key,
        "sender_name": sender_name,
        "to": to,
        "subject": subject,
        "body_md": body,
    });
    args[id_key] = id;
    args
}

/// `fetch_inbox` with the full filter bag (c3b.3 fields included).
#[allow(clippy::too_many_arguments)] // mirrors the InboxFilter struct surface
pub fn inbox_args(
    project_key: &str,
    agent_name: &str,
    urgent_only: bool,
    unread_only: bool,
    ack_overdue_only: bool,
    include_bodies: bool,
    limit: u32,
    since_ts: Option<&str>,
) -> Value {
    let mut args = Map::new();
    args.insert("project_key".into(), json!(project_key));
    args.insert("agent_name".into(), json!(agent_name));
    args.insert("urgent_only".into(), json!(urgent_only));
    args.insert("unread_only".into(), json!(unread_only));
    args.insert("ack_overdue_only".into(), json!(ack_overdue_only));
    args.insert("include_bodies".into(), json!(include_bodies));
    args.insert("limit".into(), json!(limit));
    if let Some(ts) = since_ts {
        args.insert("since_ts".into(), json!(ts));
    }
    Value::Object(args)
}

/// `acknowledge_message`.
pub fn ack_args(project_key: &str, agent_name: &str, message_id: Value) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "message_id": message_id,
    })
}

/// `mark_message_read`.
pub fn mark_read_args(project_key: &str, agent_name: &str, message_id: Value) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "message_id": message_id,
    })
}

/// `file_reservation_paths`.
pub fn reserve_args(
    project_key: &str,
    agent_name: &str,
    paths: &[String],
    ttl_seconds: u64,
    exclusive: bool,
    reason: &str,
) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "paths": paths,
        "ttl_seconds": ttl_seconds as i64,
        "exclusive": exclusive,
        "reason": reason,
    })
}

/// `release_file_reservations`.
pub fn release_args(project_key: &str, agent_name: &str) -> Value {
    json!({ "project_key": project_key, "agent_name": agent_name })
}

/// `release_file_reservations_by_reason` (bd-h3oj — additive daemon tool):
/// revoke every live lease whose reason is the bead id.
pub fn release_by_reason_args(
    project_key: &str,
    requestor_name: &str,
    bead_id: &str,
    notify_previous: bool,
) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": requestor_name,
        "bead_id": bead_id,
        "notify_previous": notify_previous,
    })
}

/// `renew_file_reservations`.
pub fn renew_args(
    project_key: &str,
    agent_name: &str,
    extend_seconds: u64,
    paths: Option<&[String]>,
) -> Value {
    let mut args = json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "extend_seconds": extend_seconds as i64,
    });
    if let Some(p) = paths {
        args["paths"] = json!(p);
    }
    args
}

/// `force_release_file_reservation`. bd-cumj: the daemon resolves the active
/// lease by path (+ optional holder) itself — no caller-supplied event id.
pub fn force_release_args(
    project_key: &str,
    agent_name: &str,
    path: &str,
    holder: Option<&str>,
    note: Option<&str>,
    notify_previous: bool,
) -> Value {
    let mut args = json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "path": path,
        "notify_previous": notify_previous,
    });
    if let Some(h) = holder {
        args["holder"] = json!(h);
    }
    if let Some(n) = note {
        args["note"] = json!(n);
    }
    args
}

/// `check_file_reservation_conflicts`.
pub fn conflicts_args(project_key: &str, agent_name: &str, paths: &[String]) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "paths": paths,
    })
}

/// `resolve_pane_identity`.
pub fn resolve_pane_identity_args(project_key: &str, pane_id: &str) -> Value {
    json!({ "project_key": project_key, "pane_id": pane_id })
}

/// `cleanup_pane_identities`.
pub fn cleanup_pane_identities_args(project_key: &str) -> Value {
    json!({ "project_key": project_key })
}

/// `acquire_build_slot`.
pub fn slot_acquire_args(
    project_key: &str,
    agent_name: &str,
    slot: &str,
    exclusive: bool,
    ttl_seconds: u64,
) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "slot": slot,
        "exclusive": exclusive,
        "ttl_seconds": ttl_seconds as i64,
    })
}

/// `release_build_slot`.
pub fn slot_release_args(project_key: &str, agent_name: &str, slot: &str) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "slot": slot,
    })
}

/// `renew_build_slot`.
pub fn slot_renew_args(
    project_key: &str,
    agent_name: &str,
    slot: &str,
    extend_seconds: u64,
) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "slot": slot,
        "extend_seconds": extend_seconds as i64,
    })
}

/// `search_messages` — only the args the wire accepts are emitted (absent
/// optional filters are omitted, matching agent-mail's optionality).
#[allow(clippy::too_many_arguments)] // mirrors the SearchFilter struct surface
pub fn search_args(
    project_key: &str,
    query: &str,
    limit: u32,
    include_body_md: bool,
    sender: Option<&str>,
    importance: Option<&str>,
    thread_id: Option<&str>,
    date_start: Option<&str>,
    date_end: Option<&str>,
    ranking: Option<&str>,
) -> Value {
    let mut args = Map::new();
    args.insert("project_key".into(), json!(project_key));
    args.insert("query".into(), json!(query));
    args.insert("limit".into(), json!(limit));
    if include_body_md {
        args.insert("include_body_md".into(), json!(true));
    }
    if let Some(s) = sender {
        args.insert("sender".into(), json!(s));
    }
    if let Some(i) = importance {
        args.insert("importance".into(), json!(i));
    }
    if let Some(t) = thread_id {
        args.insert("thread_id".into(), json!(t));
    }
    if let Some(d) = date_start {
        args.insert("date_start".into(), json!(d));
    }
    if let Some(d) = date_end {
        args.insert("date_end".into(), json!(d));
    }
    if let Some(r) = ranking {
        args.insert("ranking".into(), json!(r));
    }
    Value::Object(args)
}

/// `summarize_thread`.
pub fn summarize_thread_args(project_key: &str, thread_id: &str, llm_mode: bool) -> Value {
    json!({
        "project_key": project_key,
        "thread_id": thread_id,
        "llm_mode": llm_mode,
    })
}

/// `summarize_thread_product`.
pub fn summarize_thread_product_args(product_key: &str, thread_id: &str, llm_mode: bool) -> Value {
    json!({
        "product_key": product_key,
        "thread_id": thread_id,
        "llm_mode": llm_mode,
    })
}

/// `request_contact`.
pub fn request_contact_args(
    project_key: &str,
    from_agent: &str,
    to_agent: &str,
    to_project: Option<&str>,
    reason: &str,
) -> Value {
    let mut args = json!({
        "project_key": project_key,
        "from_agent": from_agent,
        "to_agent": to_agent,
        "reason": reason,
    });
    if let Some(tp) = to_project {
        args["to_project"] = json!(tp);
    }
    args
}

/// `respond_contact`.
pub fn respond_contact_args(
    project_key: &str,
    to_agent: &str,
    from_agent: &str,
    accept: bool,
) -> Value {
    json!({
        "project_key": project_key,
        "to_agent": to_agent,
        "from_agent": from_agent,
        "accept": accept,
    })
}

/// `list_contacts`.
pub fn list_contacts_args(project_key: &str, agent_name: &str) -> Value {
    json!({ "project_key": project_key, "agent_name": agent_name })
}

/// `set_contact_policy`.
pub fn set_contact_policy_args(project_key: &str, agent_name: &str, policy: &str) -> Value {
    json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "policy": policy,
    })
}

/// `ensure_product`.
pub fn ensure_product_args(product_key: &str) -> Value {
    json!({ "product_key": product_key })
}

/// `products_link`.
pub fn products_link_args(product_key: &str, project_key: &str) -> Value {
    json!({ "product_key": product_key, "project_key": project_key })
}

/// `search_messages_product`.
pub fn search_product_args(product_key: &str, query: &str, limit: u32) -> Value {
    json!({
        "product_key": product_key,
        "query": query,
        "limit": limit,
        "include_body_md": true,
    })
}

/// `fetch_inbox_product`.
pub fn inbox_product_args(product_key: &str, agent_name: &str, limit: u32) -> Value {
    json!({
        "product_key": product_key,
        "agent_name": agent_name,
        "limit": limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden JSON: every builder's exact wire shape, so a contract drift is
    /// caught WITHOUT any daemon (the offline replacement for the live parity
    /// test's coverage).
    #[test]
    fn golden_wire_shapes() {
        assert_eq!(ensure_project_args("p"), json!({ "human_key": "p" }));
        assert_eq!(
            register_agent_args("p", "n", "prog", "model", "task"),
            json!({
                "project_key": "p",
                "name": "n",
                "program": "prog",
                "model": "model",
                "task_description": "task",
            })
        );
        assert_eq!(
            send_args(
                "p",
                "sender",
                &["a".into(), "b".into()],
                "subj",
                "body",
                "high",
                true,
                None
            ),
            json!({
                "project_key": "p",
                "sender_name": "sender",
                "to": ["a", "b"],
                "subject": "subj",
                "body_md": "body",
                "importance": "high",
                "ack_required": true,
            })
        );
        // thread_id present only when Some.
        let with_tid = send_args("p", "s", &[], "sub", "b", "low", false, Some("t1"));
        assert_eq!(with_tid["thread_id"], json!("t1"));
        assert_eq!(
            reply_args(
                "p",
                "s",
                &["a".into()],
                "Re: subj",
                "body",
                "message_id",
                json!(42)
            ),
            json!({
                "project_key": "p",
                "sender_name": "s",
                "to": ["a"],
                "subject": "Re: subj",
                "body_md": "body",
                "message_id": 42,
            })
        );
        // toron divergence: same common fields, `thread_id` string key.
        assert_eq!(
            reply_args(
                "p",
                "s",
                &["a".into()],
                "Re: subj",
                "body",
                "thread_id",
                json!("hex-abc123")
            ),
            json!({
                "project_key": "p",
                "sender_name": "s",
                "to": ["a"],
                "subject": "Re: subj",
                "body_md": "body",
                "thread_id": "hex-abc123",
            })
        );
        assert_eq!(
            inbox_args(
                "p",
                "a",
                true,
                false,
                true,
                true,
                5,
                Some("2026-01-01T00:00:00Z")
            ),
            json!({
                "project_key": "p",
                "agent_name": "a",
                "urgent_only": true,
                "unread_only": false,
                "ack_overdue_only": true,
                "include_bodies": true,
                "limit": 5,
                "since_ts": "2026-01-01T00:00:00Z",
            })
        );
        // since_ts omitted when None.
        let no_since = inbox_args("p", "a", false, false, false, false, 10, None);
        assert!(no_since.get("since_ts").is_none());
        assert_eq!(
            reserve_args("p", "a", &["src/x.rs".into()], 3600, true, "bd-x"),
            json!({
                "project_key": "p",
                "agent_name": "a",
                "paths": ["src/x.rs"],
                "ttl_seconds": 3600,
                "exclusive": true,
                "reason": "bd-x",
            })
        );
        assert_eq!(
            search_args(
                "p",
                "query",
                5,
                true,
                Some("s"),
                Some("high"),
                Some("t"),
                Some("2026-01-01"),
                Some("2026-02-01"),
                Some("recency")
            ),
            json!({
                "project_key": "p",
                "query": "query",
                "limit": 5,
                "include_body_md": true,
                "sender": "s",
                "importance": "high",
                "thread_id": "t",
                "date_start": "2026-01-01",
                "date_end": "2026-02-01",
                "ranking": "recency",
            })
        );
        // sparse search: only present filters emitted.
        let sparse = search_args("p", "q", 3, false, None, None, None, None, None, None);
        assert_eq!(
            sparse,
            json!({ "project_key": "p", "query": "q", "limit": 3 })
        );
    }
}
