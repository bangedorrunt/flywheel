// governed-by: ADR-0019 Contract gate D0-D7
//! Captain directory: tier, delivery word, session keys (ADR-0019 D2-D7).
//!
//! Pure projections only. No clock, env, rex, herdr, or I/O inside.
//! `now_ms` and liveness enter as arguments.

use std::collections::{BTreeMap, BTreeSet};

#[cfg_attr(not(test), allow(dead_code))] // contract number registry (ADR-0019 Configuration): pinned by tests + verify-0019.sh
pub const PULL_FRESH_MS: i64 = 120_000;
#[cfg_attr(not(test), allow(dead_code))] // contract number registry (ADR-0019 Configuration): pinned by tests + verify-0019.sh
pub const LOOP_LIMIT: usize = 12;
#[cfg_attr(not(test), allow(dead_code))] // contract number registry (ADR-0019 Configuration): pinned by tests + verify-0019.sh
pub const LOOP_WINDOW_MS: i64 = 600_000;
#[cfg_attr(not(test), allow(dead_code))] // contract number registry (ADR-0019 Configuration): pinned by tests + verify-0019.sh
pub const IN_FLIGHT_TTL_SECS: u64 = 90;
#[cfg_attr(not(test), allow(dead_code))] // contract number registry (ADR-0019 Configuration): pinned by tests + verify-0019.sh
pub const SESSION_FORGET_MS: i64 = 86_400_000;
#[cfg_attr(not(test), allow(dead_code))] // contract number registry (ADR-0019 Configuration): pinned by tests + verify-0019.sh
pub const SESSION_START_GRACE_MS: i64 = 7_200_000;
pub const APPROVAL_WAIT_MS: i64 = 600_000;
#[cfg_attr(not(test), allow(dead_code))] // contract number registry (ADR-0019 Configuration): pinned by tests + verify-0019.sh
pub const DOING_MAX_CHARS: usize = 200;

/// D3 delivery tier. First-match-wins in [`tier_of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Tier {
    Push,
    Turn,
    Pull,
    Unknown,
    None,
}

/// Pure inputs to [`tier_of`]. Caller fills from the D0-selected backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSnapshot {
    pub live_block: bool,
    pub prompt_supported: bool,
    pub hook_some: bool,
    pub verified_some: bool,
    pub pin_registered: bool,
    pub location_hosted: bool,
    pub has_block: bool,
    pub inbox_enabled: bool,
    pub rex_degraded: bool,
    pub positive_no_receiver: bool,
}

pub fn tier_of(s: &TierSnapshot) -> Tier {
    if s.live_block && s.prompt_supported {
        return Tier::Push;
    }
    if s.hook_some && s.verified_some && !s.prompt_supported {
        return Tier::Turn;
    }
    if s.pin_registered && (s.location_hosted || !s.has_block) && s.inbox_enabled {
        return Tier::Pull;
    }
    if s.rex_degraded {
        return Tier::Unknown;
    }
    if !s.pin_registered && !s.has_block && !s.inbox_enabled {
        return Tier::None;
    }
    if s.positive_no_receiver {
        return Tier::None;
    }
    Tier::Unknown
}

/// D2: pull freshness from the durable reader cursor. Pure over args.
#[cfg_attr(not(test), allow(dead_code))] // contract pure fn: pinned by tests; the arrive loader wires it when a per-npub cursor store exists
pub fn pull_fresh(last_cursor_advance_ms: Option<i64>, now_ms: i64) -> bool {
    match last_cursor_advance_ms {
        Some(last) => now_ms.saturating_sub(last) <= PULL_FRESH_MS,
        None => false,
    }
}

/// D2: cursor is keyed by reader npub. A lookup by `hosted:<pin>` or
/// `block:<id>` must read stale, never `0`-as-fresh.
#[cfg_attr(not(test), allow(dead_code))] // contract pure fn: pinned by tests; npub-keyed cursor reads only
pub fn cursor_advance_for_npub(cursor: &BTreeMap<String, i64>, reader: &str) -> Option<i64> {
    if reader.starts_with("hosted:") || reader.starts_with("block:") {
        return None;
    }
    cursor.get(reader).copied()
}

/// D5 delivery word. One constructor below; no string parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Queued,
    Woke,
    // Annotations, never settle words: no constructor builds these (pinned by test).
    #[cfg_attr(not(test), allow(dead_code))]
    Read,
    Unknown,
    Denied,
    #[cfg_attr(not(test), allow(dead_code))]
    Expired,
}

