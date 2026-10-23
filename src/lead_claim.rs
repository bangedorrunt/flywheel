//! Who assigns the supervisor's `sup_epoch`.
//!
//! One authority at a time. When `inbox_lead` can be read, the row toron
//! stored is that authority and [`plan_lead_epoch`] takes the next number
//! from it. The fold only records the epoch the append kept. The envelope's
//! own epoch is a separate `claim_epoch` (the producer fence) and is never
//! copied into `sup_epoch`.
//!
//! `FLYWHEEL_SUPERVISOR_EPOCH=fold` keeps the fold's counter and writes no
//! `lead@1`. `=toron` forces the store. Unset follows the table: readable
//! means toron, unreadable means the fold.

use toron_core::delivery::Reject;
use toron_core::envelope::{Class, ContextId, ContractId, ENVELOPE_V, Envelope, MsgId, Recipient};
use toron_core::inbox::{InboxError, InboxStore};

use crate::supervisor_program::RejectCause;

/// Which side hands out `sup_epoch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochAuthority {
    /// `accepted_epoch + 1`. No `lead@1` append.
    Fold,
    /// The `inbox_lead` row.
    Toron,
}

/// What [`sync_lead`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeadStep {
    /// This holder already stored an epoch the fold has not recorded.
    CatchUp(u64),
    /// This epoch was appended just now.
    Appended(u64),
}

/// A lead append that did not land.
#[derive(Debug)]
pub enum LeadError {
    /// Toron already has this `sup_epoch` or a higher one.
    Stale,
    /// A steer is still unacked. A lead append does not hit this; a steer does.
    Pending,
    /// The store, the body, or any other refusal.
    Store(String),
}

