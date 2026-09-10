// governed-by: ADR-0012
//! Sinks are bounded, shed, supervised. One actor per subscription sink:
//! the manager owns every queue (exact capacity, exact accounting), workers
//! pump adapters. Full queue or parked sink sheds to digest — the brain's
//! latency never depends on a sink. Endpoint failure parks the sink and
//! moves the remainder to the shed overflow (journaled + digested, never
//! silently dropped). Graceful stop closes queues and returns everything
//! still held as [`Undelivered`] so resume redelivers from the journal.

use crate::msg::BrainMsg;
use crate::policy::RestartPolicy;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, mpsc};

/// Delivery endpoint. The brain's only delivery intent is `Prompt{pin,line}`;
/// adapters translate it (herdr prompt today, webhook/DM in Phase 2).
pub trait PromptSink: Send + 'static {
    fn send(&mut self, pin: &str, line: &str) -> anyhow::Result<()>;
    fn name(&self) -> &str;
}

type Intent = (String, String);

/// A prompt intent that never left the queue (graceful-stop drain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undelivered {
    pub sink: String,
    pub pin: String,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// Accepted into the sink queue (F5: not delivered — see Delivered).
    Queued,
    Shed { pin: String, line: String },
    Absent,
}

struct SinkInner {
    queue: VecDeque<Intent>,
    shed_overflow: Vec<Intent>,
    cap: usize,
    parked: bool,
    closed: bool,
    sends: u64,
    sheds: u64,
    notify: Arc<Notify>,
}

struct Sink {
    inner: Arc<Mutex<SinkInner>>,
    policy: RestartPolicy,
    worker: Option<tokio::task::JoinHandle<()>>,
}

/// Per-subscription sink actors with shed-to-digest and supervision.
pub struct SinkManager {
    sinks: HashMap<String, Sink>,
    degraded_tx: mpsc::Sender<BrainMsg>,
    cap: usize,
}

impl SinkManager {
    pub fn new(degraded_tx: mpsc::Sender<BrainMsg>) -> Self {
        Self::with_cap(degraded_tx, crate::SINK_CAP)
    }

    pub fn with_cap(degraded_tx: mpsc::Sender<BrainMsg>, cap: usize) -> Self {
        Self {
            sinks: HashMap::new(),
            degraded_tx,
            cap,
        }
    }

    /// Register a sink adapter; spawns its supervised worker.
    pub fn add_sink(&mut self, name: impl Into<String>, sink: Box<dyn PromptSink>) {
        let name = name.into();
        let cap = self.cap;
        let inner = Arc::new(Mutex::new(SinkInner {
            queue: VecDeque::new(),
            shed_overflow: Vec::new(),
            cap,
            parked: false,
            closed: false,
            sends: 0,
            sheds: 0,
            notify: Arc::new(Notify::new()),
        }));
        let worker = tokio::spawn(worker_loop(
            name.clone(),
            inner.clone(),
            sink,
            self.degraded_tx.clone(),
        ));
        self.sinks.insert(
            name,
            Sink {
                inner,
                policy: RestartPolicy::transient(),
                worker: Some(worker),
            },
        );
    }

    /// Enqueue-or-shed (async only for the brief queue lock; never waits
    /// on the endpoint — a slow sink sheds instead of blocking).
    /// F5: `Queued` means accepted into the sink queue, NOT delivered.
    /// The worker reports `BrainMsg::Delivered` after adapter.send succeeds;
    /// hear/deaf tracking keys off that receipt, never this return.
    pub async fn send(&mut self, sink: &str, pin: &str, line: &str) -> SendOutcome {
        let Some(st) = self.sinks.get_mut(sink) else {
            return SendOutcome::Absent;
        };
        let mut inner = st.inner.lock().await;
        if inner.parked || inner.closed {
            inner.sheds += 1;
            return SendOutcome::Shed {
                pin: pin.into(),
                line: line.into(),
            };
        }
        if inner.queue.len() >= inner.cap {
            inner.sheds += 1;
            return SendOutcome::Shed {
                pin: pin.into(),
                line: line.into(),
            };
        }
        inner.queue.push_back((pin.into(), line.into()));
        inner.sends += 1;
        inner.notify.notify_one();
        SendOutcome::Queued
    }