impl Delivery {
    /// Stable wire word. The ONLY rendering of a `Delivery` into evidence.
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Queued => "queued",
            Delivery::Woke => "woke",
            Delivery::Read => "read",
            Delivery::Unknown => "unknown",
            Delivery::Denied => "denied",
            Delivery::Expired => "expired",
        }
    }
}

/// Existing append word, parsed once at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendState {
    Appended,
    AlreadyAppended,
    Unknown,
    Refused,
}

/// Single constructor for the delivery word. The word follows the attempt
/// outcome, not the tier: tier selects whether a prompt is attempted, and
/// a prompt that returned ok means the model was shown it (`woke`) on any
/// tier. (Verification: unknown tier + ok → `woke`; pull/turn + not asked
/// or not ok → `queued`.)
pub fn delivery_of(append: AppendState, _tier: Tier, prompt_ok: Option<bool>) -> Delivery {
    match append {
        AppendState::Refused => Delivery::Denied,
        AppendState::Unknown => Delivery::Unknown,
        AppendState::Appended | AppendState::AlreadyAppended => match prompt_ok {
            Some(true) => Delivery::Woke,
            _ => Delivery::Queued,
        },
    }
}

/// D4/D6/D7 session row. `repo`/`branch`/`doing`/etc are null when unknown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Session {
    pub pin: String,
    pub session_key: String,
    pub tier: Tier,
    pub location_hosted: bool,
    pub pane_id: Option<String>,
    pub last_seen_ms: i64,
    pub unread: u64,
    pub doing: Option<String>,
}

impl Session {
    pub fn new(pin: impl Into<String>, session_key: impl Into<String>, tier: Tier) -> Self {
        Self {
            pin: pin.into(),
            session_key: session_key.into(),
            tier,
            location_hosted: false,
            pane_id: None,
            last_seen_ms: 0,
            unread: 0,
            doing: None,
        }
    }
}

/// D4: pure projection into a pin-then-key ordered map, capped at 100.
pub fn project_directory(sessions: Vec<Session>) -> (BTreeMap<(String, String), Session>, bool) {
    let mut sorted = sessions;
    sorted.sort_by(|a, b| {
        (a.pin.clone(), a.session_key.clone()).cmp(&(b.pin.clone(), b.session_key.clone()))
    });
    let truncated = sorted.len() > 100;
    let mut map = BTreeMap::new();
    for s in sorted.into_iter().take(100) {
        map.insert((s.pin.clone(), s.session_key.clone()), s);
    }
    (map, truncated)
}

/// D6 session key.
pub fn session_key(
    location_hosted: bool,
    pin: &str,
    harness: Option<&str>,
    harness_session: Option<&str>,
    block_id: Option<&str>,
    pane_id: Option<&str>,
    herdr_selected: bool,
) -> String {
    if location_hosted {
        return format!("hosted:{pin}");
    }
    if let (Some(h), Some(sid)) = (harness, harness_session)
        && !sid.is_empty()
    {
        return format!("{h}:{sid}");
    }
    if let Some(b) = block_id
        && !herdr_selected
    {
        return format!("block:{b}");
    }
    if let Some(p) = pane_id {
        return format!("pane:{p}");
    }
    if let Some(b) = block_id {
        return format!("block:{b}");
    }
    format!("hosted:{pin}")
}

/// D6: forget a non-kept key only when unread==0, past the window, and past
/// the start grace. Kept keys (`hosted:<pin>`, bound captain) never forget.
#[cfg_attr(not(test), allow(dead_code))] // contract pure fn: pinned by tests; the forget scheduler calls it, not this ship
pub fn should_forget(
    last_seen_ms: i64,
    now_ms: i64,
    unread: u64,
    kept: bool,
    process_start_ms: i64,
) -> bool {
    if kept || unread > 0 {
        return false;
    }
    if now_ms.saturating_sub(process_start_ms) < SESSION_START_GRACE_MS {
        return false;
    }
    now_ms.saturating_sub(last_seen_ms) >= SESSION_FORGET_MS
}

