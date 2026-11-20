// governed-by: ADR-0012
//! One host loop, two entry points (wave7-a2): the standalone `doorbell`
//! daemon (`doorbell-cli::serve`) and flywheel's embedded `__doorbell`
//! (`doorbell_host::run_embedded`) both drive their brain through
//! [`HostRunner`]. Entry points differ only by transport/config: they build
//! their `Host` + `SinkManager` from [`HostRunner::sender`] /
//! [`HostRunner::died_bell`], arm their own Msg producers, then hand the
//! pieces to [`HostRunner::drive`]. No second loop, no second sweep.

use crate::{
    Brain, BrainConfig, BrainMsg, DiedBell, FileStore, Host, RunReport, SensorHandle, SinkManager,
    brain::BrainLiveness,
};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Canonical sweep beat both hosts run (1s `Sweep` Msg, never inside the fold).
pub const SWEEP_SECS: u64 = 1;
/// Canonical ack-SLA beat both hosts run (60s `AckSlaTick` Msg).
pub const ACK_SLA_SECS: u64 = 60;

/// The one loop shape both hosts run. Built only via
/// [`RunnerConfig::standard`] so the two entry points cannot diverge.
#[derive(Clone)]
pub struct RunnerConfig {
    /// Workspace the brain is hosted for (feeds `BrainConfig::new`).
    pub workspace: String,
    /// Seconds between `Sweep` Msgs.
    pub sweep_secs: u64,
    /// Seconds between `AckSlaTick` Msgs.
    pub ack_sla_secs: u64,
    /// Daemon liveness probe; embedded passes none (it retires itself).
    pub liveness: Option<Arc<BrainLiveness>>,
}

impl RunnerConfig {
    /// The one constructor both entry points use — same shape by construction.
    pub fn standard(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            sweep_secs: SWEEP_SECS,
            ack_sla_secs: ACK_SLA_SECS,
            liveness: None,
        }
    }

    /// Attach the daemon's liveness probe (embedded leaves this empty).
    pub fn with_liveness(mut self, liveness: Arc<BrainLiveness>) -> Self {
        self.liveness = Some(liveness);
        self
    }
}

/// Capacity of the status lane (bd-0uzrq.17). A status is edge-triggered, so
/// eight is already generous: if the brain is eight statuses behind, the poll
/// producer should park rather than pile up stale values.
pub const STATUS_LANE_CAP: usize = 8;

/// Phase 1 of the hosted loop: the bounded channel ([`crate::BRAIN_CAP`],
/// slow producers park — backpressure is the queue, never a drop), the
/// [`DiedBell`], and the two canonical Msg-producer tickers.
///
/// Entry points build their host and sinks from [`HostRunner::sender`] and
/// [`HostRunner::died_bell`], arm any extra producers on the sender, then
/// call [`HostRunner::drive`]. Created either fresh ([`HostRunner::start`],
/// embedded) or around an existing pair ([`HostRunner::adopt`], the daemon
/// keeps its own sender for serve-shutdown injection).
pub struct HostRunner {
    config: RunnerConfig,
    tx: mpsc::Sender<BrainMsg>,
    rx: Option<mpsc::Receiver<BrainMsg>>,
    /// bd-0uzrq.17: the status-only lane, polled before mail. Small on
    /// purpose — a status is edge-triggered, so a backlog means the brain is
    /// behind, and parking the producer is the honest signal.
    status_tx: mpsc::Sender<BrainMsg>,
    status_rx: Option<mpsc::Receiver<BrainMsg>>,
    died_bell: Arc<DiedBell>,
    _sweep: SensorHandle,
    _sla: SensorHandle,
    _beads: Option<SensorHandle>,
}

impl HostRunner {
    /// Fresh loop: new bounded channel, tickers armed.
    pub fn start(config: RunnerConfig) -> Self {
        let (tx, rx) = mpsc::channel(crate::BRAIN_CAP);
        Self::adopt(tx, rx, config)
    }

    /// Adopt the caller's channel (the daemon's `run` keeps a sender for
    /// shutdown injection); tickers arm on the adopted sender.
    pub fn adopt(
        tx: mpsc::Sender<BrainMsg>,
        rx: mpsc::Receiver<BrainMsg>,
        config: RunnerConfig,
    ) -> Self {
        let sweep = crate::interval_ticker(
            "sweep",
            tx.clone(),
            Duration::from_secs(config.sweep_secs),
            || BrainMsg::Sweep {
                now: crate::lease::unix_now(),
            },
        );
        let sla = crate::interval_ticker(
            "ack-sla",
            tx.clone(),
            Duration::from_secs(config.ack_sla_secs),
            || BrainMsg::AckSlaTick {
                now: crate::lease::unix_now(),
            },
        );
        let (status_tx, status_rx) = mpsc::channel(STATUS_LANE_CAP);
        Self {
            config,
            tx,
            rx: Some(rx),
            status_tx,
            status_rx: Some(status_rx),
            died_bell: DiedBell::new(),
            _sweep: sweep,
            _sla: sla,
            _beads: None,
        }
    }

    /// Sender the entry point's host, sinks, and extra producers share.
    /// bd-0uzrq.17: the STATUS lane's sender. Status producers hand this to
    /// whoever owns the plane poll; the brain polls it before mail so a
    /// status cannot queue behind a mail-bound dispatch.
    pub fn status_sender(&self) -> mpsc::Sender<BrainMsg> {
        self.status_tx.clone()
    }

    pub fn sender(&self) -> mpsc::Sender<BrainMsg> {
        self.tx.clone()
    }

