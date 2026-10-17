//! Phase 4b.4: the typed `delta` of a `redirect` steer.
//!
//! §6A.4 makes a redirect's new contract a typed `delta{goal?, acceptance?,
//! scope.paths?, priority?, budget?}`. The CLI validates a JSON file against
//! that set. The host puts the object on the `steer@1` body (never a pane
//! write: panes carry no payload). The supervisor log keeps `{seq, op,
//! delta_sha, offset}`, not the delta bytes, so the fold stays small and a
//! decision never depends on the mail store's retention.
//!
//! The delta's bytes travel from the CLI to the host through a content-addressed
//! side store next to the context log (`<key>.deltas/<sha>.json`, owner-only):
//! the input file carries the sha, the host reads the bytes by sha and refuses
//! a file whose hash is not its name.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The largest delta, in bytes of canonical JSON.
pub const MAX_DELTA_BYTES: usize = 8 * 1024;
/// The typed key set (`scope` holds only `paths`).
pub const KEYS: [&str; 5] = ["goal", "acceptance", "scope", "priority", "budget"];

/// A validated delta in canonical form (sorted keys, no whitespace).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delta {
    /// The canonical JSON bytes (what the sha is over and what the body holds).
    pub json: String,
    /// Lowercase hex sha256 of `json`.
    pub sha: String,
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Whether `s` is a well-formed delta sha (64 lowercase hex digits).
pub fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Validate raw delta bytes: UTF-8, at most [`MAX_DELTA_BYTES`], a JSON object,
/// only the typed keys with the right shapes, and not empty.
pub fn parse(raw: &[u8]) -> Result<Delta> {
    if raw.len() > MAX_DELTA_BYTES * 2 {
        bail!("delta is {} bytes (max {MAX_DELTA_BYTES})", raw.len());
    }
    let text = std::str::from_utf8(raw).map_err(|_| anyhow!("delta is not UTF-8"))?;
    let v: Value = serde_json::from_str(text).map_err(|e| anyhow!("delta is not JSON: {e}"))?;
    let Value::Object(map) = &v else {
        bail!("delta must be a JSON object");
    };
    if map.is_empty() {
        bail!("delta is empty");
    }
    for (k, val) in map {
        match k.as_str() {
            "goal" | "acceptance" => {
                if !val.is_string() {
                    bail!("delta.{k} must be a string");
                }
            }
            "priority" => {
                if !(val.is_string() || val.is_number()) {
                    bail!("delta.priority must be a string or a number");
                }
            }
            "budget" => {
                if !(val.is_number() || val.is_object()) {
                    bail!("delta.budget must be a number or an object");
                }
            }
            "scope" => {
                let ok = val.as_object().is_some_and(|o| {
                    o.len() == 1
                        && o.get("paths")
                            .and_then(Value::as_array)
                            .is_some_and(|a| a.iter().all(Value::is_string))
                });
                if !ok {
                    bail!("delta.scope must be {{\"paths\": [strings]}}");
                }
            }
            other => bail!("unknown delta key {other:?} (allowed: {})", KEYS.join(", ")),
        }
    }
    let json = serde_json::to_string(&v)?;
    if json.len() > MAX_DELTA_BYTES {
        bail!("delta is {} bytes (max {MAX_DELTA_BYTES})", json.len());
    }
    let sha = sha_hex(json.as_bytes());
    Ok(Delta { json, sha })
}

fn dir_for(base: &Path, project: &str) -> PathBuf {
    base.join(format!("{}.deltas", crate::state::state_key(project)))
}

/// CLI side: write `d` into the side store (owner-only), return its sha.
pub fn store(base: &Path, project: &str, d: &Delta) -> Result<String> {
    use std::io::Write as _;
    let dir = dir_for(base, project);
    crate::private_fs::create_private_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", d.sha));
    let mut f = crate::private_fs::create_private_file(&path)?;
    f.write_all(d.json.as_bytes())?;
    Ok(d.sha.clone())
}

/// Host side: the delta named `sha`, refused unless its bytes hash to `sha`.
pub fn load(base: &Path, project: &str, sha: &str) -> Result<Delta> {
    if !is_sha(sha) {
        bail!("not a delta sha: {sha:?}");
    }
    let path = dir_for(base, project).join(format!("{sha}.json"));
    let raw = std::fs::read(&path).map_err(|e| anyhow!("delta {sha} is not in the store: {e}"))?;
    if sha_hex(&raw) != sha {
        bail!("delta file for {sha} does not hash to its name");
    }
    parse(&raw).inspect(|d| debug_assert_eq!(d.sha, sha))
}

