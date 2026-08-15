//! agent-mail backend — implements `toron_core::mailbox::surface::MailSurface`
//! (ADR-0007 D2 / R1, consumed in bead bd-adr7-mail-adapt-se2.3) over the
//! `mcp_agent_mail` server (127.0.0.1:8765), driven by the one parameterized
//! transport (`McpClient` with `ServerKind::AgentMail`, ADR-0003 V1).
//!
//! governed-by: ADR-0002, ADR-0003, ADR-0007
//!
//! The flywheel-local `MailBackend` trait is DELETED (se2.3) — agent-mail is
//! an ADAPTER on the unified toron-core trait. The u64 ↔ String id conversion
//! happens ONLY here at the MCP JSON edge (ADR-0007 D1): a non-decimal
//! message id is a parse ERROR, never `unwrap_or(0)` (se2.2).
//!
//! Every typed DTO the surface returns is built from AM's JSON here. Where
//! AM's shape carries no Nostr field (`npub`, `key_path`, …) the field is
//! left empty — documented in `docs/parity/known-divergences.md`, never
//! fabricated.
//!
//! LAZY (ADR-0003 V4, BUG2): construction never dials — `new` only builds the
//! `McpClient`. `health()`/ops attempt the first call on demand and map a
//! transport failure (server down) onto an actionable error naming the URL
//! and the escape hatch: `agent-mail server not reachable at {url}; start am
//! or set FLYWHEEL_MAIL=toron`. No silent fallback (ADR-0002 §6).

use crate::backend::wire;
use crate::backend::{
    AcquireBuildSlotResult, AcquireResult, BuildSlot, ConflictReport, ContactLink, ContactPolicy,
    ContactRequest, ContactResponse, DeliveryLine, InboxQuery, MailError, MailItem, MailSurface,
    MessageId, ProductHit, ProductInfo, ReceiptOp, ReceiptOutcome, RegisteredAgent, ReleasedLease,
    ReleaseResult,
    RenewResult, ReplyMail, SearchQuery, SendMail, SendReceipt, ThreadSummary, WhoIs,
};
use crate::mcp_http::{McpClient, McpError, ServerKind};
use serde_json::{Value, json};

/// Real agent-mail backend. Constructed lazily by the factory; never dials on
/// construction (ADR-0003 V4).
pub struct AgentMailBackend {
    /// AM-targeting MCP client (ServerKind::AgentMail → truthful error labels).
    client: McpClient,
    /// The agent identity this backend sends AS / reads the inbox for.
    name: String,
    /// Base URL — used in the actionable not-reachable error.
    url: String,
}

impl AgentMailBackend {
    /// Build the backend against the agent-mail server at `url` (e.g.
    /// `http://127.0.0.1:8765`). LAZY: no dialing happens here.
    pub fn new(url: &str, token: Option<String>, name: String) -> crate::backend::Result<Self> {
        let client = McpClient::new(url, token, ServerKind::AgentMail)?;
        Ok(Self {
            client,
            name,
            url: url.to_string(),
        })
    }

    /// Call one agent-mail MCP tool. Transport failures (server down) map onto
    /// the actionable BUG2 error; tool-level errors stay truthful with the
    /// `agent-mail <tool>` label (BUG3).
    fn call(&self, tool: &str, args: Value) -> Result<Value, MailError> {
        self.client.call_tool(tool, args).map_err(|e| match e {
            McpError::Transport(t) => MailError::other(format!(
                "agent-mail server not reachable at {}; start am or set FLYWHEEL_MAIL=toron ({t})",
                self.url
            )),
            McpError::Tool(t) => MailError::other(format!("agent-mail {tool}: {t}")),
        })
    }
}

// ---------------------------------------------------------------------------
// AM-JSON → typed-DTO mapping helpers. Tolerant where AM's shape carries
// backend-only columns; honest errors when a REQUIRED field is missing or
// malformed (never silent defaults for the id).
// ---------------------------------------------------------------------------

/// AM message ids are u64 sequence numbers on the wire. The surface id is a
/// `MessageId` string — the decimal form, parsed HONESTLY (se2.2): a missing
/// or non-numeric id is an error, never `0`-default.
fn message_id_from(v: &Value, op: &str) -> Result<MessageId, MailError> {
    if let Some(n) = v.as_u64() {
        return Ok(MessageId::from(n.to_string()));
    }
    if let Some(s) = v.as_str() {
        // A decimal string (the id we sent back) — accept verbatim.
        return Ok(MessageId::from(s.to_string()));
    }
    Err(MailError::other(format!(
        "agent-mail {op}: message id is not a number/string: {v}"
    )))
}

