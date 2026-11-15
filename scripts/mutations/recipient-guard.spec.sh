# Sourced by scripts/verify-mutations.sh.
# A divergent pin must refuse a name-addressed send (bd-pin-npub-binding-ggym).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(recipient_guard::tests::divergent_pin_send_refuses)

mutation "R1  divergent binding is allowed" \
    file=src/recipient_guard.rs \
    expect=divergent_pin_send_refuses \
    find=$'        Binding::Divergent {\n            name_npub,\n            pane_npub,\n        } => {\n            let (a, b) = (short(name_npub), short(pane_npub));\n            Err(format!(\n                "refusing to send to \\"{name}\\": the name is bound to npub {a} but the pane acting \\\n                 under it stamps npub {b} — a send addressed by name would be read by nobody. \\\n                 Repair: re-register the pin (flywheel mail register --adopt-unnamed), re-arrive \\\n                 the pane, or address the npub you mean directly."\n            ))\n        }' \
    replace=$'        Binding::Divergent {\n            name_npub: _,\n            pane_npub: _,\n        } => Ok(()),'
