//! Spawn-time `context@1` and `role@1` appends.
//!
//! Best-effort: a refusal is logged and the spawn still succeeds. The
//! operator is the doorbell's system producer, because the first role of a
//! context must be an operator-signed captain. One `claim_epoch` covers both
//! facts (context at seq 0, role at the next seq).

use toron_core::authority::Role;
use toron_core::contracts::context_v1::ContextV1;
use toron_core::contracts::role_v1::RoleV1;
use toron_core::delivery::Reject;
use toron_core::envelope::{ContextId, MsgId};
use toron_core::inbox::{InboxError, InboxStore};
use toron_core::topology::Topology;

/// Next role `gen`, or nothing when this spawn must not write a role fact.
///
/// The same role already held is a no-op. A different role steps the fence.
/// A context with no roles accepts only `captain` (the store refuses the rest,
/// and this planner does not invent a captain npub to get there).
pub fn plan_spawn_role(
    context_has_roles: bool,
    current: Option<(Role, u32)>,
    want: Role,
) -> Option<u32> {
    if let Some((held, generation)) = current {
        if held == want {
            return None;
        }
        return Some(generation.saturating_add(1));
    }
    if !context_has_roles && want != Role::Captain {
        return None;
    }
    Some(0)
}

/// `context@1` gen when the context has no fact yet. An existing fact is left
/// alone: a spawn does not retarget a context another pane already set.
pub fn plan_context_gen(existing: Option<u32>) -> Option<u32> {
    existing.is_none().then_some(1)
}

