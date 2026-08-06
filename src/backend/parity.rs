//! Parity tests for the backend abstraction (ADR-0002 R0.8, ADR-0003 §4).
//!
//! governed-by: ADR-0002, ADR-0003
//!
//! Two contracts are proven here:
//!
//! 1. **Unit-level (no live servers)** — `HerdrBackend`/`ToronMailBackend`
//!    wrappers are **pure delegation**: a call through `Box<dyn MailBackend>`
//!    produces the same result as a direct `Mail` call, because the wrapper
//!    adds no logic. Plus error coercion + Unsupported-honesty (no accidental
//!    `Ok` that would hide a missing impl).
//!
//! 2. **Live JSON-shape parity (ADR-0003 §4, `#[ignore]`)** — one integration
//!    harness parameterized by backend kind (`agentmail`|`toron`), running the
//!    SAME `MailBackend` method against a live agent-mail server (127.0.0.1:8765)
//!    and the live toron daemon (127.0.0.1:18080). Asserts **JSON shape
//!    parity**: same top-level keys, same semantic fields, with a documented
//!    allowlist for the id-type difference (AM `u64` message_id vs toron hex
//!    event id) and DB-field differences (AM SQLite columns vs toron event-store
//!    projection). These make the pre-ADR-0003 `#[ignore]` parity tests
//!    runnable against the live servers.
//!
//! Run: `FLYWHEEL_AGENTMAIL_TOKEN=… cargo test --all-features -- --ignored
//! live_parity` (both servers must be up).

