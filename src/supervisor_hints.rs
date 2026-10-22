//! Phase 4b.7: bus-watch hints for the supervisor host.
//!
//! The supervisor host is level-triggered: every round re-reads the facts (a
//! pending steer's cursor, deaf facts, new registry spawns, operator inputs)
//! and the tick is the truth. A hint is only an accelerator: "something that
//! round reads just changed, look now". Losing one costs one tick; dropping
//! the whole channel changes latency and nothing else (the same messages fold
//! to the same model, because a hint carries no data and `round` does not take
//! one).
//!
//! The source is a cheap poll of the files those reads come from: toron's
//! shared index (`index.sqlite` and its `-wal`, where `toron next` commits a
//! cursor) and the context's own input file (where `flywheel supervisor
//! fact|steer` append). A change sends one hint, coalesced to at most one per
//! [`MIN_GAP_MS`], so a busy bus cannot spin the host.
//!
//! Choice: the brief's `Hint{child}` carries no child here. A hinted round
//! reads every fact anyway (it is the same round as a tick), so the name would
//! be unused; dropping it keeps the channel a unit and the fold untouched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant, SystemTime};

/// How often the watcher looks at the files.
pub const POLL_MS: u64 = 25;
/// At most one hint per this long.
pub const MIN_GAP_MS: u64 = 50;

/// What ended a wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    /// A hint arrived: run a round now.
    Hint,
    /// The tick elapsed (or the hint channel is gone): run the periodic round.
    Tick,
}

/// Wait for a hint or `tick`, whichever is first. Pending hints are drained
/// into one wake (a burst is one round). A closed channel waits out the tick,
/// which is exactly the pre-hint behaviour.
pub fn wait(rx: &Receiver<()>, tick: Duration) -> Woke {
    match rx.recv_timeout(tick) {
        Ok(()) => {
            while rx.try_recv().is_ok() {}
            Woke::Hint
        }
        Err(RecvTimeoutError::Timeout) => Woke::Tick,
        Err(RecvTimeoutError::Disconnected) => {
            std::thread::sleep(tick);
            Woke::Tick
        }
    }
}

type Sig = Option<(SystemTime, u64)>;

fn signature(paths: &[PathBuf]) -> Vec<Sig> {
    paths
        .iter()
        .map(|p| {
            std::fs::metadata(p)
                .ok()
                .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
        })
        .collect()
}

/// The files a supervisor host's reads come from: toron's index and its WAL,
/// and the context's input file.
pub fn watched_paths(index_db: &std::path::Path, inbox: &std::path::Path) -> Vec<PathBuf> {
    let mut wal = index_db.as_os_str().to_os_string();
    wal.push("-wal");
    vec![
        index_db.to_path_buf(),
        PathBuf::from(wal),
        inbox.to_path_buf(),
    ]
}

