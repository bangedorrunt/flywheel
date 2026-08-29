// governed-by: ADR-0026
//! Failure taxonomy (ADR-0026 Spec 1).

#![allow(dead_code)]

use std::fmt;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    Auth,
    RateLimit,
    Timeout,
    Blocked,
    Provider,
    Cancelled,
    Unknown,
    Billing,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Auth => "auth",
            Self::RateLimit => "rate_limit",
            Self::Timeout => "timeout",
            Self::Blocked => "blocked",
            Self::Provider => "provider",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
            Self::Billing => "billing",
        };
        write!(f, "{s}")
    }
}

#[allow(dead_code)]
impl FailureKind {
    pub const ALL: [FailureKind; 8] = [
        FailureKind::Auth,
        FailureKind::RateLimit,
        FailureKind::Timeout,
        FailureKind::Blocked,
        FailureKind::Provider,
        FailureKind::Cancelled,
        FailureKind::Unknown,
        FailureKind::Billing,
    ];
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Auth => "Auth",
            Self::RateLimit => "RateLimit",
            Self::Timeout => "Timeout",
            Self::Blocked => "Blocked",
            Self::Provider => "Provider",
            Self::Cancelled => "Cancelled",
            Self::Unknown => "Unknown",
            Self::Billing => "Billing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedFailure {
    pub kind: FailureKind,
    pub retryable: bool,
    pub reroute_eligible: bool,
    pub evidence: String,
}

pub fn classify(
    stderr_tail: &str,
    exit_code: Option<i32>,
    body: Option<&str>,
) -> ClassifiedFailure {
    let body_str = body.unwrap_or("");
    let combined = format!("{stderr_tail} {body_str}");
    let lower = combined.to_ascii_lowercase();
    let code_str = exit_code.map(|c| c.to_string()).unwrap_or_default();
    let evidence = {
        let raw = if body_str.is_empty() {
            stderr_tail.to_string()
        } else {
            format!("{stderr_tail} | body: {body_str} | exit:{code_str}")
        };
        truncate_evidence(&raw, 512)
    };
    if lower.contains("402")
        || lower.contains("insufficient balance")
        || lower.contains("insufficient_balance")
        || lower.contains("payment required")
        || lower.contains("payment_required")
        || lower.contains("billing")
    {
        return ClassifiedFailure {
            kind: FailureKind::Billing,
            retryable: false,
            reroute_eligible: true,
            evidence,
        };
    }
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_api_key")
        || lower.contains("authentication")
        || lower.contains("invalid api")
    {
        return ClassifiedFailure {
            kind: FailureKind::Auth,
            retryable: false,
            reroute_eligible: true,
            evidence,
        };
    }
    if lower.contains("429")
        || lower.contains("quota exceeded")
        || lower.contains("quota_exceeded")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("rate-limit")
    {
        return ClassifiedFailure {
            kind: FailureKind::RateLimit,
            retryable: true,
            reroute_eligible: false,
            evidence,
        };
    }
    if lower.contains("unsupported_agent_kind")
        || lower.contains("harness binary missing")
        || lower.contains("harness binary not found")
        || (lower.contains("provider")
            && (lower.contains("not found")
                || lower.contains("missing")
                || lower.contains("error")
                || lower.contains("unavailable")))
        || lower.contains("unsupported harness")
    {
        return ClassifiedFailure {
            kind: FailureKind::Provider,
            retryable: false,
            reroute_eligible: true,
            evidence,
        };
    }
    if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("deadline")
        || lower.contains("deadline exceeded")
        || lower.contains("etimedout")
    {
        return ClassifiedFailure {
            kind: FailureKind::Timeout,
            retryable: true,
            reroute_eligible: false,
            evidence,
        };
    }
    if lower.contains("reservation conflict")
        || lower.contains("reservation_conflict")
        || (lower.contains("blocked") && lower.contains("reservation"))
        || lower.contains("file reservation") && lower.contains("conflict")
    {
        return ClassifiedFailure {
            kind: FailureKind::Blocked,
            retryable: false,
            reroute_eligible: false,
            evidence,
        };
    }
    ClassifiedFailure {
        kind: FailureKind::Unknown,
        retryable: false,
        reroute_eligible: false,
        evidence: truncate_evidence(stderr_tail, 512),
    }
}

fn truncate_evidence(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}... [truncated {} -> {}]", &s[..max], s.len(), max)
    }
}

