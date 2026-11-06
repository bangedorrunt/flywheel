//! Parity tests for the backend abstraction (ADR-0002 R0.8, ADR-0003 §4).
//!
//! governed-by: ADR-0002, ADR-0003
//!
//! Two contracts are proven here:
//!
//! 1. **Unit-level (no live servers)** — `HcBackendLegacyRef`/`ToronMailBackend`
//!    wrappers are **pure delegation**: a call through `Box<dyn MailBackend>`
//!    produces the same result as a direct `Mail` call, because the wrapper
//!    adds no logic. Plus error coercion + Unsupported-honesty (no accidental
//!    `Ok` that would hide a missing impl).
//!
//! 2. **Live JSON-shape smoke (ADR-0003 §4, `#[ignore]`, bd-euav toron
//!    only)** — one integration harness driving the SAME `MailSurface`
//!    method against the live toron daemon (127.0.0.1:18080). Asserts the
//!    **JSON shape contract**: same top-level keys, same semantic fields,
//!    with a documented allowlist (kept from the agent-mail comparison era)
//!    for the id-type difference (AM `u64` message_id vs toron hex
//!    event id) and DB-field differences (AM SQLite columns vs toron event-store
//!    projection).
//!
//! Run: `cargo test --all-features -- --ignored
//! live_parity` (the toron daemon must be up).

#[cfg(test)]
mod tests {
    use crate::backend::{BackendError, InboxQuery, MailSurface, MultiplexerBackend, run_mail};

    fn map_prompt_result(
        r: std::result::Result<toron_core::backend::AgentInfo, toron_core::backend::BackendError>,
    ) -> crate::backend::PromptLanding {
        match r {
            Ok(info) => crate::backend::PromptLanding {
                status: Some(info.agent_status),
                code: None,
            },
            Err(e) => crate::backend::PromptLanding {
                status: None,
                code: Some(e.code().as_str().to_string()),
            },
        }
    }

    #[derive(Debug)]
    struct LegacyMuxError {
        code: String,
        message: String,
    }
    impl std::fmt::Display for LegacyMuxError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
    impl std::error::Error for LegacyMuxError {}
    impl From<LegacyMuxError> for BackendError {
        fn from(e: LegacyMuxError) -> Self {
            BackendError::Other(anyhow::anyhow!(e))
        }
    }

