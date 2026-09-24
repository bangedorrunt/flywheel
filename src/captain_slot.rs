// governed-by: ADR-0011
//! Captain alias is a FCFS slot (ADR-0011 D4).
//!
//! Stale: no fresh presence AND no fresh last_wake AND no live pane.
//! Override (`toron contact captain --pin`) always wins — this module only
//! decides whether the current holder is stale and whether FCFS may claim.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptainEvidence {
    pub holder: Option<String>,
    pub presence_fresh: bool,
    pub last_wake_fresh: bool,
    pub pane_alive: bool,
    /// explicit operator override pin (always wins)
    pub override_pin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptainAction {
    Keep { pin: String },
    Override { pin: String },
    Claim { pin: String, previous_stale: bool },
    Vacant,
}

pub fn holder_stale(ev: &CaptainEvidence) -> bool {
    match &ev.holder {
        None => true,
        Some(_) => !(ev.presence_fresh || ev.last_wake_fresh || ev.pane_alive),
    }
}

pub fn next_action(ev: &CaptainEvidence, arriver: &str) -> CaptainAction {
    if let Some(pin) = &ev.override_pin {
        return CaptainAction::Override { pin: pin.clone() };
    }
    match &ev.holder {
        Some(h) if !holder_stale(ev) => CaptainAction::Keep { pin: h.clone() },
        Some(_) => CaptainAction::Claim {
            pin: arriver.to_string(),
            previous_stale: true,
        },
        None => {
            if arriver.is_empty() {
                CaptainAction::Vacant
            } else {
                CaptainAction::Claim {
                    pin: arriver.to_string(),
                    previous_stale: false,
                }
            }
        }
    }
}

/// F10: the slot is KEPT, but nothing LIVE backs the holder — no pane, no
/// dispatcher, no presence heartbeat. Only roster freshness (`whois.stale`,
/// a generous 7-day threshold) holds it, so a prior session's pin that lost
/// its pane looks live while `mail --to captain` reaches nobody. Returns the
/// holder pin so the caller can OFFER a rebind (never auto-rebind: a quiet
/// human operator is indistinguishable from a dead session pin here).
pub fn rebind_hint(ev: &CaptainEvidence, heartbeat_fresh: bool) -> Option<String> {
    let holder = ev.holder.as_ref()?;
    if ev.override_pin.is_some() {
        return None; // explicit override wins; nothing to offer
    }
    if ev.pane_alive || ev.last_wake_fresh || heartbeat_fresh {
        return None; // a live signal backs the holder
    }
    // Kept only because roster-presence says fresh.
    (!holder_stale(ev)).then(|| holder.clone())
}

/// bd-8pis: is a bound pane LIVE? A whois `pane_id` alone can lie (dead
/// panes stay bound) — against a reachable herdr daemon the pane must be
/// in the live set; when the daemon is unreachable the binding is trusted
/// (best-effort, never a warn storm from a down daemon). None pane_id is
/// never live.
pub fn bound_pane_is_live(
    pane_id: Option<&str>,
    live_panes: Option<&std::collections::HashSet<String>>,
) -> bool {
    match (pane_id, live_panes) {
        (Some(_), None) => true,
        (Some(pid), Some(set)) => set.contains(pid),
        _ => false,
    }
}

/// bd-8pis: the dead-fold health line — shared by arrive and helm so the
/// captain fold reads the same in every health surface. Pure text; the
/// caller decides liveness via [`rebind_hint`].
pub fn dead_fold_message(project: &str, pin: &str) -> String {
    format!(
        "flywheel: captain `{pin}` is held on roster-presence alone — no live pane, no \
         dispatcher, no presence heartbeat; mail to `captain` may reach nobody live. Rebind: \
         toron contact captain --project {project} --pin <pin>  (or --clear to vacate)"
    )
}

/// bd-8pis: dead-pin send warnings — pure over resolved targets,
/// pane-bound pins, and heartbeat pins. Every target with neither a bound
/// pane nor a fresh heartbeat warns (advisory; mail stays async). A lone
/// leading `@` is stripped; matching is exact after that.
pub fn dead_pin_warnings(
    targets: &[String],
    pane_pins: &std::collections::HashSet<String>,
    heartbeat_pins: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for t in targets {
        let pin = t.strip_prefix('@').unwrap_or(t.as_str());
        if pane_pins.contains(pin) || heartbeat_pins.contains(pin) {
            continue;
        }
        out.push(format!(
            "flywheel: mail send to `{t}` may reach nobody — no live herdr pane, no presence \
             heartbeat (advisory; mail stays async)"
        ));
    }
    out
}

