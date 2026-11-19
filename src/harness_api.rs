//! harness API delivery (bd-f888z): opencode v2 wakes ride its own server API
//! (`session.prompt`) instead of typing into a TUI pane. One transport port,
//! one route decision, Inject fallback. Unit tests use a fake transport and
//! never reach a real service.
//!
//! Effect law: the route decision is pure; journal the chosen route BEFORE the
//! provider runs; a retry keeps the same effect key (derived from the msg id).

#[cfg(test)]
use std::sync::{Arc, Mutex};

/// How urgent a wake is: normal queues behind the current turn, urgent steers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Queue,
    Steer,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Queue => "queue",
            Delivery::Steer => "steer",
        }
    }
}

/// Stable, idempotent message id: `msg_` + a hash of (seed, pin). A retried ring
/// of the same mail/watermark to the same pin produces the SAME id, so the
/// harness deduplicates it. FNV-1a: deterministic across processes and versions
/// (unlike DefaultHasher), which a durable effect key requires.
pub fn msg_id(seed: &str, pin: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in seed
        .as_bytes()
        .iter()
        .chain([0u8].iter())
        .chain(pin.as_bytes())
    {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("msg_{h:016x}")
}

/// One delivered prompt call, recorded by the fake transport.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptCall {
    pub session: String,
    pub text: String,
    pub delivery: Delivery,
    pub msg_id: String,
}

/// The single transport port. Real = HTTP to the discovered service; fake = tests.
pub trait HarnessApi: Send + Sync {
    fn prompt(
        &self,
        session: &str,
        text: &str,
        delivery: Delivery,
        msg_id: &str,
    ) -> anyhow::Result<()>;
}

/// In-memory transport for tests. Records calls; can be told to fail.
#[cfg(test)]
#[derive(Default)]
pub struct FakeApi {
    calls: Arc<Mutex<Vec<PromptCall>>>,
    fail: Arc<Mutex<bool>>,
}

#[cfg(test)]
impl FakeApi {
    pub fn calls(&self) -> Vec<PromptCall> {
        self.calls.lock().expect("calls lock").clone()
    }

    pub fn fail_with(&self) {
        *self.fail.lock().expect("fail lock") = true;
    }
}

#[cfg(test)]
impl HarnessApi for FakeApi {
    fn prompt(
        &self,
        session: &str,
        text: &str,
        delivery: Delivery,
        msg_id: &str,
    ) -> anyhow::Result<()> {
        if *self.fail.lock().expect("fail lock") {
            anyhow::bail!("fake api transport down");
        }
        self.calls.lock().expect("calls lock").push(PromptCall {
            session: session.to_string(),
            text: text.to_string(),
            delivery,
            msg_id: msg_id.to_string(),
        });
        Ok(())
    }
}

struct ServiceConfig {
    url: String,
    password: Option<String>,
}

fn discover_uncached() -> anyhow::Result<ServiceConfig> {
    let mut password = std::env::var("OPENCODE_SERVER_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty());
    let mut url = std::env::var("FLYWHEEL_OPENCODE_API_URL")
        .ok()
        .filter(|u| !u.is_empty());
    if url.is_none() {
        if let Ok(out) = std::process::Command::new("opencode")
            .args(["service", "get"])
            .output()
            && out.status.success()
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            && let Some(p) = v.get("password").and_then(|p| p.as_str())
            && !p.is_empty()
        {
            password = Some(p.to_string());
        }
        let out = std::process::Command::new("opencode")
            .args(["service", "status"])
            .output()
            .map_err(|e| anyhow::anyhow!("cannot run `opencode service status`: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "opencode service status failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        url = Some(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let url = url.expect("set above");
    if !url.starts_with("http://") && !url.starts_with("https://") {
        anyhow::bail!("opencode service URL is not http(s): {url:?}");
    }
    Ok(ServiceConfig {
        url: url.trim_end_matches('/').to_string(),
        password,
    })
}

/// Discover the opencode service config (URL + optional password). Short
/// per-process cache: service status is a subprocess, so a burst of wakes must
/// not re-spawn it each time. Env override wins; never a hardcoded port.
fn discover_service() -> anyhow::Result<ServiceConfig> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, ServiceConfig)>> = Mutex::new(None);
    const TTL: Duration = Duration::from_secs(30);
    if let Some(cached) = CACHE.lock().expect("cache lock").as_ref()
        && cached.0.elapsed() < TTL
    {
        return Ok(ServiceConfig {
            url: cached.1.url.clone(),
            password: cached.1.password.clone(),
        });
    }
    let found = discover_uncached()?;
    *CACHE.lock().expect("cache lock") = Some((
        Instant::now(),
        ServiceConfig {
            url: found.url.clone(),
            password: found.password.clone(),
        },
    ));
    Ok(found)
}

/// A typed failure the caller treats like any other API failure -> Inject.
#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "opencode api unauthorized (401)"),
            ApiError::Other(s) => write!(f, "opencode api failed: {s}"),
        }
    }
}
impl std::error::Error for ApiError {}

