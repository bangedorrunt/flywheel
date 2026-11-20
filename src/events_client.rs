//! Real SEP-3415 toron events client (bd-si4b5 Phase B, ADR-0039 D7/D8).
//!
//! The pure kernel (`events_bridge`) owns every decision; this module only
//! moves bytes: `events/subscribe`, `events/stream` (SSE, resumed from the
//! persisted cursor / `Last-Event-ID`), and `events/poll` as the fallback when a
//! stream cannot be established. Every wire parse here is a pure function with
//! a recorded-SEP-fixture test, so no live daemon is needed to prove it.

use crate::events_bridge::{BridgeEvent, Page, decode_stream_frame};

/// The one HTTP seam. The real impl uses `ureq`; tests use a recorded fixture.
pub trait EventsRpc: Send {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        last_event_id: Option<&str>,
    ) -> Result<serde_json::Value, String>;
    fn stream(
        &self,
        params: serde_json::Value,
        last_event_id: Option<&str>,
        on_data: &mut dyn FnMut(&str),
    ) -> Result<(), String>;
}

/// Live JSON-RPC over toron's `/mcp` mount. Bearer token on every request.
pub struct UreqRpc {
    endpoint: String,
    token: String,
    agent: ureq::Agent,
}

impl UreqRpc {
    pub fn new(url: &str, token: &str) -> Self {
        let endpoint = if url.ends_with("/mcp") {
            url.to_string()
        } else {
            format!("{}/mcp", url.trim_end_matches('/'))
        };
        Self {
            endpoint,
            token: token.to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(8))
                .build(),
        }
    }

    fn post(&self, body: &str, last_event_id: Option<&str>) -> Result<ureq::Response, String> {
        let mut req = self
            .agent
            .post(&self.endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("MCP-Protocol-Version", "2026-07-28")
            .set("Authorization", &format!("Bearer {}", self.token));
        if let Some(id) = last_event_id {
            req = req.set("Last-Event-ID", id);
        }
        req.send_string(body)
            .map_err(|e| format!("toron events request failed: {e}"))
    }
}

impl EventsRpc for UreqRpc {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        last_event_id: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let id = 1u64;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        })
        .to_string();
        let resp = self.post(&body, last_event_id)?;
        let parsed: serde_json::Value = resp
            .into_string()
            .map_err(|e| format!("toron events response unreadable: {e}"))?
            .parse()
            .map_err(|e| format!("toron events response not json: {e}"))?;
        if let Some(err) = parsed.get("error") {
            return Err(format!("toron events {method} refused: {err}"));
        }
        Ok(parsed.get("result").cloned().unwrap_or(parsed))
    }

    fn stream(
        &self,
        params: serde_json::Value,
        last_event_id: Option<&str>,
        on_data: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "events/stream", "params": params,
        })
        .to_string();
        let resp = self.post(&body, last_event_id)?;
        use std::io::BufRead as _;
        let reader = std::io::BufReader::new(resp.into_reader());
        for line in reader.lines() {
            let line = line.map_err(|e| format!("toron events stream read failed: {e}"))?;
            if let Some(data) = line.strip_prefix("data:") {
                on_data(data.trim());
            }
        }
        Ok(())
    }
}