/// D7: store at most 200 chars; team-invite URLs become `(link)`.
#[cfg_attr(not(test), allow(dead_code))] // contract pure fn: pinned by tests; doing writers truncate at the edge
pub fn truncate_doing(line: &str) -> String {
    let mut s = line.to_string();
    // Minimal invite-URL scrub: replace https? invite links with (link).
    // Keep it dependency-free: scan whitespace-separated tokens.
    let tokens: Vec<String> = s
        .split_whitespace()
        .map(|t| {
            if (t.starts_with("http://") || t.starts_with("https://")) && t.contains("invite") {
                "(link)".to_string()
            } else {
                t.to_string()
            }
        })
        .collect();
    s = tokens.join(" ");
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > DOING_MAX_CHARS {
        chars.into_iter().take(DOING_MAX_CHARS).collect()
    } else {
        s
    }
}

/// Inputs to [`resolve_address`].
#[derive(Debug, Clone, Default)]
pub struct DirSnapshot {
    pub sessions: Vec<Session>,
    pub bead_assignee: BTreeMap<String, String>,
    pub peer_held_beads: BTreeSet<String>,
    pub file_held_beads: BTreeSet<String>,
    pub arrived_pins: BTreeSet<String>,
}

/// Stable address errors (D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressError {
    Ambiguous(Vec<String>),
    PeerHeld,
    FileHeld,
    NoAssignee,
    GroupRefused,
    TierNone,
    Unarrived,
}

impl AddressError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Ambiguous(_) => "ambiguous",
            Self::PeerHeld => "peer_held",
            Self::FileHeld => "file_held",
            Self::NoAssignee => "no_assignee",
            Self::GroupRefused => "group_refused",
            Self::TierNone => "tier_none",
            Self::Unarrived => "unarrived",
        }
    }
}

fn newest_for_pin(sessions: &[Session], pin: &str) -> Vec<Session> {
    let mut v: Vec<Session> = sessions.iter().filter(|s| s.pin == pin).cloned().collect();
    v.sort_by_key(|s| (s.last_seen_ms, s.session_key.clone()));
    v
}