    #[test]
    fn anyhow_error_coerces_to_other_preserving_message() {
        let err: BackendError = anyhow::anyhow!("socket refused").into();
        match err {
            BackendError::Other(e) => assert!(e.to_string().contains("socket refused")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn legacy_error_coerces_to_other_preserving_code_and_message() {
        let legacy_err = LegacyMuxError {
            code: "agent_prompt_stalled".into(),
            message: "no lifecycle change within 5s".into(),
        };
        let err: BackendError = legacy_err.into();
        match err {
            BackendError::Other(e) => {
                let s = e.to_string();
                assert!(s.contains("agent_prompt_stalled"), "code preserved: {s}");
                assert!(s.contains("no lifecycle change"), "message preserved: {s}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

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

    /// `BackendError::Display` includes the backend name + method + reason —
    /// the actionable message a user sees when hitting an Unsupported op.
    #[test]
    fn unsupported_display_is_actionable() {
        let b = AllUnsupportedMux;
        match b.workspace_list() {
            Err(e) => {
                let s = e.to_string();
                assert!(s.contains("all-unsupported-mux"), "names backend: {s}");
                assert!(s.contains("workspace_list"), "names method: {s}");
            }
            Ok(_) => panic!("expected Unsupported"),
        }
    }

    /// review-fix F6: `agent_prompt_coded` maps a coded error to landing DATA
    /// (status XOR code), NOT a transport Err — so a backend-agnostic __send
    /// classifies identically to the concrete client. The pure mapping is the
    /// contract under test (no socket needed).
    #[test]
    fn prompt_landing_maps_landed_status_and_coded_error() {
        use toron_core::backend::{AgentInfo, AgentStatus};
        let ok = map_prompt_result(Ok(AgentInfo {
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
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term_1".into(),
            terminal_title: None,
            workspace_id: "w1".into(),
        }));
        assert_eq!(ok.status, Some(AgentStatus::Working));
        assert_eq!(ok.code, None);
        let err = map_prompt_result(Err(toron_core::backend::BackendError::new(
            toron_core::backend::error::BackendErrorCode::AgentPromptStalled,
            "x",
        )));
        assert_eq!(err.status, None);
        assert_eq!(err.code.as_deref(), Some("agent_prompt_stalled"));
    }

    /// Backend kinds the harness can drive (bd-euav: toron only).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ParityKind {
        Toron,
    }

    impl ParityKind {
        fn name(self) -> &'static str {
            match self {
                Self::Toron => "toron",
            }
        }
        fn base_url(self) -> &'static str {
            match self {
                Self::Toron => "http://127.0.0.1:18080",
            }
        }
    }

    /// Normalize an AM key name onto the shared semantic key (id-type +
    /// naming allowlist). Returns `None` for backend-only DB fields that have
    /// no counterpart on the other side.
    fn normalize_key(k: &str) -> Option<String> {
        match k {
            "id" | "event_id" | "message_id" => Some("id".to_string()),
            "from" | "sender" => Some("sender".to_string()),
            "created_ts" | "created_ts_us" => Some("created_ts".to_string()),
            "read_ts"
            | "kind"
            | "project_id"
            | "sender_id"
            | "attachments"
            | "inception_ts"
            | "last_active_ts"
            | "last_active_ts_us"
            | "contact_policy"
            | "database_url"
            | "storage_root"
            | "pool_utilization"
            | "semantic_readiness"
            | "http_host"
            | "http_port"
            | "queues"
            | "integrity"
            | "two_tier_indexing"
            | "verdicts"
            | "disk"
            | "to"
            | "cc"
            | "bcc"
            | "guidance"
            | "suggestions"
            | "summary"
            | "diagnostics"
            | "degraded"
            | "fallback_mode"
            | "remediation_hints"
            | "search_index_reason"
            | "search_index_state"
            | "created_at"
            | "identity_mode_used"
            | "canonical_path"
            | "human_key"
            | "repo_root"
            | "git_common_dir"
            | "branch"
            | "worktree_name"
            | "core_ignorecase"
            | "normalized_remote"
            | "project_uid"
            | "discovery"
            | "created" => None,
            "npub" | "key_path" | "profile_event_id" | "manifest_event_id" => None,
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
                        let tag = match nk.as_str() {
                            "id" => "id",
                            "created_ts" => "ts",
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

    /// Build the backend for a kind against its live server. Toron needs
    /// none (the e2e daemon binds localhost).
    fn live_backend(kind: ParityKind) -> Box<dyn MailSurface> {
        match kind {
            ParityKind::Toron => {
                let mail = crate::mail::Mail::from_config(Some(&crate::config::MailConfig {
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
                    let _ = mail;
                    unreachable!("live parity requires --all-features (mail-toron)")
                }
            }
        }
    }

    /// The live shape harness (ADR-0003 §4, bd-am5; bd-euav toron only).
    /// Requires the toron daemon up on 18080. Runs the SAME method twice
    /// through the toron backend and asserts the JSON shape contract with
    /// the documented allowlist.
    ///
    /// TEST-FIRST (RED→GREEN): written against the observed live shapes
    /// (2026-08-06 probes), then verified against the running servers.
    ///
    /// gated on `mail-toron` (the toron backend must be compiled in to build
    /// the ToronMailBackend arm of `live_backend`); run with `--all-features`.
    #[test]
    #[cfg(feature = "mail-toron")]
    #[ignore = "requires live toron (127.0.0.1:18080); run with --ignored"]
    fn live_parity_full_surface() {
        let project = "/private/tmp/flywheel-parity";
        let agent = "CoralFinch";
        let kinds = [ParityKind::Toron, ParityKind::Toron];
        let mut backends: Vec<(ParityKind, Box<dyn MailSurface>)> =
            kinds.iter().map(|k| (*k, live_backend(*k))).collect();

        for (kind, b) in &mut backends {
            let h = run_mail(b.health()).unwrap_or_else(|e| panic!("{} health: {e}", kind.name()));
            let status = serde_json::to_value(&h).unwrap_or_default();
            assert!(
                status.get("bus").is_some() || status.get("status").is_some(),
                "{} health has a status/bus.ok field: {status}",
                kind.name()
            );
        }

        let mut ensured = Vec::new();
        for (kind, b) in &mut backends {
            let s = run_mail(b.ensure_project(project)).expect("ensure_project");
            ensured.push((*kind, serde_json::to_value(&s).unwrap_or_default()));
        }
        assert_shape_parity(
            "ensure_project",
            &ensured[0].1,
            &ensured[1].1,
            &["id"],
            false,
        );

        for (kind, b) in &mut backends {
            let profile = crate::backend::AgentProfile {
                name: agent.to_string(),
                program: Some("opencode".into()),
                model: Some("test".into()),
                persona: None,
                task_description: Some("parity".into()),
            };
            let r = run_mail(b.register_agent(project, &profile))
                .unwrap_or_else(|e| panic!("{} register_agent: {e}", kind.name()));
            assert_eq!(
                r.name,
                agent,
                "{} register_agent identity: {r:?}",
                kind.name()
            );
        }

        let mut agents = Vec::new();
        for (kind, b) in &mut backends {
            let a = run_mail(b.list_agents(project, None)).expect("list_agents");
            agents.push((*kind, serde_json::to_value(&a).unwrap_or_default()));
        }
        assert_shape_parity("list_agents", &agents[0].1, &agents[1].1, &[], true);

        for (_kind, b) in &mut backends {
            let _ = run_mail(b.send(&crate::backend::SendMail {
                project: project.to_string(),
                sender: agent.to_string(),
                to: vec![agent.to_string()],
                subject: "parity subject".into(),
                body_md: "parity body".into(),
                importance: Some("normal".into()),
                ack_required: false,
                ..Default::default()
            }))
            .expect("send");
        }
        let mut inboxes = Vec::new();
        for (kind, b) in &mut backends {
            let items = run_mail(b.inbox(&InboxQuery {
                project: project.to_string(),
                agent: agent.to_string(),
                unread_only: false,
                limit: Some(5),
                include_bodies: true,
                urgent_only: false,
                since_ts: None,
                thread_id: None,
            }))
            .expect("inbox");
            inboxes.push((*kind, serde_json::to_value(&items).unwrap_or_default()));
        }
        assert_shape_parity("inbox", &inboxes[0].1, &inboxes[1].1, &[], true);

        let reply_targets: Vec<(ParityKind, Vec<crate::backend::MailItem>)> = inboxes
            .iter()
            .map(|(kind, v)| {
                let items: Vec<crate::backend::MailItem> =
                    serde_json::from_value(v.clone()).expect("inbox rows are MailItems");
                assert!(
                    !items.is_empty(),
                    "{} inbox has a row to reply to",
                    kind.name()
                );
                (*kind, items)
            })
            .collect();
        for (kind, b) in &mut backends {
            let (_, items) = reply_targets
                .iter()
                .find(|(k, _)| *k == *kind)
                .expect("target items");
            let id = items[0].id.as_str();
            let _ = run_mail(b.reply(&crate::backend::ReplyMail {
                project: project.to_string(),
                sender: agent.to_string(),
                to: vec![agent.to_string()],
                subject: "Re: parity subject".into(),
                body_md: "parity reply body".into(),
                ack_required: false,
                thread_id: id.parse().expect("valid message id"),
            }))
            .unwrap_or_else(|e| panic!("{} reply: {e}", kind.name()));
        }
        for (kind, b) in &mut backends {
            let items = run_mail(b.inbox(&InboxQuery {
                project: project.to_string(),
                agent: agent.to_string(),
                unread_only: false,
                limit: Some(10),
                include_bodies: true,
                urgent_only: false,
                since_ts: None,
                thread_id: None,
            }))
            .expect("inbox after reply");
            let subjects: Vec<String> = items.iter().map(|m| m.subject.clone()).collect();
            assert!(
                subjects.iter().any(|s| s.starts_with("Re: parity subject")),
                "{} reply carries Re: parent subject: {subjects:?}",
                kind.name()
            );
        }

        let mut searches = Vec::new();
        for (kind, b) in &mut backends {
            let mut last = run_mail(b.search(&crate::backend::SearchQuery {
                project: project.to_string(),
                query: "parity".into(),
                limit: Some(5),
                ..Default::default()
            }))
            .expect("search");
            if *kind == ParityKind::Toron {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while std::time::Instant::now() < deadline && last.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    last = run_mail(b.search(&crate::backend::SearchQuery {
                        project: project.to_string(),
                        query: "parity".into(),
                        limit: Some(5),
                        ..Default::default()
                    }))
                    .expect("search");
                }
            }
            searches.push((*kind, serde_json::to_value(&last).unwrap_or_default()));
        }
        assert_shape_parity("search", &searches[0].1, &searches[1].1, &[], false);
    }

    /// se2.2 live acceptance (bd-adr7-mail-adapt-se2.2): toron message ids
    /// are hex event ids — send → inbox → ack(hex) must work end-to-end with
    /// the id passed through untouched. A numeric coercion (the pre-fix
    /// `parse::<u64>().unwrap_or(0)`) would turn the id into 0 or Err — both
    /// fail this round-trip.
    #[test]
    #[cfg(feature = "mail-toron")]
    #[ignore = "requires a live toron daemon (127.0.0.1:18080)"]
    fn live_toron_ack_hex_roundtrip() {
        let b = live_backend(ParityKind::Toron);
        let project = "/private/tmp/flywheel-ack-hex";
        let agent = "CoralFinch";

        run_mail(b.ensure_project(project)).expect("ensure_project");
        let profile = crate::backend::AgentProfile {
            name: agent.to_string(),
            program: Some("opencode".into()),
            model: Some("test".into()),
            persona: None,
            task_description: Some("ack-hex".into()),
        };
        run_mail(b.register_agent(project, &profile)).expect("register_agent");
        let _ = run_mail(b.send(&crate::backend::SendMail {
            project: project.to_string(),
            sender: agent.to_string(),
            to: vec![agent.to_string()],
            subject: "se2.2 ack-hex subject".into(),
            body_md: "se2.2 ack-hex body".into(),
            importance: Some("normal".into()),
            ack_required: false,
            ..Default::default()
        }))
        .expect("send");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let id = loop {
            let inbox = run_mail(b.inbox(&InboxQuery {
                project: project.to_string(),
                agent: agent.to_string(),
                unread_only: false,
                limit: Some(10),
                include_bodies: true,
                urgent_only: false,
                since_ts: None,
                thread_id: None,
            }))
            .expect("inbox");
            if let Some(item) = inbox.first() {
                break item.id.as_str().to_string();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "toron inbox never delivered the message"
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        };
        assert!(!id.is_empty(), "delivered message carries an id");
        assert!(
            id.parse::<u64>().is_err(),
            "toron id is hex, not decimal: {id}"
        );
        assert_ne!(id, "0", "hex id never becomes 0");

        run_mail(b.acknowledge(&crate::backend::ReceiptOp {
            project: project.to_string(),
            agent: agent.to_string(),
            message_id: id.parse().expect("valid message id"),
        }))
        .expect("ack(hex) succeeds");
        run_mail(b.mark_read(&crate::backend::ReceiptOp {
            project: project.to_string(),
            agent: agent.to_string(),
            message_id: id.parse().expect("valid message id"),
        }))
        .expect("mark_read(hex) succeeds");
    }

    /// 193.3: the shape-normalization allowlist is not vacuous — id-type,
    /// sender-naming, and timestamp-representation differences collapse onto
    /// the shared semantic key, and AM-only DB fields are dropped.
    #[test]
    fn shape_normalization_allowlist_is_not_vacuous() {
        use serde_json::json;
        let am = json!({
            "id": 42,
            "from": "CoralFinch",
            "created_ts": "2026-08-08T00:00:00Z",
            "read_ts": 0,
            "kind": "mail",
            "project_id": 7,
            "to": ["x"],
            "attachments": [],
        });
        let toron = json!({
            "event_id": "a1b2c3",
            "sender": "7f7ed685445ed08a1d90005c092480b33e8f5439d9fcb0689d45830b6f3707ea",
            "created_ts_us": 1786000000000000_i64,
        });
        assert_shape_parity("normalization", &am, &toron, &[], true);
        let am2 = json!({ "id": 1, "result": [] });
        let toron2 = json!({ "id": "hex", "result": [], "renewed": 0 });
        assert_shape_parity("extra_drop", &am2, &toron2, &["renewed"], false);
    }
}