/// Text form of a steer (header, then the delta under its sha). The live
/// mail body is `steer@1`, not this string. The pane never sees either.
#[cfg(test)]
pub fn body(header: &str, delta: Option<&Delta>) -> String {
    match delta {
        Some(d) => format!("{header}\ndelta sha256:{}\n{}", d.sha, d.json),
        None => header.to_string(),
    }
}

/// The delta JSON in a steer body, if it has one (for tests and the doctor).
#[cfg(test)]
pub fn delta_in_body(body: &str) -> Option<(&str, &str)> {
    let rest = body.split_once("\ndelta sha256:")?.1;
    rest.split_once('\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Delta {
        parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn the_typed_keys_validate_and_canonicalise() {
        let d = ok(
            r#"{"priority": 2, "goal": "ship it", "scope": {"paths": ["src/a.rs"]}, "acceptance": "tests pass", "budget": {"usd": 3}}"#,
        );
        assert_eq!(
            d.json,
            r#"{"acceptance":"tests pass","budget":{"usd":3},"goal":"ship it","priority":2,"scope":{"paths":["src/a.rs"]}}"#
        );
        assert_eq!(d.sha, sha_hex(d.json.as_bytes()));
        assert!(is_sha(&d.sha));
        // The same delta in another key order and spacing is the same delta.
        assert_eq!(
            ok(r#"{"goal":"ship it"}"#).sha,
            ok("{ \"goal\" :\n \"ship it\" }").sha
        );
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let e = parse(br#"{"goal":"x","shell":"rm -rf /"}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown delta key"), "{e}");
    }

    #[test]
    fn shapes_are_checked() {
        for bad in [
            r#"{"goal": 1}"#,
            r#"{"acceptance": ["a"]}"#,
            r#"{"priority": true}"#,
            r#"{"budget": "lots"}"#,
            r#"{"scope": ["a"]}"#,
            r#"{"scope": {"paths": "a"}}"#,
            r#"{"scope": {"paths": [1]}}"#,
            r#"{"scope": {"paths": [], "extra": 1}}"#,
            r#"[]"#,
            r#""goal""#,
            r#"{}"#,
            r#"not json"#,
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn non_utf8_is_rejected() {
        assert!(
            parse(&[b'{', 0xff, 0xfe, b'}'])
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
    }

    #[test]
    fn the_size_cap_is_on_the_canonical_form() {
        let at = format!(r#"{{"goal":"{}"}}"#, "a".repeat(MAX_DELTA_BYTES - 11));
        assert_eq!(at.len(), MAX_DELTA_BYTES);
        assert!(parse(at.as_bytes()).is_ok(), "exactly the cap is allowed");
        let over = format!(r#"{{"goal":"{}"}}"#, "a".repeat(MAX_DELTA_BYTES - 10));
        assert!(
            parse(over.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("max")
        );
        // Whitespace padding does not count against the delta.
        let padded = format!("{}{}", r#"{"goal":"x"}"#, " ".repeat(MAX_DELTA_BYTES + 100));
        assert!(parse(padded.as_bytes()).is_ok());
    }

    #[test]
    fn the_store_round_trips_and_refuses_a_file_that_is_not_its_name() {
        let d = tempfile::tempdir().unwrap();
        let delta = ok(r#"{"goal":"g"}"#);
        let sha = store(d.path(), "proj", &delta).unwrap();
        assert_eq!(load(d.path(), "proj", &sha).unwrap(), delta);
        // Tamper: same name, other bytes.
        let p = dir_for(d.path(), "proj").join(format!("{sha}.json"));
        std::fs::write(&p, br#"{"goal":"evil"}"#).unwrap();
        assert!(
            load(d.path(), "proj", &sha)
                .unwrap_err()
                .to_string()
                .contains("does not hash")
        );
        assert!(
            load(d.path(), "proj", "../../etc/passwd").is_err(),
            "a sha is hex, never a path"
        );
        assert!(load(d.path(), "proj", &"0".repeat(64)).is_err(), "absent");
    }

    #[test]
    fn the_body_carries_the_delta_and_the_hash_matches() {
        let d = ok(r#"{"goal":"g","acceptance":"a"}"#);
        let b = body("steer redirect on x", Some(&d));
        assert!(b.starts_with("steer redirect on x\n"));
        let (sha, json) = delta_in_body(&b).unwrap();
        assert_eq!(sha, d.sha);
        assert_eq!(
            sha_hex(json.as_bytes()),
            sha,
            "the body hash equals the logged sha"
        );
        assert_eq!(body("h", None), "h");
        assert!(delta_in_body("h").is_none());
    }
}