pub fn claim_line(action: &CaptainAction) -> Option<String> {
    match action {
        CaptainAction::Claim {
            pin,
            previous_stale: true,
        } => Some(format!(
            "captain slot claimed: {pin} (previous holder stale)"
        )),
        CaptainAction::Claim {
            pin,
            previous_stale: false,
        } => Some(format!("captain slot claimed: {pin}")),
        CaptainAction::Override { pin } => Some(format!("captain slot override: {pin}")),
        CaptainAction::Keep { .. } | CaptainAction::Vacant => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(holder: Option<&str>) -> CaptainEvidence {
        CaptainEvidence {
            holder: holder.map(str::to_string),
            presence_fresh: false,
            last_wake_fresh: false,
            pane_alive: false,
            override_pin: None,
        }
    }

    #[test]
    fn empty_slot_fcfs_claims() {
        let a = next_action(&ev(None), "LucidFalcon");
        assert_eq!(
            a,
            CaptainAction::Claim {
                pin: "LucidFalcon".into(),
                previous_stale: false
            }
        );
        assert!(claim_line(&a).unwrap().contains("LucidFalcon"));
    }

    #[test]
    fn live_holder_kept() {
        let mut e = ev(Some("TealDune"));
        e.pane_alive = true;
        assert_eq!(
            next_action(&e, "LucidFalcon"),
            CaptainAction::Keep {
                pin: "TealDune".into()
            }
        );
        assert!(!holder_stale(&e));
    }

    #[test]
    fn stale_holder_cleared_then_claimed() {
        let e = ev(Some("DeadPin"));
        assert!(holder_stale(&e));
        let a = next_action(&e, "LucidFalcon");
        assert_eq!(
            a,
            CaptainAction::Claim {
                pin: "LucidFalcon".into(),
                previous_stale: true
            }
        );
        assert!(claim_line(&a).unwrap().contains("previous holder stale"));
    }

    #[test]
    fn override_always_wins() {
        let mut e = ev(Some("TealDune"));
        e.pane_alive = true;
        e.override_pin = Some("HumanPin".into());
        assert_eq!(
            next_action(&e, "LucidFalcon"),
            CaptainAction::Override {
                pin: "HumanPin".into()
            }
        );
    }

    /// F10: roster-fresh but no pane / dispatcher / heartbeat → offer a
    /// rebind instead of silently keeping a ghost holder.
    #[test]
    fn roster_only_holder_offers_rebind() {
        let mut e = ev(Some("QuietAnvil"));
        e.presence_fresh = true; // whois.stale=false (7-day threshold)
        assert_eq!(rebind_hint(&e, false).as_deref(), Some("QuietAnvil"));
        // Any live signal silences the offer.
        assert!(rebind_hint(&e, true).is_none(), "heartbeat backs it");
        let mut pane = e.clone();
        pane.pane_alive = true;
        assert!(rebind_hint(&pane, false).is_none(), "pane backs it");
        let mut wake = e.clone();
        wake.last_wake_fresh = true;
        assert!(rebind_hint(&wake, false).is_none(), "dispatcher backs it");
        // An already-stale holder is CLAIMED (claim_line), not merely offered.
        let dead = ev(Some("QuietAnvil"));
        assert!(rebind_hint(&dead, false).is_none());
        // Override already wins — no offer.
        let mut ov = e.clone();
        ov.override_pin = Some("HumanPin".into());
        assert!(rebind_hint(&ov, false).is_none());
    }

    /// bd-8pis: dead-pin warnings fire only for targets with neither a
    /// bound pane nor a heartbeat — live signals stay silent.
    #[test]
    fn dead_pin_warnings_fire_only_without_live_signal() {
        use std::collections::HashSet;
        let pane: HashSet<String> = ["PlumGoose".to_string()].into_iter().collect();
        let heart: HashSet<String> = ["TealDune".to_string()].into_iter().collect();
        // Bound pane: silent. Heartbeat (CLI actor, no pane): silent.
        assert!(dead_pin_warnings(&["PlumGoose".into()], &pane, &heart).is_empty());
        assert!(dead_pin_warnings(&["TealDune".into()], &pane, &heart).is_empty());
        // Roster-only pin (bound to nothing, no heartbeat): loud.
        let warns = dead_pin_warnings(&["QuietHeron".into()], &pane, &heart);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("QuietHeron"), "{}", warns[0]);
        assert!(warns[0].contains("advisory"), "{}", warns[0]);
        // Lone @ stripped; empty targets warn nothing.
        assert_eq!(
            dead_pin_warnings(&["@QuietHeron".into()], &pane, &heart).len(),
            1
        );
        assert!(dead_pin_warnings(&[], &pane, &heart).is_empty());
    }

    /// bd-8pis: bound-but-dead panes warn; daemon-down trusts the
    /// binding; unbound never lives.
    #[test]
    fn dead_pin_bound_pane_needs_live_herdr() {
        use std::collections::HashSet;
        let live: HashSet<String> = ["w1W:p2".to_string()].into_iter().collect();
        assert!(bound_pane_is_live(Some("w1W:p2"), Some(&live)));
        assert!(!bound_pane_is_live(Some("w6:p1"), Some(&live)));
        assert!(bound_pane_is_live(Some("w6:p1"), None));
        assert!(!bound_pane_is_live(None, Some(&live)));
        assert!(!bound_pane_is_live(None, None));
    }

    /// bd-8pis: the dead-fold line names the pin, the project, and the
    /// rebind verb — arrive and helm print the same text.
    #[test]
    fn dead_pin_fold_message_names_pin_and_rebind() {
        let line = dead_fold_message("flywheel", "QuietHeron");
        assert!(line.contains("QuietHeron"), "{line}");
        assert!(line.contains("--project flywheel"), "{line}");
        assert!(line.contains("may reach nobody"), "{line}");
    }

    #[test]
    fn whois_fresh_cli_actor_is_not_stolen() {
        // TealDune is the flywheel mail CLI actor — no herdr pane, not stale.
        let mut e = ev(Some("TealDune"));
        e.presence_fresh = true;
        assert_eq!(
            next_action(&e, "LucidFalcon"),
            CaptainAction::Keep {
                pin: "TealDune".into()
            }
        );
        assert!(!holder_stale(&e));
    }
}
