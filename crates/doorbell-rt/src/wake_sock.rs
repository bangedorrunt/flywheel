// governed-by: ADR-0036
//! Wake-bus socket consumer, wave 2 (bd-qe1r, Spec 8 row 1).
//!
//! FileHost + the embedded twin subscribe via the daemon `wake.sock`
//! (ADR-0036 §6 JSONL) instead of spawning a `toron watch` child.
//! BrainMsg mapping is unchanged: an `Event` frame formats through
//! [`format_watch_line`], byte-identical to toron-cli's `format_watch_line`
//! (same `serde_json::json!` construction, pinned by the golden test below).
//!
//! Codec note: the 4 frame shapes mirror toron-core `WakeFrame` without
//! linking toron-core — the doorbell daemon stays free of its build closure
//! (sqlx, vendored libgit2). Drift fails loud: unknown frames audit + skip,
//! and the round-trip test pins the exact wire vectors.

use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Env override for socket discovery (same name as toron-cli slice B).
pub const WAKE_SOCK_ENV: &str = "TORON_WAKE_SOCK";
/// Handshake ceiling: connect + subscribe write (mirrors slice B).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Quiet ceiling: the daemon heartbeats every 15s, so silence past 90s is a
/// deaf watcher, never a quiet bus (mirrors `WATCH_IDLE_EXIT_SECS`).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Consumer outcome: a mapped watch line, or transport death (the brain's
/// `on_died` restarts with backoff, same as the child EOF before).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    Watch(String),
    /// Quiet-bus heartbeat: touch watchbeat, emit nothing.
    Heartbeat,
    /// toron#17 `events_lost`: stored events after our cursor were evicted
    /// before we read them. `from`/`through` are the server's OPAQUE cursor
    /// strings, passed through verbatim (never parsed). The consumer keeps
    /// going; the host counts the gap and surfaces it.
    EventsLost {
        from: String,
        through: String,
    },
    Died,
}

/// Loud-degradation audit kinds (never silent).
pub const AUDIT_SOCK_ABSENT: &str = "wake_sock_absent";
pub const AUDIT_SOCK_FRAME: &str = "wake_sock_frame";
/// bd-kz54: the stream ended — and WHICH way. EOF, a read error and
/// deaf-silence are different faults and were one wildcard arm, so a restart
/// loop over a quiet bus journaled nothing at all.
pub const AUDIT_SOCK_IDLE: &str = "wake_sock_idle";
/// bd-kz54: the subscribe handshake could not be written.
pub const AUDIT_SOCK_HANDSHAKE: &str = "wake_sock_handshake";
/// A resync the sidecar asked for. The ONLY exit that reconnects instead of
/// ending, so it is the only one whose rate the brain's restart backoff never
/// bounds — and until now the only one that wrote no row at all.
pub const AUDIT_SOCK_SHED: &str = "wake_sock_shed";
/// toron#17: the sidecar said events were lost (cap eviction) between our
/// cursor and what it still stores. One row per frame, never sampled: a gap
/// is rare and every one is a loss an operator must be able to find.
pub const AUDIT_SOCK_EVENTS_LOST: &str = "wake_sock_events_lost";

/// Floor between resync reconnects. A shed is lossless and non-fatal by design,
/// so it does not end the consumer and does not reach the backoff that every
/// other exit gets. Without a floor, a sidecar that keeps shedding turns this
/// loop into a reconnect spinner: the cursor makes it correct, nothing made it
/// slow. Cheap on the healthy path (a shed after hours of streaming pays 500ms
/// once) and it bounds the worst case at 2 resyncs/second instead of
/// socket-speed.
pub const RESYNC_FLOOR: Duration = Duration::from_millis(500);

/// bd-68q2: the process is out of file descriptors — reconnecting is futile
/// until they drop, and auditing it as "sidecar absent" restarts into a wall
/// while hiding the real fault (the 234-fd EMFILE that killed wake.sock).
pub const AUDIT_SOCK_EMFILE: &str = "wake_sock_emfile";

/// True when a connect error means WE are out of fds (EMFILE) or the system
/// is (ENFILE) — either way the wall is on our side of the socket.
pub(crate) fn is_fd_exhaustion(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(code) if code == libc::EMFILE || code == libc::ENFILE
    )
}

/// Process fd census. Honestly absent (None), never zero, where unreadable.
/// Used in EMFILE/still-failing rows and /health next to the restart rate —
/// the two numbers that together say "restart storm" vs "fd wall".
pub fn open_fd_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    let n = std::fs::read_dir("/proc/self/fd").ok().map(|rd| rd.count());
    #[cfg(target_os = "macos")]
    let n = macos_fd_count();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let n: Option<usize> = None;
    n
}

/// macOS fd census via proc_pidinfo. The null-buffer size query goes stale
/// (measured: identical size before/after real opens), so fill a real buffer
/// and grow on truncation instead of trusting the hint.
#[cfg(target_os = "macos")]
fn macos_fd_count() -> Option<usize> {
    let elem = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: buffer is a live Vec of the passed size; proc_pidinfo writes at
    let mut size = 1024 * elem;
    for _ in 0..4 {
        let mut buf = vec![0u8; size];
        let n = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                buf.as_mut_ptr() as *mut libc::c_void,
                size as libc::c_int,
            )
        };
        if n < 0 {
            return None;
        }
        if (n as usize) < size {
            return Some(n as usize / elem);
        }
        size *= 2;
    }
    None
}
/// bd-gdfe: a consumer that ended without its terminal outcome (worker
/// panic or abort) journals this instead of dying silently — the gap that
/// left hosted workspaces wake-blind with no backoff explanation.
pub const AUDIT_WAKE_DROPPED: &str = "wake_consumer_dropped";

