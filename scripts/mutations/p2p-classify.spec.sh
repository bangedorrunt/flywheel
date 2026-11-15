# Sourced by scripts/verify-mutations.sh.
# A peer mail is a WAKE even with an empty subject (its body carries the
# structured verb), while routine prose is not (bd-ncgyf).

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(a_p2p_mail_is_a_peer_wake_never_noise)

mutation "P2  a p2p mail with no subject is classified as noise" \
    file=crates/doorbell-core/src/lib.rs \
    expect=a_p2p_mail_is_a_peer_wake_never_noise \
    find=$'        .or_else(|| {\n            body_json.as_ref().and_then(|j| {\n                j.get("verb").and_then(|x| x.as_str()).map(|verb| {\n                    (\n                        verb.to_string(),\n                        j.get("path")\n                            .and_then(|p| p.as_str())\n                            .unwrap_or("")\n                            .to_string(),\n                    )\n                })\n            })\n        });\n' \
    replace=$'        ;\n'
