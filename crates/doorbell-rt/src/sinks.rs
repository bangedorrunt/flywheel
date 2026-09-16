// governed-by: ADR-0012
//! Sinks are bounded, shed, supervised. One actor per subscription sink:
//! the manager owns every queue (exact capacity, exact accounting), workers
//! pump adapters. Full queue or parked sink sheds to digest — the brain's
//! latency never depends on a sink. Endpoint failure parks the sink and
//! moves the remainder to the shed overflow (journaled + digested, never
//! silently dropped). Graceful stop closes queues and returns everything
//! still held as [`Undelivered`] so resume redelivers from the journal.

use crate::error::RtError;
use crate::msg::BrainMsg;
use crate::policy::RestartPolicy;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, mpsc};

/// Delivery endpoint. The brain's only delivery intent is `Prompt{pin,line}`;
/// adapters translate it (herdr prompt today, webhook/DM in Phase 2).
pub trait PromptSink: Send + 'static {
    fn send(&mut self, pin: &str, line: &str) -> Result<(), RtError>;
    fn name(&self) -> &str;
}

type Intent = (String, String);

/// Builds a fresh adapter for a sink. Supervision (audit P1): without a
/// factory a parked sink could never be restarted — the failed endpoint
/// adapter is gone with its worker, so `restart` needs a way to mint a
/// new one. `add_sink` without a factory keeps working (restart then
/// refuses loudly via `false`).
type SinkFactory = std::sync::Arc<dyn Fn() -> Box<dyn PromptSink> + Send + Sync>;

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
    Shed {
        pin: String,
        line: String,
    },
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
    /// Fresh-adapter builder for supervised restarts (None = not
    /// restartable).
    factory: Option<SinkFactory>,
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

    /// Is an adapter registered under this name?
    ///
    /// bd-8ulu: a wake routed to a name nobody registered would be absorbed
    /// by nobody, so the brain must not count it as a route.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.sinks.contains_key(name)
    }

    /// Register a sink adapter; spawns its supervised worker. The sink
    /// cannot be restarted after an endpoint failure (see
    /// [`Self::add_sink_with_factory`] for supervised registration).
    pub fn add_sink(&mut self, name: impl Into<String>, sink: Box<dyn PromptSink>) {
        self.add_sink_inner(name, sink, None);
    }

    /// Register with a fresh-adapter builder so [`Self::restart_sink`] can
    /// revive the sink under the Transient policy after an endpoint
    /// failure.
    pub fn add_sink_with_factory(
        &mut self,
        name: impl Into<String>,
        sink: Box<dyn PromptSink>,
        factory: impl Fn() -> Box<dyn PromptSink> + Send + Sync + 'static,
    ) {
        self.add_sink_inner(name, sink, Some(std::sync::Arc::new(factory)));
    }

    fn add_sink_inner(
        &mut self,
        name: impl Into<String>,
        sink: Box<dyn PromptSink>,
        factory: Option<SinkFactory>,
    ) {
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
                factory,
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

    /// Restart a parked sink under the Transient policy using its
    /// registered factory, keeping the queued intents (the new worker
    /// pumps them). False = cap exhausted, not parked, or no factory —
    /// stays parked (+ caller emits Degraded/audit). Audit P1: this was
    /// unwired (zero callers) — a parked sink stayed dead until process
    /// restart, contradicting the supervised-sink contract.
    pub async fn restart_sink(&mut self, sink: &str, now_unix: u64) -> bool {
        let Some(st) = self.sinks.get_mut(sink) else {
            return false;
        };
        let Some(factory) = st.factory.clone() else {
            return false;
        };
        {
            let inner = st.inner.lock().await;
            if !inner.parked || inner.closed {
                return false;
            }
        }
        if !st.policy.should_restart(now_unix) {
            return false;
        }
        let degraded_tx = self.degraded_tx.clone();
        let name = sink.to_string();
        {
            let mut inner = st.inner.lock().await;
            inner.parked = false;
            inner.notify.notify_one();
        }
        if let Some(w) = st.worker.take() {
            let _ = w.await;
        }
        let inner = st.inner.clone();
        st.worker = Some(tokio::spawn(worker_loop(
            name,
            inner,
            factory(),
            degraded_tx,
        )));
        true
    }

    /// Move shed overflow back into the queue (supervised restart: the
    /// new worker pumps it again — at-least-once). Notifies so a
    /// freshly-parked worker wakes. Returns the count requeued.
    pub async fn requeue_shed_overflow(&mut self, sink: &str) -> usize {
        let Some(st) = self.sinks.get(sink) else {
            return 0;
        };
        let mut inner = st.inner.lock().await;
        let n = inner.shed_overflow.len();
        if n > 0 {
            let overflow: Vec<Intent> = inner.shed_overflow.drain(..).collect();
            inner.queue.extend(overflow);
            inner.notify.notify_one();
        }
        n
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
                    // Wait WITHOUT holding the lock across the await. Holding
                    // it wedges `send()` (audit P0, probe-proven): the parked
                    // worker owns the lock while awaiting, so a sender can
                    // never take the lock to push+notify — the first prompt
                    // after the worker goes idle hung forever. Clone the
                    // Notify handle, drop the guard, then await: tokio's
                    // Notify stores a permit when no waiter is registered,
                    // so a send between drop and await cannot be missed.
                    let notify = guard.notify.clone();
                    drop(guard);
                    notify.notified().await;
                    continue;
                }
            }
        };
        let (pin, line) = next;
        if let Err(e) = adapter.send(&pin, &line) {
            // Park + shed the remainder AND the failed intent under one
            // lock. Audit P2: the failed intent itself used to be dropped
            // here (only the queue remainder shed) — contradicting
            // at-least-once; it now lands in shed overflow first so the
            // original order is preserved.
            let mut guard = inner.lock().await;
            guard.parked = true;
            guard.sheds += (guard.queue.len() + 1) as u64;
            guard.shed_overflow.push((pin, line));
            let rest: Vec<Intent> = guard.queue.drain(..).collect();
            guard.shed_overflow.extend(rest);
            drop(guard);
            let _ = degraded_tx
                .send(BrainMsg::Degraded {
                    source: format!("sink:{name}"),
                    reason: format!("delivery failed: {e:#}"),
                })
                .await;
            return;
        }
        // F5: delivery receipt — hear keys off this, never enqueue.
        let _ = degraded_tx.send(BrainMsg::Delivered { pin }).await;
    }
}