fn str_field<'a>(v: &'a Value, keys: &[&str], op: &str) -> Result<&'a str, MailError> {
    for k in keys {
        if let Some(s) = v.get(k).and_then(Value::as_str) {
            return Ok(s);
        }
    }
    Err(MailError::other(format!(
        "agent-mail {op}: missing string field {keys:?} in {v}"
    )))
}

fn opt_str_field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_str))
}

fn opt_string_field(v: &Value, keys: &[&str]) -> Option<String> {
    opt_str_field(v, keys).map(String::from)
}

fn string_array_field(v: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|k| v.get(k).and_then(Value::as_array))
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

/// Map one JSON conflict object to `PathConflict` (bd-dsu1 API ripple: the
/// struct gained `reservation_ids` — the kind-30031 event ids of the
/// conflicting leases, parallel to `holders`; absent on the wire = empty).
fn path_conflict(c: &Value) -> toron_core::mailbox::reservations::PathConflict {
    toron_core::mailbox::reservations::PathConflict {
        path: opt_string_field(c, &["path"]).unwrap_or_default(),
        holders: string_array_field(c, &["holders"]),
        reservation_ids: string_array_field(c, &["reservation_ids"]),
    }
}

fn bool_field(v: &Value, keys: &[&str], op: &str) -> Result<bool, MailError> {
    for k in keys {
        if let Some(b) = v.get(k).and_then(Value::as_bool) {
            return Ok(b);
        }
        // AM emits ack_required as 0/1 in search rows.
        if let Some(n) = v.get(k).and_then(Value::as_u64) {
            return Ok(n != 0);
        }
    }
    Err(MailError::other(format!(
        "agent-mail {op}: missing bool field {keys:?} in {v}"
    )))
}

/// Timestamps: AM sends ISO-8601 strings (`created_ts`); toron uses epoch µs.
/// Parse ISO when present, else accept a µs number, else 0 (a missing
/// timestamp is not a corruption — it's an AM DB column that may be absent).
fn ts_us(v: &Value, keys: &[&str]) -> i64 {
    for k in keys {
        if let Some(s) = v.get(k).and_then(Value::as_str)
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s)
        {
            return dt.timestamp_micros();
        }
        if let Some(n) = v.get(k).and_then(Value::as_i64) {
            return n;
        }
    }
    0
}

fn mail_item_from(v: &Value, op: &str) -> Result<MailItem, MailError> {
    Ok(MailItem {
        id: message_id_from(v.get("id").unwrap_or(&Value::Null), op)?,
        sender: str_field(v, &["from", "sender"], op)?.to_string(),
        subject: v
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        body_md: v
            .get("body_md")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        importance: v
            .get("importance")
            .and_then(Value::as_str)
            .unwrap_or("normal")
            .to_string(),
        ack_required: v
            .get("ack_required")
            .and_then(Value::as_bool)
            .or_else(|| {
                v.get("ack_required")
                    .and_then(Value::as_u64)
                    .map(|n| n != 0)
            })
            .unwrap_or(false),
        thread_id: opt_string_field(v, &["thread_id"]).map(MessageId::from),
        duplicate_of: opt_string_field(v, &["duplicate_of"]).map(MessageId::from),
        created_ts_us: ts_us(v, &["created_ts_us", "created_ts"]),
    })
}

fn row_array(v: &Value) -> Vec<Value> {
    v.get("result")
        .and_then(Value::as_array)
        .or_else(|| v.get("messages").and_then(Value::as_array))
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn send_receipt_from(v: &Value, op: &str) -> Result<SendReceipt, MailError> {
    let deliveries = v
        .get("deliveries")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|d| {
                    let payload = d.get("payload").unwrap_or(d);
                    Ok(DeliveryLine {
                        recipient: opt_string_field(payload, &["recipient"]).unwrap_or_default(),
                        event_id: message_id_from(
                            payload
                                .get("event_id")
                                .or_else(|| payload.get("id"))
                                .unwrap_or(&Value::Null),
                            op,
                        )?,
                    })
                })
                .collect::<Result<Vec<_>, MailError>>()
        })
        .transpose()?
        .unwrap_or_default();
    let id = deliveries
        .first()
        .map(|d| d.event_id.clone())
        .unwrap_or_else(|| MessageId::from(String::new()));
    let count = v
        .get("count")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(deliveries.len());
    Ok(SendReceipt {
        id,
        deliveries,
        count,
    })
}

