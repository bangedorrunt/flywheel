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

fn format_env_argv_cmd(env_pairs: &[(String, String)], argv: &[String]) -> String {
    let mut parts = vec!["env".to_string()];
    for (k, v) in env_pairs {
        parts.push(format!("{k}={}", sh_quote(&v.replace(['\n', '\r'], " "))));
    }
    for a in argv {
        parts.push(sh_quote(&a.replace(['\n', '\r'], " ")));
    }
    parts.join(" ")
}

#[cfg(test)]
pub fn build_rex_burst_shell_cmd(
    env_pairs: &[(String, String)],
    argv: &[String],
    output_file: &Path,
) -> String {
    format!(
        "{} | tee {}",
        format_env_argv_cmd(env_pairs, argv),
        sh_quote(&output_file.display().to_string())
    )
}

pub fn build_rex_burst_shell_cmd_with_sentinel(
    env_pairs: &[(String, String)],
    argv: &[String],
    output_file: &Path,
    exit_file: &Path,
) -> String {
    let out_q = sh_quote(&output_file.display().to_string());
    let exit_tmp_q = sh_quote(&format!("{}.tmp", exit_file.display()));
    let exit_q = sh_quote(&exit_file.display().to_string());
    format!(
        "({} 2>&1; printf '%s\\n' \"$?\" > {exit_tmp_q} && mv {exit_tmp_q} {exit_q}) | tee {out_q}",
        format_env_argv_cmd(env_pairs, argv)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BurstOutcome {
    Succeeded {
        pane_id: String,
        exit_code: i32,
        output: String,
    },
    TypedFailure {
        pane_id: String,
        exit_code: i32,
        output: String,
    },
    TimedOut {
        pane_id: String,
        partial_output: String,
    },
}

trait BurstRunner {
    fn run_burst(
        &self,
        ws_id: &str,
        dir: &Path,
        shell_cmd: &str,
        out_file: &Path,
        exit_file: &Path,
        timeout: std::time::Duration,
    ) -> Result<BurstOutcome>;
}

struct RexBurstRunner<'a> {
    cfg: &'a crate::config::Config,
}

impl BurstRunner for RexBurstRunner<'_> {
    fn run_burst(
        &self,
        ws_id: &str,
        dir: &Path,
        shell_cmd: &str,
        out_file: &Path,
        exit_file: &Path,
        timeout: std::time::Duration,
    ) -> Result<BurstOutcome> {
        let backend = crate::backend::factory::multiplexer_backend(self.cfg)?;
        let panes = backend.pane_list(Some(ws_id))?;
        let prev = panes
            .last()
            .map(|p| p.pane_id.clone())
            .ok_or_else(|| anyhow!("burst: no panes in workspace {ws_id}"))?;
        let split = backend
            .pane_split(
                &prev,
                "down",
                Some(&crate::utf8_path(dir, "workspace dir")?),
            )
            .map_err(|e| anyhow!("burst: rex split in {ws_id}: {e:#}"))?;
        crate::pane::wait_rex_shell_ready(
            &*backend,
            &split.pane_id,
            "burst",
            &split.pane_id,
            std::time::Duration::from_secs(15),
        )?;
        let launch_state_dir = out_file.parent().unwrap_or(dir);
        let prepared = crate::pane::prepare_rex_launch_command(
            launch_state_dir,
            "burst",
            &std::collections::BTreeMap::new(),
            "sh",
            &["-c".to_string(), shell_cmd.to_string()],
        )?;
        backend
            .pane_run(&split.pane_id, &prepared)
            .map_err(|e| anyhow!("burst: rex run in {}: {e:#}", split.pane_id))?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            if exit_file.exists() {
                let raw_code = fs::read_to_string(exit_file).unwrap_or_default();
                let exit_code = raw_code.trim().parse::<i32>().unwrap_or(1);
                let output = fs::read_to_string(out_file).unwrap_or_default();
                return Ok(if exit_code == 0 {
                    BurstOutcome::Succeeded {
                        pane_id: split.pane_id,
                        exit_code,
                        output,
                    }
                } else {
                    BurstOutcome::TypedFailure {
                        pane_id: split.pane_id,
                        exit_code,
                        output,
                    }
                });
            }
            if std::time::Instant::now() >= deadline {
                let partial_output = fs::read_to_string(out_file).unwrap_or_default();
                return Ok(BurstOutcome::TimedOut {
                    pane_id: split.pane_id,
                    partial_output,
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
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
    cfg: &crate::config::Config,
    project: &str,
    kind: &str,
    stage: &str,
    prompt: &str,
    resume: Option<&str>,
    model: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let state_dir = crate::config::state_dir();
    let project_dir = crate::project_dir(cfg, project);
    let ws_label = crate::state::load_spawn_meta(project);
    let runner = RexBurstRunner { cfg };
    let out = execute_burst(
        &runner,
        &state_dir,
        &project_dir,
        ws_label.as_deref(),
        project,
        kind,
        stage,
        prompt,
        resume,
        model,
        dry_run,
        std::time::Duration::from_secs(120),
    )?;
    match out {
        BurstExecResult::DryRun(payload) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                let harness = payload["profile"]["harness"].as_str().unwrap_or(kind);
                let shell_cmd = payload["shell_cmd"].as_str().unwrap_or("");
                println!("burst (dry-run) [{project}] {harness} {stage}:\n{shell_cmd}");
            }
        }
        BurstExecResult::Executed {
            entry,
            journal_path,
        } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                println!(
                    "burst journaled {} (exit={:?}) -> {}",
                    entry.run_id,
                    entry.exit_code,
                    journal_path.display()
                );
            }
        }
    }
    Ok(())
}

