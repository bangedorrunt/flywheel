//! Parity tests for the backend abstraction (ADR-0002 R0.8).
//!
//! Contract being proven: the trait wrappers (`HerdrBackend`, `AgentMailMcpBackend`)
//! are **pure delegation** — a call through `Box<dyn MultiplexerBackend>` produces
//! the same result as a direct `Client` call, because the wrapper adds no logic.
//!
//! What's testable at unit level (no live socket/HTTP):
//! - **Error coercion**: `anyhow::Error -> BackendError::Other` and
//!   `HerdrError -> BackendError::Other` both preserve the message (the path
//!   real transport errors take).
//! - **Unsupported honesty**: trait methods that should be `Unsupported` ARE
//!   (no accidental `Ok` that would hide a missing impl).
//! - **Delegation shape (code inspection)**: the wrapper fns in
//!   `src/backend/{herdr,agentmail}.rs` contain only delegation + error
//!   coercion — no logic that could diverge from the direct call. This is the
//!   structural guarantee of byte-identical results.
//!
//! What's deferred (integration): live socket/HTTP round-trips proving
//! `Client::agent_list()` and `HerdrBackend::agent_list()` return identical
//! JSON against a real herdr session. Needs a test herdr session + test
//! agent-mail server; stubbed as `#[ignore]` below for when that infra lands.
//!
//! Feature-combo matrix (default / --all-features / --no-default-features):
//! deferred to bd-cargo-feature-flags (R0.5) which owns the backend cargo
//! features. Today only `serve` is a feature; backend features don't exist yet.

#[cfg(test)]
mod tests {
    use crate::backend::{BackendError, MailBackend, MultiplexerBackend};
    use crate::herdr::HerdrError;

    // ---- error coercion: the path real transport errors take ----

    #[test]
    fn anyhow_error_coerces_to_other_preserving_message() {
        let err: BackendError = anyhow::anyhow!("socket refused").into();
        match err {
            BackendError::Other(e) => assert!(e.to_string().contains("socket refused")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn herdr_error_coerces_to_other_preserving_code_and_message() {
        let herdr_err = HerdrError {
            code: "agent_prompt_stalled".into(),
            message: "no lifecycle change within 5s".into(),
        };
        let err: BackendError = herdr_err.into();
        match err {
            BackendError::Other(e) => {
                let s = e.to_string();
                assert!(s.contains("agent_prompt_stalled"), "code preserved: {s}");
                assert!(s.contains("no lifecycle change"), "message preserved: {s}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    // ---- Unsupported honesty: defaults don't accidentally Ok ----

    /// A backend that overrides only `name()` inherits `Unsupported` for every
    /// operation. This is the open-to-extension contract (ADR-0002 §6): a new
    /// backend starts as all-`Unsupported` and graduates methods one at a time.
    /// If a default ever returned `Ok`, a stub backend would silently claim
    /// capability it doesn't have.
    struct AllUnsupportedMux;
    impl MultiplexerBackend for AllUnsupportedMux {
        fn name(&self) -> &'static str {
            "all-unsupported-mux"
        }
    }

    struct AllUnsupportedMail;
    impl MailBackend for AllUnsupportedMail {
        fn name(&self) -> &'static str {
            "all-unsupported-mail"
        }
    }

    #[test]
    fn every_mux_default_method_is_unsupported_not_ok() {
        let mut b = AllUnsupportedMux;
        // spot-check across the method categories (workspace/pane/agent/events)
        assert!(matches!(
            b.workspace_list(),
            Err(BackendError::Unsupported { .. })
        ));
        assert!(matches!(
            b.pane_split("p", "right", None),
            Err(BackendError::Unsupported { .. })
        ));
        assert!(matches!(
            b.agent_list(),
            Err(BackendError::Unsupported { .. })
        ));
        assert!(matches!(
            b.subscribe(&[]),
            Err(BackendError::Unsupported { .. })
        ));
        // name() is the one non-default — it must succeed
        assert_eq!(b.name(), "all-unsupported-mux");
    }

    #[test]
    fn every_mail_default_method_is_unsupported_not_ok() {
        let mut b = AllUnsupportedMail;
        assert!(matches!(b.health(), Err(BackendError::Unsupported { .. })));
        assert!(matches!(
            b.ensure_project("p"),
            Err(BackendError::Unsupported { .. })
        ));
        assert!(matches!(
            b.send("p", &[], "s", "b", "normal", false, None),
            Err(BackendError::Unsupported { .. })
        ));
        assert!(matches!(
            b.reserve("p", "a", &[], 60, true, "r"),
            Err(BackendError::Unsupported { .. })
        ));
        assert_eq!(b.name(), "all-unsupported-mail");
    }

    /// `BackendError::Display` includes the backend name + method + reason —
    /// the actionable message a user sees when hitting an Unsupported op.
    #[test]
    fn unsupported_display_is_actionable() {
        let mut b = AllUnsupportedMux;
        match b.workspace_list() {
            Err(e) => {
                let s = e.to_string();
                assert!(s.contains("all-unsupported-mux"), "names backend: {s}");
                assert!(s.contains("workspace_list"), "names method: {s}");
            }
            Ok(_) => panic!("expected Unsupported"),
        }
    }

    // ---- delegation shape (structural parity guarantee) ----
    //
    // The wrappers in src/backend/{herdr,agentmail}.rs are PURE DELEGATION:
    // each method body is a single forward to the underlying Client/Mail
    // method (+ error coercion via `?` or `.map_err(BackendError::from)`).
    // There is no logic that could diverge from a direct call. This is
    // verified by code review (the fn bodies are one line each); the tests
    // above prove the error path; live round-trips (below, #[ignore]) prove
    // the happy path against a real socket/HTTP once infra exists.

    /// Integration parity: `HerdrBackend::agent_list` returns the same JSON as
    /// `Client::agent_list` against a live herdr session. Needs a test session.
    #[test]
    #[ignore = "needs a test herdr session (herdr --session test-parity); run with --ignored once infra lands"]
    fn herdr_agent_list_matches_direct_call() {
        // TODO: spin up `herdr --session test-parity`, spawn a dummy agent,
        // call both Client::agent_list() and HerdrBackend::new(client).agent_list(),
        // assert byte-identical Vec<AgentInfo>.
    }

    /// Integration parity: `AgentMailMcpBackend::send` round-trips through the
    /// real agent-mail MCP identically to `Mail::send`. Needs a test project.
    #[test]
    #[ignore = "needs a test agent-mail project; run with --ignored once infra lands"]
    fn agentmail_send_matches_direct_call() {
        // TODO: stand up a test agent-mail project, send via both
        // Mail::send and AgentMailMcpBackend::new(mail).send(...), assert
        // identical response JSON.
    }
}