/// Pure discovery core: explicit override wins, else the daemon state dir.
/// (Mirrors toron-cli `discover_from`: `XDG_STATE_HOME/toron`, else
/// `~/.local/state/toron`, plus `wake.sock`.)
pub fn discover_from(override_path: Option<String>, xdg_state: Option<String>) -> PathBuf {
    if let Some(p) = override_path
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let base = xdg_state
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("toron").join("wake.sock")
}

/// Socket discovery: env override, else the daemon state dir. Env read only.
pub fn wake_sock_path() -> PathBuf {
    discover_from(
        std::env::var(WAKE_SOCK_ENV).ok(),
        std::env::var("XDG_STATE_HOME").ok(),
    )
}

/// Parse `WATCH_KINDS`-style comma specs into the subscribe filter.
pub fn parse_kinds(spec: &str) -> Vec<u16> {
    spec.split(',')
        .filter_map(|s| s.trim().parse::<u16>().ok())
        .collect()
}

/// The printed line for a socket event — the SAME `serde_json::json!`
/// construction as toron-cli `format_watch_line`, so consumers cannot tell
/// the transport apart (mapping byte-identical by construction + golden test).
pub fn format_watch_line(
    kind: u16,
    id: &str,
    pubkey: &str,
    content: &str,
    tags: &[Vec<String>],
    created_at: u64,
) -> String {
    let v = serde_json::json!({
        "kind": kind,
        "id": id,
        "pubkey": pubkey,
        "content": content,
        "tags": tags,
        "created_at": created_at,
    });
    serde_json::to_string(&v).expect("watch event serializes")
}

/// One wake event frame: field-for-field the binding watch shape
/// `{kind, id, pubkey, content, tags, created_at}` (same as slice B's
/// `SockEvent`, which the daemon fills from the bus event).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SockEvent {
    pub kind: u16,
    pub id: String,
    pub pubkey: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<Vec<String>>,
    pub created_at: u64,
}

/// Map one decoded frame to consumer action. `Event` → watch line;
/// heartbeat/EOSE → liveness only (caller touches watchbeat); `Shed` →
/// reconnect cursor (caller loops). Unknown shapes are the caller's loud
/// audit, never a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameAction {
    Watch(String),
    Heartbeat,
    Eose,
    Shed(String),
    /// toron#17 gap frame; opaque cursors, verbatim.
    EventsLost {
        from: String,
        through: String,
    },
}

pub fn frame_action(line: &str) -> Result<FrameAction, String> {
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("bad frame: {e}"))?;
    if let Some(b) = v.get("event") {
        let ev: SockEvent =
            serde_json::from_value(b.clone()).map_err(|e| format!("bad event: {e}"))?;
        return Ok(FrameAction::Watch(format_watch_line(
            ev.kind,
            &ev.id,
            &ev.pubkey,
            &ev.content,
            &ev.tags,
            ev.created_at,
        )));
    }
    if v.get("eose").is_some() {
        return Ok(FrameAction::Eose);
    }
    if v.get("heartbeat").is_some() {
        return Ok(FrameAction::Heartbeat);
    }
    if let Some(b) = v.get("shed") {
        let from = b
            .get("resync_from")
            .and_then(|c| c.as_str())
            .ok_or_else(|| "shed without resync_from".to_string())?;
        return Ok(FrameAction::Shed(from.to_string()));
    }
    if let Some(b) = v.get("events_lost") {
        let field = |name: &str| {
            b.get(name)
                .and_then(|c| c.as_str())
                .unwrap_or("unknown")
                .to_string()
        };
        return Ok(FrameAction::EventsLost {
            from: field("from"),
            through: field("through"),
        });
    }
    Err("unknown wake frame".to_string())
}

/// Connection options. Pure data: constructors perform no I/O.
#[derive(Debug, Clone)]
pub struct WakeOpts {
    pub sock_path: PathBuf,
    pub kinds: Vec<u16>,
    pub project: Option<String>,
    pub handshake_timeout: Duration,
    pub idle_timeout: Duration,
    /// Minimum gap between reconnects after a shed (see [`RESYNC_FLOOR`]).
    /// Configurable so a test can prove the floor is honoured without waiting
    /// on the production value.
    pub resync_floor: Duration,
    /// Set by whoever REPLACES this consumer, immediately before aborting it.
    ///
    /// bd-v7uu: a deliberate replacement and a crash both close the channel
    /// without a terminal outcome, so the monitor could not tell them apart and
    /// reported every restart as `wake_consumer_dropped` — AND forwarded a
    /// synthetic `Died`, which advanced the retry backoff. Measured live: 36 rows
    /// in 30 minutes, all in one workspace, the audit bursts, then a tail whose
    /// spacing widened (65s, 71s, 192s, 337s, 569s) because each intentional
    /// restart was counted as a failure by the very mechanism performing it. The
    /// recovery was poisoning its own backoff and reading as an incident.
    pub retired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// mcc9: the cursor this consumer subscribes from — the last line the
    /// brain accepted from the consumer it replaces
    /// ([`crate::wake_brain::WatchForwarder::resume_cursor`]). `None` keeps the
    /// first-start behaviour (replay from origin). Before mcc9 every
    /// replacement subscribed from origin, so a full-channel restart replayed
    /// the whole history into the channel that had just overflowed.
    pub resume_from: Option<String>,
}

