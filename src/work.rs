//! br/bv pass-through for the work-graph commands. br/bv are the canonical
//! tracker surfaces; flywheel wraps them with consistent output and exit codes.

use std::process::Command;

use anyhow::{bail, Result};
use serde_json::Value;

pub struct Bv {
    pub cwd: String,
}

impl Bv {
    pub fn new(cwd: &str) -> Self {
        Bv {
            cwd: cwd.to_string(),
        }
    }

    /// Run `bv --robot-<flag>` and return parsed JSON (bv always emits JSON in robot mode).
    pub fn robot(&self, flag: &str) -> Result<Value> {
        let out = Command::new("bv")
            .arg(format!("--robot-{flag}"))
            .current_dir(&self.cwd)
            .output()
            .map_err(|e| anyhow::anyhow!("bv not found: {e}"))?;
        if !out.status.success() {
            bail!(
                "bv --robot-{flag} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(serde_json::from_str(stdout.trim())
            .unwrap_or_else(|_| serde_json::json!({ "raw": stdout.trim() })))
    }

    /// Run `br <cmd> --json` for tracker introspection.
    pub fn br_json(&self, args: &[&str]) -> Result<Value> {
        let out = Command::new("br")
            .args(args)
            .arg("--json")
            .current_dir(&self.cwd)
            .output()
            .map_err(|e| anyhow::anyhow!("br not found: {e}"))?;
        if !out.status.success() {
            bail!(
                "br {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(serde_json::from_str(stdout.trim())
            .unwrap_or_else(|_| serde_json::json!({ "raw": stdout.trim() })))
    }
}