pub fn resolve_address(
    snap: &DirSnapshot,
    to: &str,
    sender: &str,
) -> Result<Session, AddressError> {
    if !snap.arrived_pins.contains(sender) {
        return Err(AddressError::Unarrived);
    }
    if to == "*" || to.starts_with("repo:") || to.ends_with("/*") || to.contains("/*") {
        return Err(AddressError::GroupRefused);
    }
    if let Some(bead) = to.strip_prefix("bead:") {
        if snap.file_held_beads.contains(bead) {
            return Err(AddressError::FileHeld);
        }
        if snap.peer_held_beads.contains(bead) {
            return Err(AddressError::PeerHeld);
        }
        match snap.bead_assignee.get(bead) {
            None => return Err(AddressError::NoAssignee),
            Some(pin) => {
                let v = newest_for_pin(&snap.sessions, pin);
                return v.into_iter().last().ok_or(AddressError::NoAssignee);
            }
        }
    }
    if let Some((pin, key)) = to.split_once('@') {
        let hit = snap
            .sessions
            .iter()
            .find(|s| s.pin == pin && s.session_key == key)
            .cloned();
        return hit.ok_or(AddressError::NoAssignee);
    }
    // Bare pin: newest session, ambiguous when 2+ live rows share the pin.
    if !snap.arrived_pins.contains(to) && newest_for_pin(&snap.sessions, to).is_empty() {
        return Err(AddressError::Unarrived);
    }
    let v = newest_for_pin(&snap.sessions, to);
    match v.len() {
        0 => Err(AddressError::NoAssignee),
        1 => {
            let s = v.into_iter().last().expect("len 1");
            if s.tier == Tier::None {
                return Err(AddressError::TierNone);
            }
            Ok(s)
        }
        _ => Err(AddressError::Ambiguous(
            v.into_iter().map(|s| s.session_key).collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn snap_tier(
        live_block: bool,
        prompt: bool,
        hook: bool,
        verified: bool,
        registered: bool,
        hosted: bool,
        has_block: bool,
        inbox: bool,
        degraded: bool,
    ) -> TierSnapshot {
        TierSnapshot {
            live_block,
            prompt_supported: prompt,
            hook_some: hook,
            verified_some: verified,
            pin_registered: registered,
            location_hosted: hosted,
            has_block,
            inbox_enabled: inbox,
            rex_degraded: degraded,
            positive_no_receiver: false,
        }
    }

    #[test]
    fn numeric_values_are_pinned() {
        assert_eq!(PULL_FRESH_MS, 120_000);
        assert_eq!(LOOP_LIMIT, 12);
        assert_eq!(LOOP_WINDOW_MS, 600_000);
        assert_eq!(IN_FLIGHT_TTL_SECS, 90);
        assert_eq!(SESSION_FORGET_MS, 86_400_000);
        assert_eq!(SESSION_START_GRACE_MS, 7_200_000);
        assert_eq!(APPROVAL_WAIT_MS, 600_000);
        assert_eq!(DOING_MAX_CHARS, 200);
        // One behavior bound through the constant, not a copied literal.
        assert!(pull_fresh(Some(0), PULL_FRESH_MS));
        assert!(!pull_fresh(Some(0), PULL_FRESH_MS + 1));
    }

    #[test]
    fn tier_push_beats_herdr_absence() {
        // Empty herdr listing with a live block: still push.
        let s = snap_tier(true, true, false, false, true, false, true, true, false);
        assert_eq!(tier_of(&s), Tier::Push);
    }

    #[test]
    fn tier_pull_for_hosted_no_block() {
        let s = snap_tier(false, false, false, false, true, true, false, true, false);
        assert_eq!(tier_of(&s), Tier::Pull);
    }

    #[test]
    fn tier_unknown_on_degraded_read() {
        let s = snap_tier(false, false, false, false, true, false, true, true, true);
        assert_eq!(tier_of(&s), Tier::Unknown);
    }

    #[test]
    fn tier_none_only_on_positive_evidence() {
        let s = TierSnapshot {
            live_block: false,
            prompt_supported: false,
            hook_some: false,
            verified_some: false,
            pin_registered: false,
            location_hosted: false,
            has_block: false,
            inbox_enabled: false,
            rex_degraded: false,
            positive_no_receiver: true,
        };
        assert_eq!(tier_of(&s), Tier::None);
        // Absent pin alone without the full triple is not none.
        let s2 = snap_tier(false, false, false, false, false, false, false, true, false);
        assert_ne!(tier_of(&s2), Tier::None);
    }

    #[test]
    fn tier_turn_needs_hook_and_verified() {
        let s = snap_tier(false, false, true, true, true, false, true, true, false);
        assert_eq!(tier_of(&s), Tier::Turn);
        // Grok row today: hook None -> never turn.
        let grok = snap_tier(false, false, false, false, true, false, true, true, false);
        assert_ne!(tier_of(&grok), Tier::Turn);
        // Hook without verified bit is not turn.
        let half = snap_tier(false, false, true, false, true, false, true, true, false);
        assert_ne!(tier_of(&half), Tier::Turn);
    }

    #[test]
    fn pull_fresh_only_from_npub_cursor() {
        let mut cursor = BTreeMap::new();
        cursor.insert("npub1abc".to_string(), 1_000);
        // npub lookup reads the row.
        assert_eq!(cursor_advance_for_npub(&cursor, "npub1abc"), Some(1_000));
        assert!(pull_fresh(
            cursor_advance_for_npub(&cursor, "npub1abc"),
            1_000 + 60_000
        ));
        assert!(!pull_fresh(
            cursor_advance_for_npub(&cursor, "npub1abc"),
            1_000 + 120_001
        ));
        // Hosted/block keys must read stale even when a row exists.
        cursor.insert("hosted:Pin".to_string(), 2_000);
        cursor.insert("block:abc".to_string(), 2_000);
        assert_eq!(cursor_advance_for_npub(&cursor, "hosted:Pin"), None);
        assert_eq!(cursor_advance_for_npub(&cursor, "block:abc"), None);
        assert!(!pull_fresh(
            cursor_advance_for_npub(&cursor, "hosted:Pin"),
            2_000
        ));
        // Unresolvable npub reads stale, never 0-as-fresh.
        assert_eq!(cursor_advance_for_npub(&cursor, "npub1missing"), None);
        assert!(!pull_fresh(None, 5_000));
    }

    #[test]
    fn delivery_single_constructor_push_woke_rest_queued() {
        assert_eq!(
            delivery_of(AppendState::Appended, Tier::Push, Some(true)),
            Delivery::Woke
        );
        assert_eq!(
            delivery_of(AppendState::Appended, Tier::Push, Some(false)),
            Delivery::Queued
        );
        assert_eq!(
            delivery_of(AppendState::Appended, Tier::Push, None),
            Delivery::Queued
        );
        assert_eq!(
            delivery_of(AppendState::Appended, Tier::Pull, None),
            Delivery::Queued
        );
        assert_eq!(
            delivery_of(AppendState::AlreadyAppended, Tier::Turn, None),
            Delivery::Queued
        );
        assert_eq!(
            delivery_of(AppendState::Unknown, Tier::Push, Some(true)),
            Delivery::Unknown
        );
        assert_eq!(
            delivery_of(AppendState::Refused, Tier::Push, Some(true)),
            Delivery::Denied
        );
        // The word follows the attempt outcome, not the tier: ok → woke on
        // any tier (verification: unknown tier + ok → woke).
        assert_eq!(
            delivery_of(AppendState::Appended, Tier::Unknown, Some(true)),
            Delivery::Woke
        );
        assert_eq!(
            delivery_of(AppendState::Appended, Tier::Pull, Some(true)),
            Delivery::Woke
        );
        // `read` and `expired` are annotations, never a settle word: no
        // input triple constructs them. The explicit construction below is
        // the use the dead-code lint counts; the loop above is the guarantee.
        let _ = (Delivery::Read, Delivery::Expired);
        for append in [
            AppendState::Appended,
            AppendState::AlreadyAppended,
            AppendState::Unknown,
            AppendState::Refused,
        ] {
            for tier in [
                Tier::Push,
                Tier::Turn,
                Tier::Pull,
                Tier::Unknown,
                Tier::None,
            ] {
                for prompt in [None, Some(false), Some(true)] {
                    assert!(
                        !matches!(
                            delivery_of(append, tier, prompt),
                            Delivery::Read | Delivery::Expired
                        ),
                        "annotations are written by readers, never the constructor"
                    );
                }
            }
        }
    }

    #[test]
    fn resolve_group_and_bead_codes() {
        let mut snap = DirSnapshot::default();
        snap.arrived_pins.insert("Cap".into());
        snap.arrived_pins.insert("Holder".into());
        snap.arrived_pins.insert("Worker".into());
        snap.sessions.push(Session {
            pin: "Cap".into(),
            session_key: "block:1".into(),
            tier: Tier::Push,
            location_hosted: false,
            pane_id: Some("block:1".into()),
            last_seen_ms: 10,
            unread: 0,
            doing: None,
        });
        // Group fan-out refused even for the bound captain.
        assert_eq!(
            resolve_address(&snap, "*", "Cap"),
            Err(AddressError::GroupRefused)
        );
        assert_eq!(
            resolve_address(&snap, "repo:flywheel", "Cap"),
            Err(AddressError::GroupRefused)
        );
        // Bead with no assignee.
        assert_eq!(
            resolve_address(&snap, "bead:bd-1", "Cap"),
            Err(AddressError::NoAssignee)
        );
        snap.bead_assignee.insert("bd-1".into(), "Worker".into());
        snap.sessions.push(Session {
            pin: "Worker".into(),
            session_key: "block:2".into(),
            tier: Tier::Push,
            location_hosted: false,
            pane_id: Some("block:2".into()),
            last_seen_ms: 20,
            unread: 0,
            doing: None,
        });
        snap.file_held_beads.insert("bd-1".into());
        assert_eq!(
            resolve_address(&snap, "bead:bd-1", "Cap"),
            Err(AddressError::FileHeld)
        );
        snap.file_held_beads.clear();
        snap.peer_held_beads.insert("bd-1".into());
        assert_eq!(
            resolve_address(&snap, "bead:bd-1", "Cap"),
            Err(AddressError::PeerHeld)
        );
        snap.peer_held_beads.clear();
        assert!(resolve_address(&snap, "bead:bd-1", "Cap").is_ok());
    }

    #[test]
    fn resolve_pin_ambiguous_and_tier_none_and_unarrived() {
        let mut snap = DirSnapshot::default();
        snap.arrived_pins.insert("Sender".into());
        snap.arrived_pins.insert("Solo".into());
        snap.arrived_pins.insert("Duo".into());
        snap.arrived_pins.insert("Ghost".into());
        snap.sessions.push(Session {
            pin: "Solo".into(),
            session_key: "block:s1".into(),
            tier: Tier::Push,
            location_hosted: false,
            pane_id: Some("block:s1".into()),
            last_seen_ms: 5,
            unread: 0,
            doing: None,
        });
        snap.sessions.push(Session {
            pin: "Duo".into(),
            session_key: "block:d1".into(),
            tier: Tier::Push,
            location_hosted: false,
            pane_id: Some("block:d1".into()),
            last_seen_ms: 6,
            unread: 0,
            doing: None,
        });
        snap.sessions.push(Session {
            pin: "Duo".into(),
            session_key: "block:d2".into(),
            tier: Tier::Push,
            location_hosted: false,
            pane_id: Some("block:d2".into()),
            last_seen_ms: 7,
            unread: 0,
            doing: None,
        });
        snap.sessions.push(Session {
            pin: "Ghost".into(),
            session_key: "hosted:Ghost".into(),
            tier: Tier::None,
            location_hosted: true,
            pane_id: None,
            last_seen_ms: 8,
            unread: 0,
            doing: None,
        });
        assert!(resolve_address(&snap, "Solo", "Sender").is_ok());
        let err = resolve_address(&snap, "Duo", "Sender").unwrap_err();
        match err {
            AddressError::Ambiguous(keys) => assert_eq!(keys.len(), 2),
            _ => panic!("expected ambiguous"),
        }
        // Explicit key disambiguates.
        assert!(resolve_address(&snap, "Duo@block:d1", "Sender").is_ok());
        assert_eq!(
            resolve_address(&snap, "Ghost", "Sender"),
            Err(AddressError::TierNone)
        );
        // Sender or target never arrived.
        assert_eq!(
            resolve_address(&snap, "Solo", "Stranger"),
            Err(AddressError::Unarrived)
        );
        assert_eq!(
            resolve_address(&snap, "Never", "Sender"),
            Err(AddressError::Unarrived)
        );
    }

    #[test]
    fn directory_caps_at_100_pin_key_ordered() {
        let mut sessions = Vec::new();
        for i in 0..105 {
            sessions.push(Session {
                pin: format!("Pin{:03}", 104 - i),
                session_key: format!("block:{:03}", 104 - i),
                tier: Tier::Pull,
                location_hosted: false,
                pane_id: None,
                last_seen_ms: i as i64,
                unread: 0,
                doing: None,
            });
        }
        let (map, truncated) = project_directory(sessions);
        assert!(truncated);
        assert_eq!(map.len(), 100);
        let first = map.keys().next().expect("first");
        assert_eq!(first.0, "Pin000");
    }

    #[test]
    fn session_keys_follow_table() {
        assert_eq!(
            session_key(true, "Grok", None, None, Some("b1"), None, false),
            "hosted:Grok"
        );
        assert_eq!(
            session_key(
                false,
                "P",
                Some("grok"),
                Some("sess9"),
                Some("b1"),
                None,
                false
            ),
            "grok:sess9"
        );
        assert_eq!(
            session_key(false, "P", None, None, Some("b1"), None, false),
            "block:b1"
        );
        assert_eq!(
            session_key(false, "P", None, None, None, Some("w1:p2"), true),
            "pane:w1:p2"
        );
        // Rex block is never stored under a herdr key.
        assert!(
            session_key(false, "P", None, None, Some("b1"), Some("w1:p2"), false)
                .starts_with("block:")
        );
    }

    #[test]
    fn forget_needs_zero_unread_window_and_grace() {
        // Kept keys never forget.
        assert!(!should_forget(0, SESSION_FORGET_MS + 10, 0, true, 0));
        // Unread mail blocks forget.
        assert!(!should_forget(0, SESSION_FORGET_MS + 10, 1, false, 0));
        // Start grace blocks forget.
        assert!(!should_forget(0, SESSION_START_GRACE_MS - 1, 0, false, 0));
        // Past window + grace + zero unread forgets.
        assert!(should_forget(
            0,
            SESSION_FORGET_MS + SESSION_START_GRACE_MS + 1,
            0,
            false,
            0
        ));
    }

    #[test]
    fn doing_truncates_and_scrubs_invites() {
        let long = "x".repeat(300);
        assert_eq!(truncate_doing(&long).chars().count(), DOING_MAX_CHARS);
        let with_link = "join https://example.com/invite abc now";
        let got = truncate_doing(with_link);
        assert!(!got.contains("invite"), "{got}");
        assert!(got.contains("(link)"), "{got}");
    }
}