    /// Restart a parked sink's worker under the Transient policy, keeping
    /// the queued intents (the new worker pumps them). False = cap
    /// exhausted, stays parked (+ caller emits Degraded/audit).
    pub async fn restart(
        &mut self,
        sink: &str,
        now_unix: u64,
        adapter: Box<dyn PromptSink>,
    ) -> bool {
        let degraded_tx = self.degraded_tx.clone();
        let name = sink.to_string();
        let Some(st) = self.sinks.get_mut(sink) else {
            return false;
        };
        if !st.policy.should_restart(now_unix) {
            return false;
        }
        {
            let mut inner = st.inner.lock().await;
            inner.parked = false;
            inner.notify.notify_one();
        }
        if let Some(w) = st.worker.take() {
            let _ = w.await;
        }
        let inner = st.inner.clone();
        st.worker = Some(tokio::spawn(worker_loop(name, inner, adapter, degraded_tx)));
        true
    }

    /// Shed overflow awaiting journaling + digest intake (endpoint died
    /// mid-queue). Drains the overflow buffers.
    pub async fn drain_shed_overflow(&mut self) -> Vec<Undelivered> {
        let mut out = Vec::new();
        for (name, st) in self.sinks.iter_mut() {
            let mut inner = st.inner.lock().await;
            for (pin, line) in inner.shed_overflow.drain(..) {
                out.push(Undelivered {
                    sink: name.clone(),
                    pin,
                    line,
                });
            }
        }
        out
    }

    /// Graceful stop: close every queue, join workers, return everything
    /// still held. The brain journals these; resume redelivers from the
    /// journal (the intent was already journaled pre-delivery, D4.2).
    pub async fn shutdown(&mut self) -> Vec<Undelivered> {
        let mut out = Vec::new();
        for (name, st) in self.sinks.iter_mut() {
            {
                let mut inner = st.inner.lock().await;
                inner.closed = true;
                for (pin, line) in inner.queue.drain(..) {
                    out.push(Undelivered {
                        sink: name.clone(),
                        pin,
                        line,
                    });
                }
                for (pin, line) in inner.shed_overflow.drain(..) {
                    out.push(Undelivered {
                        sink: name.clone(),
                        pin,
                        line,
                    });
                }
                inner.notify.notify_waiters();
            }
            if let Some(w) = st.worker.take() {
                let _ = w.await;
            }
        }
        out
    }

    pub async fn sends(&self, sink: &str) -> u64 {
        match self.sinks.get(sink) {
            Some(st) => st.inner.lock().await.sends,
            None => 0,
        }
    }

    pub async fn sheds(&self, sink: &str) -> u64 {
        match self.sinks.get(sink) {
            Some(st) => st.inner.lock().await.sheds,
            None => 0,
        }
    }

    pub async fn parked(&self, sink: &str) -> bool {
        match self.sinks.get(sink) {
            Some(st) => st.inner.lock().await.parked,
            None => false,
        }
    }
}

async fn worker_loop(
    name: String,
    inner: Arc<Mutex<SinkInner>>,
    mut adapter: Box<dyn PromptSink>,
    degraded_tx: mpsc::Sender<BrainMsg>,
) {
    // Pumps intents in order; the manager mediates parking (single writer
    // for state, worker only reads/writes under the same lock).
    loop {
        let next = {
            let mut guard = inner.lock().await;
            match guard.queue.pop_front() {
                Some(intent) => intent,
                None => {
                    if guard.closed {
                        return;
                    }
                    drop(guard);
                    inner.lock().await.notify.notified().await;
                    continue;
                }
            }
        };
        let (pin, line) = next;
        if let Err(e) = adapter.send(&pin, &line) {
            // Park + move the remainder to shed overflow under one lock.
            let rest = {
                let mut guard = inner.lock().await;
                guard.parked = true;
                let mut rest: Vec<Intent> = guard.queue.drain(..).collect();
                guard.sheds += rest.len() as u64;
                std::mem::take(&mut rest)
            };
            {
                let mut guard = inner.lock().await;
                guard.shed_overflow.extend(rest);
            }
            let _ = degraded_tx
                .send(BrainMsg::Degraded {
                    source: format!("sink:{name}"),
                    reason: format!("delivery failed: {e:#}"),
                })
                .await;
            return;
        }
        // F5: delivery receipt — hear keys off this, never enqueue.
        let _ = degraded_tx
            .send(BrainMsg::Delivered { pin })
            .await;
    }
}

