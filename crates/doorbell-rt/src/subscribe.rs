// governed-by: ADR-0012
//! Subscription store + verb authz (D5/D6, bd-asr2.8).
//!
//! Subscription = identity × scope × filter × sink × budget. One shared
//! classification pass per workspace; N per-subscriber policy evaluations
//! (budgets/queues/digests, pure, evaluated by the Brain against each
//! subscription's overrides); delivery adapters in `sinks.rs`. The journal
//! stays totally ordered, tagged by subscriber (additive field — v1
//! readers tolerate it; full v2 scoping lands in bd-asr2.9).
//!
//! D6 authz matrix (operator = the daemon's operator credential; local =
//! same-host process over UDS/file-perm trust):
//!
//! | Verb                                    | Local | Operator |
//! |-----------------------------------------+-------+----------|
//! | read (status/health/why/digest/events)  |   ✔   |    ✔     |
//! | subscribe herdr-sink, own workspace     |   ✔   |    ✔     |
//! | subscribe webhook/DM or cross-workspace |   ✖   |    ✔     |
//! | budget set ≤ default (own)              |   ✔   |    ✔     |
//! | budget set > default, inject, mint      |   ✖   |    ✔     |

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Who is woken.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Identity {
    /// Toron pin (spawned crew, human operator pin).
    Pin(String),
    /// Role address (`role:<name>`, bound via 30013).
    Role(String),
    /// Operator npub (human subscription).
    Npub(String),
    /// Service key (minted by the daemon, D7; endpoint in Phase 4).
    ServiceKey(String),
}

impl Identity {
    pub fn tag(&self) -> String {
        match self {
            Identity::Pin(p) => format!("pin:{p}"),
            Identity::Role(r) => format!("role:{r}"),
            Identity::Npub(n) => format!("npub:{n}"),
            Identity::ServiceKey(k) => format!("svc:{k}"),
        }
    }
}

/// Workspace scope. Cross-workspace stays same-host (D10 revisit trigger).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Scope {
    One(String),
    Many(Vec<String>),
}

impl Scope {
    pub fn workspaces(&self) -> Vec<String> {
        match self {
            Scope::One(w) => vec![w.clone()],
            Scope::Many(ws) => ws.clone(),
        }
    }

    pub fn is_cross(&self, home: &str) -> bool {
        match self {
            Scope::One(w) => w != home,
            Scope::Many(_) => true,
        }
    }
}

/// Ring-class filter. Empty classes = all rings.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Filter {
    pub classes: Vec<String>,
    pub kinds: Vec<u16>,
}

/// Delivery edge (P1 corollary: adapters translate Prompt intents).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SinkKind {
    Herdr,
    Webhook,
    Dm,
}

impl SinkKind {
    /// The name this kind's adapter is registered under in a
    /// [`crate::sinks::SinkManager`].
    ///
    /// One mapping (bd-8ulu): the subscription row's `sink` is what routes a
    /// wake, and the manager keys adapters by name — if those two spellings
    /// ever diverged, a subscribed wake would be sent to a sink nobody
    /// registered and the send would report success anyway.
    #[must_use]
    pub fn sink_name(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
            Self::Webhook => "webhook",
            Self::Dm => "dm",
        }
    }
}

/// Per-subscription budget override (5/30/3 when absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Budget {
    pub window_minutes: u32,
    pub max_prompts: u32,
}

impl Budget {
    pub fn default_530() -> Self {
        Self {
            window_minutes: 30,
            max_prompts: 3,
        }
    }

    /// Raises above default need the operator (D6).
    pub fn needs_operator(&self) -> bool {
        let d = Self::default_530();
        self.window_minutes < d.window_minutes || self.max_prompts > d.max_prompts
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Subscription {
    pub id: String,
    pub identity: Identity,
    pub scope: Scope,
    pub filter: Filter,
    pub sink: SinkKind,
    pub budget: Budget,
    /// Project context for daemon-side auto-host (bd-2o1v): the daemon
    /// hosts a loop for every subscribed `(project, workspace)` it can
    /// NAME — legacy rows without a project are never guessed into a host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Where THIS row's wake lands (bd-8ulu), for sinks that are an
    /// endpoint rather than a pane: the webhook URL. Per-row, not
    /// per-daemon — identity x scope x sink x endpoint is the whole point
    /// of the row, and two subscribers routinely target two endpoints.
    /// Absent for `herdr` (the pane registry is the destination) and for a
    /// legacy row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl Subscription {
    /// The adapter name a wake through this row must be sent to.
    ///
    /// bd-8ulu: an endpoint-carrying row gets an adapter OF ITS OWN, named
    /// by its endpoint, so two webhook subscribers can point at two
    /// destinations. Everything else (herdr, dm, a webhook row that never
    /// named a URL) keeps the bare [`SinkKind::sink_name`].
    #[must_use]
    pub fn route_name(&self) -> String {
        match (&self.sink, &self.endpoint) {
            (SinkKind::Webhook, Some(url)) if !url.trim().is_empty() => {
                format!("webhook:{}", url.trim())
            }
            (kind, _) => kind.sink_name().to_string(),
        }
    }
}

/// The actor attempting a verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Actor {
    pub is_operator: bool,
    pub is_local: bool,
}

impl Actor {
    pub fn operator() -> Self {
        Self {
            is_operator: true,
            is_local: true,
        }
    }

