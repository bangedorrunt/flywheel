//! Stripped burst profile and journaled one-shot Rex reviewers (`D5`).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstProfile {
    pub harness: String,
    pub stage: String,
    pub resume_id: Option<String>,
    pub model: Option<String>,
    pub disable_mcp: bool,
    pub disable_hooks: bool,
}

impl BurstProfile {
    pub fn stripped(
        harness: &str,
        stage: &str,
        resume_id: Option<String>,
        model: Option<String>,
    ) -> Self {
        Self {
            harness: harness.trim().to_ascii_lowercase(),
            stage: stage.trim().to_string(),
            resume_id,
            model,
            disable_mcp: true,
            disable_hooks: true,
        }
    }
}

pub fn burst_env(profile: &BurstProfile, pin: &str, project: &str) -> Vec<(String, String)> {
    let mut env = vec![
        ("AGENT_NAME".to_string(), pin.to_string()),
        ("FLYWHEEL_PROJECT".to_string(), project.to_string()),
        ("FLYWHEEL_BURST".to_string(), "1".to_string()),
    ];
    if profile.disable_hooks {
        env.push(("FLYWHEEL_CREW".to_string(), "1".to_string()));
        env.push((
            "CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_string(),
            "1".to_string(),
        ));
    }
    env
}

pub fn build_burst_argv(profile: &BurstProfile, prompt: &str) -> Result<Vec<String>> {
    let full_prompt = format!(
        "MODE crew (burst) — Read-only/bounded burst. Output ONLY the stage contract.\n\n{prompt}"
    );
    match profile.harness.as_str() {
        "claude" => {
            let mut argv = vec![
                "claude".to_string(),
                "-p".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ];
            if profile.disable_mcp {
                argv.push("--strict-mcp-config".to_string());
                argv.push("--mcp-config".to_string());
                argv.push(r#"{"mcpServers":{}}"#.to_string());
            }
            if let Some(resume) = &profile.resume_id {
                argv.push("--resume".to_string());
                argv.push(resume.clone());
            }
            argv.push(full_prompt);
            Ok(argv)
        }
        "codex" => {
            let mut argv = vec![
                "codex".to_string(),
                "exec".to_string(),
                "--json".to_string(),
            ];
            if let Some(resume) = &profile.resume_id {
                argv.push("resume".to_string());
                argv.push(resume.clone());
            }
            argv.push(full_prompt);
            Ok(argv)
        }
        "opencode" => {
            let mut argv = vec![
                "opencode".to_string(),
                "run".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ];
            if let Some(model) = &profile.model {
                argv.push("--model".to_string());
                argv.push(model.clone());
            }
            argv.push(full_prompt);
            Ok(argv)
        }
        "pi" => {
            let mut argv = vec![
                "pi".to_string(),
                "-p".to_string(),
                "--mode".to_string(),
                "json".to_string(),
            ];
            if profile.disable_mcp {
                argv.push("--no-extensions".to_string());
            }
            if let Some(model) = &profile.model {
                argv.push("--model".to_string());
                argv.push(model.clone());
            }
            argv.push(full_prompt);
            Ok(argv)
        }
        other => bail!("unsupported burst harness: {other}"),
    }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub fn build_rex_burst_shell_cmd(
    env_pairs: &[(String, String)],
    argv: &[String],
    output_file: &Path,
) -> String {
    let mut parts = vec!["env".to_string()];
    for (k, v) in env_pairs {
        parts.push(format!("{k}={}", sh_quote(&v.replace(['\n', '\r'], " "))));
    }
    for a in argv {
        parts.push(sh_quote(&a.replace(['\n', '\r'], " ")));
    }
    format!(
        "{} | tee {}",
        parts.join(" "),
        sh_quote(&output_file.display().to_string())
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstJournalEntry {
    pub run_id: String,
    pub project: String,
    pub stage: String,
    pub harness: String,
    pub pane_id: Option<String>,
    pub resume_id: Option<String>,
    pub prompt_bytes: usize,
    pub exit_code: Option<i32>,
    pub outcome_text: String,
    pub recorded_at_unix: i64,
}

pub fn burst_journal_path(state_dir: &Path, project: &str) -> PathBuf {
    state_dir
        .join("journals")
        .join(format!("burst-{}.jsonl", crate::state::state_key(project)))
}

pub fn record_burst_journal(journal_path: &Path, entry: &BurstJournalEntry) -> Result<()> {
    if let Some(parent) = journal_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path)?;
    let line = serde_json::to_string(entry)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[cfg(test)]
pub fn read_burst_journal(journal_path: &Path) -> Result<Vec<BurstJournalEntry>> {
    if !journal_path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(journal_path)?;
    let mut entries = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        entries.push(serde_json::from_str(line)?);
    }
    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_burst(
    _cfg: &crate::config::Config,
    project: &str,
    kind: &str,
    stage: &str,
    prompt: &str,
    resume: Option<&str>,
    model: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let lib = crate::stages::StageLibrary::compiled_in();
    let stage_prompt = lib
        .resolve(stage)
        .ok_or_else(|| anyhow!("unknown stage {stage:?}"))?;
    let composed = crate::pane::compose_dispatch_prompt(Some(stage_prompt), Some(prompt), prompt);
    let profile = BurstProfile::stripped(
        kind,
        stage,
        resume.map(str::to_string),
        model.map(str::to_string),
    );
    let pin = format!("burst-{}", profile.harness);
    let env_pairs = burst_env(&profile, &pin, project);
    let argv = build_burst_argv(&profile, &composed)?;
    let state_dir = crate::config::state_dir();
    let out_file = state_dir.join("burst").join(format!(
        "{}-{}.log",
        crate::state::state_key(project),
        stage
    ));
    let shell_cmd = build_rex_burst_shell_cmd(&env_pairs, &argv, &out_file);
    let journal_path = burst_journal_path(&state_dir, project);

    if dry_run {
        let payload = serde_json::json!({
            "dry_run": true,
            "project": project,
            "profile": profile,
            "env": env_pairs,
            "argv": argv,
            "shell_cmd": shell_cmd,
            "journal_path": journal_path,
        });
        if json {
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            println!(
                "burst (dry-run) [{project}] {} {stage}:\n{shell_cmd}",
                profile.harness
            );
        }
        return Ok(());
    }

    let Some(ws_label) = crate::state::load_spawn_meta(project) else {
        bail!(
            "burst: no live rex workspace for {project}; pass --dry-run or start a workspace first"
        );
    };
    let entry = BurstJournalEntry {
        run_id: format!("burst-{}", chrono::Utc::now().timestamp_millis()),
        project: project.to_string(),
        stage: stage.to_string(),
        harness: profile.harness.clone(),
        pane_id: Some(ws_label),
        resume_id: profile.resume_id.clone(),
        prompt_bytes: composed.len(),
        exit_code: Some(0),
        outcome_text: shell_cmd.clone(),
        recorded_at_unix: chrono::Utc::now().timestamp(),
    };
    record_burst_journal(&journal_path, &entry)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
    } else {
        println!(
            "burst journaled {} -> {}",
            entry.run_id,
            journal_path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripped_burst_profile_disables_hooks_and_mcp_per_harness() {
        let claude = BurstProfile::stripped("claude", "ce-review", Some("sess-1".into()), None);
        let env = burst_env(&claude, "burst-claude", "flywheel");
        assert!(env.contains(&("FLYWHEEL_CREW".into(), "1".into())));
        assert!(env.contains(&("FLYWHEEL_BURST".into(), "1".into())));
        let c_argv = build_burst_argv(&claude, "review diff").unwrap();
        assert!(c_argv.contains(&"--strict-mcp-config".into()));
        assert!(c_argv.contains(&"--resume".into()));

        let pi = BurstProfile::stripped("pi", "ce-verify", None, Some("openai/gpt-5".into()));
        let p_argv = build_burst_argv(&pi, "verify gate").unwrap();
        assert!(p_argv.contains(&"--no-extensions".into()));
        assert!(p_argv.contains(&"--model".into()));
    }

    #[test]
    fn build_rex_burst_shell_cmd_tees_output_and_stays_single_line() {
        let profile = BurstProfile::stripped("codex", "ce-review", None, None);
        let env = burst_env(&profile, "burst-codex", "flywheel");
        let argv = build_burst_argv(&profile, "line1\nline2").unwrap();
        let cmd = build_rex_burst_shell_cmd(&env, &argv, Path::new("/tmp/burst.log"));
        assert!(!cmd.contains('\n'));
        assert!(!cmd.contains('\r'));
        assert!(cmd.contains("| tee '/tmp/burst.log'"));
    }

    #[test]
    fn burst_journal_entry_roundtrips_and_appends_replayably() {
        let dir = std::env::temp_dir().join(format!("fw-burst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = burst_journal_path(&dir, "flywheel");
        let e1 = BurstJournalEntry {
            run_id: "r1".into(),
            project: "flywheel".into(),
            stage: "ce-review".into(),
            harness: "claude".into(),
            pane_id: Some("pane-1".into()),
            resume_id: None,
            prompt_bytes: 120,
            exit_code: Some(0),
            outcome_text: "AGREE".into(),
            recorded_at_unix: 1_700_000_000,
        };
        let e2 = BurstJournalEntry {
            run_id: "r2".into(),
            ..e1.clone()
        };
        record_burst_journal(&path, &e1).unwrap();
        record_burst_journal(&path, &e2).unwrap();
        let loaded = read_burst_journal(&path).unwrap();
        assert_eq!(loaded, vec![e1, e2]);
        let _ = fs::remove_dir_all(&dir);
    }
}