/// Real HTTP transport: POST /api/session/{session}/prompt with the v2 body.
pub struct HttpApi {
    cfg: ServiceConfig,
}

impl HttpApi {
    pub fn discover() -> anyhow::Result<Self> {
        Ok(Self {
            cfg: discover_service()?,
        })
    }
}

impl HarnessApi for HttpApi {
    fn prompt(
        &self,
        session: &str,
        text: &str,
        delivery: Delivery,
        msg_id: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{}/api/session/{}/prompt", self.cfg.url, session);
        let body = serde_json::json!({
            "text": text,
            "id": msg_id,
            "delivery": delivery.as_str(),
        });
        let mut req = ureq::post(&url).timeout(std::time::Duration::from_secs(5));
        if let Some(pw) = &self.cfg.password {
            req = req.set("Authorization", &format!("Basic {pw}"));
        }
        req.send_json(body).map_err(|e| match e {
            ureq::Error::Status(401, _) => anyhow::Error::new(ApiError::Unauthorized),
            other => anyhow::Error::new(ApiError::Other(other.to_string())),
        })?;
        Ok(())
    }
}

/// Dispatch a wake whose route is already decided, using the real transport
/// (HTTP) for `Api` and the supplied pane closure for `Inject`. This is the
/// single seam every delivery site calls (bd-f888z). `seed` derives the
/// idempotent msg id from the mail/watermark; `pin` scopes it.
pub fn dispatch(
    route: &crate::harness_caps::WakeRoute,
    text: &str,
    seed: &str,
    pin: &str,
    urgent: bool,
    inject: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<RouteTaken> {
    if let crate::harness_caps::WakeRoute::Api { .. } = route {
        crate::state::journal_wake_route(pin, seed, "intent", route_tag(route));
        let id = msg_id(seed, pin);
        let (api, discovery_ok): (Box<dyn HarnessApi>, bool) = match HttpApi::discover() {
            Ok(a) => (Box::new(a), true),
            Err(_) => (Box::new(api_unavailable()), false),
        };
        let taken = deliver(&*api, route, text, &id, urgent, inject)?;
        crate::state::journal_wake_route(
            pin,
            seed,
            "outcome",
            route_taken_tag(taken, discovery_ok),
        );
        return Ok(taken);
    }
    let taken = deliver(
        &api_unavailable(),
        route,
        text,
        &msg_id(seed, pin),
        urgent,
        inject,
    )?;
    crate::state::journal_wake_route(pin, seed, "outcome", route_taken_tag(taken, true));
    Ok(taken)
}

fn route_taken_tag(taken: RouteTaken, discovery_ok: bool) -> &'static str {
    match taken {
        RouteTaken::Api => "api",
        RouteTaken::Inject if !discovery_ok => "inject (service down)",
        RouteTaken::Inject => "inject (api failed)",
        RouteTaken::NoDoor => "no-door",
    }
}

/// A transport that always fails — used when the API route is not selected, so
/// `deliver` takes only the Inject/NoDoor branches.
fn api_unavailable() -> impl HarnessApi {
    struct Down;
    impl HarnessApi for Down {
        fn prompt(&self, _s: &str, _t: &str, _d: Delivery, _m: &str) -> anyhow::Result<()> {
            anyhow::bail!("no api route")
        }
    }
    Down
}

