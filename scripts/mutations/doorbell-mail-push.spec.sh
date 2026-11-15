# Sourced by scripts/verify-mutations.sh.
# Mail-delivered push must ring promptly and dedup by message identity (bd-0uzrq.18).

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(mail_delivered_watch_rings_once_then_poll_dedups two_mails_on_one_thread_to_idle_pin_give_two_rings)

mutation "M1  mail-delivered push mapping dropped" \
    file=crates/doorbell-core/src/lib.rs \
    expect=mail_delivered_watch_rings_once_then_poll_dedups \
    find=$'    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_mail_delivered {\n        return Some(EventClass::PlanePMail);\n    }\n' \
    replace=$'    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_mail_delivered {\n        return None;\n    }\n'

mutation "M2  thread key recorded in rung swallows same-thread mail" \
    file=crates/doorbell-core/src/lib.rs \
    expect=two_mails_on_one_thread_to_idle_pin_give_two_rings \
    find=$'    if !key.is_empty() && next.rung.contains(&key) {\n        actions.push(DoorbellAction::Audit {\n            kind: \"already_rung\".into(),\n            pin: pin.to_string(),\n            detail: line,\n        });\n        return;\n    }\n    if should_inject(status) {\n        match inject_gate_for_plane(status, is_act_now_line(&line)) {\n            InjectGate::Inject => {\n                if !key.is_empty() {\n                    next.rung.insert(key);\n                }\n                actions.push(DoorbellAction::Prompt {\n                    pin: pin.to_string(),\n                    line,\n                });\n            }' \
    replace=$'    let rung_id = thread\n        .as_deref()\n        .filter(|t| !t.is_empty())\n        .unwrap_or(key.as_str());\n    if !rung_id.is_empty() && next.rung.contains(rung_id) {\n        actions.push(DoorbellAction::Audit {\n            kind: \"already_rung\".into(),\n            pin: pin.to_string(),\n            detail: line,\n        });\n        return;\n    }\n    if should_inject(status) {\n        match inject_gate_for_plane(status, is_act_now_line(&line)) {\n            InjectGate::Inject => {\n                if !rung_id.is_empty() {\n                    next.rung.insert(rung_id.to_string());\n                }\n                actions.push(DoorbellAction::Prompt {\n                    pin: pin.to_string(),\n                    line,\n                });\n            }'