#[allow(dead_code)]
pub fn is_terminal_on_second_failure(first_kind: FailureKind, second_kind: FailureKind) -> bool {
    first_kind == second_kind
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failure_kind_variant_spelling_frozen() {
        let names: Vec<&str> = FailureKind::ALL.iter().map(|k| k.variant_name()).collect();
        assert_eq!(
            names,
            vec![
                "Auth",
                "RateLimit",
                "Timeout",
                "Blocked",
                "Provider",
                "Cancelled",
                "Unknown",
                "Billing"
            ]
        );
        assert_eq!(FailureKind::ALL.len(), 8);
    }
    #[test]
    fn failure_kind_display_matches_wire() {
        assert_eq!(FailureKind::Billing.to_string(), "billing");
        assert_eq!(FailureKind::RateLimit.to_string(), "rate_limit");
        assert_eq!(FailureKind::Auth.to_string(), "auth");
        assert_eq!(FailureKind::Provider.to_string(), "provider");
        assert_eq!(FailureKind::Timeout.to_string(), "timeout");
        assert_eq!(FailureKind::Blocked.to_string(), "blocked");
        assert_eq!(FailureKind::Unknown.to_string(), "unknown");
        assert_eq!(FailureKind::Cancelled.to_string(), "cancelled");
    }
    #[test]
    fn failure_classify_billing_402() {
        let c = classify("Error 402 Insufficient Balance", None, None);
        assert_eq!(c.kind, FailureKind::Billing);
        assert!(!c.retryable);
        assert!(c.reroute_eligible);
    }
    #[test]
    fn failure_classify_billing_payment_required() {
        let c = classify("payment required — billing account empty", None, None);
        assert_eq!(c.kind, FailureKind::Billing);
    }
    #[test]
    fn failure_classify_billing_body() {
        let c = classify(
            "harness failed",
            Some(1),
            Some("402 billing insufficient balance"),
        );
        assert_eq!(c.kind, FailureKind::Billing);
    }
    #[test]
    fn failure_classify_rate_limit_429() {
        let c = classify("429 Too Many Requests", None, None);
        assert_eq!(c.kind, FailureKind::RateLimit);
        assert!(c.retryable);
        assert!(!c.reroute_eligible);
    }
    #[test]
    fn failure_classify_rate_limit_quota() {
        let c = classify("quota exceeded for model", None, None);
        assert_eq!(c.kind, FailureKind::RateLimit);
    }
    #[test]
    fn failure_classify_rate_limit_phrase() {
        let c = classify("rate limit hit, retry after 60s", None, None);
        assert_eq!(c.kind, FailureKind::RateLimit);
    }
    #[test]
    fn failure_classify_auth_401() {
        let c = classify("401 Unauthorized", None, None);
        assert_eq!(c.kind, FailureKind::Auth);
    }
    #[test]
    fn failure_classify_auth_invalid_api_key() {
        let c = classify("invalid api key provided", None, None);
        assert_eq!(c.kind, FailureKind::Auth);
    }
    #[test]
    fn failure_classify_auth_403() {
        let c = classify("403 forbidden authentication failed", None, None);
        assert_eq!(c.kind, FailureKind::Auth);
    }
    #[test]
    fn failure_classify_provider_unsupported_kind() {
        let c = classify("unsupported_agent_kind: pa", None, None);
        assert_eq!(c.kind, FailureKind::Provider);
    }
    #[test]
    fn failure_classify_provider_harness_missing() {
        let c = classify("harness binary missing: /usr/local/bin/pa", None, None);
        assert_eq!(c.kind, FailureKind::Provider);
    }
    #[test]
    fn failure_classify_timeout_deadline() {
        let c = classify("spawn timed out after 45s deadline exceeded", None, None);
        assert_eq!(c.kind, FailureKind::Timeout);
    }
    #[test]
    fn failure_classify_timeout_keyword() {
        let c = classify("operation timeout", None, None);
        assert_eq!(c.kind, FailureKind::Timeout);
    }
    #[test]
    fn failure_classify_blocked_reservation_conflict() {
        let c = classify(
            "reservation conflict: path src/main.rs held by GrayCarp",
            None,
            None,
        );
        assert_eq!(c.kind, FailureKind::Blocked);
    }
    #[test]
    fn failure_classify_unknown_no_match() {
        let c = classify("some random harness panic: signal 11", Some(139), None);
        assert_eq!(c.kind, FailureKind::Unknown);
        assert!(c.evidence.contains("signal 11"));
    }
    #[test]
    fn failure_classify_unknown_truncates_long_tail() {
        let long = "x".repeat(1000);
        let c = classify(&long, None, None);
        assert_eq!(c.kind, FailureKind::Unknown);
        assert!(c.evidence.contains("truncated"));
    }
    #[test]
    fn failure_classify_never_guesses_work_quality() {
        let c = classify(
            "test failed: 3 passed, 1 failed — cargo test",
            Some(1),
            None,
        );
        assert_eq!(c.kind, FailureKind::Unknown);
    }
    #[test]
    fn failure_classify_blocked_never_without_reservation_signal() {
        let c = classify("agent is blocked", None, None);
        assert_eq!(c.kind, FailureKind::Unknown);
    }
    #[test]
    fn failure_classify_priority_billing_over_auth() {
        let c = classify("402 billing and 401 unauthorized in same body", None, None);
        assert_eq!(c.kind, FailureKind::Billing);
    }
    #[test]
    fn failure_classify_chain_bound_second_same_class_terminal() {
        assert!(is_terminal_on_second_failure(
            FailureKind::Billing,
            FailureKind::Billing
        ));
        assert!(!is_terminal_on_second_failure(
            FailureKind::Billing,
            FailureKind::Auth
        ));
    }
    #[test]
    fn failure_classify_case_insensitive() {
        let c = classify("402 INSUFFICIENT BALANCE", None, None);
        assert_eq!(c.kind, FailureKind::Billing);
        let c = classify("RATE LIMIT exceeded", None, None);
        assert_eq!(c.kind, FailureKind::RateLimit);
    }
}