/// Pure parse of an `events/subscribe` result -> (subscription id, expiry us,
/// cursor). A malformed result is a typed failure, never a panic.
pub fn subscription_from_result(v: &serde_json::Value) -> Result<(String, i64, String), String> {
    let id = v
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "events/subscribe returned no id".to_string())?;
    let expires = v
        .get("expiresAt")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let cursor = v
        .get("cursor")
        .map(|c| match c {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    Ok((id.to_string(), expires, cursor))
}

/// Pure parse of an `events/poll` result into the kernel's `Page`.
pub fn page_from_poll(v: &serde_json::Value) -> Page {
    let events: Vec<BridgeEvent> = v
        .get("events")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row
                        .get("event_id")
                        .or_else(|| row.get("id"))
                        .and_then(serde_json::Value::as_str)?;
                    let kind = row
                        .get("name")
                        .or_else(|| row.get("kind"))
                        .and_then(serde_json::Value::as_str)?;
                    Some(BridgeEvent {
                        id: id.to_string(),
                        kind: kind.to_string(),
                        // Keep only ids: the body is never carried past this
                        // seam, so a log or a hint cannot leak it (ADR-0039 D8).
                        payload: serde_json::json!({"event_id": id, "name": kind}).to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let next_cursor = v
        .get("cursor")
        .map(|c| match c {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    Page {
        events,
        next_cursor,
        gap: v
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        truncated: false,
    }
}

/// One client bound to a pin-scoped subscription. `subscribe` runs once; the
/// stored id is reused on every poll, and the caller persists the cursor via
/// the kernel's `JournalCursor` command.
pub struct ToronEventsClient<R: EventsRpc> {
    rpc: R,
    name: String,
    ttl_us: i64,
    sub_id: String,
}

impl<R: EventsRpc> ToronEventsClient<R> {
    pub fn new(rpc: R, name: &str, ttl_secs: u64) -> Self {
        Self {
            rpc,
            name: name.to_string(),
            ttl_us: (ttl_secs * 1_000_000) as i64,
            sub_id: String::new(),
        }
    }

    pub fn subscription_id(&self) -> &str {
        &self.sub_id
    }

    /// `events/subscribe` with TTL <= 24h. Idempotent per client: a second call
    /// re-subscribes only after expiry (the caller checks `expires_us`).
    pub fn subscribe(&mut self) -> Result<i64, String> {
        let result = self.rpc.call(
            "events/subscribe",
            serde_json::json!({
                "name": self.name,
                "arguments": {},
                "delivery": "stream",
                "ttl_us": self.ttl_us,
            }),
            None,
        )?;
        let (id, expires, _cursor) = subscription_from_result(&result)?;
        self.sub_id = id;
        Ok(expires)
    }

    /// Try the SSE stream first; on any transport failure fall back to
    /// `events/poll` (the Phase A door). The callback frames are decoded and
    /// folded into one page so the kernel sees the same shape either way.
    pub fn fetch(&self, cursor: &str) -> Result<Page, String> {
        if self.sub_id.is_empty() {
            return Err("events client is not subscribed".to_string());
        }
        match self.try_stream(cursor) {
            Ok(Some(page)) => Ok(page),
            Ok(None) | Err(_) => {
                let result = self.rpc.call(
                    "events/poll",
                    serde_json::json!({
                        "subscriptionId": self.sub_id,
                        "cursor": cursor,
                    }),
                    Some(cursor),
                )?;
                Ok(page_from_poll(&result))
            }
        }
    }

    fn try_stream(&self, cursor: &str) -> Result<Option<Page>, String> {
        let sub = self.sub_id.clone();
        let mut events: Vec<BridgeEvent> = Vec::new();
        let mut next_cursor = cursor.to_string();
        let mut gap = false;
        let mut terminated = false;
        self.rpc.stream(
            serde_json::json!({
                "subscriptionId": sub.clone(),
                "cursor": cursor,
            }),
            Some(cursor),
            &mut |data| {
                let Some(frame) = decode_stream_frame(data, &sub) else {
                    return;
                };
                let cursor_opt = frame.cursor.clone();
                if frame.terminated {
                    terminated = true;
                }
                match frame.method.as_str() {
                    "notifications/events/event" if !frame.event_id.is_empty() => {
                        events.push(BridgeEvent {
                            id: frame.event_id,
                            kind: frame.event_name,
                            payload: String::new(),
                        });
                    }
                    "notifications/events/gap" => gap = true,
                    _ => {}
                }
                if (gap || terminated)
                    && let Some(c) = cursor_opt
                {
                    next_cursor = c;
                }
            },
        )?;
        if events.is_empty() && !gap && !terminated {
            return Ok(None);
        }
        Ok(Some(Page {
            events,
            next_cursor,
            gap,
            truncated: false,
        }))
    }
}

impl<R: EventsRpc> crate::events_bridge::EventsTransport for ToronEventsClient<R> {
    fn poll(&self, subscription: &str, cursor: &str) -> Result<Page, String> {
        // The stored subscription id wins; the arg is the fallback for a
        // caller that supplies the id directly (and for the no-op seam).
        let sub = if self.sub_id.is_empty() {
            subscription
        } else {
            &self.sub_id
        };
        let _ = sub; // routing is carried by the sub id inside the RPC params
        self.fetch(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorded SEP-3415 exchange (no live daemon, no network).
    struct RecordedRpc {
        subscribe: Result<serde_json::Value, String>,
        stream: Result<Vec<String>, String>,
        poll: Result<serde_json::Value, String>,
    }
    impl EventsRpc for RecordedRpc {
        fn call(
            &self,
            method: &str,
            _params: serde_json::Value,
            _last_event_id: Option<&str>,
        ) -> Result<serde_json::Value, String> {
            match method {
                "events/subscribe" => self.subscribe.clone(),
                "events/poll" => self.poll.clone(),
                other => Err(format!("unexpected call {other}")),
            }
        }
        fn stream(
            &self,
            _params: serde_json::Value,
            _last_event_id: Option<&str>,
            on_data: &mut dyn FnMut(&str),
        ) -> Result<(), String> {
            match &self.stream {
                Ok(frames) => {
                    for f in frames {
                        on_data(f);
                    }
                    Ok(())
                }
                Err(e) => Err(e.clone()),
            }
        }
    }

    fn rpc(
        stream: Result<Vec<String>, String>,
        poll: Result<serde_json::Value, String>,
    ) -> RecordedRpc {
        RecordedRpc {
            subscribe: Ok(
                serde_json::json!({"id": "sub-1", "expiresAt": 9_000_000_000i64, "cursor": "0"}),
            ),
            stream,
            poll,
        }
    }

    #[test]
    fn subscribe_parses_the_pin_scoped_subscription() {
        let mut client = ToronEventsClient::new(
            rpc(Ok(vec![]), Ok(serde_json::Value::Null)),
            "mail.addressed",
            3600,
        );
        let expires = client.subscribe().expect("subscribe");
        assert_eq!(expires, 9_000_000_000);
        assert_eq!(client.subscription_id(), "sub-1");
    }

    #[test]
    fn stream_events_become_one_page_and_never_carry_the_body() {
        let frames = vec![
            serde_json::json!({
                "method":"notifications/events/event",
                "params":{"_meta":{"io.modelcontextprotocol/subscriptionId":"sub-1"},
                          "event":{"event_id":"e1","name":"mail.addressed","body":"SECRET BODY"}}
            })
            .to_string(),
        ];
        let mut client = ToronEventsClient::new(
            rpc(Ok(frames), Ok(serde_json::Value::Null)),
            "mail.addressed",
            3600,
        );
        client.subscribe().expect("subscribe");
        let page = client.fetch("0").expect("fetch");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].id, "e1");
        assert!(!page.events[0].payload.contains("SECRET BODY"));
    }

    #[test]
    fn a_stream_failure_falls_back_to_poll() {
        let poll = serde_json::json!({
            "events":[{"event_id":"p1","name":"role.changed","body":"SECRET"}],
            "cursor":"7", "truncated": false
        });
        let mut client = ToronEventsClient::new(
            rpc(Err("stream down".to_string()), Ok(poll)),
            "mail.addressed",
            3600,
        );
        client.subscribe().expect("subscribe");
        let page = client.fetch("0").expect("fallback poll");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].id, "p1");
        assert_eq!(page.next_cursor, "7");
        assert!(!page.events[0].payload.contains("SECRET"));
    }

    #[test]
    fn a_gap_stream_frame_is_a_resync_page() {
        let frames =
            vec![serde_json::json!({
            "method":"notifications/events/gap",
            "params":{"_meta":{"io.modelcontextprotocol/subscriptionId":"sub-1"},"cursor":"42"}
        }).to_string()];
        let mut client = ToronEventsClient::new(
            rpc(Ok(frames), Ok(serde_json::Value::Null)),
            "mail.addressed",
            3600,
        );
        client.subscribe().expect("subscribe");
        let page = client.fetch("0").expect("fetch");
        assert!(page.gap);
        assert_eq!(page.next_cursor, "42");
    }
}