    /// The death bell the entry point's host and brain share.
    pub fn died_bell(&self) -> Arc<DiedBell> {
        Arc::clone(&self.died_bell)
    }

    /// Arm the read-only beads-ledger poller on this loop's channel.
    pub fn spawn_beads_poller(&mut self, repo: PathBuf) {
        self._beads = Some(crate::pollers::spawn_beads_poller(self.tx.clone(), repo));
    }

    /// Phase 2: build the brain around the entry point's host/sinks/store
    /// and drive it to exit. `shutdown` is the cancellation token: when it
    /// fires, a [`BrainMsg::Shutdown`] is queued and the brain drains to its
    /// journaled exit. Entry points with no token pass
    /// `std::future::pending()` — exit then comes from the brain's own
    /// `Shutdown` Msg (liveness retire / serve shutdown injection), exactly
    /// as before.
    pub async fn drive<H, S>(
        mut self,
        host: H,
        sinks: SinkManager,
        store: FileStore,
        shutdown: S,
    ) -> RunReport
    where
        H: Host,
        S: Future<Output = ()> + Send,
    {
        let mut brain = Brain::new(
            BrainConfig::new(self.config.workspace.clone()),
            host,
            store,
            sinks,
        )
        .with_died_bell(Arc::clone(&self.died_bell));
        if let Some(liveness) = self.config.liveness.clone() {
            brain = brain.with_liveness(liveness);
        }
        let rx = self.rx.take().expect("HostRunner::drive runs once");
        let status_rx = self.status_rx.take();
        let run = brain.run_with_status(rx, status_rx);
        tokio::pin!(run);
        tokio::select! {
            report = &mut run => report,
            _ = shutdown => {
                let _ = self.tx.send(BrainMsg::Shutdown).await;
                run.await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Effect, MockHost, NoTextSink};

    /// Both entry points construct through `standard`: the shape is pinned
    /// here, so a drift in either call site fails this test, not production.
    #[test]
    fn both_hosts_share_one_runner_shape() {
        let daemon = RunnerConfig::standard("ws").with_liveness(Arc::new(BrainLiveness::new()));
        let embedded = RunnerConfig::standard("ws");
        assert_eq!(daemon.workspace, embedded.workspace);
        assert_eq!(daemon.sweep_secs, SWEEP_SECS);
        assert_eq!(embedded.sweep_secs, SWEEP_SECS);
        assert_eq!(daemon.ack_sla_secs, ACK_SLA_SECS);
        assert_eq!(embedded.ack_sla_secs, ACK_SLA_SECS);
        assert_eq!((SWEEP_SECS, ACK_SLA_SECS), (1, 60));
        assert!(daemon.liveness.is_some());
        assert!(embedded.liveness.is_none());
    }

    /// One tick per message: every Msg sent before the token fires is
    /// dispatched exactly once, then the Shutdown the runner queued on the
    /// token ends the loop (cancellation, no hang). The observable is the
    /// journal: `on_ack_sla` journals every tick unconditionally, while
    /// idle sweeps write no row. Three sent ticks plus the ack-sla ticker's
    /// own immediate first beat (tokio intervals fire once at startup by
    /// design) mean exactly four `AckSlaTick` rows: a lost message shows 3,
    /// a double dispatch shows 5+.
    #[tokio::test]
    async fn runner_dispatches_each_message_once_then_exits_on_token() {
        let dir =
            std::env::temp_dir().join(format!("doorbell-runner-dispatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let runner = HostRunner::start(RunnerConfig::standard("runner-test"));
        let tx = runner.sender();
        let mut host = MockHost::new(1);
        for i in 0..3 {
            host.script.push_back(vec![Effect::Prompt {
                event_id: None,
                pin: "p".into(),
                line: format!("line-{i}"),
            }]);
        }
        let sinks = SinkManager::new(tx.clone());
        for now in [7u64, 8, 9] {
            tx.send(BrainMsg::AckSlaTick { now }).await.unwrap();
        }
        let (fire, wait) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = fire.send(());
        });
        let store = FileStore::new(&dir);
        let report = tokio::time::timeout(
            Duration::from_secs(15),
            runner.drive(host, sinks, store, async move {
                let _ = wait.await;
            }),
        )
        .await
        .expect("drive must exit on the token");
        let _ = report.stats;
        let raw = std::fs::read_to_string(FileStore::new(&dir).journal_file("runner-test"))
            .expect("brain journals to the store");
        let ticks = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["msg"]["rt_kind"] == "AckSlaTick")
            .count();
        assert_eq!(
            ticks, 4,
            "3 sent ticks + 1 ticker immediate beat, journal:\n{raw}"
        );
    }

    /// Cancellation with nothing queued: an already-fired token still ends
    /// the loop cleanly.
    #[tokio::test]
    async fn runner_exits_on_pre_fired_token() {
        let runner = HostRunner::start(RunnerConfig::standard("runner-test"));
        let tx = runner.sender();
        let mut sinks = SinkManager::new(tx);
        sinks.add_sink_with_factory("hc", Box::new(NoTextSink::new()), || {
            Box::new(NoTextSink::new())
        });
        let store = FileStore::new(
            std::env::temp_dir().join(format!("doorbell-runner-token-{}", std::process::id())),
        );
        let report = tokio::time::timeout(
            Duration::from_secs(15),
            runner.drive(
                MockHost::new(1),
                sinks,
                store,
                async { /* fires immediately */ },
            ),
        )
        .await
        .expect("drive must exit on a pre-fired token");
        let _ = report.stats;
    }
}
