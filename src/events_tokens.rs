//! Per-seat events token lifecycle (bd-5vi2x, ADR-0039 D6).
//!
//! ONE module shared by the Claude channel bridge (bd-si4b5) and the doorbell
//! subscriber (bd-l5vb2). The operator-authorized side mints a pin token,
//! writes the plaintext to a 0600 file, and journals ONLY hashes — never the
//! secret. Consumers read the file by path (`FLYWHEEL_TORON_TOKEN_FILE`) and
//! re-read it on 401 so a rotation takes effect without a restart.
//!
//! Every decision is a pure function; the CLI and the clock are ports. Tests
//! use a fake CLI and a temp state dir (TestSandbox), never the operator's
//! `~/.config/toron`, `~/.claude.json`, or a live daemon.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// Pin tokens cap at 24h (toron CLI rejects larger). Rotate well before that.
pub const PIN_TTL_SECS: u64 = 86_400;
pub const ROTATE_BEFORE_SECS: u64 = 3_600;

/// What a consumer needs to know about a token: hashes and times only. There
/// is deliberately no `plaintext` field anywhere in this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenState {
    pub hash: String,
    pub minted_us: i64,
    pub expires_us: i64,
    pub path: String,
}

impl TokenState {
    /// True when the token is within `before_us` of expiry (or expired).
    pub fn needs_rotation(&self, now_us: i64, before_us: i64) -> bool {
        now_us >= self.expires_us.saturating_sub(before_us)
    }
}

/// A rotation decision. The old hash is revoked AFTER the new token is safely
/// on disk, so a failed mint never leaves the seat with no credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RotationPlan {
    pub revoke_hash: String,
    pub ttl_secs: u64,
}

pub fn rotation_plan(
    state: &TokenState,
    now_us: i64,
    before_us: i64,
) -> Option<RotationPlan> {
    state.needs_rotation(now_us, before_us).then(|| RotationPlan {
        revoke_hash: state.hash.clone(),
        ttl_secs: PIN_TTL_SECS,
    })
}

/// SHA-256 of the plaintext — exactly toron's `hash_token`, so the revoke hash
/// is derivable locally without ever storing the secret.
pub fn token_hash(plaintext: &str) -> String {
    hex::encode(Sha256::digest(plaintext.as_bytes()))
}

/// Parse `toron events token mint` stdout. The plaintext is returned to the
/// caller, which writes it straight to the 0600 file; it must never be logged
/// or put in a journal row.
pub fn parse_mint_output(stdout: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("token mint output is not json: {e}"))?;
    let token = v
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| "token mint output has no token field".to_string())?;
    if token.is_empty() || token.len() > 1024 {
        return Err("token mint output has an implausible token".to_string());
    }
    Ok(token.to_string())
}

/// Journal rows carry a hash and times, never a plaintext.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenEvent {
    MintIntent {
        pin: String,
        project: String,
        ttl_secs: u64,
    },
    Minted {
        hash: String,
        expires_us: i64,
    },
    RevokeIntent {
        hash: String,
    },
    Revoked {
        hash: String,
    },
}

impl TokenEvent {
    pub fn to_json_line(&self, now_us: i64) -> String {
        let (op, rest) = match self {
            Self::MintIntent {
                pin,
                project,
                ttl_secs,
            } => (
                "mint-intent",
                json!({"pin": pin, "project": project, "ttl_secs": ttl_secs}),
            ),
            Self::Minted { hash, expires_us } => (
                "minted",
                json!({"hash": hash, "expires_us": expires_us}),
            ),
            Self::RevokeIntent { hash } => ("revoke-intent", json!({"hash": hash})),
            Self::Revoked { hash } => ("revoked", json!({"hash": hash})),
        };
        let mut row = rest;
        row["op"] = json!(op);
        row["ts_us"] = json!(now_us);
        serde_json::to_string(&row).unwrap_or_default()
    }
}

/// The toron CLI seam. Tests use a fake; production shells out with the
/// operator key already in the environment (never passed as an argument).
pub trait TokenCli {
    fn mint(&self, pin: &str, project: &str, ttl_secs: u64) -> Result<String, String>;
    fn revoke(&self, hash: &str) -> Result<(), String>;
}

pub struct ToronTokenCli {
    pub bin: String,
}

