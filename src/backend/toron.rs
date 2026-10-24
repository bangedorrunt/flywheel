//! toron mail backend — implements `toron_core::mailbox::surface::MailSurface`
//! (ADR-0007 D2 / R1, consumed in bead bd-adr7-mail-adapt-se2.3) over the
//! `Mail` facade (the toron transport: integrated in-process mailbox or the
//! remote `toron serve --mcp-http` daemon).
//!
//! governed-by: ADR-0002, ADR-0003, ADR-0007
//!
//! This file is the TORON side of the unified mail surface (bd-euav: the
//! only backend). The flywheel-local
//! `MailBackend` trait was deleted with se2.3 — toron implements
//! the ONE trait from toron-core. Value→DTO conversion happens
//! here at the facade edge: the facade's JSON is the AM-parity wire shape
//! (`toron_mcp::ToJson`), and every surface DTO derives `Deserialize`, so the
//! conversion is `serde_json::from_value` except where the envelope differs
//! (send receipt, renew) — mapped explicitly, never with silent defaults.
//!
//! Runtime note: the facade methods are SYNC (they `block_on` the shared
//! tokio runtime — c3b.1). The surface methods are async. Calling the facade
//! synchronously from inside an async fn running ON the same shared runtime
//! would nest `block_on` (panic). Every facade call therefore moves to a
//! `tokio::task::spawn_blocking` thread first (F8-style), where its
//! `block_on(shared)` is a plain thread→runtime bridge. No per-op runtime is
//! created; the shared runtime's blocking pool is reused.

use serde::Deserialize;
use serde_json::Value;

use crate::backend::AgentProfile;
use crate::backend::{
    AcquireBuildSlotResult, AcquireResult, BuildSlot, ConflictReport, ContactLink, ContactRequest,
    ContactResponse, DeliveryLine, InboxQuery, MailError, MailItem, MailSurface, MessageId,
    ProductHit, ProductInfo, ReceiptOp, ReceiptOutcome, RegisteredAgent, ReleaseResult,
    RenewResult, ReplyMail, SearchQuery, SendMail, SendReceipt, ThreadSummary, WhoIs,
};
use crate::mail::{Mail, MailInboxFilter, MailSearchFilter};
use toron_core::mailbox::unwrap::ContactPolicy;

/// Toron mail backend: delegates to the toron `Mail` facade (`Mail::from_config`
/// picks embedded in-process vs the remote `[mail.toron]` daemon).
pub struct ToronMailBackend {
    mail: Mail,
}

impl ToronMailBackend {
    /// Construct from the `Mail` facade (which `Mail::from_config` builds;
    /// `[mail.toron]` config is optional — in-process by default, remote
    /// daemon when `url` is set).
    pub fn new(mail: Mail) -> Self {
        Self { mail }
    }
}