/// Shared sent-log handle for the recording test sinks.
type SentLog = Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// Recording sink for tests; `stall_ms` simulates a slow endpoint.
pub struct MockSink {
    pub sent: SentLog,
    pub stall_ms: u64,
    pub fail_after: Option<usize>,
}

impl MockSink {
    pub fn new() -> (Self, SentLog) {
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
    fn send(&mut self, pin: &str, line: &str) -> Result<(), RtError> {
        if self.stall_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.stall_ms));
        }
        let fail_after = self.fail_after;
        let mut sent = self.sent_mut();
        if let Some(n) = fail_after
            && sent.len() >= n
        {
            return Err(RtError::EndpointDown);
        }
        sent.push((pin.into(), line.into()));
        Ok(())
    }

    fn name(&self) -> &str {
        "mock"
    }
}
/// herdr CLI prompt sink (bd-ene2 Phase 3): delivers via
/// `herdr agent prompt <name> <line>`, resolving the pin through the
/// shared routing table the `FileHost` index refresh owns. Unknown pin
/// audits LOUD to stderr and counts delivered-skipped as Ok: the pin
/// left the registry mid-queue (operator error), and parking the whole
/// sink over one stale intent would shed every other pin's wake. This
/// is the one documented delivery difference from the embedded path
/// (which sheds unknown pins to digest); the miss stays visible in the
/// audit trail either way.
pub struct CliPromptSink {
    bin: String,
    targets: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
    audit_dir: Option<std::path::PathBuf>,
}

impl CliPromptSink {
    pub fn new(
        bin: impl Into<String>,
        targets: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
    ) -> Self {
        Self {
            bin: bin.into(),
            targets,
            audit_dir: None,
        }
    }