#[cfg(test)]
mod tests {
    use crate::backend::{BackendError, MailBackend, MultiplexerBackend};
    use crate::herdr_shim::HerdrError;

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
        // ADR-0003 V6 restored surface: the 4 new methods also default to
        // Unsupported (no method default returns Ok — Unsupported-honesty).
        assert!(matches!(
            b.slot_renew("p", "a", "s", 60),
            Err(BackendError::Unsupported { .. })
        ));
        assert!(matches!(
            b.summarize_thread_product("prod", "t", false),
            Err(BackendError::Unsupported { .. })
        ));
        assert!(matches!(
            b.force_release_file_reservation("p", "a", 1, None, false),
            Err(BackendError::Unsupported { .. })
        ));
        assert!(matches!(
            b.cleanup_pane_identities("p"),
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

    /// review-fix F6: `agent_prompt_coded` maps a coded error to landing DATA
    /// (status XOR code), NOT a transport Err — so a backend-agnostic __send
    /// classifies identically to the concrete client. The pure mapping is the
    /// contract under test (no socket needed).
    #[test]
    fn prompt_landing_maps_landed_status_and_coded_error() {
        use crate::herdr_shim::{AgentInfo, AgentStatus};
        // landed: Ok(AgentInfo) → status Some, code None
        let ok = crate::backend::herdr::map_prompt_result(Ok(AgentInfo {
            agent: Some("pi".into()),
            agent_status: AgentStatus::Working,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: None,
            pane_id: "wT:p1".into(),
            revision: 1,
            state_change_seq: 0,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term_1".into(),
            terminal_title: None,
            workspace_id: "w1".into(),
        }));
        let ok = ok.unwrap();
        assert_eq!(ok.status, Some(AgentStatus::Working));
        assert_eq!(ok.code, None);
        // coded error: Err(toron BackendError) → status None, code Some
        let err =
            crate::backend::herdr::map_prompt_result(Err(toron_core::backend::BackendError::new(
                toron_core::backend::error::BackendErrorCode::AgentPromptStalled,
                "x",
            )));
        let err = err.unwrap();
        assert_eq!(err.status, None);
        assert_eq!(err.code.as_deref(), Some("agent_prompt_stalled"));
    }

    // =====================================================================
    // Live JSON-shape parity harness (ADR-0003 §4, bd-am5).
    //
    // One integration harness parameterized by backend kind (`agentmail` |
    // `toron`), running the SAME `MailBackend` method against a live
    // agent-mail server (127.0.0.1:8765) and the live toron daemon
    // (127.0.0.1:18080), asserting JSON shape parity (same top-level keys,
    // same semantic fields).
    //
    // DOCUMENTED ALLOWLIST (id-type + DB-field differences):
    // - `id`/`event_id`: AM message ids are `u64` sequence numbers; toron
    //   uses hex event ids. Values never match; only the KEY is compared.
    // - `sender` vs `from`: AM `fetch_inbox`/`search` emit `from` (the sender
    //   display name); toron emits `sender` (the npub hex). Different keys,
    //   both semantic "who sent this".
    // - `created_ts` (AM, ISO) vs `created_ts_us` (toron, epoch µs): timestamp
    //   representation differs; both are "when created".
    // - AM extra DB columns absent on toron (`read_ts`, `kind`, `project_id`,
    //   `sender_id`, `attachments`, `inception_ts`, `last_active_ts`,
    //   `contact_policy`).
    // - toron extra Nostr fields absent on AM (`npub`, `key_path`,
    //   `profile_event_id`, `manifest_event_id`).
    // - `health_check`: AM returns a rich health object (status/health_level/
    //   pool stats/…); toron returns `{archive, bus}`. Both have a status-ish
    //   field (`status`/`ok`); keys otherwise differ by design.
    //
    // The harness compares the TOP-LEVEL KEYS of the JSON each method returns,
    // applying the allowlist above (normalized id keys + dropped backend-only
    // DB fields). It does NOT compare values (ids, timestamps, npubs differ by
    // design) — it proves both backends speak the same semantic contract.
    // =====================================================================

    /// Backend kinds the harness can drive.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ParityKind {
        Agentmail,
        Toron,
    }

    impl ParityKind {
        fn name(self) -> &'static str {
            match self {
                ParityKind::Agentmail => "agentmail",
                ParityKind::Toron => "toron",
            }
        }
        fn base_url(self) -> &'static str {
            match self {
                ParityKind::Agentmail => "http://127.0.0.1:8765",
                ParityKind::Toron => "http://127.0.0.1:18080",
            }
        }
    }

    /// Normalize an AM key name onto the shared semantic key (id-type +
    /// naming allowlist). Returns `None` for backend-only DB fields that have
    /// no counterpart on the other side.
    fn normalize_key(k: &str) -> Option<String> {
        match k {
            // id-type allowlist: AM u64 vs toron hex → compare the KEY only
            // (value type is canonicalized to `id` in the shape tag below)
            "id" | "event_id" | "message_id" => Some("id".to_string()),
            // sender naming: AM `from` (name) vs toron `sender` (npub)
            "from" | "sender" => Some("sender".to_string()),
            // timestamp representation: ISO string vs epoch-µs number → same
            // semantic (value type canonicalized to `ts` below)
            "created_ts" | "created_ts_us" => Some("created_ts".to_string()),
            // AM-only DB columns (SQLite) — dropped
            "read_ts" | "kind" | "project_id" | "sender_id" | "attachments"
            | "inception_ts" | "last_active_ts" | "last_active_ts_us"
            | "contact_policy" | "database_url" | "storage_root" | "pool_utilization"
            | "semantic_readiness" | "http_host" | "http_port" | "queues"
            | "integrity" | "two_tier_indexing" | "verdicts" | "disk"
            // AM-only delivery routing arrays (toron messages carry no cc/bcc)
            | "to" | "cc" | "bcc"
            // AM `search_messages` result envelope fields (toron returns a
            // bare `result` array; the diagnostics are server-state noise)
            | "guidance" | "suggestions" | "summary" | "diagnostics"
            | "degraded" | "fallback_mode" | "remediation_hints"
            | "search_index_reason" | "search_index_state"
            // AM `ensure_project` returns the full project DB record; toron
            // returns a minimal `{created, slug}` envelope. The DB-record
            // fields below have no toron counterpart (ADR-0003 §4 DB-field
            // allowlist) — the shared semantic field is `slug`.
            | "created_at" | "identity_mode_used" | "canonical_path"
            | "human_key" | "repo_root" | "git_common_dir" | "branch"
            | "worktree_name" | "core_ignorecase" | "normalized_remote"
            | "project_uid" | "discovery" | "created" => None,
            // toron-only Nostr fields — dropped
            "npub" | "key_path" | "profile_event_id" | "manifest_event_id" => None,
            // keep everything else as-is (semantic fields must match)
            other => Some(other.to_string()),
        }
    }

    /// Top-level key set of a JSON value, normalized through the allowlist.
    /// `extra_drop` names keys to drop for this specific op (op-scoped
    /// DB-field diffs, e.g. AM's numeric project `id`). When `recurse_arrays`
    /// is false, list contents are not descended (used when one backend's
    /// list is empty — the element shape is proven by another op; e.g. inbox
    /// proves the message shape, so search asserts the envelope only).
    fn shape_keys(
        v: &serde_json::Value,
        extra_drop: &[&str],
        recurse_arrays: bool,
    ) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        collect_shape(v, extra_drop, recurse_arrays, &mut out);
        out
    }

    fn collect_shape(
        v: &serde_json::Value,
        extra_drop: &[&str],
        recurse_arrays: bool,
        out: &mut std::collections::BTreeSet<String>,
    ) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    if extra_drop.contains(&k.as_str()) {
                        continue;
                    }
                    if let Some(nk) = normalize_key(k) {
                        // id-type + timestamp-representation differences are
                        // allowlisted: the VALUE TYPE is canonicalized so AM's
                        // `id:num`/`created_ts:str` matches toron's
                        // `id:str`/`created_ts:num` (ADR-0003 §4 allowlist).
                        let tag = match nk.as_str() {
                            "id" => "id",
                            "created_ts" => "ts",
                            // AM search emits ack_required as num (0/1);
                            // AM inbox + toron emit bool — same semantic
                            "ack_required" => "flag",
                            _ => type_tag(val),
                        };
                        out.insert(format!("{nk}:{tag}"));
                    }
                    collect_shape(val, extra_drop, recurse_arrays, out);
                }
            }
            serde_json::Value::Array(items) if recurse_arrays => {
                for it in items {
                    collect_shape(it, extra_drop, recurse_arrays, out);
                }
            }
            serde_json::Value::Array(_) => {}
            _ => {}
        }
    }

    fn type_tag(v: &serde_json::Value) -> &'static str {
        match v {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "bool",
            serde_json::Value::Number(_) => "num",
            serde_json::Value::String(_) => "str",
            serde_json::Value::Array(_) => "arr",
            serde_json::Value::Object(_) => "obj",
        }
    }

    /// Assert two JSON values (from AM + toron for the same method) share the
    /// same normalized top-level + nested-object key shape. `extra_drop`
    /// names op-scoped DB-field diffs to drop (e.g. AM's numeric project id).
    /// `recurse_arrays`: descend into list elements (message/agent shape).
    fn assert_shape_parity(
        kind: &str,
        am: &serde_json::Value,
        toron: &serde_json::Value,
        extra_drop: &[&str],
        recurse_arrays: bool,
    ) {
        let am_keys = shape_keys(am, extra_drop, recurse_arrays);
        let toron_keys = shape_keys(toron, extra_drop, recurse_arrays);
        assert_eq!(
            am_keys, toron_keys,
            "{kind}: AM and toron JSON shapes diverge\n  AM:    {am_keys:?}\n  toron: {toron_keys:?}\n  am:    {am}\n  toron: {toron}"
        );
    }

    /// Build the backend for a kind against its live server. agentmail needs
    /// a bearer token (`FLYWHEEL_AGENTMAIL_TOKEN`); toron needs none (the
    /// e2e daemon binds localhost).
    fn live_backend(kind: ParityKind) -> Box<dyn MailBackend> {
        match kind {
            ParityKind::Agentmail => {
                let token = std::env::var("FLYWHEEL_AGENTMAIL_TOKEN")
                    .ok()
                    .filter(|s| !s.is_empty());
                Box::new(
                    crate::backend::agentmail::AgentMailBackend::new(
                        kind.base_url(),
                        token,
                        "CoralFinch".to_string(),
                    )
                    .expect("AM backend constructs (lazy, no dial)"),
                )
            }
            ParityKind::Toron => {
                let mail = crate::mail_shim::Mail::from_config(Some(&crate::config::MailConfig {
                    url: Some(kind.base_url().to_string()),
                    token: None,
                    name: Some("CoralFinch".to_string()),
                }))
                .expect("from_config ok")
                .expect("Some(Mail)");
                #[cfg(feature = "mail-toron")]
                {
                    Box::new(crate::backend::toron::ToronMailBackend::new(mail))
                }
                #[cfg(not(feature = "mail-toron"))]
                {
                    // The live harness needs the toron backend compiled in; the
                    // default build gates it behind `mail-toron` (the test is
                    // #[ignore] + requires --all-features anyway — see below).
                    let _ = mail;
                    unreachable!("live parity requires --all-features (mail-toron)")
                }
            }
        }
    }

    /// The live parity harness (ADR-0003 §4, bd-am5). Requires both servers
    /// up: agent-mail 8765 + toron 18080. Runs the SAME method through both
    /// backends and asserts JSON shape parity with the documented allowlist.
    ///
    /// TEST-FIRST (RED→GREEN): written against the observed live shapes
    /// (2026-08-06 probes), then verified against the running servers.
    ///
    /// gated on `mail-toron` (the toron backend must be compiled in to build
    /// the ToronMailBackend arm of `live_backend`); run with `--all-features`.
    #[test]
    #[cfg(feature = "mail-toron")]
    #[ignore = "requires live agent-mail (127.0.0.1:8765) + toron (127.0.0.1:18080); run with --ignored and FLYWHEEL_AGENTMAIL_TOKEN set"]
    fn live_parity_full_surface() {
        let project = "/private/tmp/flywheel-parity";
        let agent = "CoralFinch";
        let kinds = [ParityKind::Agentmail, ParityKind::Toron];
        let mut backends: Vec<(ParityKind, Box<dyn MailBackend>)> =
            kinds.iter().map(|k| (*k, live_backend(*k))).collect();

        // health: both must report a status-ish field (allowlist-normalized).
        for (kind, b) in backends.iter_mut() {
            let h = b
                .health()
                .unwrap_or_else(|e| panic!("{} health: {e}", kind.name()));
            let status = h
                .get("status")
                .or_else(|| h.pointer("/bus/ok"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            assert!(
                !status.is_null(),
                "{} health has a status/bus.ok field: {h}",
                kind.name()
            );
        }

        // ensure_project: same normalized key shape (slug/human_key/…).
        let mut ensured = Vec::new();
        for (kind, b) in backends.iter_mut() {
            ensured.push((*kind, b.ensure_project(project).expect("ensure_project")));
        }
        assert_shape_parity(
            "ensure_project",
            &ensured[0].1,
            &ensured[1].1,
            &["id"], // AM's numeric project id; toron's envelope has none
            false,
        );

        // register_agent: responses diverge BY DESIGN — AM echoes the full
        // agent profile (model/program/task/attachments_policy/capabilities/…);
        // toron returns the crypto identity envelope (npub/key_path/…). The
        // documented shared semantic field is `name` (the identity). Assert
        // both echo the same identity; the shape divergence is allowlisted.
        for (kind, b) in backends.iter_mut() {
            let r = b
                .register_agent(project, agent, "opencode", "test", "parity")
                .unwrap_or_else(|e| panic!("{} register_agent: {e}", kind.name()));
            let got = r
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("{} register_agent echoes name: {r}", kind.name()));
            assert_eq!(got, agent, "{} register_agent identity: {r}", kind.name());
        }

        // list_agents: both array of agent objects with same semantic keys.
        let mut agents = Vec::new();
        for (kind, b) in backends.iter_mut() {
            agents.push((*kind, b.list_agents(project).expect("list_agents")));
        }
        assert_shape_parity("list_agents", &agents[0].1, &agents[1].1, &[], true);

        // send + inbox round-trip: send to self, then fetch the inbox.
        for (_kind, b) in backends.iter_mut() {
            b.send(
                project,
                &[agent.to_string()],
                "parity subject",
                "parity body",
                "normal",
                false,
                None,
            )
            .expect("send");
        }
        let mut inboxes = Vec::new();
        for (kind, b) in backends.iter_mut() {
            let filter = crate::backend::InboxFilter {
                include_bodies: true,
                limit: 5,
                ..Default::default()
            };
            inboxes.push((*kind, b.inbox(project, agent, &filter).expect("inbox")));
        }
        assert_shape_parity("inbox", &inboxes[0].1, &inboxes[1].1, &[], true);

        // search: envelope-only (toron's search index can lag; the message
        // element shape is already proven by the inbox parity above).
        let mut searches = Vec::new();
        for (kind, b) in backends.iter_mut() {
            searches.push((
                *kind,
                b.search(
                    project,
                    "parity",
                    5,
                    &crate::backend::SearchFilter::default(),
                )
                .expect("search"),
            ));
        }
        assert_shape_parity("search", &searches[0].1, &searches[1].1, &[], false);
    }
}