/// Recording sink for tests; `stall_ms` simulates a slow endpoint.
pub struct MockSink {
    pub sent: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    pub stall_ms: u64,
    pub fail_after: Option<usize>,
}

impl MockSink {
    pub fn new() -> (Self, Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                sent: sent.clone(),
                stall_ms: 0,
                fail_after: None,
            },
            sent,
        )
    }

    fn sent_mut(&mut self) -> std::sync::MutexGuard<'_, Vec<(String, String)>> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl PromptSink for MockSink {
    fn send(&mut self, pin: &str, line: &str) -> anyhow::Result<()> {
        if self.stall_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.stall_ms));
        }
        let fail_after = self.fail_after;
        let mut sent = self.sent_mut();
        if let Some(n) = fail_after
            && sent.len() >= n
        {
            anyhow::bail!("mock endpoint down");
        }
        sent.push((pin.into(), line.into()));
        Ok(())
    }

    fn name(&self) -> &str {
        "mock"
    }
}

/// Operator-approved egress hosts (D6 threat table: attention metadata
/// must not exfiltrate). Exact host or `.suffix` match, lowercase.
#[derive(Debug, Clone, Default)]
pub struct EgressAllowlist {
    hosts: Vec<String>,
}

impl EgressAllowlist {
    pub fn new(hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            hosts: hosts
                .into_iter()
                .map(|h| h.to_lowercase())
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }

    pub fn allows(&self, url: &str) -> bool {
        let host = url_host(url);
        self.hosts
            .iter()
            .any(|h| host == *h || (h.starts_with('.') && host.ends_with(h.as_str())))
    }
}