fn route_tag(route: &crate::harness_caps::WakeRoute) -> &'static str {
    match route {
        crate::harness_caps::WakeRoute::Api { .. } => "api",
        crate::harness_caps::WakeRoute::Inject { .. } => "inject",
        crate::harness_caps::WakeRoute::NoDoor(_) => "no-door",
    }
}

/// What actually carried a wake — journaled BEFORE the provider runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteTaken {
    Api,
    Inject,
    NoDoor,
}

/// The one delivery seam (bd-f888z). `Api` routes through the transport; any
/// transport failure falls back to the Inject closure, so a down service never
/// loses a wake. The fallback closure is the ONLY thing that types into a pane.
/// Returns which route ran, for the caller to journal.
pub fn deliver(
    api: &dyn HarnessApi,
    route: &crate::harness_caps::WakeRoute,
    text: &str,
    id: &str,
    urgent: bool,
    inject: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<RouteTaken> {
    match route {
        crate::harness_caps::WakeRoute::Api { session } => {
            let delivery = if urgent {
                Delivery::Steer
            } else {
                Delivery::Queue
            };
            match api.prompt(session, text, delivery, id) {
                Ok(()) => Ok(RouteTaken::Api),
                Err(_) => {
                    inject()?;
                    Ok(RouteTaken::Inject)
                }
            }
        }
        crate::harness_caps::WakeRoute::Inject { .. } => {
            inject()?;
            Ok(RouteTaken::Inject)
        }
        crate::harness_caps::WakeRoute::NoDoor(_) => Ok(RouteTaken::NoDoor),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_id_is_stable_and_idempotent() {
        let a = msg_id("wm-7b0", "BoldCliff");
        assert_eq!(a, msg_id("wm-7b0", "BoldCliff"));
        assert_ne!(a, msg_id("wm-7b0", "ScarletCove"));
        assert_ne!(a, msg_id("wm-7b1", "BoldCliff"));
        assert!(a.starts_with("msg_"), "{a}");
    }

    #[test]
    fn delivery_strings_match_the_v2_api() {
        assert_eq!(Delivery::Queue.as_str(), "queue");
        assert_eq!(Delivery::Steer.as_str(), "steer");
    }

    #[test]
    fn api_route_delivers_without_typing_and_picks_delivery() {
        use crate::harness_caps::WakeRoute;
        let api = FakeApi::default();
        let route = WakeRoute::Api {
            session: "ses-abc".into(),
        };
        let mut typed = false;
        let taken = deliver(&api, &route, "wake", "msg_1", true, || {
            typed = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(taken, RouteTaken::Api);
        assert!(!typed, "an API wake must never type into the pane");
        assert_eq!(api.calls().len(), 1);
        assert_eq!(api.calls()[0].delivery, Delivery::Steer);
    }

    #[test]
    fn api_failure_falls_back_to_inject() {
        use crate::harness_caps::WakeRoute;
        let api = FakeApi::default();
        api.fail_with();
        let route = WakeRoute::Api {
            session: "ses-abc".into(),
        };
        let mut typed = false;
        let taken = deliver(&api, &route, "wake", "msg_1", false, || {
            typed = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(taken, RouteTaken::Inject);
        assert!(typed, "a down service must fall back to typing");
    }

    #[test]
    fn inject_route_only_types() {
        use crate::harness_caps::WakeRoute;
        let api = FakeApi::default();
        let route = WakeRoute::Inject { interrupt: true };
        let mut typed = false;
        let taken = deliver(&api, &route, "wake", "msg_1", false, || {
            typed = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(taken, RouteTaken::Inject);
        assert!(typed);
        assert!(api.calls().is_empty(), "Inject must not touch the API");
    }

    #[test]
    fn fake_api_records_a_prompt() {
        let api = FakeApi::default();
        api.prompt("ses-1", "wake", Delivery::Queue, "msg_1")
            .unwrap();
        assert_eq!(
            api.calls(),
            vec![PromptCall {
                session: "ses-1".into(),
                text: "wake".into(),
                delivery: Delivery::Queue,
                msg_id: "msg_1".into(),
            }]
        );
    }
}