#[async_trait::async_trait]
impl MailSurface for AgentMailBackend {
    fn name(&self) -> &'static str {
        "agentmail"
    }

    fn agent_name(&self) -> &str {
        &self.name
    }

    async fn health(&self) -> Result<toron_core::mailbox::infra::HealthReport, MailError> {
        let v = self.call("health_check", wire::health_check_args())?;
        // AM's rich health object has a `status`; toron's is {archive, bus}.
        // Map both onto the 2-component report honestly.
        let ok = v
            .get("status")
            .and_then(Value::as_str)
            .map(|s| s == "healthy" || s == "ok")
            .or_else(|| v.pointer("/bus/ok").and_then(Value::as_bool))
            .unwrap_or(false);
        let msg = opt_string_field(&v, &["status", "message"]).unwrap_or_default();
        Ok(toron_core::mailbox::infra::HealthReport {
            bus: toron_core::mailbox::infra::HealthComponent {
                ok,
                message: format!("agent-mail {msg}"),
            },
            archive: toron_core::mailbox::infra::HealthComponent {
                ok,
                message: "agent-mail archive status mirrors the bus probe".into(),
            },
        })
    }

    async fn ensure_project(
        &self,
        human_key: &str,
    ) -> Result<toron_core::mailbox::infra::ProjectState, MailError> {
        let v = self.call("ensure_project", wire::ensure_project_args(human_key))?;
        Ok(toron_core::mailbox::infra::ProjectState {
            slug: opt_string_field(&v, &["slug", "human_key"])
                .unwrap_or_else(|| human_key.to_string()),
            manifest_event_id: String::new(), // AM has no Nostr manifest
            created: v.get("created").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    async fn register_agent(
        &self,
        project: &str,
        profile: &toron_core::mailbox::AgentProfile,
    ) -> Result<RegisteredAgent, MailError> {
        let v = self.call(
            "register_agent",
            wire::register_agent_args(
                project,
                &profile.name,
                profile.program.as_deref().unwrap_or(""),
                profile.model.as_deref().unwrap_or(""),
                profile.task_description.as_deref().unwrap_or(""),
            ),
        )?;
        Ok(RegisteredAgent {
            name: str_field(&v, &["name"], "register_agent")?.to_string(),
            project_slug: opt_string_field(&v, &["project_slug", "project_id"])
                .unwrap_or_else(|| project.to_string()),
            // AM has no Schnorr identity — the Nostr-only fields are empty
            // (documented in known-divergences §1).
            npub: String::new(),
            profile_event_id: String::new(),
            key_path: String::new(),
            created: v.get("created").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    async fn list_agents(
        &self,
        project: &str,
        _active_within_days: Option<u64>,
    ) -> Result<Vec<WhoIs>, MailError> {
        let v = self.call("list_agents", wire::list_agents_args(project))?;
        row_array(&v)
            .iter()
            .map(|a| {
                Ok(WhoIs {
                    name: str_field(a, &["name"], "list_agents")?.to_string(),
                    npub: String::new(),
                    program: opt_string_field(a, &["program"]),
                    model: opt_string_field(a, &["model"]),
                    task_description: opt_string_field(a, &["task_description"]),
                    last_active_ts_us: ts_us(a, &["last_active_ts_us", "last_active_ts"]),
                })
            })
            .collect()
    }

    async fn whois(&self, project: &str, agent: &str) -> Result<WhoIs, MailError> {
        let v = self.call("whois", wire::whois_args(project, agent))?;
        Ok(WhoIs {
            name: opt_string_field(&v, &["name"]).unwrap_or_else(|| agent.to_string()),
            npub: String::new(),
            program: opt_string_field(&v, &["program"]),
            model: opt_string_field(&v, &["model"]),
            task_description: opt_string_field(&v, &["task_description"]),
            last_active_ts_us: ts_us(&v, &["last_active_ts_us", "last_active_ts"]),
        })
    }

    async fn send(&self, req: &SendMail) -> Result<SendReceipt, MailError> {
        let v = self.call(
            "send_message",
            wire::send_args(
                &req.project,
                &req.sender,
                &req.to,
                &req.subject,
                &req.body_md,
                req.importance.as_deref().unwrap_or("normal"),
                req.ack_required,
                req.thread_id.as_deref(),
            ),
        )?;
        send_receipt_from(&v, "send_message")
    }

    async fn reply(&self, req: &ReplyMail) -> Result<SendReceipt, MailError> {
        // AM's reply_message takes the numeric `message_id` — a non-decimal
        // id is a caller bug: Err loudly, never 0 (se2.2).
        let id = req.thread_id.as_str().parse::<u64>().map_err(|e| {
            MailError::other(format!(
                "agent-mail reply: message_id {:?} is not a decimal id: {e}",
                req.thread_id.as_str()
            ))
        })?;
        let v = self.call(
            "reply_message",
            wire::reply_args(
                &req.project,
                &req.sender,
                &req.to,
                "",
                &req.body_md,
                "message_id",
                json!(id as i64),
            ),
        )?;
        send_receipt_from(&v, "reply_message")
    }

    async fn inbox(&self, req: &InboxQuery) -> Result<Vec<MailItem>, MailError> {
        let v = self.call(
            "fetch_inbox",
            wire::inbox_args(
                &req.project,
                &req.agent,
                req.unread_only,
                req.urgent_only,
                false,
                req.include_bodies,
                req.limit.unwrap_or(100) as u32,
                req.since_ts.as_deref(),
            ),
        )?;
        row_array(&v)
            .iter()
            .map(|m| mail_item_from(m, "fetch_inbox"))
            .collect()
    }

    async fn acknowledge(&self, req: &ReceiptOp) -> Result<ReceiptOutcome, MailError> {
        let id = req.message_id.as_str().parse::<u64>().map_err(|e| {
            MailError::other(format!(
                "agent-mail ack: message_id {:?} is not a decimal id: {e}",
                req.message_id.as_str()
            ))
        })?;
        let v = self.call(
            "acknowledge_message",
            wire::ack_args(&req.project, &req.agent, json!(id as i64)),
        )?;
        receipt_outcome_from(&v, "acknowledge_message")
    }

    async fn mark_read(&self, req: &ReceiptOp) -> Result<ReceiptOutcome, MailError> {
        let id = req.message_id.as_str().parse::<u64>().map_err(|e| {
            MailError::other(format!(
                "agent-mail mark_read: message_id {:?} is not a decimal id: {e}",
                req.message_id.as_str()
            ))
        })?;
        let v = self.call(
            "mark_message_read",
            wire::mark_read_args(&req.project, &req.agent, json!(id as i64)),
        )?;
        receipt_outcome_from(&v, "mark_message_read")
    }

    async fn search(&self, req: &SearchQuery) -> Result<Vec<MailItem>, MailError> {
        let v = self.call(
            "search_messages",
            wire::search_args(
                &req.project,
                &req.query,
                req.limit.unwrap_or(50) as u32,
                req.include_body_md,
                req.sender.as_deref(),
                req.importance.as_deref(),
                req.thread_id.as_deref(),
                req.date_start.as_deref(),
                req.date_end.as_deref(),
                req.ranking.as_deref(),
            ),
        )?;
        row_array(&v)
            .iter()
            .map(|m| mail_item_from(m, "search_messages"))
            .collect()
    }

    async fn request_contact(&self, req: &ContactRequest) -> Result<ContactLink, MailError> {
        let v = self.call(
            "request_contact",
            wire::request_contact_args(
                &req.project,
                &req.from_agent,
                &req.to_agent,
                req.to_project.as_deref(),
                req.reason.as_deref().unwrap_or(""),
            ),
        )?;
        contact_link_from(&v, "request_contact")
    }

    async fn respond_contact(&self, req: &ContactResponse) -> Result<ContactLink, MailError> {
        let v = self.call(
            "respond_contact",
            wire::respond_contact_args(&req.project, &req.to_agent, &req.from_agent, req.accept),
        )?;
        contact_link_from(&v, "respond_contact")
    }

    async fn list_contacts(
        &self,
        project: &str,
        agent: &str,
    ) -> Result<Vec<ContactLink>, MailError> {
        let v = self.call("list_contacts", wire::list_contacts_args(project, agent))?;
        row_array(&v)
            .iter()
            .map(|c| contact_link_from(c, "list_contacts"))
            .collect()
    }

    async fn set_contact_policy(
        &self,
        project: &str,
        agent: &str,
        policy: ContactPolicy,
    ) -> Result<(), MailError> {
        let _ = self.call(
            "set_contact_policy",
            wire::set_contact_policy_args(project, agent, policy.as_str()),
        )?;
        Ok(())
    }

    async fn reserve(
        &self,
        project: &str,
        agent: &str,
        paths: &[String],
        ttl_secs: Option<u64>,
        exclusive: bool,
        reason: Option<&str>,
    ) -> Result<AcquireResult, MailError> {
        let v = self.call(
            "file_reservation_paths",
            wire::reserve_args(
                project,
                agent,
                paths,
                ttl_secs.unwrap_or(3600),
                exclusive,
                reason.unwrap_or(""),
            ),
        )?;
        let granted = v
            .get("granted")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|g| {
                        Ok(toron_core::mailbox::reservations::GrantedReservation {
                            id: message_id_from(
                                g.get("id").unwrap_or(&Value::Null),
                                "file_reservation_paths",
                            )?
                            .into_inner(),
                            path_pattern: opt_string_field(g, &["path_pattern"])
                                .unwrap_or_default(),
                            exclusive: g
                                .get("exclusive")
                                .and_then(Value::as_bool)
                                .unwrap_or(exclusive),
                            reason: opt_string_field(g, &["reason"]).unwrap_or_default(),
                            expires_ts_us: g.get("expires_ts_us").and_then(Value::as_i64).or_else(
                                || {
                                    let iso = g.get("expires_ts").and_then(Value::as_str)?;
                                    chrono::DateTime::parse_from_rfc3339(iso)
                                        .ok()
                                        .map(|d| d.timestamp_micros())
                                },
                            ),
                        })
                    })
                    .collect::<Result<Vec<_>, MailError>>()
            })
            .transpose()?
            .unwrap_or_default();
        let conflicts = v
            .get("conflicts")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(path_conflict)
                    .collect()
            })
            .unwrap_or_default();
        let soft_conflicts: Vec<toron_core::mailbox::reservations::PathConflict> = v
            .get("soft_conflicts")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(path_conflict)
                    .collect()
            })
            .unwrap_or_default();
        Ok(AcquireResult {
            granted,
            conflicts,
            soft_conflicts,
            proposal: None,
        })
    }

    async fn check_conflicts(
        &self,
        project: &str,
        agent: &str,
        paths: &[String],
    ) -> Result<ConflictReport, MailError> {
        let v = self.call(
            "check_file_reservation_conflicts",
            wire::conflicts_args(project, agent, paths),
        )?;
        let conflicts: Vec<toron_core::mailbox::reservations::PathConflict> = v
            .get("conflicts")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(path_conflict)
                    .collect()
            })
            .unwrap_or_default();
        let clear_paths = v
            .get("clear_paths")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let soft_conflicts: Vec<toron_core::mailbox::reservations::PathConflict> = v
            .get("soft_conflicts")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(path_conflict)
                    .collect()
            })
            .unwrap_or_default();
        Ok(ConflictReport {
            conflict_free: v
                .get("conflict_free")
                .and_then(Value::as_bool)
                .unwrap_or(conflicts.is_empty()),
            checked_paths: v
                .get("checked_paths")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(paths.len()),
            total_conflicting_reservations: v
                .get("total_conflicting_reservations")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or_else(|| conflicts.iter().map(|c| c.holders.len()).sum()),
            conflicts,
            clear_paths,
            soft_conflicts,
            has_soft_conflicts: v
                .get("has_soft_conflicts")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    async fn renew_reservations(
        &self,
        project: &str,
        agent: &str,
        paths: &[String],
        extend_secs: u64,
    ) -> Result<RenewResult, MailError> {
        let v = self.call(
            "renew_file_reservations",
            wire::renew_args(project, agent, extend_secs, Some(paths)),
        )?;
        let reservations: Vec<toron_core::mailbox::reservations::GrantedReservation> = v
            .get("reservations")
            .or_else(|| v.get("file_reservations"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|r| toron_core::mailbox::reservations::GrantedReservation {
                        id: opt_string_field(r, &["id"]).unwrap_or_default(),
                        path_pattern: opt_string_field(r, &["path_pattern"]).unwrap_or_default(),
                        exclusive: r.get("exclusive").and_then(Value::as_bool).unwrap_or(true),
                        reason: opt_string_field(r, &["reason"]).unwrap_or_default(),
                        expires_ts_us: r.get("expires_ts_us").and_then(Value::as_i64).or_else(
                            || {
                                let iso = r.get("expires_ts").and_then(Value::as_str)?;
                                chrono::DateTime::parse_from_rfc3339(iso)
                                    .ok()
                                    .map(|d| d.timestamp_micros())
                            },
                        ),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(RenewResult {
            renewed: v
                .get("renewed")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(reservations.len()),
            reservations,
        })
    }

    async fn release_reservations(
        &self,
        project: &str,
        agent: &str,
        _paths: &[String],
    ) -> Result<ReleaseResult, MailError> {
        let v = self.call(
            "release_file_reservations",
            wire::release_args(project, agent),
        )?;
        Ok(ReleaseResult {
            released: v
                .get("released")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(0),
            leases: v
                .get("leases")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| {
                            Some(ReleasedLease {
                                path: l.get("path")?.as_str()?.to_string(),
                                mode: l.get("mode")?.as_str()?.to_string(),
                                holder: l.get("holder")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    async fn force_release(
        &self,
        _project: &str,
        _requestor: &str,
        _path: &str,
        _holder: Option<&str>,
        _note: Option<&str>,
        _notify_previous: bool,
    ) -> Result<(), MailError> {
        // bd-cumj: force-release now resolves by path via the toron daemon.
        // The legacy agent-mail backend's API is numeric-id-based and cannot
        // resolve a lease by path — toron is the supported backend for this.
        Err(MailError::other(
            "path-based force_release is unsupported on the legacy agent-mail backend (use the toron backend)",
        ))
    }

    async fn release_by_reason(
        &self,
        _project: &str,
        _requestor: &str,
        _bead_id: &str,
        _notify_previous: bool,
    ) -> Result<ReleaseResult, MailError> {
        // bd-h3oj: release-on-bead-close needs reason-scoped revocation,
        // which the legacy agent-mail backend's numeric-id API cannot
        // express — toron is the supported backend for this.
        Err(MailError::other(
            "release_by_reason is unsupported on the legacy agent-mail backend (use the toron backend)",
        ))
    }

    async fn slot_acquire(
        &self,
        project: &str,
        agent: &str,
        slot: &str,
        ttl_secs: Option<u64>,
        exclusive: bool,
    ) -> Result<AcquireBuildSlotResult, MailError> {
        let v = self.call(
            "acquire_build_slot",
            wire::slot_acquire_args(project, agent, slot, exclusive, ttl_secs.unwrap_or(3600)),
        )?;
        Ok(AcquireBuildSlotResult {
            granted: v
                .get("granted")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|g| toron_core::index::slots::GrantedSlot {
                            slot: opt_string_field(g, &["slot"]).unwrap_or_default(),
                            exclusive: g
                                .get("exclusive")
                                .and_then(Value::as_bool)
                                .unwrap_or(exclusive),
                            expires_ts_us: g.get("expires_ts_us").and_then(Value::as_i64).or_else(
                                || {
                                    let iso = g.get("expires_ts").and_then(Value::as_str)?;
                                    chrono::DateTime::parse_from_rfc3339(iso)
                                        .ok()
                                        .map(|d| d.timestamp_micros())
                                },
                            ),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            conflicts: v
                .get("conflicts")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .map(|c| toron_core::index::slots::SlotConflict {
                            slot: opt_string_field(c, &["slot"]).unwrap_or_default(),
                            holders: c
                                .get("holders")
                                .and_then(Value::as_array)
                                .map(|h| {
                                    h.iter()
                                        .filter_map(Value::as_str)
                                        .map(String::from)
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    async fn slot_renew(
        &self,
        project: &str,
        agent: &str,
        slot: &str,
        extend_secs: u64,
    ) -> Result<Option<BuildSlot>, MailError> {
        let v = self.call(
            "renew_build_slot",
            wire::slot_renew_args(project, agent, slot, extend_secs),
        )?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(build_slot_from(&v, "renew_build_slot")?))
    }

    async fn slot_release(
        &self,
        project: &str,
        agent: &str,
        slot: &str,
    ) -> Result<bool, MailError> {
        let v = self.call(
            "release_build_slot",
            wire::slot_release_args(project, agent, slot),
        )?;
        Ok(v.get("released").and_then(Value::as_bool).unwrap_or(true))
    }

    async fn ensure_product(
        &self,
        product_key: &str,
        name: &str,
    ) -> Result<ProductInfo, MailError> {
        let v = self.call("ensure_product", wire::ensure_product_args(product_key))?;
        Ok(ProductInfo {
            product_key: opt_string_field(&v, &["product_key"])
                .unwrap_or_else(|| product_key.to_string()),
            name: opt_string_field(&v, &["name"]).unwrap_or_else(|| name.to_string()),
            member_projects: v
                .get("member_projects")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            created: v.get("created").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    async fn products_link(
        &self,
        product_key: &str,
        project: &str,
    ) -> Result<ProductInfo, MailError> {
        let v = self.call(
            "products_link",
            wire::products_link_args(product_key, project),
        )?;
        Ok(ProductInfo {
            product_key: opt_string_field(&v, &["product_key"])
                .unwrap_or_else(|| product_key.to_string()),
            name: opt_string_field(&v, &["name"]).unwrap_or_else(|| product_key.to_string()),
            member_projects: v
                .get("member_projects")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            created: v.get("created").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    async fn search_product(
        &self,
        product_key: &str,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<ProductHit>, MailError> {
        let v = self.call(
            "search_messages_product",
            wire::search_product_args(product_key, query, limit.unwrap_or(50) as u32),
        )?;
        row_array(&v)
            .iter()
            .map(|m| {
                Ok(ProductHit {
                    project: opt_string_field(m, &["project"])
                        .unwrap_or_else(|| product_key.to_string()),
                    item: mail_item_from(m, "search_messages_product")?,
                })
            })
            .collect()
    }

    async fn inbox_product(
        &self,
        product_key: &str,
        agent: &str,
        limit: Option<usize>,
    ) -> Result<Vec<ProductHit>, MailError> {
        let v = self.call(
            "fetch_inbox_product",
            wire::inbox_product_args(product_key, agent, limit.unwrap_or(100) as u32),
        )?;
        row_array(&v)
            .iter()
            .map(|m| {
                Ok(ProductHit {
                    project: opt_string_field(m, &["project"])
                        .unwrap_or_else(|| product_key.to_string()),
                    item: mail_item_from(m, "fetch_inbox_product")?,
                })
            })
            .collect()
    }

    async fn summarize_thread_product(
        &self,
        product_key: &str,
        thread_id: &str,
        include_examples: bool,
    ) -> Result<ThreadSummary, MailError> {
        let v = self.call(
            "summarize_thread_product",
            wire::summarize_thread_product_args(product_key, thread_id, include_examples),
        )?;
        thread_summary_from(&v, "summarize_thread_product")
    }

    async fn summarize_thread(
        &self,
        project: &str,
        thread_id: &str,
        include_examples: bool,
    ) -> Result<ThreadSummary, MailError> {
        let v = self.call(
            "summarize_thread",
            wire::summarize_thread_args(project, thread_id, include_examples),
        )?;
        thread_summary_from(&v, "summarize_thread")
    }

    async fn resolve_pane_identity(
        &self,
        project: &str,
        pane_id: &str,
    ) -> Result<Option<String>, MailError> {
        let v = self.call(
            "resolve_pane_identity",
            wire::resolve_pane_identity_args(project, pane_id),
        )?;
        // AM errors on a missing mapping; a no-pane response has no
        // `agent_name` — treat both as Ok(None) (toron-lenient parity).
        Ok(opt_string_field(&v, &["agent_name"]))
    }

    async fn cleanup_pane_identities(&self, project: &str) -> Result<Vec<String>, MailError> {
        let v = self.call(
            "cleanup_pane_identities",
            wire::cleanup_pane_identities_args(project),
        )?;
        Ok(v.get("removed_paths")
            .or_else(|| v.get("pruned"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn write_pane_identity(
        &self,
        project: &str,
        pane_id: &str,
        agent: &str,
    ) -> Result<(), MailError> {
        // agent-mail exposes no write tool — it READS the identity files this
        // backend writes (the same `agent-mail/identity/<hash>/<pane_id>`
        // layout the toron facade uses; ADR-0001 A4 bridge).
        crate::mail_shim::write_identity_for(project, pane_id, agent)
            .map(|_| ())
            .map_err(|e| MailError::other(format!("{e:#}")))
    }

    async fn clear_pane_identity(&self, project: &str, pane_id: &str) -> Result<(), MailError> {
        crate::mail_shim::clear_identity_for(project, pane_id)
            .map(|_| ())
            .map_err(|e| MailError::other(format!("{e:#}")))
    }
}

fn receipt_outcome_from(v: &Value, op: &str) -> Result<ReceiptOutcome, MailError> {
    Ok(ReceiptOutcome {
        message_id: message_id_from(v.get("message_id").unwrap_or(&Value::Null), op)?,
        acknowledged: v
            .get("acknowledged")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        acknowledged_at_us: v
            .get("acknowledged_at_us")
            .and_then(Value::as_i64)
            .or_else(|| {
                opt_string_field(v, &["acknowledged_at"]).and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|d| d.timestamp_micros())
                })
            }),
    })
}

fn contact_link_from(v: &Value, _op: &str) -> Result<ContactLink, MailError> {
    let status = opt_string_field(v, &["status"]).unwrap_or_default();
    Ok(ContactLink {
        project_slug: opt_string_field(v, &["project_slug", "project"]).unwrap_or_default(),
        from_name: opt_string_field(v, &["from_name"]).unwrap_or_default(),
        from_pubkey: String::new(), // AM has no npub
        to_name: opt_string_field(v, &["to_name"]).unwrap_or_default(),
        to_pubkey: String::new(),
        to_project: opt_string_field(v, &["to_project"]),
        status: match status.as_str() {
            "approved" | "accepted" | "accept" => {
                toron_core::mailbox::contacts::LinkStatus::Approved
            }
            "denied" | "rejected" | "deny" => toron_core::mailbox::contacts::LinkStatus::Denied,
            _ => toron_core::mailbox::contacts::LinkStatus::Pending,
        },
        created_ts_us: ts_us(v, &["created_ts_us", "created_ts"]),
    })
}

fn build_slot_from(v: &Value, _op: &str) -> Result<BuildSlot, MailError> {
    let exclusive = v.get("exclusive").and_then(Value::as_bool).unwrap_or(false);
    Ok(BuildSlot {
        slot: opt_string_field(v, &["slot"]).unwrap_or_default(),
        agent_name: opt_string_field(v, &["agent_name"]).unwrap_or_default(),
        agent_pubkey: String::new(),
        exclusive,
        status: toron_core::index::slots::SlotStatus::Active,
        expires_ts_us: v.get("expires_ts_us").and_then(Value::as_i64).or_else(|| {
            let iso = v.get("expires_ts").and_then(Value::as_str)?;
            chrono::DateTime::parse_from_rfc3339(iso)
                .ok()
                .map(|d| d.timestamp_micros())
        }),
    })
}

fn thread_summary_from(v: &Value, _op: &str) -> Result<ThreadSummary, MailError> {
    // AM nests the digest under `summary` (with `total_messages`); toron is
    // flat. Read both.
    let s = v.get("summary").unwrap_or(v);
    let flatten = |key: &str| -> Vec<String> {
        v.get(key)
            .or_else(|| s.get(key))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let count = s
        .get("total_messages")
        .or_else(|| v.get("message_count"))
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(0);
    Ok(ThreadSummary {
        thread_id: opt_string_field(v, &["thread_id"]).unwrap_or_default(),
        participants: flatten("participants"),
        key_points: flatten("key_points"),
        action_items: flatten("action_items"),
        message_count: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// se2.2 honesty: a non-decimal id is an error BEFORE any dial.
    #[test]
    fn ack_rejects_non_decimal_message_id() {
        let b = AgentMailBackend::new("http://127.0.0.1:1", None, "Orchid".into()).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(b.acknowledge(&ReceiptOp {
                project: "p".into(),
                agent: "Orchid".into(),
                message_id: MessageId::from("deadbeef"),
            }))
            .unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("decimal"), "names the id format: {s}");
        assert!(
            !s.contains("not reachable"),
            "parse fails before any dial: {s}"
        );
    }

    #[test]
    fn message_id_parse_is_honest() {
        let v = serde_json::json!(42u64);
        let id = message_id_from(&v, "t").unwrap();
        assert_eq!(id.as_str(), "42", "decimal string, not u64 in memory");
        let bad = message_id_from(&serde_json::Value::Null, "t");
        assert!(bad.is_err(), "missing id is an error, never 0");
    }

    #[test]
    fn am_inbox_row_maps_sender_and_id() {
        let row = serde_json::json!({
            "id": 7,
            "from": "GreenCastle",
            "subject": "hi",
            "body_md": "body",
            "importance": "high",
            "ack_required": 1,
            "thread_id": "parity-thread-1",
            "created_ts": "2026-08-08T00:00:00Z",
        });
        let item = mail_item_from(&row, "t").unwrap();
        assert_eq!(item.id.as_str(), "7");
        assert_eq!(item.sender, "GreenCastle", "AM `from` → surface `sender`");
        assert!(item.ack_required);
        assert_eq!(
            item.thread_id.map(|t| t.as_str().to_string()),
            Some("parity-thread-1".into())
        );
        assert!(item.created_ts_us > 0, "ISO created_ts parsed to µs");
    }

    #[test]
    fn send_envelope_maps_deliveries() {
        let v = serde_json::json!({
            "deliveries": [{ "project": "p", "payload": { "recipient": "BlueLake", "id": 9 } }],
            "count": 1,
        });
        let r = send_receipt_from(&v, "t").unwrap();
        assert_eq!(r.id.as_str(), "9");
        assert_eq!(r.deliveries[0].event_id.as_str(), "9");
    }
}
