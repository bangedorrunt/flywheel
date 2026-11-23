//! `flywheel proof status` (bd-7qhz0): show the latest CI Proof runs.
//!
//! The proof train is a GitHub Actions workflow; this module is a read-only
//! projection over those runs. No local proof loop, no daemon, no reducer.

/// A CI Proof run, projected for `flywheel proof status`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CiRun {
    pub id: u64,
    pub sha: String,
    pub status: String,
    pub conclusion: Option<String>,
}

fn ci_runs() -> Vec<CiRun> {
    let out = std::process::Command::new("gh")
        .args([
            "run",
            "list",
            "--workflow",
            "Proof",
            "--limit",
            "10",
            "--json",
            "databaseId,headSha,status,conclusion",
        ])
        .output();
    let Ok(out) = out else { return vec![] };
    if !out.status.success() {
        return vec![];
    }
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return vec![];
    };
    doc.as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some(CiRun {
                id: r.get("databaseId")?.as_u64()?,
                sha: r.get("headSha")?.as_str()?.to_string(),
                status: r.get("status")?.as_str()?.to_string(),
                conclusion: r
                    .get("conclusion")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        })
        .collect()
}

pub fn cmd_status(json: bool) -> anyhow::Result<()> {
    let runs = ci_runs();
    if json {
        println!("{}", serde_json::to_string(&runs)?);
    } else {
        for run in runs {
            println!(
                "run {} sha={} status={} conclusion={}",
                run.id,
                run.sha,
                run.status,
                run.conclusion.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}
