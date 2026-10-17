//! bd-pin-npub-binding-ggym: the send door's recipient guard.
//!
//! A pin name is a convenience, not an address. The mailbox binds a name to an
//! npub (G2: name resolution is the mailbox's), and the pane that answers to
//! that name can act under a different key — measured in this repo:
//! `toron mail whois flywheel Buffy` reported name npub `6778eb…` while the pane
//! stamped `facd8c…`. A send addressed by that name reports `ok` and the
//! recipient never reads it.
//!
//! The guard refuses only on POSITIVE two-sided divergence, the same rule
//! `flywheel status` already prints ([`crate::identity_divergence`]). A missing
//! side is not evidence: an unbound name, a mailbox that cannot be asked, or a
//! name with no pane still routes, because the mailbox decides — a guard that
//! refused on absence would take working mail dark without stopping a single
//! real divergence.

use serde_json::Value;

/// What kind of binding a recipient name has, read from the mailbox `whois`
/// payload (`npub`, `pane_npub`, `identity_divergent`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// Both sides are known and agree: the name is a safe address.
    Bound { npub: String },
    /// The name's npub and its pane's npub are both known and differ. The
    /// name routes to a key the pane does not read.
    Divergent {
        name_npub: String,
        pane_npub: String,
    },
    /// Nothing to compare: no whois row, or only one side known.
    Unknown,
}

/// True for a recipient that is already an explicit npub. The guard never
/// touches those: addressing the key directly is the operator's own door, and
/// the whole point of the repair.
pub fn is_explicit_npub(recipient: &str) -> bool {
    recipient.trim().to_ascii_lowercase().starts_with("npub1")
}

/// Read a binding out of a `whois` payload. A missing payload, a missing npub,
/// or a missing pane npub all read [`Binding::Unknown`].
pub fn binding_from_whois(whois: Option<&Value>) -> Binding {
    let Some(whois) = whois else {
        return Binding::Unknown;
    };
    let npub = whois.get("npub").and_then(Value::as_str).unwrap_or("");
    let pane_npub = whois.get("pane_npub").and_then(Value::as_str).unwrap_or("");
    let flagged = whois
        .get("identity_divergent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if npub.is_empty() || pane_npub.is_empty() {
        // One side is not evidence, but the mailbox's own flag still is.
        return if flagged {
            Binding::Divergent {
                name_npub: npub.to_string(),
                pane_npub: pane_npub.to_string(),
            }
        } else {
            Binding::Unknown
        };
    }
    if flagged || npub != pane_npub {
        Binding::Divergent {
            name_npub: npub.to_string(),
            pane_npub: pane_npub.to_string(),
        }
    } else {
        Binding::Bound {
            npub: npub.to_string(),
        }
    }
}

/// The guard. `Err` is the refusal text: it names the name, both sides, and
/// the repair, and it never suggests a silent reroute to the pane's key.
pub fn guard_recipient(name: &str, binding: &Binding) -> Result<(), String> {
    if is_explicit_npub(name) {
        return Ok(());
    }
    match binding {
        Binding::Divergent {
            name_npub,
            pane_npub,
        } => {
            let (a, b) = (short(name_npub), short(pane_npub));
            Err(format!(
                "refusing to send to \"{name}\": the name is bound to npub {a} but the pane acting \
                 under it stamps npub {b} — a send addressed by name would be read by nobody. \
                 Repair: re-register the pin (flywheel mail register --adopt-unnamed), re-arrive \
                 the pane, or address the npub you mean directly."
            ))
        }
        Binding::Bound { .. } | Binding::Unknown => Ok(()),
    }
}

/// The mailbox's own divergence flag, for a caller that already has the two
/// sides. Kept here so the guard and `status` cannot drift.
pub fn divergent(name_npub: &str, pane_npub: &str) -> bool {
    !name_npub.trim().is_empty() && !pane_npub.trim().is_empty() && name_npub != pane_npub
}

fn short(npub: &str) -> String {
    if npub.is_empty() {
        return "(unbound)".to_string();
    }
    npub.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// bd-pin-npub-binding-ggym: a name whose pane acts under another key must
    /// refuse the send and name the repair, and the absent cases must not.
    #[test]
    fn divergent_pin_send_refuses() {
        let live = json!({
            "name": "Buffy",
            "npub": "6778eb30f5bb7b08ac2a47049378ff6b5c7fcc4f6a9c227f2ef5e06809999469",
            "pane_npub": "facd8c1f2a3b4c5d6e7f8091a2b3c4d5e6f70819a2b3c4d5e6f70819a2b3c4d5",
            "identity_divergent": true,
            "pane_id": "w22:p2",
        });
        let binding = binding_from_whois(Some(&live));
        let Binding::Divergent {
            name_npub,
            pane_npub,
        } = binding.clone()
        else {
            panic!("the live whois payload is divergent: {binding:?}");
        };
        assert_eq!(
            name_npub,
            "6778eb30f5bb7b08ac2a47049378ff6b5c7fcc4f6a9c227f2ef5e06809999469"
        );
        assert_eq!(
            pane_npub,
            "facd8c1f2a3b4c5d6e7f8091a2b3c4d5e6f70819a2b3c4d5e6f70819a2b3c4d5"
        );
        let refusal = guard_recipient("Buffy", &binding).expect_err("a divergent pin must refuse");
        assert!(refusal.contains("6778eb30f5bb"), "{refusal}");
        assert!(refusal.contains("facd8c1f2a3b"), "{refusal}");
        assert!(
            refusal.contains("re-register") && refusal.contains("npub you mean"),
            "names the repair: {refusal}"
        );

        // Both sides known and equal: the name is a safe address.
        let same = json!({ "npub": "aa11", "pane_npub": "aa11", "identity_divergent": false });
        let bound = binding_from_whois(Some(&same));
        assert_eq!(
            bound,
            Binding::Bound {
                npub: "aa11".into()
            }
        );
        assert!(guard_recipient("Someone", &bound).is_ok());

        // Absence is not evidence: no row, a half row, no pane stamp.
        assert!(guard_recipient("Nobody", &binding_from_whois(None)).is_ok());
        let half = json!({ "npub": "aa11", "identity_divergent": false });
        assert_eq!(binding_from_whois(Some(&half)), Binding::Unknown);
        assert!(guard_recipient("Nobody", &binding_from_whois(Some(&half))).is_ok());
        let no_pane = json!({ "npub": "aa11" });
        assert!(guard_recipient("Nobody", &binding_from_whois(Some(&no_pane))).is_ok());

        // An explicit npub is the repair, never a refusal.
        let still_divergent = binding_from_whois(Some(&live));
        assert!(guard_recipient("npub1qqqqqq", &still_divergent).is_ok());
        assert!(is_explicit_npub("npub1abc"));
        assert!(!is_explicit_npub("Buffy"));

        // The guard agrees with the flag the mailbox sets.
        assert!(divergent("aa", "bb"));
        assert!(!divergent("aa", "aa"));
        assert!(!divergent("", "bb"), "a missing side is not divergence");
    }
}