/// A thread that sends a hint when any watched file changes.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    /// Start watching `paths`; hints go to `tx` (a full channel drops the
    /// hint: one is already pending).
    pub fn spawn(
        paths: Vec<PathBuf>,
        tx: SyncSender<()>,
        poll: Duration,
        min_gap: Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let sp = stop.clone();
        let join = std::thread::Builder::new()
            .name("flywheel-supervisor-hints".into())
            .spawn(move || {
                let mut last = signature(&paths);
                let mut sent = Instant::now() - min_gap;
                let mut pending = false;
                while !sp.load(Ordering::Relaxed) {
                    std::thread::sleep(poll);
                    let now = signature(&paths);
                    if now != last {
                        last = now;
                        pending = true;
                    }
                    // A change inside the gap is held and sent when it ends:
                    // never lost, never faster than one per `min_gap`.
                    if pending && sent.elapsed() >= min_gap {
                        pending = false;
                        sent = Instant::now();
                        let _ = tx.try_send(());
                    }
                }
            })
            .expect("spawn flywheel-supervisor-hints");
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_hint_ends_a_long_wait_at_once() {
        let (tx, rx) = sync_channel::<()>(1);
        let t = Instant::now();
        let h = std::thread::spawn(move || wait(&rx, ms(5_000)));
        std::thread::sleep(ms(30));
        tx.send(()).unwrap();
        assert_eq!(h.join().unwrap(), Woke::Hint);
        assert!(
            t.elapsed() < ms(500),
            "a 5 s tick did not delay the hinted round: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn without_a_hint_the_wait_is_the_tick() {
        let (_tx, rx) = sync_channel::<()>(1);
        let t = Instant::now();
        assert_eq!(wait(&rx, ms(120)), Woke::Tick);
        assert!(t.elapsed() >= ms(110));
    }

    #[test]
    fn a_dropped_channel_degrades_to_the_plain_tick() {
        let (tx, rx) = sync_channel::<()>(1);
        drop(tx);
        let t = Instant::now();
        assert_eq!(wait(&rx, ms(120)), Woke::Tick);
        assert!(t.elapsed() >= ms(110), "no busy loop on a dead channel");
    }

    #[test]
    fn a_burst_of_hints_is_one_wake() {
        let (tx, rx) = sync_channel::<()>(8);
        for _ in 0..8 {
            tx.send(()).unwrap();
        }
        assert_eq!(wait(&rx, ms(5_000)), Woke::Hint);
        let t = Instant::now();
        assert_eq!(wait(&rx, ms(100)), Woke::Tick, "the burst was drained");
        assert!(t.elapsed() >= ms(90));
    }

    #[test]
    fn the_watched_paths_are_the_index_its_wal_and_the_input_file() {
        let p = watched_paths(
            std::path::Path::new("/d/index.sqlite"),
            std::path::Path::new("/s/k.in.jsonl"),
        );
        assert_eq!(
            p,
            vec![
                PathBuf::from("/d/index.sqlite"),
                PathBuf::from("/d/index.sqlite-wal"),
                PathBuf::from("/s/k.in.jsonl")
            ]
        );
    }

    fn recv_within(rx: &Receiver<()>, d: Duration) -> bool {
        rx.recv_timeout(d).is_ok()
    }

    /// `fs::write` is create-then-write: one change can show as two (and so
    /// two hints). Let a step finish and drop the extras before the next one.
    fn settle(rx: &Receiver<()>) {
        std::thread::sleep(ms(120));
        while rx.try_recv().is_ok() {}
    }

    #[test]
    fn a_change_to_any_watched_file_sends_a_hint_and_quiet_sends_none() {
        let d = tempfile::tempdir().unwrap();
        let (db, wal, inbox) = (
            d.path().join("i.sqlite"),
            d.path().join("i.sqlite-wal"),
            d.path().join("in"),
        );
        std::fs::write(&db, b"x").unwrap();
        let (tx, rx) = sync_channel::<()>(4);
        let _w = Watcher::spawn(
            vec![db.clone(), wal.clone(), inbox.clone()],
            tx,
            ms(10),
            ms(20),
        );
        assert!(!recv_within(&rx, ms(150)), "nothing changed: no hint");
        std::fs::write(&wal, b"first").unwrap(); // the file did not exist: appearing is a change
        assert!(recv_within(&rx, ms(500)), "the WAL appeared");
        settle(&rx);
        std::fs::write(&inbox, b"{}\n").unwrap();
        assert!(recv_within(&rx, ms(500)), "the input file appeared");
        settle(&rx);
        std::fs::write(&db, b"longer content").unwrap();
        assert!(recv_within(&rx, ms(500)), "the index changed");
        settle(&rx);
        assert!(!recv_within(&rx, ms(150)), "and then it is quiet again");
    }

    #[test]
    fn a_busy_bus_is_coalesced_to_one_hint_per_gap() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("i.sqlite-wal");
        std::fs::write(&f, b"0").unwrap();
        let (tx, rx) = sync_channel::<()>(64);
        let _w = Watcher::spawn(vec![f.clone()], tx, ms(5), ms(100));
        std::thread::sleep(ms(60));
        let t = Instant::now();
        let mut n = 0u64;
        while t.elapsed() < ms(450) {
            n += 1;
            std::fs::write(&f, n.to_string().repeat(n as usize % 7 + 1)).unwrap();
            std::thread::sleep(ms(7));
        }
        std::thread::sleep(ms(150));
        let hints = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert!(
            hints >= 2,
            "a long burst is not collapsed to nothing: {hints}"
        );
        assert!(
            hints <= 6,
            "at most one per 100 ms over ~0.6 s, not one per write ({n} writes): {hints}"
        );
    }

    #[test]
    fn a_change_inside_the_gap_is_held_and_sent_not_lost() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("w");
        std::fs::write(&f, b"0").unwrap();
        let (tx, rx) = sync_channel::<()>(8);
        let _w = Watcher::spawn(vec![f.clone()], tx, ms(5), ms(150));
        std::thread::sleep(ms(60)); // let the watcher take its first look before the change
        std::fs::write(&f, b"11").unwrap();
        assert!(recv_within(&rx, ms(400)));
        std::thread::sleep(ms(20)); // still inside the 150 ms gap of that hint
        while rx.try_recv().is_ok() {}
        std::fs::write(&f, b"222").unwrap(); // inside the gap of the first
        assert!(
            recv_within(&rx, ms(600)),
            "the held change arrives once the gap ends"
        );
    }

    #[test]
    fn dropping_the_watcher_stops_its_thread() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("w");
        std::fs::write(&f, b"0").unwrap();
        let (tx, rx) = sync_channel::<()>(8);
        let w = Watcher::spawn(vec![f.clone()], tx, ms(5), ms(10));
        std::thread::sleep(ms(60));
        drop(w);
        std::fs::write(&f, b"changed after the drop").unwrap();
        assert!(
            !recv_within(&rx, ms(150)),
            "no hint after drop (and the channel is closed)"
        );
    }
}