/// Run one sync facade op (which internally `block_on`s the shared runtime)
/// from the async surface WITHOUT nesting `block_on` on the same runtime:
/// `spawn_blocking` moves the call to a blocking thread first (F8-style).
async fn run_facade<T>(
    mail: Mail,
    f: impl FnOnce(&Mail) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, MailError>
where
    T: Send + 'static,
{
    let (mail, f) = (mail.clone(), f);
    tokio::task::spawn_blocking(move || f(&mail))
        .await
        .map_err(|e| MailError::other(format!("facade task join failed: {e}")))?
        .map_err(|e| MailError::other(format!("{e:#}")))
}

/// `send`/`reply` envelope: the facade JSON is `{deliveries, count,
/// verified_sender}` with `deliveries[].payload = {recipient, event_id}` —
/// the surface `SendReceipt` derives its `id` from the first delivery, so the
/// conversion is explicit. `verified_sender` / delivery `project` keys are not
/// consumed by this conversion (serde ignores the extra keys) — they are not
/// deleted from the wire, only from this struct.
#[derive(Deserialize)]
struct SendEnvelope {
    deliveries: Vec<DeliveryEnvelope>,
    count: usize,
}

#[derive(Deserialize)]
struct DeliveryEnvelope {
    payload: DeliveryPayloadEnvelope,
}

#[derive(Deserialize)]
struct DeliveryPayloadEnvelope {
    recipient: String,
    event_id: String,
}

fn send_receipt_from(v: Value) -> Result<SendReceipt, MailError> {
    let env: SendEnvelope = serde_json::from_value(v)
        .map_err(|e| MailError::other(format!("send receipt shape mismatch: {e}")))?;
    let deliveries: Vec<DeliveryLine> =
        env.deliveries
            .into_iter()
            .map(|d| {
                let event_id =
                    d.payload.event_id.parse::<MessageId>().map_err(|e| {
                        MailError::other(format!("send receipt event_id invalid: {e}"))
                    })?;
                Ok(DeliveryLine {
                    recipient: d.payload.recipient,
                    event_id,
                })
            })
            .collect::<Result<Vec<_>, MailError>>()?;
    let id = deliveries
        .first()
        .map(|d| d.event_id.clone())
        .ok_or_else(|| MailError::other("send receipt contains no deliveries"))?;
    Ok(SendReceipt {
        id,
        deliveries,
        count: env.count,
    })
}

/// `renew` envelope → `RenewResult` (AM-parity shape + bd-evqs reason).
/// The facade emits `{renewed, file_reservations:[{id, path_pattern, reason,
/// old_expires_ts, new_expires_ts}]}`; the surface wants `{renewed,
/// reservations: [GrantedReservation]}`. `exclusive` is absent from the
/// envelope — defaulted honestly (the renewal never changes it). `reason`
/// (bd-evqs) rides through so the loop's closed-bead sweep can release stale
/// leases.
fn renew_result_from(v: Value) -> RenewResult {
    let renewed = v.get("renewed").and_then(Value::as_u64).unwrap_or(0) as usize;
    let reservations = v
        .get("file_reservations")
        .or_else(|| v.get("reservations"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|r| toron_core::mailbox::reservations::GrantedReservation {
                    id: r
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    path_pattern: r
                        .get("path_pattern")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    exclusive: true,
                    reason: r
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    expires_ts_us: r
                        .get("new_expires_ts")
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse().ok()),
                    // zfnx.11 epoch rides lease content, not the wire
                    // shape — absent here reads as genesis 0.
                    epoch: r.get("epoch").and_then(Value::as_u64).unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    RenewResult {
        renewed,
        reservations,
    }
}

fn into_typed<T>(v: Value, op: &str) -> Result<T, MailError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(v)
        .map_err(|e| MailError::other(format!("{op} response shape mismatch: {e}")))
}

/// Tolerant row-envelope extraction (`{"result": [...]}`, `{"messages":
/// [...]}`, or a bare array) shared by the product fan-outs.
fn row_array(v: &Value) -> Vec<Value> {
    v.get("result")
        .and_then(Value::as_array)
        .or_else(|| v.get("messages").and_then(Value::as_array))
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl MailSurface for ToronMailBackend {
    fn name(&self) -> &'static str {
        "toron"
    }

    fn agent_name(&self) -> &str {
        &self.mail.name
    }

    async fn send(&self, req: &SendMail) -> Result<SendReceipt, MailError> {
        let mail = self.mail.clone();
        let (project, to, subject, body, importance, ack, thread) = (
            req.project.clone(),
            req.to.clone(),
            req.subject.clone(),
            req.body_md.clone(),
            req.importance.clone().unwrap_or_else(|| "normal".into()),
            req.ack_required,
            req.thread_id.clone(),
        );
        let v = run_facade(mail, move |m| {
            m.send(
                &project,
                &to,
                &subject,
                &body,
                &importance,
                ack,
                thread.as_deref(),
            )
        })
        .await?;
        send_receipt_from(v)
    }

    async fn reply(&self, req: &ReplyMail) -> Result<SendReceipt, MailError> {
        let mail = self.mail.clone();
        let (project, message_id, body, to) = (
            req.project.clone(),
            req.thread_id.as_str().to_string(),
            req.body_md.clone(),
            req.to.clone(),
        );
        let v = run_facade(mail, move |m| {
            m.reply(&project, &message_id, &body, Some(&to))
        })
        .await?;
        send_receipt_from(v)
    }

    async fn inbox(&self, req: &InboxQuery) -> Result<Vec<MailItem>, MailError> {
        let mail = self.mail.clone();
        let (project, agent, urgent_only, since_ts) = (
            req.project.clone(),
            req.agent.clone(),
            req.urgent_only,
            req.since_ts.clone(),
        );
        let filter = MailInboxFilter {
            urgent_only,
            unread_only: req.unread_only,
            ack_overdue_only: false,
            include_bodies: req.include_bodies,
            limit: req.limit.unwrap_or(100) as u32,
            since_ts,
            thread_id: req.thread_id.clone(),
        };
        let v = run_facade(mail, move |m| m.inbox_filtered(&project, &agent, &filter)).await?;
        into_typed(v, "inbox")
    }

    async fn acknowledge(&self, req: &ReceiptOp) -> Result<ReceiptOutcome, MailError> {
        let mail = self.mail.clone();
        let (project, agent, id) = (
            req.project.clone(),
            req.agent.clone(),
            req.message_id.as_str().to_string(),
        );
        let v = run_facade(mail, move |m| m.ack(&project, &agent, &id)).await?;
        into_typed(v, "ack")
    }

    async fn mark_read(&self, req: &ReceiptOp) -> Result<ReceiptOutcome, MailError> {
        let mail = self.mail.clone();
        let (project, agent, id) = (
            req.project.clone(),
            req.agent.clone(),
            req.message_id.as_str().to_string(),
        );
        let v = run_facade(mail, move |m| m.mark_read(&project, &agent, &id)).await?;
        into_typed(v, "mark_read")
    }

    async fn search(&self, req: &SearchQuery) -> Result<Vec<MailItem>, MailError> {
        let mail = self.mail.clone();
        let (project, query) = (req.project.clone(), req.query.clone());
        let filter = MailSearchFilter {
            sender: req.sender.clone(),
            importance: req.importance.clone(),
            thread_id: req.thread_id.clone(),
            date_start: req.date_start.clone(),
            date_end: req.date_end.clone(),
            ranking: req.ranking.clone(),
            include_body_md: req.include_body_md,
        };
        let limit = req.limit.unwrap_or(50) as u32;
        let v = run_facade(mail, move |m| m.search(&project, &query, limit, &filter)).await?;
        // The facade envelope is `{"result": [...]}`.
        let arr = v.get("result").cloned().unwrap_or(v);
        into_typed(arr, "search")
    }

    async fn request_contact(&self, req: &ContactRequest) -> Result<ContactLink, MailError> {
        let mail = self.mail.clone();
        let (project, from, to, reason) = (
            req.project.clone(),
            req.from_agent.clone(),
            req.to_agent.clone(),
            req.reason.clone(),
        );
        let to_project = req.to_project.clone();
        let reason = reason.unwrap_or_default();
        let v = run_facade(mail, move |m| {
            m.request_contact(&project, &from, &to, to_project.as_deref(), &reason, None)
        })
        .await?;
        into_typed(v, "request_contact")
    }

    async fn respond_contact(&self, req: &ContactResponse) -> Result<ContactLink, MailError> {
        let mail = self.mail.clone();
        let (project, to, from, accept) = (
            req.project.clone(),
            req.to_agent.clone(),
            req.from_agent.clone(),
            req.accept,
        );
        let v = run_facade(mail, move |m| {
            m.respond_contact(&project, &to, &from, accept)
        })
        .await?;
        into_typed(v, "respond_contact")
    }

    async fn list_contacts(
        &self,
        project: &str,
        agent: &str,
    ) -> Result<Vec<ContactLink>, MailError> {
        let mail = self.mail.clone();
        let (project, agent) = (project.to_string(), agent.to_string());
        let v = run_facade(mail, move |m| m.list_contacts(&project, &agent)).await?;
        into_typed(v, "list_contacts")
    }

    async fn set_contact_policy(
        &self,
        project: &str,
        agent: &str,
        policy: ContactPolicy,
    ) -> Result<(), MailError> {
        let mail = self.mail.clone();
        let (project, agent, policy) = (project.to_string(), agent.to_string(), policy);
        let v = run_facade(mail, move |m| {
            m.set_contact_policy(&project, &agent, policy.as_str())
        })
        .await?;
        let _ = v;
        Ok(())
    }

    async fn register_agent(
        &self,
        project: &str,
        profile: &AgentProfile,
    ) -> Result<RegisteredAgent, MailError> {
        let mail = self.mail.clone();
        let (project, name, program, model, task) = (
            project.to_string(),
            profile.name.clone(),
            profile.program.clone().unwrap_or_default(),
            profile.model.clone().unwrap_or_default(),
            profile.task_description.clone().unwrap_or_default(),
        );
        let v = run_facade(mail, move |m| {
            m.register_agent(&project, &name, &program, &model, &task)
        })
        .await?;
        into_typed(v, "register_agent")
    }

    async fn list_agents(
        &self,
        project: &str,
        active_within_days: Option<u64>,
    ) -> Result<Vec<WhoIs>, MailError> {
        let mail = self.mail.clone();
        let (project, days) = (project.to_string(), active_within_days);
        let v = run_facade(mail, move |m| m.list_agents(&project, days)).await?;
        into_typed(v, "list_agents")
    }

    async fn whois(&self, project: &str, agent: &str) -> Result<WhoIs, MailError> {
        let mail = self.mail.clone();
        let (project, agent) = (project.to_string(), agent.to_string());
        let v = run_facade(mail, move |m| m.whois(&project, &agent)).await?;
        into_typed(v, "whois")
    }

    async fn health(&self) -> Result<toron_core::mailbox::infra::HealthReport, MailError> {
        let mail = self.mail.clone();
        let v = run_facade(mail, super::super::mail::Mail::health).await?;
        into_typed(v, "health")
    }

    async fn ensure_project(
        &self,
        human_key: &str,
    ) -> Result<toron_core::mailbox::infra::ProjectState, MailError> {
        let mail = self.mail.clone();
        let key = human_key.to_string();
        let v = run_facade(mail, move |m| m.ensure_project(&key)).await?;
        into_typed(v, "ensure_project")
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
        let mail = self.mail.clone();
        let (project, agent, paths, ttl, exclusive, reason) = (
            project.to_string(),
            agent.to_string(),
            paths.to_vec(),
            ttl_secs.unwrap_or(3600),
            exclusive,
            reason.unwrap_or("").to_string(),
        );
        let v = run_facade(mail, move |m| {
            m.reserve(&project, &agent, &paths, ttl, exclusive, &reason)
        })
        .await?;
        into_typed(v, "reserve")
    }

    async fn check_conflicts(
        &self,
        project: &str,
        agent: &str,
        paths: &[String],
    ) -> Result<ConflictReport, MailError> {
        let mail = self.mail.clone();
        let (project, agent, paths) = (project.to_string(), agent.to_string(), paths.to_vec());
        let v = run_facade(mail, move |m| m.conflicts_for(&project, &agent, &paths)).await?;
        into_typed(v, "check_conflicts")
    }

    async fn renew_reservations(
        &self,
        project: &str,
        agent: &str,
        paths: &[String],
        extend_secs: u64,
    ) -> Result<RenewResult, MailError> {
        let mail = self.mail.clone();
        let (project, agent, paths, extend) = (
            project.to_string(),
            agent.to_string(),
            paths.to_vec(),
            extend_secs,
        );
        let v = run_facade(mail, move |m| {
            m.renew(&project, &agent, extend, Some(&paths), None)
        })
        .await?;
        Ok(renew_result_from(v))
    }

    async fn release_reservations(
        &self,
        project: &str,
        agent: &str,
        paths: &[String],
    ) -> Result<ReleaseResult, MailError> {
        let mail = self.mail.clone();
        let (project, agent, paths) = (project.to_string(), agent.to_string(), paths.to_vec());
        let v = run_facade(mail, move |m| m.release_paths(&project, &agent, &paths)).await?;
        into_typed(v, "release_reservations")
    }

    async fn force_release(
        &self,
        project: &str,
        requestor: &str,
        path: &str,
        holder: Option<&str>,
        note: Option<&str>,
        notify_previous: bool,
    ) -> Result<(), MailError> {
        let mail = self.mail.clone();
        let (project, requestor, path, holder, note, notify) = (
            project.to_string(),
            requestor.to_string(),
            path.to_string(),
            holder.map(str::to_string),
            note.unwrap_or("").to_string(),
            notify_previous,
        );
        run_facade(mail, move |m| {
            m.force_release_file_reservation(
                &project,
                &requestor,
                &path,
                holder.as_deref(),
                if note.is_empty() { None } else { Some(&note) },
                notify,
            )
            .map(|_| ())
        })
        .await
    }

    async fn release_by_reason(
        &self,
        project: &str,
        requestor: &str,
        bead_id: &str,
        notify_previous: bool,
    ) -> Result<ReleaseResult, MailError> {
        let mail = self.mail.clone();
        let (project, requestor, bead_id) = (
            project.to_string(),
            requestor.to_string(),
            bead_id.to_string(),
        );
        let v = run_facade(mail, move |m| {
            m.release_by_reason(&project, &requestor, &bead_id, notify_previous)
        })
        .await?;
        into_typed(v, "release_by_reason")
    }

    async fn slot_acquire(
        &self,
        project: &str,
        agent: &str,
        slot: &str,
        ttl_secs: Option<u64>,
        exclusive: bool,
    ) -> Result<AcquireBuildSlotResult, MailError> {
        let mail = self.mail.clone();
        let (project, agent, slot, ttl, exclusive) = (
            project.to_string(),
            agent.to_string(),
            slot.to_string(),
            ttl_secs.unwrap_or(3600),
            exclusive,
        );
        let v = run_facade(mail, move |m| {
            m.acquire_slot(&project, &agent, &slot, exclusive, ttl)
        })
        .await?;
        into_typed(v, "slot_acquire")
    }

    async fn slot_renew(
        &self,
        project: &str,
        agent: &str,
        slot: &str,
        extend_secs: u64,
    ) -> Result<Option<BuildSlot>, MailError> {
        let mail = self.mail.clone();
        let (project, agent, slot, extend) = (
            project.to_string(),
            agent.to_string(),
            slot.to_string(),
            extend_secs,
        );
        let v = run_facade(mail, move |m| m.renew_slot(&project, &agent, &slot, extend)).await?;
        // The facade JSON for a no-op renew is `{...}` without a lease.
        if v.get("slot").is_none() && v.get("expires_ts_us").is_none() {
            return Ok(None);
        }
        Ok(Some(into_typed(v, "slot_renew")?))
    }

    async fn slot_release(
        &self,
        project: &str,
        agent: &str,
        slot: &str,
    ) -> Result<bool, MailError> {
        let mail = self.mail.clone();
        let (project, agent, slot) = (project.to_string(), agent.to_string(), slot.to_string());
        let v = run_facade(mail, move |m| m.release_slot(&project, &agent, &slot)).await?;
        Ok(v.get("released").and_then(Value::as_bool).unwrap_or(false))
    }

    async fn ensure_product(
        &self,
        product_key: &str,
        name: &str,
    ) -> Result<ProductInfo, MailError> {
        let mail = self.mail.clone();
        let (product_key, name) = (product_key.to_string(), name.to_string());
        let v = run_facade(mail, move |m| m.ensure_product(&product_key, Some(&name))).await?;
        into_typed(v, "ensure_product")
    }

    async fn products_link(
        &self,
        product_key: &str,
        project: &str,
    ) -> Result<ProductInfo, MailError> {
        let mail = self.mail.clone();
        let (product_key, project) = (product_key.to_string(), project.to_string());
        let v = run_facade(mail, move |m| m.products_link(&product_key, &project)).await?;
        into_typed(v, "products_link")
    }

    async fn search_product(
        &self,
        product_key: &str,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<ProductHit>, MailError> {
        let mail = self.mail.clone();
        let (product_key, query) = (product_key.to_string(), query.to_string());
        let limit = limit.unwrap_or(50) as u32;
        let key_for_facade = product_key.clone();
        let v = run_facade(mail, move |m| {
            m.search_product(&key_for_facade, &query, limit)
        })
        .await?;
        // Envelope `{"result": [...]}` of `MailItem`-shaped rows (+ `project`).
        let arr = row_array(&v);
        arr.into_iter()
            .map(|m| {
                Ok(ProductHit {
                    project: m
                        .get("project")
                        .and_then(Value::as_str)
                        .unwrap_or(&product_key)
                        .to_string(),
                    item: into_typed(m, "search_product")?,
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
        let mail = self.mail.clone();
        let (product_key, agent) = (product_key.to_string(), agent.to_string());
        let limit = limit.unwrap_or(100) as u32;
        let key_for_facade = product_key.clone();
        let v = run_facade(mail, move |m| {
            m.inbox_product(&key_for_facade, &agent, limit)
        })
        .await?;
        // Envelope `{"messages": [...]}` / `{"result": [...]}` of
        // `MailItem`-shaped rows (+ `project`).
        let arr = row_array(&v);
        arr.into_iter()
            .map(|m| {
                Ok(ProductHit {
                    project: m
                        .get("project")
                        .and_then(Value::as_str)
                        .unwrap_or(&product_key)
                        .to_string(),
                    item: into_typed(m, "inbox_product")?,
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
        let mail = self.mail.clone();
        let (product_key, thread_id) = (product_key.to_string(), thread_id.to_string());
        let v = run_facade(mail, move |m| {
            m.summarize_thread_product(&product_key, &thread_id, include_examples)
        })
        .await?;
        into_typed(v, "summarize_thread_product")
    }

    async fn summarize_thread(
        &self,
        project: &str,
        thread_id: &str,
        include_examples: bool,
    ) -> Result<ThreadSummary, MailError> {
        let mail = self.mail.clone();
        let (project, thread_id) = (project.to_string(), thread_id.to_string());
        let v = run_facade(mail, move |m| {
            m.summarize_thread(&project, &thread_id, include_examples)
        })
        .await?;
        into_typed(v, "summarize_thread")
    }

    async fn resolve_pane_identity(
        &self,
        project: &str,
        pane_id: &str,
    ) -> Result<Option<String>, MailError> {
        let mail = self.mail.clone();
        let (project, pane_id) = (project.to_string(), pane_id.to_string());
        let v = run_facade(mail, move |m| {
            Ok(m.resolve_pane_identity(&project, &pane_id))
        })
        .await?;
        Ok(v.get("agent_name")
            .and_then(Value::as_str)
            .map(String::from))
    }

    async fn cleanup_pane_identities(&self, project: &str) -> Result<Vec<String>, MailError> {
        let mail = self.mail.clone();
        let project = project.to_string();
        let v = run_facade(mail, move |m| m.cleanup_pane_identities(&project)).await?;
        Ok(v.get("pruned")
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
        let mail = self.mail.clone();
        let (project, pane_id, agent) =
            (project.to_string(), pane_id.to_string(), agent.to_string());
        run_facade(mail, move |m| {
            m.write_pane_identity(&project, &pane_id, &agent)
                .map(|_| ())
        })
        .await
    }

    async fn clear_pane_identity(&self, project: &str, pane_id: &str) -> Result<(), MailError> {
        let mail = self.mail.clone();
        let (project, pane_id) = (project.to_string(), pane_id.to_string());
        run_facade(mail, move |m| {
            m.clear_pane_identity(&project, &pane_id).map(|_| ())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MailSurface;

    /// name() reports the right identifier — the one piece we can assert
    /// without a live Mail. Constructed via `Mail::from_config`; a url set
    /// exercises the remote-daemon constructor (no HTTP happens — `name()`
    /// reads the facade field).
    #[test]
    fn toron_name_is_toron() {
        let mail = Mail::from_config(Some(&crate::config::MailConfig {
            url: Some("http://127.0.0.1:18080".into()),
            token: None,
            name: Some("test-orchestrator".into()),
        }))
        .expect("from_config yields Some(Mail)")
        .expect("Some(Mail) unwraps");
        let b = ToronMailBackend::new(mail);
        assert_eq!(b.name(), "toron");
        assert_eq!(b.agent_name(), "test-orchestrator");
    }

    /// The send envelope conversion is explicit: a missing/odd shape is a
    /// typed error, never a silent empty receipt.
    #[test]
    fn send_envelope_converts_to_send_receipt() {
        let v = serde_json::json!({
            "deliveries": [{
                "project": "demo",
                "payload": { "recipient": "BlueLake", "event_id": "abc123" }
            }],
            "count": 1,
            "verified_sender": true,
        });
        let r = send_receipt_from(v).expect("convert");
        assert_eq!(r.count, 1);
        assert_eq!(r.deliveries[0].recipient, "BlueLake");
        assert_eq!(r.deliveries[0].event_id.as_str(), "abc123");
        assert_eq!(r.id.as_str(), "abc123", "id = first delivery's event id");

        let bad = serde_json::json!({ "count": "nope" });
        assert!(
            send_receipt_from(bad).is_err(),
            "shape mismatch is an error"
        );
    }

    /// Renew envelope → RenewResult: `file_reservations` (facade) and
    /// `reservations` (daemon) both parse; ids stay strings.
    #[test]
    fn renew_envelope_converts_to_renew_result() {
        let v = serde_json::json!({
            "renewed": 1,
            "file_reservations": [{
                "id": "deadbeef",
                "path_pattern": "src/**",
                "reason": "bd-oj9o",
                "old_expires_ts": "",
                "new_expires_ts": "1786200000000000",
            }],
        });
        let r = renew_result_from(v);
        assert_eq!(r.renewed, 1);
        assert_eq!(r.reservations[0].id, "deadbeef");
        assert_eq!(
            r.reservations[0].expires_ts_us,
            Some(1786200000000000),
            "new_expires_ts parses as i64, never unwrap_or(0)"
        );
        // bd-evqs: the reason (owning bead id) must survive the envelope —
        // the loop's closed-bead sweep reads it to release stale leases.
        assert_eq!(r.reservations[0].reason, "bd-oj9o");
    }

    /// bd-evqs: the daemon's `reservations` shape also carries reason (the
    /// daemon MCP `GrantedReservationOut` includes it).
    #[test]
    fn renew_result_reason_survives_daemon_shape() {
        let v = serde_json::json!({
            "renewed": 2,
            "reservations": [
                {
                    "id": "abc",
                    "path_pattern": "src/a.rs",
                    "exclusive": true,
                    "reason": "bd-12",
                    "expires_ts_us": 1786200000000000i64,
                },
                {
                    "id": "def",
                    "path_pattern": "src/b.rs",
                    "exclusive": false,
                    "reason": "",
                    "expires_ts_us": 1786200000000000i64,
                },
            ],
        });
        let r = renew_result_from(v);
        assert_eq!(r.renewed, 2);
        assert_eq!(r.reservations[0].reason, "bd-12");
        assert_eq!(r.reservations[1].reason, "", "missing reason stays empty");
    }
}