/// Append the facts this spawn still owes. `Ok(None)` means both were already
/// current. The store must be [`InboxStore::with_operator`] of
/// [`crate::inbox_ports::system_producer`], and `from` is that producer.
pub async fn append_spawn_facts(
    store: &InboxStore,
    project: &str,
    topology: Topology,
    npub: &str,
    role: Role,
) -> Result<Option<&'static str>, String> {
    if !toron_core::envelope::is_npub_hex(npub) {
        return Err("role subject is not 64 lowercase hex".into());
    }
    let producer = crate::inbox_ports::system_producer(project);
    let has_roles = !store
        .roles(project)
        .await
        .map_err(|e| e.to_string())?
        .is_empty();
    let current = store
        .role_of(project, npub)
        .await
        .map_err(|e| e.to_string())?
        .map(|fact| (fact.role, fact.generation));
    let role_gen = plan_spawn_role(has_roles, current, role);
    if role_gen.is_none() && !has_roles && role != Role::Captain {
        eprintln!("flywheel: comms first role must be captain; role fact skipped");
    }
    let ctx_gen = plan_context_gen(
        store
            .context_of(project)
            .await
            .map_err(|e| e.to_string())?
            .map(|fact| fact.generation),
    );
    if ctx_gen.is_none() && role_gen.is_none() {
        return Ok(None);
    }
    // ponytail: two spawns racing the first context both send gen 1. A
    // StaleGen on the context means the other won; this claim still writes
    // the role. A lost race is not retried inside the spawn.
    let claim = store
        .claim_epoch(&producer, &format!("spawn:{}", std::process::id()))
        .await
        .map_err(|e| e.to_string())?;
    let context = ContextId(project.to_string());
    let mut seq = 0u64;
    let mut wrote_ctx = false;
    if let Some(generation) = ctx_gen {
        let env = ContextV1 {
            topology,
            generation,
        }
        .into_envelope(
            MsgId {
                producer: producer.clone(),
                epoch: claim.epoch,
                seq,
            },
            context.clone(),
            producer.clone(),
        );
        match store.append_envelope(&env).await {
            Ok(_) => {
                wrote_ctx = true;
                seq = seq.saturating_add(1);
            }
            Err(InboxError::Reject(Reject::StaleGen { .. })) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut wrote_role = false;
    if let Some(generation) = role_gen {
        let env = RoleV1 {
            npub: npub.to_string(),
            role,
            generation,
        }
        .into_envelope(
            MsgId {
                producer: producer.clone(),
                epoch: claim.epoch,
                seq,
            },
            context,
        );
        match store.append_envelope(&env).await {
            Ok(_) => wrote_role = true,
            Err(InboxError::Reject(Reject::StaleGen { .. })) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(match (wrote_ctx, wrote_role) {
        (true, true) => Some("context@1 and role@1"),
        (true, false) => Some("context@1"),
        (false, true) => Some("role@1"),
        (false, false) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::inbox::InboxStore;

    #[test]
    fn plan_skips_the_same_role_and_refuses_a_crew_on_an_empty_context() {
        assert_eq!(
            plan_spawn_role(true, Some((Role::Crew, 2)), Role::Crew),
            None
        );
        assert_eq!(
            plan_spawn_role(true, Some((Role::Crew, 2)), Role::Verifier),
            Some(3)
        );
        assert_eq!(
            plan_spawn_role(true, Some((Role::Crew, u32::MAX)), Role::Peer),
            Some(u32::MAX)
        );
        assert_eq!(plan_spawn_role(false, None, Role::Crew), None);
        assert_eq!(plan_spawn_role(false, None, Role::Captain), Some(0));
        assert_eq!(plan_spawn_role(true, None, Role::Crew), Some(0));
        assert_eq!(plan_context_gen(None), Some(1));
        assert_eq!(plan_context_gen(Some(1)), None);
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
    async fn captain_then_crew_share_one_epoch_and_a_repeat_writes_nothing() {
        let store = operated(mem().await, "proj");
        let captain = "aa".repeat(32);
        let crew = "bb".repeat(32);
        let landed = append_spawn_facts(&store, "proj", Topology::Swarm, &captain, Role::Captain)
            .await
            .unwrap();
        assert_eq!(landed, Some("context@1 and role@1"));
        let ctx = store.context_of("proj").await.unwrap().unwrap();
        assert_eq!(ctx.topology, Topology::Swarm);
        assert_eq!(ctx.generation, 1);
        let captain_role = store.role_of("proj", &captain).await.unwrap().unwrap();
        assert_eq!(captain_role.role, Role::Captain);
        assert_eq!(captain_role.generation, 0);
        let ctx_key: Vec<_> = ctx.msg_key.split(':').collect();
        let role_key: Vec<_> = captain_role.msg_key.split(':').collect();
        assert_eq!(ctx_key[1], role_key[1], "one claim_epoch for both facts");
        assert_eq!(ctx_key[2], "0");
        assert_eq!(role_key[2], "1");

        let landed = append_spawn_facts(&store, "proj", Topology::Swarm, &crew, Role::Crew)
            .await
            .unwrap();
        assert_eq!(landed, Some("role@1"));
        let crew_role = store.role_of("proj", &crew).await.unwrap().unwrap();
        assert_eq!(crew_role.role, Role::Crew);
        assert_eq!(crew_role.generation, 0);
        assert_eq!(
            store.context_of("proj").await.unwrap().unwrap().generation,
            1
        );

        let head = store.head(&captain).await.unwrap();
        let again = append_spawn_facts(&store, "proj", Topology::Swarm, &captain, Role::Captain)
            .await
            .unwrap();
        assert_eq!(again, None);
        assert_eq!(store.head(&captain).await.unwrap(), head);
        assert_eq!(
            store
                .role_of("proj", &captain)
                .await
                .unwrap()
                .unwrap()
                .generation,
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crew_first_stores_the_context_and_no_role() {
        let store = operated(mem().await, "proj");
        let crew = "bb".repeat(32);
        let landed = append_spawn_facts(&store, "proj", Topology::Loop, &crew, Role::Crew)
            .await
            .unwrap();
        assert_eq!(landed, Some("context@1"));
        assert_eq!(
            store.context_of("proj").await.unwrap().unwrap().topology,
            Topology::Loop
        );
        assert!(store.role_of("proj", &crew).await.unwrap().is_none());
        assert!(store.roles("proj").await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unoperated_store_stores_neither_fact() {
        let store = mem().await;
        let captain = "aa".repeat(32);
        let err = append_spawn_facts(&store, "proj", Topology::Swarm, &captain, Role::Captain)
            .await
            .unwrap_err();
        assert!(
            err.to_ascii_lowercase().contains("unauthorized") || err.contains("operator"),
            "{err}"
        );
        assert!(store.context_of("proj").await.unwrap().is_none());
        assert!(store.role_of("proj", &captain).await.unwrap().is_none());
    }
}
