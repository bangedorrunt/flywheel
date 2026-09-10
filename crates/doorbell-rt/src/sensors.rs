// governed-by: ADR-0012
//! Sensors are replaceable edges: small tasks with their own lifecycle.
//! Each `tell`s [`BrainMsg`] into the bounded brain channel; death arrives
//! as `Degraded` (fault-as-data), never fatal. Panics are caught at the
//! task boundary and converted the same way.

use crate::msg::BrainMsg;
use tokio::sync::mpsc;

/// A spawned sensor task.
pub struct SensorHandle {
    pub name: String,
    handle: tokio::task::JoinHandle<()>,
}

impl SensorHandle {
    /// Await the task; a panic becomes `Degraded{panic}` into the brain.
    pub async fn join_into_degraded(self, brain_tx: mpsc::Sender<BrainMsg>) {
        if let Err(e) = self.handle.await {
            let reason = if e.is_panic() {
                let p = e.into_panic();
                let s = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "non-string panic".into());
                format!("panic: {s}")
            } else {
                "cancelled".into()
            };
            let _ = brain_tx
                .send(BrainMsg::Degraded {
                    source: self.name,
                    reason,
                })
                .await;
        }
    }
}
pub fn supervise<F, Fut>(name: impl Into<String>, tx: mpsc::Sender<BrainMsg>, task: F) -> SensorHandle
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let name = name.into();
    let task_name = name.clone();
    let tx_out = tx.clone();
    let handle = tokio::spawn(async move {
        let outcome = task().await;
        let reason = match outcome {
            Ok(()) => "exited clean".into(),
            Err(e) => format!("error: {e:#}"),
        };
        let _ = tx_out
            .send(BrainMsg::Degraded {
                source: task_name,
                reason,
            })
            .await;
    });
    drop(tx);
    SensorHandle { name, handle }
}
pub fn interval_ticker(
    name: impl Into<String>,
    tx: mpsc::Sender<BrainMsg>,
    interval: std::time::Duration,
    make_msg: fn() -> BrainMsg,
) -> SensorHandle {
    let name = name.into();
    let task_name = name.clone();
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if tx.send(make_msg()).await.is_err() {
                break;
            }
        }
        let _ = tx
            .send(BrainMsg::Degraded {
                source: task_name,
                reason: "ticker brain closed".into(),
            })
            .await;
    });
    SensorHandle { name, handle }
}

/// Bridge an off-loop producer into the brain (pipe pattern): fetch I/O
/// off-loop, deliver back as a message. `.5` plugs the bus watch, herdr
/// forwarders, and watch-child NDJSON lines here.
pub fn bridge<T, F>(
    name: impl Into<String>,
    tx: mpsc::Sender<BrainMsg>,
    mut input: mpsc::Receiver<T>,
    mut map: F,
) -> SensorHandle
where
    T: Send + 'static,
    F: FnMut(T) -> Option<BrainMsg> + Send + 'static,
{
    let name = name.into();
    let task_name = name.clone();
    let handle = tokio::spawn(async move {
        while let Some(item) = input.recv().await {
            if let Some(msg) = map(item) {
                // `send().await` = natural backpressure: a slow brain parks
                // this sensor instead of dropping silently.
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        }
        let _ = tx
            .send(BrainMsg::Degraded {
                source: task_name,
                reason: "producer closed".into(),
            })
            .await;
    });
    SensorHandle { name, handle }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bridge_death_is_degraded_data() {
        let (brain_tx, mut brain_rx) = mpsc::channel(16);
        let (prod_tx, prod_rx) = mpsc::channel(4);
        let h = bridge("bus-watch", brain_tx.clone(), prod_rx, |line: String| {
            Some(BrainMsg::Watch(line))
        });
        prod_tx.send("{\"kind\":9}".into()).await.unwrap();
        assert!(matches!(brain_rx.recv().await, Some(BrainMsg::Watch(_))));
        drop(prod_tx);
        // Producer close lands as Degraded, tagged with the source.
        let mut saw_degraded = false;
        while let Some(m) = brain_rx.recv().await {
            if let BrainMsg::Degraded { source, .. } = m {
                assert_eq!(source, "bus-watch");
                saw_degraded = true;
                break;
            }
        }
        assert!(saw_degraded);
        h.handle.abort();
    }

    #[tokio::test]
    async fn backpressure_parks_sender_never_drops() {
        // Bounded brain inbox (cap 1): a slow brain parks the sensor;
        // nothing is silently dropped — both sends eventually land.
        let (brain_tx, mut brain_rx) = mpsc::channel(1);
        let (prod_tx, prod_rx) = mpsc::channel(4);
        let _h = bridge("slow", brain_tx, prod_rx, |i: u8| {
            Some(BrainMsg::Fact {
                summary: format!("{i}"),
            })
        });
        prod_tx.send(1).await.unwrap();
        prod_tx.send(2).await.unwrap();
        // Hold the first message without consuming: the bridge parks on the
        // second send instead of dropping it.
        let first = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            brain_rx.recv(),
        )
        .await
        .expect("first lands")
        .expect("open");
        assert!(matches!(first, BrainMsg::Fact { .. }));
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            brain_rx.recv(),
        )
        .await
        .expect("parked sender still delivers")
        .expect("open");
        assert!(matches!(second, BrainMsg::Fact { .. }));
    }
}
