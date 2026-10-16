//! One wake per mail, counting both hosts (D3).
//!
//! The supervisor host rings an urgent steer itself (an `Interrupt`, then the
//! `URGENT steer (wm N)` wake line) and the doorbell's inbox host rings any
//! unread mail with the normal wake line. Both watch the same inbox, so one
//! steer could be rung twice, and the supervisor's ring was outside the inbox
//! host's ring budget.
//!
//! The smallest sound fix that keeps steers host-side is a tiny ledger both
//! hosts consult before typing: one file per (recipient, watermark) in the
//! state dir, created with `create_new` (atomic), so whichever host rings
//! first owns that watermark's first ring.
//!
//! - The supervisor claims `(npub, wm)` right after appending the steer. If
//!   the inbox host already rang that watermark, the supervisor does not ring
//!   again (the recipient was already woken).
//! - The inbox host asks before every ring. If the supervisor holds the claim,
//!   the inbox host's FIRST ring at that watermark is skipped (the urgent ring
//!   already happened) but still counts in its budget; it re-rings later
//!   exactly as budgeted if the mail stays unread, and deaf escalation is
//!   unchanged.
//!
//! Entries are pruned after a day. Nothing here reads mail content.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const STEER: &str = "steer";
const STEER_SEEN: &str = "steer-seen";
const INBOX: &str = "inbox";
const KEEP: Duration = Duration::from_secs(24 * 3600);

/// The ledger directory for one (project, workspace).
#[derive(Clone, Debug)]
pub struct RingLedger {
    dir: PathBuf,
}

impl RingLedger {
    /// A ledger under `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, npub: &str, wm: u64) -> PathBuf {
        self.dir.join(format!("{npub}-{wm}"))
    }

    /// Create `path` holding `owner`, atomically and complete: write a temp
    /// file, then `hard_link` it into place (fails if the claim exists), so a
    /// reader never sees a half-written claim.
    fn create(&self, path: &Path, owner: &str) -> std::io::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        // Not AlreadyExists: that kind means "someone holds the claim".
        fs::create_dir_all(&self.dir).map_err(std::io::Error::other)?;
        let tmp = self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(owner.as_bytes()))
            .map_err(std::io::Error::other)?;
        let r = fs::hard_link(&tmp, path);
        let _ = fs::remove_file(&tmp);
        r
    }

    fn read(path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn prune(&self) {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return;
        };
        for e in rd.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > KEEP);
            if old {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    /// The supervisor host asks to ring an urgent steer at `(npub, wm)`.
    /// True: it owns the first ring, type it. False: the inbox host already
    /// rang this watermark (or the ledger is unreadable and a ring was
    /// already recorded), do not ring again.
    pub fn steer_may_ring(&self, npub: &str, wm: u64) -> bool {
        self.prune();
        match self.create(&self.path(npub, wm), STEER) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            // A ledger that cannot be written must not silence a steer: ring.
            Err(_) => true,
        }
    }

    /// The inbox host asks to ring `(npub, wm)`. False: the supervisor's
    /// urgent ring is this watermark's first ring; skip it (it still counts in
    /// the caller's budget). Later asks at the same watermark are true.
    pub fn inbox_may_ring(&self, npub: &str, wm: u64) -> bool {
        let path = self.path(npub, wm);
        for _ in 0..2 {
            match self.create(&path, INBOX) {
                Ok(()) => return true,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return true,
            }
            match Self::read(&path).as_deref() {
                Some(STEER) => {
                    // Only the inbox host rewrites a claim, and there is one
                    // inbox host per workspace (the lease).
                    return match fs::write(&path, STEER_SEEN) {
                        Ok(()) => false,
                        Err(_) => true,
                    };
                }
                Some(INBOX) | Some(STEER_SEEN) => return true,
                // Gone between create and read (pruned), or half-written:
                // go round once more, then ring.
                _ => {}
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn ledger() -> (tempfile::TempDir, RingLedger) {
        let d = tempfile::tempdir().unwrap();
        let l = RingLedger::new(d.path().join("rings"));
        (d, l)
    }

    #[test]
    fn the_supervisor_rings_first_then_the_inbox_hosts_first_ring_is_skipped_once() {
        let (_d, l) = ledger();
        assert!(l.steer_may_ring("npub1a", 7));
        assert!(
            !l.inbox_may_ring("npub1a", 7),
            "the urgent ring was this wm's first"
        );
        assert!(
            l.inbox_may_ring("npub1a", 7),
            "a budgeted re-ring still goes out"
        );
        assert!(l.inbox_may_ring("npub1a", 7));
    }

    #[test]
    fn the_inbox_host_rings_first_then_the_supervisor_does_not_ring_again() {
        let (_d, l) = ledger();
        assert!(l.inbox_may_ring("npub1a", 7));
        assert!(!l.steer_may_ring("npub1a", 7), "already woken at this wm");
        assert!(
            l.inbox_may_ring("npub1a", 7),
            "its own re-rings are its budget's"
        );
    }

    #[test]
    fn a_second_steer_claim_at_the_same_wm_does_not_ring_twice() {
        let (_d, l) = ledger();
        assert!(l.steer_may_ring("npub1a", 7));
        assert!(!l.steer_may_ring("npub1a", 7));
    }

    #[test]
    fn other_watermarks_and_recipients_are_independent() {
        let (_d, l) = ledger();
        assert!(l.steer_may_ring("npub1a", 7));
        assert!(l.inbox_may_ring("npub1a", 8), "a later mail rings normally");
        assert!(l.inbox_may_ring("npub1b", 7), "another recipient");
        assert!(l.steer_may_ring("npub1b", 9));
    }

    #[test]
    fn exactly_one_first_ring_whoever_is_first_under_contention() {
        for round in 0..200u64 {
            let (_d, l) = ledger();
            let l = Arc::new(l);
            let b = Arc::new(Barrier::new(2));
            let (l1, b1) = (l.clone(), b.clone());
            let (l2, b2) = (l.clone(), b.clone());
            let s = std::thread::spawn(move || {
                b1.wait();
                l1.steer_may_ring("npub1a", round)
            });
            let i = std::thread::spawn(move || {
                b2.wait();
                l2.inbox_may_ring("npub1a", round)
            });
            let (s, i) = (s.join().unwrap(), i.join().unwrap());
            assert_eq!(
                (s as u8) + (i as u8),
                1,
                "round {round}: supervisor rang={s}, inbox rang={i}; one wake per mail"
            );
        }
    }

    #[test]
    fn an_unwritable_ledger_never_silences_a_steer() {
        let d = tempfile::tempdir().unwrap();
        let blocker = d.path().join("rings");
        fs::write(&blocker, "a file where the directory should be").unwrap();
        let l = RingLedger::new(blocker);
        assert!(l.steer_may_ring("npub1a", 7));
        assert!(l.inbox_may_ring("npub1a", 7));
    }

    #[test]
    fn old_entries_are_pruned() {
        let (_d, l) = ledger();
        assert!(l.steer_may_ring("npub1a", 1));
        let p = l.path("npub1a", 1);
        let old = SystemTime::now() - KEEP - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(l.steer_may_ring("npub1a", 2));
        assert!(!p.exists(), "a day-old claim is gone");
        assert!(l.path("npub1a", 2).exists());
    }
}