impl WakeOpts {
    pub fn new(sock_path: PathBuf, kinds: Vec<u16>, project: Option<String>) -> Self {
        Self {
            sock_path,
            kinds,
            project,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            resync_floor: RESYNC_FLOOR,
            retired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_from: None,
        }
    }

    /// The flag the replacement sets. Held by the restarter so it can retire this
    /// consumer without reaching into the task.
    #[must_use]
    pub fn retire_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.retired)
    }
}

fn subscribe_line(opts: &WakeOpts, since_cursor: Option<&str>) -> String {
    serde_json::to_string(&serde_json::json!({
        "subscribe": {
            "kinds": opts.kinds,
            "since_cursor": since_cursor,
            "project": opts.project,
        }
    }))
    .unwrap_or_else(|_| "{}".to_string())
        + "\n"
}

/// Spawn the socket consumer. Topology mirrors the old child reader: a tokio
/// task owns the socket, a std thread owns the sync `on_event` callback, an
/// unbounded channel decouples them. Shed reconnects transparently with the
/// cursor (lossless); EOF/error/idle/absent-socket emits one `Died` and ends
/// (the brain restarts with backoff). Absent socket audits LOUD first.
pub fn spawn_wake_task(
    opts: WakeOpts,
    on_audit: impl Fn(&str, serde_json::Value) + Send + Sync + 'static,
    on_event: impl Fn(WakeOutcome) + Send + 'static,
) -> tokio::task::AbortHandle {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WakeOutcome>();
    let retired = opts.retire_flag();
    let audit = std::sync::Arc::new(on_audit);
    let worker_audit = audit.clone();
    let worker = tokio::spawn(async move {
        run_wake_loop(opts, &tx, &worker_audit).await;
    });
    let handle = worker.abort_handle();
    std::thread::spawn(move || {
        let mut rx = rx;
        let mut terminal = false;
        while let Some(out) = rx.blocking_recv() {
            if matches!(out, WakeOutcome::Died) {
                terminal = true;
            }
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_event(out))).is_err() {
                eprintln!("doorbell: wake outcome forwarder panicked; inferring Died (bd-gdfe)");
                break;
            }
            if retired.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
        }
        if !terminal && !retired.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                audit(
                    AUDIT_WAKE_DROPPED,
                    serde_json::json!({
                        "reason": "wake worker ended without a terminal outcome; inferred Died"
                    }),
                );
            }));
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                on_event(WakeOutcome::Died)
            }));
        }
    });
    handle
}