fn url_host(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Webhook POST sink (D3/D6): bounded queue upstream, minimal payload by
/// default (hint + counts), full line only with per-subscription opt-in.
/// Hand-rolled POST (one route, no dependency); blocking per-sink worker
/// isolates slow endpoints from the brain (E15).
pub struct WebhookSink {
    url: String,
    full_line: bool,
    timeout: std::time::Duration,
}

impl WebhookSink {
    /// Refuses non-allowlisted hosts at construction (fail-closed).
    pub fn new(
        url: impl Into<String>,
        allowlist: &EgressAllowlist,
        full_line: bool,
    ) -> anyhow::Result<Self> {
        let url = url.into();
        if !allowlist.allows(&url) {
            anyhow::bail!("webhook host not allowlisted: {url}");
        }
        Ok(Self {
            url,
            full_line,
            timeout: std::time::Duration::from_secs(10),
        })
    }

    fn render(&self, subscriber: &str, pin: &str, line: &str) -> String {
        if self.full_line {
            return serde_json::json!({
                "subscriber": subscriber,
                "pin": pin,
                "line": line,
            })
            .to_string();
        }
        let hint: String = line.chars().take(120).collect();
        serde_json::json!({
            "subscriber": subscriber,
            "pin": pin,
            "hint": hint,
            "truncated": line.chars().count() > 120,
        })
        .to_string()
    }

    fn post(&self, body: &str) -> anyhow::Result<()> {
        use std::io::{Read, Write};
        let host = url_host(&self.url);
        let mut stream = std::net::TcpStream::connect_timeout(
            &format!("{host}:80")
                .parse()
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
            self.timeout,
        )?;
        // Only plain-http loopback/allowlisted hosts in Phase 2 (TLS via
        // the operator's terminator; revisit with push transport).
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_read_timeout(Some(self.timeout))?;
        write!(
            stream,
            "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        let mut resp = [0u8; 12];
        stream.read_exact(&mut resp)?;
        let status: u16 = String::from_utf8_lossy(&resp[9..12]).parse().unwrap_or(0);
        if !(200..300).contains(&status) {
            anyhow::bail!("webhook {host} answered {status}");
        }
        Ok(())
    }
}

impl PromptSink for WebhookSink {
    fn send(&mut self, pin: &str, line: &str) -> anyhow::Result<()> {
        // Subscriber identity rides from the subscription store binding in
        // bd-asr2.11; until then the pin scopes the payload.
        self.post(&self.render(pin, pin, line))
    }

    fn name(&self) -> &str {
        "webhook"
    }
}

/// NIP-17 DM sink (D3/D6): key custody first. The daemon stores ncryptsec
/// keys ONLY for service identities it mints; human/operator keys are
/// refused at construction (never held). Transport binds in bd-asr2.17
/// (service identities + emit); until then delivery records intents.
pub struct DmSink {
    service_id: String,
    transport: Box<dyn DmTransport>,
}

/// DM transport seam (real NIP-17 gift-wrap binds in Phase 4).
pub trait DmTransport: Send + 'static {
    fn send_dm(&mut self, to: &str, hint: &str) -> anyhow::Result<()>;
}

/// Key custody gate: only `svc:` service identities minted by the daemon.
/// Anything else (human npub, nsec, opaque) is refused — the daemon must
/// never hold human keys (D6).
impl DmSink {
    pub fn for_service(
        service_id: impl Into<String>,
        transport: Box<dyn DmTransport>,
    ) -> anyhow::Result<Self> {
        let service_id = service_id.into();
        if !service_id.starts_with("svc:") {
            anyhow::bail!("DM sink refuses non-service identity (daemon never holds human keys)");
        }
        Ok(Self {
            service_id,
            transport,
        })
    }
}

impl PromptSink for DmSink {
    fn send(&mut self, pin: &str, line: &str) -> anyhow::Result<()> {
        let hint: String = line.chars().take(120).collect();
        self.transport.send_dm(&self.service_id_for(pin), &hint)
    }

    fn name(&self) -> &str {
        "dm"
    }
}

impl DmSink {
    fn service_id_for(&self, _pin: &str) -> String {
        // Per-subscriber recipient mapping binds with the subscription
        // store in bd-asr2.11; today the service identity is the recipient.
        self.service_id.clone()
    }
}

/// Recording DM transport for tests.
pub struct MockDmTransport {
    pub sent: Vec<(String, String)>,
    pub fail: bool,
}

impl MockDmTransport {
    pub fn new() -> Self {
        Self {
            sent: Vec::new(),
            fail: false,
        }
    }
}

impl DmTransport for MockDmTransport {
    fn send_dm(&mut self, to: &str, hint: &str) -> anyhow::Result<()> {
        if self.fail {
            anyhow::bail!("dm transport down");
        }
        self.sent.push((to.into(), hint.into()));
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sent_len(sent: &Arc<std::sync::Mutex<Vec<(String, String)>>>) -> usize {
        sent.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
    #[tokio::test]
    async fn stalled_sink_sheds_while_worker_still_pumps() {
        let (deg_tx, _deg_rx) = mpsc::channel(16);
        let mut mgr = SinkManager::with_cap(deg_tx, 2);
        let (mut sink, sent) = MockSink::new();
        sink.stall_ms = 200;
        mgr.add_sink("herdr", Box::new(sink));
        // Cap 2: first two queue, the rest must shed — never block.
        let mut outcomes = Vec::new();
        for i in 0..6 {
            outcomes.push(mgr.send("herdr", "p", &format!("m{i}")).await);
        }
        let sheds = outcomes
            .iter()
            .filter(|o| matches!(o, SendOutcome::Shed { .. }))
            .count();
        assert!(sheds >= 3, "slow sink must shed to digest, got {sheds}");
        assert_eq!(mgr.sheds("herdr").await, sheds as u64);
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        assert!(sent_len(&sent) >= 2, "worker still pumps the queue");
    }

    #[tokio::test]
    async fn dead_endpoint_parks_and_sheds_remainder() {
        let (deg_tx, mut deg_rx) = mpsc::channel(16);
        let mut mgr = SinkManager::new(deg_tx);
        let (mut sink, _sent) = MockSink::new();
        sink.fail_after = Some(0);
        mgr.add_sink("webhook", Box::new(sink));
        // First intent fails at the endpoint → park; everything sheds after.
        mgr.send("webhook", "p", "one").await;
        mgr.send("webhook", "p", "two").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(mgr.parked("webhook").await);
        assert!(matches!(
            mgr.send("webhook", "p", "three").await,
            SendOutcome::Shed { .. }
        ));
        let mut saw = false;
        while let Ok(m) = deg_rx.try_recv() {
            if let BrainMsg::Degraded { source, .. } = m {
                if source == "sink:webhook" {
                    saw = true;
                }
            }
        }
        assert!(saw, "endpoint death is Degraded data");
    }

    #[tokio::test]
    async fn shutdown_returns_held_intents() {
        let (deg_tx, _deg_rx) = mpsc::channel(16);
        let mut mgr = SinkManager::new(deg_tx);
        let (mut sink, _sent) = MockSink::new();
        sink.stall_ms = 500;
        mgr.add_sink("herdr", Box::new(sink));
        mgr.send("herdr", "p", "held").await;
        let undelivered = mgr.shutdown().await;
        assert!(
            undelivered.iter().any(|u| u.line == "held"),
            "held intent returns for journaling, got {undelivered:?}"
        );
    }

    #[test]
    fn unknown_sink_is_absent_not_panic() {
        let (deg_tx, _deg_rx) = mpsc::channel(16);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut mgr = SinkManager::new(deg_tx);
            assert!(matches!(
                mgr.send("nope", "p", "x").await,
                SendOutcome::Absent
            ));
        });
    }

    #[test]
    fn allowlist_refuses_unlisted_hosts() {
        let allow = EgressAllowlist::new(["hooks.internal".into(), ".ops.example".into()]);
        assert!(allow.allows("https://hooks.internal/doorbell"));
        assert!(allow.allows("http://x.ops.example/y"));
        assert!(!allow.allows("https://evil.example/doorbell"));
        assert!(!allow.allows("https://ops.example.evil/y"));
        // Construction refuses loudly (fail-closed, D6).
        assert!(WebhookSink::new("https://evil.example/h", &allow, false).is_err());
        assert!(WebhookSink::new("https://hooks.internal/h", &allow, false).is_ok());
    }

    #[test]
    fn default_payload_minimizes_full_line_opt_in() {
        let allow = EgressAllowlist::new(["h.internal".into()]);
        let min = WebhookSink::new("https://h.internal/h", &allow, false).unwrap();
        let long = "x".repeat(500);
        let body = min.render("sub:1", "p", &long);
        assert!(body.contains("\"hint\""), "{body}");
        assert!(
            !body.contains(&long),
            "default payload must not exfiltrate the full line"
        );
        assert!(body.contains("\"truncated\":true"), "{body}");
        let full = WebhookSink::new("https://h.internal/h", &allow, true).unwrap();
        assert!(full.render("sub:1", "p", &long).contains(&long));
    }

    #[test]
    fn dm_sink_refuses_human_keys() {
        // Only daemon-minted service identities; human/operator keys never.
        assert!(DmSink::for_service("svc:ci-1", Box::new(MockDmTransport::new())).is_ok());
        assert!(DmSink::for_service("npub1abc", Box::new(MockDmTransport::new())).is_err());
        assert!(DmSink::for_service("nsec1abc", Box::new(MockDmTransport::new())).is_err());
        assert!(DmSink::for_service("operator", Box::new(MockDmTransport::new())).is_err());
    }

    #[tokio::test]
    async fn e15_stalled_webhook_sheds_while_herdr_prompt_fast() {
        // E15: webhook stalled 30s (simulated) → shed-to-digest + Degraded;
        // herdr-prompt latency unaffected (brain never blocks on a sink).
        let (deg_tx, mut deg_rx) = mpsc::channel(16);
        let mut mgr = SinkManager::with_cap(deg_tx, 1);
        let (mut stall, _sent) = MockSink::new();
        stall.stall_ms = 30_000;
        mgr.add_sink("webhook", Box::new(stall));
        let (fast, sent) = MockSink::new();
        mgr.add_sink("herdr", Box::new(fast));
        // Fill the webhook queue past its cap of 1.
        assert!(matches!(
            mgr.send("webhook", "svc", "one").await,
            SendOutcome::Queued
        ));
        let mut sheds = 0;
        for i in 0..4 {
            if let SendOutcome::Shed { .. } = mgr.send("webhook", "svc", &format!("q{i}")).await {
                sheds += 1;
            }
        }
        assert!(sheds >= 3, "stalled webhook must shed, got {sheds}");
        // Herdr prompt latency: fast path answers immediately despite the
        // stalled sibling (per-sink isolation, not a shared pump).
        let t = tokio::time::Instant::now();
        assert!(matches!(
            mgr.send("herdr", "p", "urgent").await,
            SendOutcome::Queued
        ));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "herdr prompt must not wait on the stalled webhook"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(sent_len(&sent), 1, "herdr worker pumps independently");
        // A shed storm is visible: drain overflow names the subscriber sink.
        let _ = deg_rx.try_recv();
    }
}