impl std::fmt::Display for LeadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale => write!(f, "stale sup_epoch"),
            Self::Pending => write!(f, "steer pending"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

/// `forced` is `FLYWHEEL_SUPERVISOR_EPOCH`. Only `fold` and `toron` are
/// words. Anything else, including unset, follows `lead_readable`.
pub fn epoch_authority(forced: Option<&str>, lead_readable: bool) -> EpochAuthority {
    match forced {
        Some("fold") => EpochAuthority::Fold,
        Some("toron") => EpochAuthority::Toron,
        _ => {
            if lead_readable {
                EpochAuthority::Toron
            } else {
                EpochAuthority::Fold
            }
        }
    }
}

/// The next `sup_epoch` to append.
///
/// A stored row is the authority: the next fact is one above it. The fold's
/// counter is used only when there is no row, or when a log from before
/// `lead@1` has already passed the row (the store would accept the lower
/// number and the fold would refuse it). When the row is ahead of the fold,
/// the fold's counter is not read.
pub fn plan_lead_epoch(stored: Option<u64>, fold_accepted: u64) -> u64 {
    let floor = fold_accepted.saturating_add(1);
    match stored {
        Some(current) => {
            let next = current.saturating_add(1);
            if next > fold_accepted { next } else { floor }
        }
        None => floor,
    }
}

/// The key that holds the lead: the same `system_producer(project)` the host
/// signs every steer with (`from`). toron's steer fence (F7) accepts a steer
/// only when `from` equals the holder on record, so the holder is this key
/// and never a per-process name. The pid stays in the fold's own claim
/// (`supervise-<project>-<pid>`) and in the epoch claim's owner string.
pub fn lead_holder(project: &str) -> String {
    crate::inbox_ports::system_producer(project)
}

/// Toron's fence, named as the fold's cause. Other refusals are not a fence.
///
/// `NoLead` and `NotLeadHolder` (toron F7) are matched on the wire tag, not
/// the variant, so this builds against a toron from before the fence too
/// (where those refusals cannot occur).
pub fn store_reject_cause(err: &InboxError) -> Option<RejectCause> {
    match err {
        InboxError::Reject(Reject::StaleSupEpoch { .. }) => Some(RejectCause::StaleEpoch),
        InboxError::Reject(Reject::SteerPending { .. }) => Some(RejectCause::SteerPending),
        InboxError::Reject(r) => match reject_tag(r).as_deref() {
            Some("no_lead") => Some(RejectCause::NoLead),
            Some("not_lead_holder") => Some(RejectCause::NotLeadHolder),
            _ => None,
        },
        _ => None,
    }
}

/// The `reject` wire tag of a refusal (`#[serde(tag = "reject")]`).
fn reject_tag(r: &Reject) -> Option<String> {
    serde_json::to_value(r)
        .ok()?
        .get("reject")?
        .as_str()
        .map(str::to_string)
}

fn lead_error(err: InboxError) -> LeadError {
    match store_reject_cause(&err) {
        Some(RejectCause::StaleEpoch) => LeadError::Stale,
        Some(RejectCause::SteerPending) => LeadError::Pending,
        _ => LeadError::Store(err.to_string()),
    }
}

fn lead_envelope(
    project: &str,
    producer: &str,
    epoch: u64,
    holder: &str,
    sup_epoch: u64,
) -> Result<Envelope, String> {
    let body = serde_json::json!({ "holder": holder, "sup_epoch": sup_epoch });
    let env = Envelope {
        v: ENVELOPE_V,
        msg: MsgId {
            producer: producer.to_string(),
            epoch,
            seq: 0,
        },
        from: producer.to_string(),
        to: Recipient::Npub(producer.to_string()),
        class: Class::Control,
        contract: ContractId::new("lead", 1),
        context: ContextId(project.to_string()),
        task: None,
        generation: 0,
        causation: None,
        deadline_ts_us: None,
        lease_ms: None,
        body,
    };
    env.validate().map_err(|e| format!("{e:?}"))?;
    Ok(env)
}

/// Append one `lead@1` at `sup_epoch`. `store` must be `with_operator` of
/// the system producer: the first lead fact is operator-signed. The
/// envelope epoch comes from `claim_epoch` and is not `sup_epoch`.
pub async fn append_lead_at(
    store: &InboxStore,
    project: &str,
    holder: &str,
    sup_epoch: u64,
) -> Result<(), LeadError> {
    if holder.trim().is_empty() {
        return Err(LeadError::Store("lead holder must be non-empty".into()));
    }
    let producer = crate::inbox_ports::system_producer(project);
    let claim = store
        .claim_epoch(&producer, &format!("lead:{}", std::process::id()))
        .await
        .map_err(|e| LeadError::Store(e.to_string()))?;
    let env = lead_envelope(project, &producer, claim.epoch, holder, sup_epoch)
        .map_err(LeadError::Store)?;
    match store.append_envelope(&env).await {
        Ok(_) => Ok(()),
        Err(e) => Err(lead_error(e)),
    }
}

/// Read the row and append the next epoch, or catch up when this holder
/// already stored one the fold missed. A lost race re-reads once.
pub async fn sync_lead(
    store: &InboxStore,
    project: &str,
    holder: &str,
    fold_accepted: u64,
) -> Result<LeadStep, LeadError> {
    let mut raced = false;
    for _ in 0..2 {
        let row = store
            .lead_of(project)
            .await
            .map_err(|e| LeadError::Store(e.to_string()))?;
        if let Some(row) = &row
            && row.holder == holder
            && row.sup_epoch > fold_accepted
        {
            return Ok(LeadStep::CatchUp(row.sup_epoch));
        }
        let epoch = plan_lead_epoch(row.as_ref().map(|r| r.sup_epoch), fold_accepted);
        match append_lead_at(store, project, holder, epoch).await {
            Ok(()) => return Ok(LeadStep::Appended(epoch)),
            Err(LeadError::Stale) => raced = true,
            Err(e) => return Err(e),
        }
    }
    if raced {
        Err(LeadError::Stale)
    } else {
        Err(LeadError::Store("lead claim did not append".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::inbox::InboxStore;

    #[test]
    fn plan_lead_epoch_follows_the_store_when_the_row_is_ahead() {
        assert_eq!(plan_lead_epoch(None, 0), 1);
        assert_eq!(plan_lead_epoch(None, 5), 6);
        assert_eq!(plan_lead_epoch(Some(7), 0), 8);
        assert_eq!(plan_lead_epoch(Some(3), 5), 6);
        assert_eq!(plan_lead_epoch(Some(u64::MAX), 0), u64::MAX);
    }

    #[test]
    fn epoch_authority_is_one_side_at_a_time() {
        assert_eq!(epoch_authority(None, true), EpochAuthority::Toron);
        assert_eq!(epoch_authority(None, false), EpochAuthority::Fold);
        assert_eq!(epoch_authority(Some("fold"), true), EpochAuthority::Fold);
        assert_eq!(epoch_authority(Some("toron"), false), EpochAuthority::Toron);
        assert_eq!(epoch_authority(Some("nope"), true), EpochAuthority::Toron);
    }

    #[test]
    fn store_fences_map_onto_the_fold_causes() {
        let stale = InboxError::Reject(Reject::StaleSupEpoch {
            sent: 1,
            current: 7,
        });
        let pending = InboxError::Reject(Reject::SteerPending {
            id: "p:1:0".into(),
            op: "cancel".into(),
        });
        let other = InboxError::Reject(Reject::BadBody {
            contract: "lead@1".into(),
            error: "holder must be non-empty".into(),
        });
        assert_eq!(store_reject_cause(&stale), Some(RejectCause::StaleEpoch));
        assert_eq!(
            store_reject_cause(&pending),
            Some(RejectCause::SteerPending)
        );
        assert_eq!(store_reject_cause(&other), None);
    }

    /// rv23 #6 (toron F7): the steer fence's two refusals are causes, not
    /// `None`. Built from the wire so this compiles against a pre-F7 toron,
    /// where the variants do not exist (and the test has nothing to map).
    #[test]
    fn the_steer_fence_refusals_map_onto_causes() {
        let wire = [
            (
                serde_json::json!({"reject": "no_lead", "context": "proj"}),
                RejectCause::NoLead,
            ),
            (
                serde_json::json!({"reject": "not_lead_holder", "actor": "x", "holder": "y", "sup_epoch": 3}),
                RejectCause::NotLeadHolder,
            ),
        ];
        let mut mapped = 0;
        for (v, cause) in wire {
            // `Reject` borrows (`Deserialize<'static>` only): a leaked line.
            let line: &'static str = Box::leak(v.to_string().into_boxed_str());
            let Ok(r) = serde_json::from_str::<Reject>(line) else {
                continue;
            };
            assert_eq!(store_reject_cause(&InboxError::Reject(r)), Some(cause));
            mapped += 1;
        }
        assert!(mapped == 0 || mapped == 2, "F7 adds both refusals");
    }

    async fn mem() -> InboxStore {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = dir.join("index.sqlite");
        let pool = toron_core::index::schema::connect(&db.to_string_lossy())
            .await
            .unwrap();
        toron_core::index::schema::migrate(&pool).await.unwrap();
        InboxStore::new(pool)
    }

    fn operated(store: InboxStore, project: &str) -> InboxStore {
        store.with_operator(crate::inbox_ports::system_producer(project))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lead_epoch_a_row_ahead_of_the_fold_is_the_number_appended() {
        let store = operated(mem().await, "proj");
        append_lead_at(&store, "proj", "other", 7).await.unwrap();
        let step = sync_lead(&store, "proj", "lead-1", 0).await.unwrap();
        assert_eq!(step, LeadStep::Appended(8));
        let row = store.lead_of("proj").await.unwrap().unwrap();
        assert_eq!(row.sup_epoch, 8);
        assert_eq!(row.holder, "lead-1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lead_epoch_catch_up_does_not_append_a_second_fact() {
        let store = operated(mem().await, "proj");
        append_lead_at(&store, "proj", "lead-1", 4).await.unwrap();
        let key = store.lead_of("proj").await.unwrap().unwrap();
        let step = sync_lead(&store, "proj", "lead-1", 0).await.unwrap();
        assert_eq!(step, LeadStep::CatchUp(4));
        let again = store.lead_of("proj").await.unwrap().unwrap();
        assert_eq!(again.sup_epoch, 4);
        assert_eq!(again.holder, key.holder);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lead_epoch_an_empty_table_uses_the_fold_next_so_the_first_fact_lands() {
        let store = operated(mem().await, "proj");
        let step = sync_lead(&store, "proj", "lead-1", 5).await.unwrap();
        assert_eq!(step, LeadStep::Appended(6));
        assert_eq!(store.lead_of("proj").await.unwrap().unwrap().sup_epoch, 6);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lead_epoch_a_stale_append_writes_nothing() {
        let store = operated(mem().await, "proj");
        append_lead_at(&store, "proj", "lead-1", 4).await.unwrap();
        let err = append_lead_at(&store, "proj", "other", 4)
            .await
            .unwrap_err();
        assert!(matches!(err, LeadError::Stale), "{err}");
        let row = store.lead_of("proj").await.unwrap().unwrap();
        assert_eq!(row.sup_epoch, 4);
        assert_eq!(row.holder, "lead-1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lead_epoch_an_unoperated_store_stores_nothing() {
        let store = mem().await;
        let err = append_lead_at(&store, "proj", "lead-1", 1)
            .await
            .unwrap_err();
        assert!(matches!(err, LeadError::Store(_)), "{err}");
        assert!(store.lead_of("proj").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lead_epoch_a_stale_steer_is_refused_by_the_store_and_leaves_no_envelope() {
        use crate::supervisor_host::{SteerSend, steer_envelope};
        use crate::supervisor_program::SteerOp;
        let store = operated(mem().await, "proj");
        append_lead_at(&store, "proj", "lead-1", 2).await.unwrap();
        let child = "cc".repeat(32);
        let send = SteerSend {
            seq: 1,
            child: "a".into(),
            op: SteerOp::Cancel,
            generation: 1,
            sup_epoch: 1,
            urgent: true,
            delta_sha: None,
        };
        let producer = crate::inbox_ports::system_producer("proj");
        let claim = store.claim_epoch(&producer, "steer-test").await.unwrap();
        let env = steer_envelope("proj", &producer, claim.epoch, &child, &send, None).unwrap();
        let err = store.append_envelope(&env).await.unwrap_err();
        assert_eq!(
            store_reject_cause(&err),
            Some(RejectCause::StaleEpoch),
            "{err:?}"
        );
        assert!(store.lookup(&env.msg).await.unwrap().is_none());
    }
}