    pub fn with_audit_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.audit_dir = Some(dir);
        self
    }

    fn miss(&self, pin: &str) {
        eprintln!("doorbell: [DoorbellMiss] pin {pin} not in registry — no pane to prompt (loud)");
    }
}

impl PromptSink for CliPromptSink {
    fn send(&mut self, pin: &str, line: &str) -> Result<(), RtError> {
        let name = self
            .targets
            .lock()
            .ok()
            .and_then(|t| t.get(pin).cloned())
            .unwrap_or_default();
        if name.is_empty() {
            self.miss(pin);
            return Ok(());
        }
        let out = crate::file_host::run_cli(&self.bin, &["agent", "prompt", &name, line], 30)
            .ok_or(RtError::PromptSpawnFailed)?;
        if !out.status.success() {
            return Err(RtError::PromptFailed(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "herdr-cli"
    }
}

/// Operator-approved egress hosts, comma/space separated. Absent = empty
/// allowlist = no webhook endpoint ever registers (fail-closed).
///
/// The daemon is launched by launchd/systemd, so an operator surface has to
/// be an env var it can carry in the unit — same shape as
/// `DOORBELL_TORON_BIN`/`DOORBELL_HERDR_BIN`.
pub const EGRESS_ALLOWLIST_ENV: &str = "DOORBELL_WEBHOOK_ALLOWLIST";

/// Operator-approved egress hosts (D6 threat table: attention metadata
/// must not exfiltrate). Exact host or `.suffix` match, lowercase.
#[derive(Debug, Clone, Default)]
pub struct EgressAllowlist {
    hosts: Vec<String>,
}

impl EgressAllowlist {
    /// Read the operator's allowlist from the environment (bd-8ulu).
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(
            std::env::var(EGRESS_ALLOWLIST_ENV)
                .unwrap_or_default()
                .split([',', ' ', ';', '\n'])
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string),
        )
    }

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
    /// Parsed once at construction — the send path never re-parses the URL.
    host: String,
    port: u16,
    path: String,
    full_line: bool,
    timeout: std::time::Duration,
}

/// `(host, port, path)` from an explicit-scheme URL.
///
/// Phase 2 speaks plain http (TLS terminates at the operator's edge, see
/// [`WebhookSink::post`]), so the port must come from the URL: an `https://`
/// endpoint still dials the port written here rather than being silently
/// rewritten to 443 and 80. Returns `None` for a host-less or port-less
/// malformed URL so [`WebhookSink::new`] can fail closed instead of dialling
/// something the operator did not write (bd-8ulu: the old send path hardcoded
/// port 80 and path `/`, so no real endpoint could be reached at all).
fn url_host_port_path(url: &str) -> Option<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let authority = authority.split(['?', '#']).next().unwrap_or("");
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_lowercase(), port, path))
}

