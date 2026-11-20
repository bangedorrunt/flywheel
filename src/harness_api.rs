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

/// Real transport shells out to `opencode api`, which resolves the running
/// service AND its password itself (the service.json password is never read or
/// logged by flywheel). A non-zero exit is a typed failure -> Inject fallback.
pub struct CliApi;

impl HarnessApi for CliApi {
    fn prompt(
        &self,
        session: &str,
        text: &str,
        delivery: Delivery,
        msg_id: &str,
    ) -> anyhow::Result<()> {
        // bd-64qoy: antigravity's door is `agy agentapi send-message
        // <conversation> <text>`; `session` carries `agy:<conversation-id>`
        // captured at spawn. The CLI/env resolve the service; the id is for
        // the caller's journal, never logged here.
        if let Some(conversation) = session.strip_prefix("agy:") {
            if conversation.is_empty() {
                anyhow::bail!("antigravity api route has an empty conversation id");
            }
            let bin =
                std::env::var("ANTIGRAVITY_AGENTAPI_EXE").unwrap_or_else(|_| "agy".to_string());
            let out = std::process::Command::new(bin)
                .args(["agentapi", "send-message", conversation, text])
                .output()
                .map_err(|e| anyhow::anyhow!("cannot run `agy agentapi send-message`: {e}"))?;
            if !out.status.success() {
                anyhow::bail!(
                    "agy agentapi send-message failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            return Ok(());
        }
        let body = serde_json::json!({
            "text": text,
            "id": msg_id,
            "delivery": delivery.as_str(),
        });
        let mut cmd = std::process::Command::new("opencode");
        // FLYWHEEL_OPENCODE_API_URL maps to the CLI's --server, never a
        // hardcoded port. No password is read or logged.
        if let Ok(server) = std::env::var("FLYWHEEL_OPENCODE_API_URL")
            && !server.trim().is_empty()
        {
            cmd.args(["--server", server.trim()]);
        }
        let out = cmd
            .args([
                "api",
                "POST",
                &format!("/api/session/{session}/prompt"),
                "--data",
                &body.to_string(),
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("cannot run `opencode api`: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "opencode api prompt failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
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
        let taken = deliver(&CliApi, route, text, &id, urgent, inject)?;
        crate::state::journal_wake_route(pin, seed, "outcome", route_taken_tag(taken));
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
    crate::state::journal_wake_route(pin, seed, "outcome", route_taken_tag(taken));
    Ok(taken)
}

fn route_taken_tag(taken: RouteTaken) -> &'static str {
    match taken {
        RouteTaken::Api => "api",
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
    use crate::harness_caps::WakeRoute;

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
        let api = FakeApi::default();
        let route = crate::harness_caps::WakeRoute::Api {
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
        let api = FakeApi::default();
        api.fail_with();
        let route = crate::harness_caps::WakeRoute::Api {
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

    /// bd-64qoy: an antigravity agentapi failure falls back to Inject and the
    /// API is still recorded as attempted; the wake is never lost.
    #[test]
    fn an_agy_api_failure_falls_back_to_inject() {
        let route = crate::harness_caps::WakeRoute::Api {
            session: "agy:conv-7".into(),
        };
        let api = FakeApi::default();
        api.fail_with();
        let mut typed = false;
        let taken = deliver(&api, &route, "wake", "msg_7", false, || {
            typed = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(taken, RouteTaken::Inject);
        assert!(typed, "a down agentapi must fall back to a typed wake");
        assert!(api.calls().is_empty(), "the failed call is not a delivery");
    }

    /// bd-64qoy: the chosen route is journaled BEFORE the transport runs, and
    /// the outcome after, with the same (seed, pin) idempotency key.
    #[test]
    fn dispatch_journals_intent_before_the_provider_and_outcome_after() {
        let _home = crate::config::test_state_home();
        let route = crate::harness_caps::WakeRoute::Api {
            session: "agy:conv-9".into(),
        };
        // A real transport that cannot fail: /bin/true stands in for `agy`.
        unsafe { std::env::set_var("ANTIGRAVITY_AGENTAPI_EXE", "/usr/bin/true") };
        let taken = dispatch(&route, "wake", "wm-9", "AmberProbe", false, || Ok(())).unwrap();
        assert_eq!(taken, RouteTaken::Api);
        let rows = std::fs::read_to_string(crate::config::state_dir().join("wake-routes.jsonl"))
            .unwrap_or_default();
        let phases: Vec<String> = rows
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["seed"] == "wm-9" && v["pin"] == "AmberProbe")
            .filter_map(|v| v["phase"].as_str().map(str::to_string))
            .collect();
        assert_eq!(
            phases,
            vec!["intent".to_string(), "outcome".to_string()],
            "{rows}"
        );
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