impl TokenCli for ToronTokenCli {
    fn mint(&self, pin: &str, project: &str, ttl_secs: u64) -> Result<String, String> {
        let out = std::process::Command::new(&self.bin)
            .args([
                "events",
                "token",
                "mint",
                "--pin",
                pin,
                "--project",
                project,
                "--ttl-secs",
                &ttl_secs.to_string(),
            ])
            .output()
            .map_err(|e| format!("toron token mint spawn failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "toron token mint failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        parse_mint_output(&String::from_utf8_lossy(&out.stdout))
    }

    fn revoke(&self, hash: &str) -> Result<(), String> {
        let out = std::process::Command::new(&self.bin)
            .args(["events", "token", "revoke", "--hash", hash])
            .output()
            .map_err(|e| format!("toron token revoke spawn failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "toron token revoke failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

fn journal(journal_path: &Path, row: &str, now_us: i64) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(parent) = journal_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("token journal dir failed: {e}"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path)
        .map_err(|e| format!("token journal open failed: {e}"))?;
    writeln!(file, "{row}").map_err(|e| format!("token journal write failed: {e}"))?;
    let _ = now_us;
    Ok(())
}

/// Write the plaintext to a 0600 file, atomically replacing any old token.
/// The file holds ONLY the token — no JSON, no metadata, no newline beyond one.
pub fn write_token_file(path: &Path, plaintext: &str) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("token dir failed: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("token file create failed: {e}"))?;
        file.write_all(plaintext.as_bytes())
            .map_err(|e| format!("token file write failed: {e}"))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, plaintext).map_err(|e| format!("token file write failed: {e}"))?;
    Ok(())
}

/// Read the current token from a file. Consumers call this on startup and
/// again on 401 so a rotated token is picked up without a restart.
pub fn read_token_file(path: &Path) -> Result<String, String> {
    let token = std::fs::read_to_string(path)
        .map_err(|e| format!("token file read failed: {e}"))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err("token file is empty".to_string());
    }
    Ok(token)
}

/// Mint a fresh pin token, journal the intent BEFORE the CLI runs and the hash
/// AFTER (no plaintext in either row), write the 0600 file, return its state.
pub fn mint_and_store<C: TokenCli>(
    cli: &C,
    pin: &str,
    project: &str,
    ttl_secs: u64,
    token_path: &Path,
    journal_path: &Path,
    now_us: i64,
) -> Result<TokenState, String> {
    let intent = TokenEvent::MintIntent {
        pin: pin.to_string(),
        project: project.to_string(),
        ttl_secs,
    }
    .to_json_line(now_us);
    journal(journal_path, &intent, now_us)?;

    let plaintext = cli.mint(pin, project, ttl_secs)?;
    write_token_file(token_path, &plaintext)?;
    let hash = token_hash(&plaintext);
    let done = TokenEvent::Minted {
        hash: hash.clone(),
        expires_us: now_us.saturating_add((ttl_secs as i64).saturating_mul(1_000_000)),
    }
    .to_json_line(now_us);
    journal(journal_path, &done, now_us)?;

    Ok(TokenState {
        hash,
        minted_us: now_us,
        expires_us: now_us.saturating_add((ttl_secs as i64).saturating_mul(1_000_000)),
        path: token_path.display().to_string(),
    })
}

/// Rotate an expiring token: mint+store the new one FIRST, then revoke the old
/// hash. Journal rows never contain plaintext.
pub fn rotate<C: TokenCli>(
    cli: &C,
    plan: &RotationPlan,
    pin: &str,
    project: &str,
    token_path: &Path,
    journal_path: &Path,
    now_us: i64,
) -> Result<TokenState, String> {
    let fresh = mint_and_store(
        cli,
        pin,
        project,
        plan.ttl_secs,
        token_path,
        journal_path,
        now_us,
    )?;
    let intent = TokenEvent::RevokeIntent {
        hash: plan.revoke_hash.clone(),
    }
    .to_json_line(now_us);
    journal(journal_path, &intent, now_us)?;
    cli.revoke(&plan.revoke_hash)?;
    let done = TokenEvent::Revoked {
        hash: plan.revoke_hash.clone(),
    }
    .to_json_line(now_us);
    journal(journal_path, &done, now_us)?;
    Ok(fresh)
}

/// Read the last `minted` row's hash and expiry from the journal. The rotation
/// tick uses this as the source of truth for what to revoke and when.
pub fn last_minted(journal_path: &Path) -> Result<(String, i64), String> {
    let body = std::fs::read_to_string(journal_path)
        .map_err(|e| format!("token journal read failed: {e}"))?;
    for line in body.lines().rev() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("op").and_then(Value::as_str) != Some("minted") {
            continue;
        }
        let hash = row
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| "minted row has no hash".to_string())?;
        let expires = row
            .get("expires_us")
            .and_then(Value::as_i64)
            .ok_or_else(|| "minted row has no expires_us".to_string())?;
        return Ok((hash.to_string(), expires));
    }
    Err("no minted row in token journal".to_string())
}

/// Where the per-seat token lives. Under the seat's state dir, never in the
/// MCP config (which carries only this path via FLYWHEEL_TORON_TOKEN_FILE).
pub fn token_path(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    state_dir
        .join("events-tokens")
        .join(format!("{}-{}.token", sanitize(project), sanitize(pin)))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeCli {
        minted: Vec<(String, u64)>,
    }
    impl TokenCli for FakeCli {
        fn mint(&self, _pin: &str, _project: &str, ttl: u64) -> Result<String, String> {
            let _ = ttl;
            Ok(format!("secret-token-{}", self.minted.len()))
        }
        fn revoke(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn rotation_triggers_before_expiry_not_after() {
        let state = TokenState {
            hash: "h".into(),
            minted_us: 0,
            expires_us: 1_000_000,
            path: "p".into(),
        };
        assert!(state.needs_rotation(700_000, 300_000));
        assert!(!state.needs_rotation(699_999, 300_000));
        let plan = rotation_plan(&state, 700_000, 300_000).expect("due");
        assert_eq!(plan.revoke_hash, "h");
        assert_eq!(plan.ttl_secs, PIN_TTL_SECS);
    }

    #[test]
    fn minted_token_is_0600_and_journal_has_no_plaintext() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_file = dir.path().join("seat.token");
        let journal_file = dir.path().join("journal.jsonl");
        let cli = FakeCli {
            minted: vec![],
        };
        let state = mint_and_store(
            &cli,
            "Seat",
            "proj",
            PIN_TTL_SECS,
            &token_file,
            &journal_file,
            5_000_000,
        )
        .expect("mint");
        assert_eq!(
            std::fs::read_to_string(&token_file).expect("token"),
            "secret-token-0"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&token_file)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the token file must be 0600: {mode:o}");
        }
        let journal = std::fs::read_to_string(&journal_file).expect("journal");
        assert!(!journal.contains("secret-token"), "no plaintext in journal: {journal}");
        assert!(journal.contains("mint-intent") && journal.contains("minted"));
        assert!(journal.contains(&state.hash));
    }

    #[test]
    fn mint_output_is_parsed_and_rejects_garbage() {
        assert_eq!(
            parse_mint_output(r#"{"token":"abc","principal":"x"}"#).unwrap(),
            "abc"
        );
        assert!(parse_mint_output("not json").is_err());
        assert!(parse_mint_output(r#"{"token":""}"#).is_err());
    }

    #[test]
    fn token_file_is_reread_after_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("seat.token");
        write_token_file(&path, "first").expect("write first");
        assert_eq!(read_token_file(&path).unwrap(), "first");
        write_token_file(&path, "rotated").expect("write rotated");
        assert_eq!(
            read_token_file(&path).unwrap(),
            "rotated",
            "a consumer re-reading on 401 picks up the new token"
        );
    }

    #[test]
    fn rotate_revokes_the_old_hash_after_the_new_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let token_file = dir.path().join("seat.token");
        let journal_file = dir.path().join("journal.jsonl");
        let cli = FakeCli {
            minted: vec![],
        };
        let plan = RotationPlan {
            revoke_hash: "old-hash".into(),
            ttl_secs: PIN_TTL_SECS,
        };
        let state = rotate(
            &cli,
            &plan,
            "Seat",
            "proj",
            &token_file,
            &journal_file,
            5_000_000,
        )
        .expect("rotate");
        assert_ne!(state.hash, "old-hash");
        let journal = std::fs::read_to_string(&journal_file).expect("journal");
        assert!(journal.contains("revoke-intent") && journal.contains("revoked"));
        assert!(!journal.contains("secret-token"));
    }
}