enum BurstExecResult {
    DryRun(serde_json::Value),
    Executed {
        entry: BurstJournalEntry,
        journal_path: PathBuf,
    },
}

#[allow(clippy::too_many_arguments)]
fn execute_burst(
    runner: &dyn BurstRunner,
    state_dir: &Path,
    project_dir: &Path,
    ws_label: Option<&str>,
    project: &str,
    kind: &str,
    stage: &str,
    prompt: &str,
    resume: Option<&str>,
    model: Option<&str>,
    dry_run: bool,
    timeout: std::time::Duration,
) -> Result<BurstExecResult> {
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
    let run_id = format!("burst-{}", chrono::Utc::now().timestamp_millis());
    let burst_dir = state_dir.join("burst");
    let out_file = burst_dir.join(format!("{}-{run_id}.log", crate::state::state_key(project)));
    let exit_file = burst_dir.join(format!(
        "{}-{run_id}.exit",
        crate::state::state_key(project)
    ));
    let shell_cmd =
        build_rex_burst_shell_cmd_with_sentinel(&env_pairs, &argv, &out_file, &exit_file);
    let journal_path = burst_journal_path(state_dir, project);

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
        return Ok(BurstExecResult::DryRun(payload));
    }

    let Some(ws_id) = ws_label else {
        bail!(
            "burst: no live rex workspace for {project}; pass --dry-run or start a workspace first"
        );
    };
    fs::create_dir_all(&burst_dir)?;
    let _ = fs::remove_file(&exit_file);
    let outcome = runner.run_burst(
        ws_id,
        project_dir,
        &shell_cmd,
        &out_file,
        &exit_file,
        timeout,
    )?;
    let (pane_id, exit_code, outcome_text) = match outcome {
        BurstOutcome::Succeeded {
            pane_id,
            exit_code,
            output,
        } => (pane_id, Some(exit_code), output),
        BurstOutcome::TypedFailure {
            pane_id,
            exit_code,
            output,
        } => (pane_id, Some(exit_code), output),
        BurstOutcome::TimedOut {
            pane_id,
            partial_output,
        } => (
            pane_id,
            None,
            if partial_output.trim().is_empty() {
                "[timed out]".to_string()
            } else {
                format!("[timed out]\n{partial_output}")
            },
        ),
    };
    let entry = BurstJournalEntry {
        run_id,
        project: project.to_string(),
        stage: stage.to_string(),
        harness: profile.harness,
        pane_id: Some(pane_id),
        resume_id: profile.resume_id,
        prompt_bytes: composed.len(),
        exit_code,
        outcome_text,
        recorded_at_unix: chrono::Utc::now().timestamp(),
    };
    record_burst_journal(&journal_path, &entry)?;
    Ok(BurstExecResult::Executed {
        entry,
        journal_path,
    })
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

    struct FakeBurstRunner {
        outcome: std::sync::Mutex<Option<BurstOutcome>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeBurstRunner {
        fn new(outcome: BurstOutcome) -> Self {
            Self {
                outcome: std::sync::Mutex::new(Some(outcome)),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl BurstRunner for FakeBurstRunner {
        fn run_burst(
            &self,
            _ws_id: &str,
            _dir: &Path,
            _shell_cmd: &str,
            _out_file: &Path,
            _exit_file: &Path,
            _timeout: std::time::Duration,
        ) -> Result<BurstOutcome> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.outcome.lock().unwrap().take().unwrap())
        }
    }

    #[test]
    fn execute_burst_dry_run_emits_no_runner_call_or_journal() {
        let dir = std::env::temp_dir().join(format!("fw-burst-dry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let runner = FakeBurstRunner::new(BurstOutcome::Succeeded {
            pane_id: "p1".into(),
            exit_code: 0,
            output: "unused".into(),
        });
        let res = execute_burst(
            &runner,
            &dir,
            &dir,
            Some("ws-1"),
            "flywheel",
            "claude",
            "ce-review",
            "check diff",
            None,
            None,
            true,
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(res, BurstExecResult::DryRun(_)));
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            read_burst_journal(&burst_journal_path(&dir, "flywheel"))
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn execute_burst_distinguishes_succeeded_typed_failure_and_timeout() {
        let dir = std::env::temp_dir().join(format!("fw-burst-outcomes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (outcome, expected_exit, expected_text) in [
            (
                BurstOutcome::Succeeded {
                    pane_id: "p1".into(),
                    exit_code: 0,
                    output: "AGREE".into(),
                },
                Some(0),
                "AGREE",
            ),
            (
                BurstOutcome::TypedFailure {
                    pane_id: "p2".into(),
                    exit_code: 2,
                    output: "boom".into(),
                },
                Some(2),
                "boom",
            ),
            (
                BurstOutcome::TimedOut {
                    pane_id: "p3".into(),
                    partial_output: "".into(),
                },
                None,
                "[timed out]",
            ),
        ] {
            let runner = FakeBurstRunner::new(outcome);
            let res = execute_burst(
                &runner,
                &dir,
                &dir,
                Some("ws-1"),
                "flywheel",
                "claude",
                "ce-review",
                "check diff",
                None,
                None,
                false,
                std::time::Duration::from_secs(1),
            )
            .unwrap();
            match res {
                BurstExecResult::Executed { entry, .. } => {
                    assert_eq!(entry.exit_code, expected_exit);
                    assert_eq!(entry.outcome_text, expected_text);
                }
                BurstExecResult::DryRun(_) => panic!("expected Executed"),
            }
        }
        assert_eq!(
            read_burst_journal(&burst_journal_path(&dir, "flywheel"))
                .unwrap()
                .len(),
            3
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