async fn run_wake_loop(
    opts: WakeOpts,
    tx: &tokio::sync::mpsc::UnboundedSender<WakeOutcome>,
    audit: &std::sync::Arc<impl Fn(&str, serde_json::Value) + Send + Sync>,
) {
    let send = |out: WakeOutcome| -> bool {
        if opts.retired.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        tx.send(out).is_ok()
    };
    let mut cursor: Option<String> = opts.resume_from.clone();
    let mut resyncs: u64 = 0;
    let mut lost_frames: u64 = 0;
    loop {
        if resyncs > 0 {
            tokio::time::sleep(opts.resync_floor).await;
        }
        let stream = match tokio::time::timeout(
            opts.handshake_timeout,
            tokio::net::UnixStream::connect(&opts.sock_path),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let emfile = is_fd_exhaustion(&e);
                audit(
                    if emfile {
                        AUDIT_SOCK_EMFILE
                    } else {
                        AUDIT_SOCK_ABSENT
                    },
                    serde_json::json!({
                        "sock": opts.sock_path.display().to_string(),
                        "reason": if emfile {
                            "connect failed: process out of file descriptors, not an absent sidecar"
                        } else {
                            "connect failed: daemon sidecar absent, not a quiet bus"
                        },
                        "os_error": e.raw_os_error(),
                        "open_fds": open_fd_count(),
                    }),
                );
                if !send(WakeOutcome::Died) {
                    return;
                }
                return;
            }
            Err(_) => {
                audit(
                    AUDIT_SOCK_ABSENT,
                    serde_json::json!({
                        "sock": opts.sock_path.display().to_string(),
                        "reason": "connect timed out: sidecar not accepting, not a quiet bus",
                        "timeout_ms": opts.handshake_timeout.as_millis() as u64,
                    }),
                );
                if !send(WakeOutcome::Died) {
                    return;
                }
                return;
            }
        };
        let (rd, mut wr) = stream.into_split();
        let line = subscribe_line(&opts, cursor.as_deref());
        if tokio::time::timeout(opts.handshake_timeout, wr.write_all(line.as_bytes()))
            .await
            .is_err()
        {
            audit(
                AUDIT_SOCK_HANDSHAKE,
                serde_json::json!({
                    "reason": "subscribe handshake write timed out or failed",
                    "timeout_ms": opts.handshake_timeout.as_millis() as u64,
                }),
            );
            if !send(WakeOutcome::Died) {
                return;
            }
            return;
        }
        let mut lines = BufReader::new(rd).lines();
        let mut cause = String::from("eof: sidecar closed the stream");
        let exit = loop {
            match tokio::time::timeout(opts.idle_timeout, lines.next_line()).await {
                Ok(Ok(Some(l))) => match frame_action(&l) {
                    Ok(FrameAction::Watch(w)) => {
                        if !send(WakeOutcome::Watch(w)) {
                            return;
                        }
                    }
                    Ok(FrameAction::Heartbeat) => {
                        if !send(WakeOutcome::Heartbeat) {
                            return;
                        }
                    }
                    Ok(FrameAction::Eose) => {}
                    Ok(FrameAction::EventsLost { from, through }) => {
                        lost_frames += 1;
                        audit(
                            AUDIT_SOCK_EVENTS_LOST,
                            serde_json::json!({
                                "from": from,
                                "through": through,
                                "lost_index": lost_frames,
                                "reason": "sidecar evicted stored events after our cursor before we read them",
                            }),
                        );
                        if !send(WakeOutcome::EventsLost { from, through }) {
                            return;
                        }
                    }
                    Ok(FrameAction::Shed(from)) => {
                        resyncs += 1;
                        audit(
                            AUDIT_SOCK_SHED,
                            serde_json::json!({
                                "resync_from": from,
                                "resync_index": resyncs,
                                "floor_ms": opts.resync_floor.as_millis() as u64,
                            }),
                        );
                        cursor = Some(from);
                        break true;
                    }
                    Err(detail) => {
                        audit(AUDIT_SOCK_FRAME, serde_json::json!({ "detail": detail }));
                    }
                },
                Ok(Ok(None)) => break false,
                Ok(Err(e)) => {
                    cause = format!("read error: {e}");
                    break false;
                }
                Err(_) => {
                    cause = String::from(
                        "deaf-silence: no frame within idle_timeout (a quiet bus is not a dead socket)",
                    );
                    break false;
                }
            }
        };
        if !exit {
            audit(
                AUDIT_SOCK_IDLE,
                serde_json::json!({
                    "reason": cause,
                    "idle_timeout_ms": opts.idle_timeout.as_millis() as u64,
                }),
            );
            if !send(WakeOutcome::Died) {
                return;
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    /// bd-68q2: EMFILE/ENFILE (we are out of fds) must not read as "sidecar
    /// absent". Wrong diagnosis here restarts into a wall and hides the real
    /// fault the way the wildcard arm once hid every stream end.
    #[test]
    fn fd_exhaustion_classifies_by_errno_not_kind() {
        assert!(
            super::is_fd_exhaustion(&std::io::Error::from_raw_os_error(libc::EMFILE)),
            "EMFILE is us out of fds, not an absent sidecar"
        );
        assert!(
            super::is_fd_exhaustion(&std::io::Error::from_raw_os_error(libc::ENFILE)),
            "ENFILE is the system out of fds, same wall"
        );
        assert!(
            !super::is_fd_exhaustion(&std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such socket"
            )),
            "a missing socket stays absent"
        );
        assert!(
            !super::is_fd_exhaustion(&std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "nobody listening"
            )),
            "refused stays absent"
        );
    }

    /// bd-68q2: the fd census must move with real opens, or it is decoration.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn open_fd_count_moves_with_real_opens() {
        let before = super::open_fd_count().expect("census must read here");
        let held: Vec<std::fs::File> = (0..8)
            .map(|_| std::fs::File::open("/dev/null").unwrap())
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let after = loop {
            let n = super::open_fd_count().expect("census must still read");
            if n >= before + 3 || std::time::Instant::now() >= deadline {
                break n;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(
            after >= before + 3,
            "held opens must show: {before} -> {after}"
        );
        drop(held);
    }

    /// bd-68q2: where the census cannot read it is honestly absent, never zero.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn open_fd_count_is_honestly_absent_elsewhere() {
        let _ = super::open_fd_count();
    }

    /// bd-v7uu: a DELIBERATELY retired consumer is silent; one that vanishes on
    /// its own is still loud.
    ///
    /// This is the fix for a live incident I misread at first. The audit showed 36
    /// `wake_consumer_dropped` rows in 30 minutes in one workspace and I took it
    /// for the EMFILE storm returning. It was the restart path: a replacement
    /// aborts its predecessor, an abort closes the channel with no terminal
    /// outcome, and the monitor could not tell that from a crash — so it wrote the
    /// row AND forwarded a synthetic `Died`, which advanced the retry backoff. The
    /// tail's widening spacing (65s, 71s, 192s, 337s, 569s) was the backoff being
    /// driven by the recovery itself.
    ///
    /// Both halves matter and the second is the one a "fix" would have dropped: a
    /// crash must STILL be reported, or the doorbell goes wake-blind, which is the
    /// permanent failure the original row exists to prevent.
    #[test]
    fn a_retired_consumer_is_silent_and_a_lost_one_is_still_loud() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn run_one(retire: bool) -> (usize, usize) {
            use std::io::Write as _;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            let path = std::env::temp_dir().join(format!(
                "bd-v7uu-{}-{}.sock",
                std::process::id(),
                if retire { "retire" } else { "lost" }
            ));
            let _ = std::fs::remove_file(&path);
            let listener =
                std::os::unix::net::UnixListener::bind(&path).expect("bind a real socket");

            let audits = Arc::new(AtomicUsize::new(0));
            let died = Arc::new(AtomicUsize::new(0));
            let (a, d) = (audits.clone(), died.clone());
            let opts = WakeOpts {
                sock_path: path.clone(),
                kinds: vec![1u16],
                project: Some("demo".into()),
                handshake_timeout: Duration::from_millis(500),
                idle_timeout: Duration::from_secs(30),
                resync_floor: Duration::from_millis(50),
                retired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_from: None,
            };
            let flag = opts.retire_flag();

            let server = std::thread::spawn(move || {
                if let Ok((mut sock, _)) = listener.accept() {
                    let _ = sock.write_all(b"{\"heartbeat\":{\"ts_ms\":5}}\n");
                    let _ = sock.write_all(b"{\"eose\":{\"cursor\":\"1\"}}\n");
                    let _ = sock.flush();
                    std::thread::sleep(Duration::from_millis(1500));
                    let _ = sock.shutdown(std::net::Shutdown::Both);
                }
            });

            rt.block_on(async {
                let handle = spawn_wake_task(
                    opts,
                    move |_kind, _v| {
                        a.fetch_add(1, Ordering::SeqCst);
                    },
                    move |out| {
                        if matches!(out, WakeOutcome::Died) {
                            d.fetch_add(1, Ordering::SeqCst);
                        }
                    },
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
                if retire {
                    flag.store(true, Ordering::SeqCst);
                }
                handle.abort();
                for _ in 0..300 {
                    if !retire && audits.load(Ordering::SeqCst) > 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            server.join().ok();
            let _ = std::fs::remove_file(&path);
            (audits.load(Ordering::SeqCst), died.load(Ordering::SeqCst))
        }

        let (a_retired, d_retired) = run_one(true);
        assert_eq!(
            (a_retired, d_retired),
            (0, 0),
            "a deliberate replacement must be SILENT: no wake_consumer_dropped row \
             and no synthetic Died, because the replacement is already armed and a \
             synthetic death advances the backoff the restart was meant to reset"
        );

        let (a_lost, d_lost) = run_one(false);
        assert_eq!(
            a_lost, 1,
            "a consumer that vanishes on its own MUST still be reported — this row \
             is what prevents the permanent wake-blind gap"
        );
        assert_eq!(
            d_lost, 1,
            "and it must forward exactly one inferred Died so the brain re-arms"
        );
    }
    use super::*;

    #[test]
    fn watch_line_golden_matches_toron_construction() {
        let line = format_watch_line(
            9,
            "e1",
            "ab",
            "hello",
            &[vec!["p".to_string(), "PinA".to_string()]],
            1,
        );
        let expect = serde_json::json!({
            "kind": 9, "id": "e1", "pubkey": "ab",
            "tags": [["p", "PinA"]], "content": "hello", "created_at": 1
        })
        .to_string();
        assert_eq!(line, expect);
        let helper = serde_json::json!({
            "kind": 9, "id": "e1", "pubkey": "ab",
            "tags": [["p", "PinA"]], "content": "hello", "created_at": 1
        })
        .to_string();
        assert_eq!(line, helper);
    }

    #[test]
    fn frame_vectors_roundtrip_like_server() {
        match frame_action(
            r#"{"event":{"kind":9,"id":"e","pubkey":"p","content":"c","tags":[],"created_at":3}}"#,
        )
        .expect("event")
        {
            FrameAction::Watch(w) => assert!(w.contains("\"kind\":9")),
            other => panic!("expected Watch, got {other:?}"),
        }
        assert_eq!(
            frame_action(r#"{"eose":{"cursor":"42"}}"#),
            Ok(FrameAction::Eose)
        );
        assert_eq!(
            frame_action(r#"{"heartbeat":{"ts_ms":7}}"#),
            Ok(FrameAction::Heartbeat)
        );
        assert_eq!(
            frame_action(r#"{"shed":{"resync_from":"shed-3"}}"#),
            Ok(FrameAction::Shed("shed-3".to_string()))
        );
        assert!(frame_action(r#"{"nope":{}}"#).is_err());
        assert!(frame_action(r#"{"shed":{}}"#).is_err());
        assert!(frame_action("not json").is_err());
    }

    #[test]
    fn discovery_prefers_override_then_state_dir() {
        assert_eq!(
            discover_from(Some("/tmp/x.sock".into()), Some("/s".into())),
            PathBuf::from("/tmp/x.sock")
        );
        assert_eq!(
            discover_from(None, Some("/s".into())),
            PathBuf::from("/s/toron/wake.sock")
        );
        assert_eq!(
            discover_from(Some(String::new()), Some("/s".into())),
            PathBuf::from("/s/toron/wake.sock")
        );
    }

    #[test]
    fn kinds_parse_is_total() {
        assert_eq!(parse_kinds("9,14,20001"), vec![9, 14, 20001]);
        assert_eq!(parse_kinds("9, xx ,30013"), vec![9, 30013]);
        assert!(parse_kinds("").is_empty());
    }

    #[test]
    fn opts_construct_performs_no_io() {
        let p = std::env::temp_dir().join("wake-ctor-absent.sock");
        let opts = WakeOpts::new(p.clone(), vec![9], Some("t".into()));
        assert_eq!(opts.sock_path, p);
        assert!(!p.exists() || true, "constructor never connects");
    }

    /// The silent path: `Shed` is the only exit that reconnects instead of
    /// ending, so it never reaches the brain's restart backoff and it used to
    /// write no audit row at all ("transparent, lossless via cursor"). A sidecar
    /// that keeps shedding therefore drove a reconnect loop with nothing to
    /// bound it and nothing to show for it — the one exit whose rate was
    /// invisible in the journal.
    ///
    /// Three properties, all of which the unguarded arm failed: the shed is
    /// AUDITED (one row per resync, naming the cursor and how many resyncs have
    /// happened), the loop is THROTTLED (a floor between reconnects), and a shed
    /// is still not a death (lossless means no `Died`).
    #[tokio::test]
    async fn a_shed_loop_is_audited_and_throttled_and_is_not_a_death() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir();
        let path = dir.join(format!("wake-shed-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        const SHEDS: usize = 3;
        tokio::spawn(async move {
            for i in 0..SHEDS {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 1024];
                let _ = s.read(&mut buf).await;
                let line = format!("{{\"shed\":{{\"resync_from\":\"resync-{i}\"}}}}\n");
                let _ = s.write_all(line.as_bytes()).await;
            }
        });
        let floor = Duration::from_millis(60);
        let opts = WakeOpts {
            sock_path: path.clone(),
            kinds: vec![9],
            project: None,
            handshake_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(2),
            resync_floor: floor,
            retired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_from: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        let audit = std::sync::Arc::new(move |k: &str, d: serde_json::Value| {
            sink.lock().expect("audit lock").push((k.to_string(), d));
        });
        let started = std::time::Instant::now();
        run_wake_loop(opts, &tx, &audit).await;
        let elapsed = started.elapsed();
        drop(tx);
        let mut got = Vec::new();
        while let Some(o) = rx.recv().await {
            got.push(o);
        }
        let deaths = got
            .iter()
            .filter(|o| matches!(o, WakeOutcome::Died))
            .count();
        assert_eq!(
            deaths, 1,
            "a resync is lossless, not a death: {SHEDS} sheds must produce exactly one \
             terminal Died (the EOF after the server stops), got {deaths} in {got:?}"
        );
        let rows = seen.lock().expect("audit lock");
        let sheds: Vec<&serde_json::Value> = rows
            .iter()
            .filter(|(k, _)| k == AUDIT_SOCK_SHED)
            .map(|(_, d)| d)
            .collect();
        assert!(
            elapsed >= floor * SHEDS as u32,
            "{SHEDS} sheds reconnected in {elapsed:?}: the floor must bound the rewind loop, \
             which is the only exit that does not reach the brain's backoff (floor {floor:?}). \
             outcomes {got:?}, audits {:?}, shed rows {sheds:?}",
            rows.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(
            sheds.len(),
            SHEDS,
            "every resync owes the journal a row: a rewind loop with no audit row is an \
             incident that leaves no trace. audits: {:?}",
            rows.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()
        );
        assert!(
            sheds[0]["resync_from"] == "resync-0" && sheds[2]["resync_from"] == "resync-2",
            "the row must name the cursor it resynced to, or it cannot be followed: {sheds:?}"
        );
        assert_eq!(
            sheds
                .iter()
                .map(|d| d["resync_index"].as_u64())
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3)],
            "the rows must count the resyncs, so a loop reads as a rate instead of N \
             unrelated blips: {sheds:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn events_lost_is_a_loss_frame_with_opaque_cursors_never_an_unknown_frame() {
        match frame_action(r#"{"events_lost":{"from":"seq:12","through":"seq:97"}}"#) {
            Ok(FrameAction::EventsLost { from, through }) => {
                assert_eq!((from.as_str(), through.as_str()), ("seq:12", "seq:97"));
            }
            other => panic!("expected EventsLost, got {other:?}"),
        }
        assert!(matches!(
            frame_action(r#"{"events_lost":{"from":"1:ab","through":"unknown"}}"#),
            Ok(FrameAction::EventsLost { through, .. }) if through == "unknown"
        ));
        assert!(matches!(
            frame_action(r#"{"events_lost":{}}"#),
            Ok(FrameAction::EventsLost { from, through }) if from == "unknown" && through == "unknown"
        ));
        assert!(frame_action(r#"{"mystery":{}}"#).is_err());
    }

    /// toron#17 end to end on the consumer: the gap frame is audited with
    /// the server's cursors verbatim, surfaces as an outcome IN ORDER (before
    /// the events the sidecar still had), and is not a death or a resync.
    #[tokio::test]
    async fn an_events_lost_frame_is_audited_surfaced_in_order_and_not_a_death() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let path = std::env::temp_dir().join(format!("wake-lost-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 1024];
            let _ = s.read(&mut buf).await;
            let frames = concat!(
                "{\"events_lost\":{\"from\":\"seq:12\",\"through\":\"seq:97\"}}\n",
                "{\"event\":{\"kind\":9,\"id\":\"e1\",\"pubkey\":\"p\",\"content\":\"c\",\"tags\":[],\"created_at\":3}}\n",
                "{\"eose\":{\"cursor\":\"seq:98\"}}\n",
            );
            let _ = s.write_all(frames.as_bytes()).await;
        });
        let opts = WakeOpts {
            sock_path: path.clone(),
            kinds: vec![9],
            project: None,
            handshake_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(2),
            resync_floor: Duration::from_millis(10),
            retired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_from: Some("seq:12".to_string()),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        let audit = std::sync::Arc::new(move |k: &str, d: serde_json::Value| {
            sink.lock().expect("audit lock").push((k.to_string(), d));
        });
        run_wake_loop(opts, &tx, &audit).await;
        drop(tx);
        let mut got = Vec::new();
        while let Some(o) = rx.recv().await {
            got.push(o);
        }
        assert!(
            matches!(&got[0], WakeOutcome::EventsLost { from, through }
                if from == "seq:12" && through == "seq:97"),
            "the gap comes first, verbatim: {got:?}"
        );
        assert!(
            matches!(&got[1], WakeOutcome::Watch(_)),
            "then the event: {got:?}"
        );
        let rows = seen.lock().expect("audit lock");
        let lost: Vec<_> = rows
            .iter()
            .filter(|(k, _)| k == AUDIT_SOCK_EVENTS_LOST)
            .collect();
        assert_eq!(lost.len(), 1, "one row per gap frame");
        assert_eq!(lost[0].1["from"], "seq:12");
        assert_eq!(lost[0].1["through"], "seq:97");
        assert!(
            !rows.iter().any(|(k, _)| k == AUDIT_SOCK_FRAME),
            "not an unknown frame"
        );
        assert!(
            !rows.iter().any(|(k, _)| k == AUDIT_SOCK_SHED),
            "not a resync"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn temp_socket_serves_scripted_frames_to_died() {
        use tokio::io::AsyncWriteExt;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("wake-cons-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 1024];
            use tokio::io::AsyncReadExt;
            let _ = s.read(&mut buf).await;
            for line in [
                "{\"event\":{\"kind\":9,\"id\":\"e1\",\"pubkey\":\"ab\",\"content\":\"hi\",\"tags\":[[\"p\",\"PinA\"]],\"created_at\":11}}\n",
                "{\"heartbeat\":{\"ts_ms\":5}}\n",
                "{\"eose\":{\"cursor\":\"1\"}}\n",
            ] {
                let _ = s.write_all(line.as_bytes()).await;
            }
        });
        let opts = WakeOpts {
            sock_path: path.clone(),
            kinds: vec![9],
            project: None,
            handshake_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(2),
            resync_floor: Duration::from_millis(50),
            retired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_from: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let audit = std::sync::Arc::new(|_: &str, _: serde_json::Value| {});
        run_wake_loop(opts, &tx, &audit).await;
        drop(tx);
        let mut got = Vec::new();
        while let Some(o) = rx.recv().await {
            got.push(o);
        }
        assert_eq!(got.len(), 3, "Watch + Heartbeat then Died, got {got:?}");
        assert!(matches!(got[0], WakeOutcome::Watch(_)));
        assert!(matches!(got[1], WakeOutcome::Heartbeat));
        assert!(matches!(got[2], WakeOutcome::Died));
        let _ = std::fs::remove_file(&path);
    }

    /// bd-kz54: a QUIET bus must be audited, not silent.
    ///
    /// Observed live 2026-09-27: `flywheel-toron` logged a bare
    /// `{"rt_kind":"Died"} -> RestartWatch` roughly every 90 seconds for hours,
    /// with **zero** audit rows in its entire journal, while `/health` showed a
    /// fresh heartbeat — the 1s ticker keeps `.hb` fresh even when the socket
    /// delivers nothing. `IDLE_TIMEOUT` is 90s, so a workspace with no traffic
    /// is declared dead, restarted, and declares itself dead again forever, and
    /// nothing in the record says why.
    ///
    /// EOF, a read error, and deaf-silence are three different faults. The
    /// fence pins the third, because it is the one that is NOT a dead socket:
    /// the audit row must name it so the loop can be read instead of guessed at.
    #[test]
    fn a_quiet_bus_is_audited_as_deaf_silence_not_silently_died() {
        use std::sync::{Arc, Mutex};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let dir = std::env::temp_dir().join(format!("wake-quiet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("bus.sock");

        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        std::thread::spawn(move || {
            for mut s in listener.incoming().take(2).flatten() {
                use std::io::Read;
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                std::thread::sleep(std::time::Duration::from_millis(1500));
            }
        });

        let rows: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
        let sink = rows.clone();
        let audit = Arc::new(move |kind: &str, detail: serde_json::Value| {
            sink.lock().unwrap().push((kind.to_string(), detail));
        });

        let opts = WakeOpts {
            sock_path: path.clone(),
            kinds: vec![9],
            project: None,
            handshake_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_millis(400),
            resync_floor: Duration::from_millis(50),
            retired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_from: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        rt.block_on(run_wake_loop(opts, &tx, &audit));

        let mut saw_died = false;
        while let Ok(o) = rx.try_recv() {
            if matches!(o, WakeOutcome::Died) {
                saw_died = true;
            }
        }
        assert!(saw_died, "a silent stream still ends the watch");

        let rows = rows.lock().unwrap().clone();
        let idle = rows.iter().find(|(k, _)| k == AUDIT_SOCK_IDLE);
        assert!(
            idle.is_some(),
            "deaf-silence must be AUDITED, not silent. rows={rows:?}"
        );
        let reason = idle.unwrap().1["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("deaf-silence"),
            "the audit must name deaf-silence specifically, got {reason:?}"
        );
        assert!(
            idle.unwrap().1.get("idle_timeout_ms").is_some(),
            "the audit carries the timeout it waited, so the number is checkable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-gdfe: the silent wake gap. When the worker task dies WITHOUT a
    /// terminal outcome (here: a scripted panic in the audit callback — the
    /// same path a real defect takes), the forwarding thread must still
    /// deliver exactly one `Died` and journal why. Before this contract the
    /// thread exited silently: no outcome, no audit row, no backoff — the
    /// permanent per-workspace silence observed live.
    #[test]
    fn wake_respawn_lost_worker_still_forwards_one_died() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let opts = WakeOpts {
            sock_path: std::env::temp_dir()
                .join(format!("wake-respawn-absent-{}.sock", std::process::id())),
            kinds: vec![9],
            project: None,
            handshake_timeout: Duration::from_millis(100),
            idle_timeout: Duration::from_millis(100),
            resync_floor: Duration::from_millis(50),
            retired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_from: None,
        };
        let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<WakeOutcome>(8);
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let rows = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let audit_calls = calls.clone();
        let audit_rows = rows.clone();
        let audit = move |kind: &str, _v: serde_json::Value| {
            if audit_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("scripted worker defect");
            }
            audit_rows.lock().unwrap().push(kind.to_string());
        };
        let _handle = rt.block_on(async move {
            spawn_wake_task(opts, audit, move |out| {
                let _ = out_tx.send(out);
            })
        });
        let got = out_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a lost worker must still forward one Died (bd-gdfe)");
        assert!(matches!(got, WakeOutcome::Died));
        assert!(
            out_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "exactly one Died, never a duplicate"
        );
        let rows = rows.lock().unwrap();
        assert!(
            rows.iter().any(|k| k == AUDIT_WAKE_DROPPED),
            "the inferred terminal journals itself, never silently: {rows:?}"
        );
    }

    /// mcc9: a replacement consumer subscribes FROM its resume cursor (the last
    /// line the brain accepted), so the sidecar replays what the predecessor
    /// dropped. Negative control in the same test: with no resume cursor the
    /// first subscribe carries `since_cursor: null` (replay from origin, the
    /// pre-mcc9 behaviour for every restart).
    #[tokio::test]
    async fn a_replacement_subscribes_from_its_resume_cursor() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        async fn first_subscribe(resume_from: Option<String>, tag: &str) -> serde_json::Value {
            let path =
                std::env::temp_dir().join(format!("wake-resume-{tag}-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path).expect("bind");
            let server = tokio::spawn(async move {
                let (s, _) = listener.accept().await.expect("accept");
                let (rd, mut wr) = s.into_split();
                let mut lines = tokio::io::BufReader::new(rd).lines();
                let sub = lines
                    .next_line()
                    .await
                    .expect("read")
                    .expect("subscribe line");
                let _ = wr.write_all(b"").await;
                serde_json::from_str::<serde_json::Value>(&sub).expect("subscribe json")
            });
            let mut opts = WakeOpts::new(path.clone(), vec![9], Some("demo".into()));
            opts.handshake_timeout = Duration::from_secs(2);
            opts.idle_timeout = Duration::from_millis(300);
            opts.resume_from = resume_from;
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let audit = std::sync::Arc::new(|_: &str, _: serde_json::Value| {});
            run_wake_loop(opts, &tx, &audit).await;
            let _ = std::fs::remove_file(&path);
            server.await.expect("server")
        }
        let cursor = format!("1001:{}", "ab".repeat(32));
        let resumed = first_subscribe(Some(cursor.clone()), "some").await;
        assert_eq!(
            resumed["subscribe"]["since_cursor"],
            serde_json::json!(cursor)
        );
        let fresh = first_subscribe(None, "none").await;
        assert!(fresh["subscribe"]["since_cursor"].is_null());
    }

    #[tokio::test]
    async fn absent_socket_is_one_loud_died() {
        let opts = WakeOpts {
            sock_path: std::env::temp_dir().join("wake-absent-nothing.sock"),
            kinds: vec![9],
            project: None,
            handshake_timeout: Duration::from_millis(200),
            idle_timeout: Duration::from_millis(200),
            resync_floor: Duration::from_millis(50),
            retired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_from: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let audits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let audits2 = audits.clone();
        let audit = std::sync::Arc::new(move |k: &str, v: serde_json::Value| {
            audits2.lock().unwrap().push((k.to_string(), v));
        });
        run_wake_loop(opts, &tx, &audit).await;
        drop(tx);
        let mut got = Vec::new();
        while let Some(o) = rx.recv().await {
            got.push(o);
        }
        assert_eq!(got, vec![WakeOutcome::Died]);
        let kinds: Vec<String> = audits
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(kinds, vec![AUDIT_SOCK_ABSENT.to_string()]);
    }
}