    pub fn local_process() -> Self {
        Self {
            is_operator: false,
            is_local: true,
        }
    }

    pub fn remote() -> Self {
        Self {
            is_operator: false,
            is_local: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Needs the operator credential.
    OperatorOnly(String),
    /// Unknown subscription / bad reference.
    Absent(String),
}

/// Authorize a subscribe verb under D6. `home` is the requesting
/// workspace (cross-workspace detection).
pub fn authorize_subscribe(
    actor: Actor,
    sink: SinkKind,
    scope: &Scope,
    home: &str,
    budget: Budget,
) -> Result<(), Refusal> {
    if !actor.is_local && !actor.is_operator {
        return Err(Refusal::OperatorOnly("non-local subscribe".into()));
    }
    if (sink != SinkKind::Herdr || scope.is_cross(home)) && !actor.is_operator {
        return Err(Refusal::OperatorOnly(
            "webhook/DM sink or cross-workspace scope needs the operator".into(),
        ));
    }
    if budget.needs_operator() && !actor.is_operator {
        return Err(Refusal::OperatorOnly(
            "budget raise above default needs the operator".into(),
        ));
    }
    Ok(())
}

/// Durable subscription registry (JSON file beside the journals).
pub struct SubscriptionStore {
    path: PathBuf,
    subs: BTreeMap<String, Subscription>,
}

impl SubscriptionStore {
    pub fn open(state_dir: &Path) -> Self {
        let path = state_dir.join("doorbell").join("subscriptions.json");
        let subs = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path, subs }
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(raw) = serde_json::to_string_pretty(&self.subs) {
            let _ = std::fs::write(&self.path, raw);
        }
    }

    /// Roundtrip: authorize, store, persist. Returns the stored row.
    pub fn subscribe(
        &mut self,
        actor: Actor,
        sub: Subscription,
        home: &str,
    ) -> Result<Subscription, Refusal> {
        authorize_subscribe(actor, sub.sink, &sub.scope, home, sub.budget)?;
        if sub.scope.workspaces().is_empty() {
            return Err(Refusal::Absent("empty scope".into()));
        }
        self.subs.insert(sub.id.clone(), sub.clone());
        self.persist();
        Ok(sub)
    }

    pub fn unsubscribe(&mut self, _actor: Actor, id: &str) -> Result<Subscription, Refusal> {
        // Unsubscribe is self-service (own row) or operator; ownership
        // binds in Phase 2 with service identities — until then any local
        // actor may drop a row (loud: the drop is journaled by the caller).
        self.subs
            .remove(id)
            .map(Ok)
            .unwrap_or(Err(Refusal::Absent(format!("unknown subscription {id}"))))
            .inspect(|_| {
                self.persist();
            })
    }

    pub fn list(&self) -> Vec<Subscription> {
        self.subs.values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Subscription> {
        self.subs.get(id).cloned()
    }
}

#[cfg(test)]
mod tests {
    /// bd-8ulu: the name a subscription's `sink` routes to IS the name the
    /// manager registers, in one place. A divergence would send a subscribed
    /// wake to a sink nobody registered (and the send reports success anyway).
    #[test]
    fn sink_names_are_distinct_and_non_empty() {
        let names: Vec<&str> = [SinkKind::Herdr, SinkKind::Webhook, SinkKind::Dm]
            .into_iter()
            .map(SinkKind::sink_name)
            .collect();
        assert_eq!(names, vec!["herdr", "webhook", "dm"]);
        for n in &names {
            assert!(!n.is_empty());
            assert_eq!(*n, n.to_ascii_lowercase());
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "sink names must be distinct");
    }

    use super::*;

    fn sub() -> Subscription {
        Subscription {
            id: "s1".into(),
            identity: Identity::Pin("p".into()),
            scope: Scope::One("w".into()),
            filter: Filter::default(),
            sink: SinkKind::Herdr,
            budget: Budget::default_530(),
            project: None,
            endpoint: None,
        }
    }

    /// bd-8ulu: the route a row names. Only an endpoint-carrying webhook row
    /// gets an adapter of its own; everything else keeps the bare sink name
    /// (so no pre-existing routing changes spelling).
    #[test]
    fn route_name_is_per_endpoint_only_for_webhook() {
        let herdr = sub();
        assert_eq!(herdr.route_name(), "herdr");
        let mut dm = sub();
        dm.sink = SinkKind::Dm;
        dm.endpoint = Some("https://ignored.example/x".into());
        assert_eq!(dm.route_name(), "dm", "only webhook binds an endpoint");
        let mut bare = sub();
        bare.sink = SinkKind::Webhook;
        assert_eq!(bare.route_name(), "webhook", "no URL = the default name");
        bare.endpoint = Some("   ".into());
        assert_eq!(bare.route_name(), "webhook", "blank URL is no URL");
        let mut hook = sub();
        hook.sink = SinkKind::Webhook;
        hook.endpoint = Some(" http://127.0.0.1:18999/wake ".into());
        assert_eq!(hook.route_name(), "webhook:http://127.0.0.1:18999/wake");
        let mut other = hook.clone();
        other.endpoint = Some("http://127.0.0.1:19000/wake".into());
        assert_ne!(
            hook.route_name(),
            other.route_name(),
            "two subscribers must be able to target two endpoints"
        );
    }

    #[test]
    fn local_herdr_own_workspace_roundtrips() {
        let dir = std::env::temp_dir().join(format!("dbrt-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SubscriptionStore::open(&dir);
        let back = store
            .subscribe(Actor::local_process(), sub(), "w")
            .expect("local herdr subscribe");
        assert_eq!(back.id, "s1");
        assert_eq!(store.list().len(), 1);
        // Durable: reopen reads it back.
        let store2 = SubscriptionStore::open(&dir);
        assert_eq!(
            store2.get("s1").unwrap().identity,
            Identity::Pin("p".into())
        );
        // Unsubscribe roundtrips.
        let dropped = store.unsubscribe(Actor::local_process(), "s1").unwrap();
        assert_eq!(dropped.id, "s1");
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_operator_webhook_subscribe_refused() {
        let dir = std::env::temp_dir().join(format!("dbrt-subdeny-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SubscriptionStore::open(&dir);
        let mut hook = sub();
        hook.id = "web".into();
        hook.sink = SinkKind::Webhook;
        assert!(matches!(
            store.subscribe(Actor::local_process(), hook.clone(), "w"),
            Err(Refusal::OperatorOnly(_))
        ));
        // Operator passes.
        assert!(store.subscribe(Actor::operator(), hook, "w").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-2o1v: the `project` field is additive — legacy rows (no field on
    /// disk) deserialize with `None`, and the daemon never guesses a host
    /// for them; a named project roundtrips.
    #[test]
    fn project_field_is_additive_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("dbrt-subproj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SubscriptionStore::open(&dir);
        let mut with_project = sub();
        with_project.project = Some("flywheel".into());
        store
            .subscribe(Actor::local_process(), with_project, "w")
            .expect("subscribe");
        // Legacy JSON (pre-project rows on disk) still loads as None.
        let sub_path = dir.join("doorbell").join("subscriptions.json");
        let raw = std::fs::read_to_string(&sub_path).expect("persisted");
        let mut legacy: serde_json::Value = serde_json::from_str(&raw).expect("persisted json");
        for row in legacy.as_object_mut().expect("map").values_mut() {
            row.as_object_mut().map(|m| m.remove("project"));
        }
        std::fs::write(
            &sub_path,
            serde_json::to_string_pretty(&legacy).expect("legacy"),
        )
        .expect("rewrite legacy");
        let reopened = SubscriptionStore::open(&dir);
        assert_eq!(
            reopened.list()[0].project,
            None,
            "legacy row without project reads None, never a guess"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_workspace_refused_for_non_operator() {
        let dir = std::env::temp_dir().join(format!("dbrt-subx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SubscriptionStore::open(&dir);
        let mut x = sub();
        x.id = "x".into();
        x.scope = Scope::Many(vec!["w".into(), "v".into()]);
        assert!(matches!(
            store.subscribe(Actor::local_process(), x.clone(), "w"),
            Err(Refusal::OperatorOnly(_))
        ));
        assert!(store.subscribe(Actor::operator(), x, "w").is_ok());
        // Budget raise above default needs the operator too.
        let mut rich = sub();
        rich.id = "rich".into();
        rich.budget = Budget {
            window_minutes: 30,
            max_prompts: 10,
        };
        assert!(matches!(
            store.subscribe(Actor::local_process(), rich.clone(), "w"),
            Err(Refusal::OperatorOnly(_))
        ));
        assert!(store.subscribe(Actor::operator(), rich, "w").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