impl WebhookSink {
    /// Refuses non-allowlisted hosts at construction (fail-closed).
    pub fn new(
        url: impl Into<String>,
        allowlist: &EgressAllowlist,
        full_line: bool,
    ) -> Result<Self, RtError> {
        let url = url.into();
        if !allowlist.allows(&url) {
            return Err(RtError::NotAllowlisted(url));
        }
        let Some((host, port, path)) = url_host_port_path(&url) else {
            return Err(RtError::InvalidUrl(url));
        };
        Ok(Self {
            host,
            port,
            path,
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

    fn post(&self, body: &str) -> Result<(), RtError> {
        use std::io::{Read, Write};
        let host = self.host.as_str();
        let authority = if self.port == 80 {
            host.to_string()
        } else {
            format!("{host}:{}", self.port)
        };
        let mut stream = std::net::TcpStream::connect_timeout(
            &authority
                .parse()
                .map_err(|e| RtError::InvalidUrl(format!("{authority}: {e:?}")))?,
            self.timeout,
        )?;
        // Only plain-http loopback/allowlisted hosts in Phase 2 (TLS via
        // the operator's terminator; revisit with push transport).
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_read_timeout(Some(self.timeout))?;
        write!(
            stream,
            "POST {} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.path,
            body.len()
        )?;
        let mut resp = [0u8; 12];
        stream.read_exact(&mut resp)?;
        let status: u16 = String::from_utf8_lossy(&resp[9..12]).parse().unwrap_or(0);
        if !(200..300).contains(&status) {
            return Err(RtError::BadStatus {
                host: host.to_string(),
                status,
            });
        }
        Ok(())
    }
}

impl PromptSink for WebhookSink {
    fn send(&mut self, pin: &str, line: &str) -> Result<(), RtError> {
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
    fn send_dm(&mut self, to: &str, hint: &str) -> Result<(), RtError>;
}

/// Key custody gate: only `svc:` service identities minted by the daemon.
/// Anything else (human npub, nsec, opaque) is refused — the daemon must
/// never hold human keys (D6).
impl DmSink {
    pub fn for_service(
        service_id: impl Into<String>,
        transport: Box<dyn DmTransport>,
    ) -> Result<Self, RtError> {
        let service_id = service_id.into();
        if !service_id.starts_with("svc:") {
            return Err(RtError::DmRefused);
        }
        Ok(Self {
            service_id,
            transport,
        })
    }
}

impl PromptSink for DmSink {
    fn send(&mut self, pin: &str, line: &str) -> Result<(), RtError> {
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

impl Default for MockDmTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl DmTransport for MockDmTransport {
    fn send_dm(&mut self, to: &str, hint: &str) -> Result<(), RtError> {
        if self.fail {
            return Err(RtError::DmDown);
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

    /// Audit P0 regression: send AFTER the worker has drained its queue and
    /// parked. The parked worker must NOT hold the queue lock across its
    /// wait — holding it wedged every later `send()` (the first prompt after
    /// idle hung forever). Guard-free wait + Notify permit = no lost wakeup.
    #[tokio::test]
    async fn send_after_worker_idle_does_not_wedge() {
        let (deg_tx, _deg_rx) = mpsc::channel(16);
        let mut mgr = SinkManager::new(deg_tx);
        let (sink, sent) = MockSink::new();
        mgr.add_sink("herdr", Box::new(sink));
        // Yield long enough for the worker to drain the (empty) queue and park.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            mgr.send("herdr", "p", "after-idle").await
        })
        .await
        .expect("send after idle must not wedge on the parked worker's lock");
        assert!(matches!(outcome, SendOutcome::Queued), "got {outcome:?}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(sent_len(&sent), 1, "intent must deliver after the park");
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

    /// bd-8ulu: the send path must honour the URL's port and path. It used to
    /// hardcode `Host: {host}` against port 80 and `POST / HTTP/1.1`, so no
    /// real endpoint (which is almost never root-path on 80) could be reached
    /// and the operator's URL was silently only a host.
    #[test]
    fn webhook_url_parses_port_and_path() {
        assert_eq!(
            url_host_port_path("http://127.0.0.1:18999/wake"),
            Some(("127.0.0.1".to_string(), 18999, "/wake".to_string()))
        );
        assert_eq!(
            url_host_port_path("http://hooks.internal"),
            Some(("hooks.internal".to_string(), 80, "/".to_string()))
        );
        assert_eq!(
            url_host_port_path("https://ops.example:8443/a/b"),
            Some(("ops.example".to_string(), 8443, "/a/b".to_string())),
            "an https URL keeps the written port (TLS terminates at the edge)"
        );
        // Malformed is refused rather than guessed at.
        assert_eq!(url_host_port_path("hooks.internal:18999/wake"), None);
        assert_eq!(url_host_port_path("http://"), None);
        assert_eq!(url_host_port_path("http://h:notaport/"), None);
    }

    #[test]
    fn webhook_sink_refuses_malformed_and_unallowlisted_urls() {
        let allow = EgressAllowlist::new(["127.0.0.1".to_string()]);
        assert!(WebhookSink::new("http://127.0.0.1:18999/wake", &allow, false).is_ok());
        assert!(
            WebhookSink::new("http://elsewhere.example:9/w", &allow, false).is_err(),
            "an unallowlisted host is refused at construction"
        );
        assert!(
            WebhookSink::new("127.0.0.1:18999/wake", &allow, false).is_err(),
            "a schemeless URL is malformed, not a host:port"
        );
    }

    /// `has` gates the brain's route test, so it must answer for exactly the
    /// names `add_sink*` registered.
    #[test]
    fn manager_reports_registered_sinks() {
        let (deg_tx, _deg_rx) = mpsc::channel(16);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut mgr = SinkManager::new(deg_tx);
            assert!(!mgr.has("webhook"), "nothing registered yet");
            let (sink, _sent) = MockSink::new();
            mgr.add_sink("webhook", Box::new(sink));
            assert!(mgr.has("webhook"));
            assert!(!mgr.has("dm"));
        });
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

    #[tokio::test]
    async fn failed_intent_is_preserved_in_shed_overflow() {
        // Audit P2: the intent that FAILED at the endpoint used to be
        // dropped (only the queue remainder shed) — contradicting
        // at-least-once. It must land in shed overflow, first.
        let (deg_tx, _deg_rx) = mpsc::channel(16);
        let mut mgr = SinkManager::new(deg_tx);
        let (mut sink, _sent) = MockSink::new();
        sink.fail_after = Some(0);
        mgr.add_sink("webhook", Box::new(sink));
        mgr.send("webhook", "p", "the-failed-one").await;
        mgr.send("webhook", "p", "queued-behind").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(mgr.parked("webhook").await);
        let drained = mgr.drain_shed_overflow().await;
        let lines: Vec<&str> = drained.iter().map(|u| u.line.as_str()).collect();
        assert_eq!(
            lines.first(),
            Some(&"the-failed-one"),
            "failed intent is shed first, got {lines:?}"
        );
        assert!(lines.contains(&"queued-behind"), "{lines:?}");
    }

    #[tokio::test]
    async fn restart_sink_revives_and_requeues() {
        // Audit P1: supervised restart with a fresh adapter; the shed
        // overflow requeues and the new worker delivers it.
        let (deg_tx, _deg_rx) = mpsc::channel(16);
        let mut mgr = SinkManager::new(deg_tx);
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counting {
            n: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            fail: bool,
        }
        impl PromptSink for Counting {
            fn send(&mut self, _pin: &str, _line: &str) -> Result<(), RtError> {
                if self.fail {
                    return Err(RtError::EndpointDown);
                }
                self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn name(&self) -> &str {
                "counting"
            }
        }
        let n2 = n.clone();
        mgr.add_sink_with_factory(
            "webhook",
            Box::new(Counting {
                n: n.clone(),
                fail: true,
            }),
            move || {
                Box::new(Counting {
                    n: n2.clone(),
                    fail: false,
                })
            },
        );
        mgr.send("webhook", "p", "one").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(mgr.parked("webhook").await);
        assert!(mgr.restart_sink("webhook", 1_000).await, "restarts");
        assert_eq!(mgr.requeue_shed_overflow("webhook").await, 1);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            n.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "fresh adapter delivered the requeued intent"
        );
        assert!(!mgr.parked("webhook").await);
        // Missing sink / not parked: restart refuses without burning a
        // policy slot.
        assert!(!mgr.restart_sink("missing", 9_000).await);
        assert!(!mgr.restart_sink("webhook", 9_000).await);
    }

    #[tokio::test]
    async fn restart_sink_refuses_without_factory() {
        let (deg_tx, _deg_rx) = mpsc::channel(16);
        let mut mgr = SinkManager::new(deg_tx);
        let (mut sink, _sent) = MockSink::new();
        sink.fail_after = Some(0);
        mgr.add_sink("webhook", Box::new(sink));
        mgr.send("webhook", "p", "x").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(mgr.parked("webhook").await);
        assert!(
            !mgr.restart_sink("webhook", 1_000).await,
            "no factory: restart refuses loudly (false)",
        );
        assert!(mgr.parked("webhook").await, "still parked");
    }
}
