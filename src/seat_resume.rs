//! A seat resumed in a new pane keeps its conversation and its wakes.
//!
//! Each harness reopens a session its own way (`harness_registry::SessionResume`).
//! `flywheel resume <pin>` re-binds the seat to the pane it runs in and starts
//! the harness there with the seat's env and the resume flag.

use anyhow::{Result, anyhow, bail};

use crate::harness_registry::{SessionResume, canonical, deprecated_for, session_resume};

/// Recorded ids may carry a harness prefix (`agy:<conversation>`).
fn bare_id(recorded: &str) -> &str {
    recorded.split_once(':').map_or(recorded, |(_, id)| id)
}

/// The argv that reopens a seat's session, from what the registry recorded.
pub fn resume_args(harness: &str, recorded: Option<&str>) -> Result<Vec<String>> {
    let recorded = recorded.map(str::trim).filter(|s| !s.is_empty());
    match session_resume(harness) {
        SessionResume::ChosenId { resume, .. } | SessionResume::RecordedId { resume } => {
            let id = recorded.ok_or_else(|| {
                anyhow!(
                    "resume: this {harness} seat recorded no session id, so it cannot reopen it \
                     (seats spawned before resume support, or an agy seat whose hook never ran)"
                )
            })?;
            Ok(vec![resume.to_string(), bare_id(id).to_string()])
        }
        SessionResume::SeatDirContinue { flag } => Ok(vec![flag.to_string()]),
        SessionResume::Unsupported => {
            bail!("resume: harness {harness} cannot reopen a seat session")
        }
    }
}

/// The full resume argv: the session flag plus what every seat of that harness
/// carries (Claude's plugin dir).
pub fn resume_launch_args(harness: &str, recorded: Option<&str>) -> Result<Vec<String>> {
    let mut args = resume_args(harness, recorded)?;
    args.extend(crate::cmd::spawn::claude_plugin_args(harness));
    Ok(args)
}

/// One line for a harness that has been replaced, or `None`.
pub fn deprecation_notice(harness: &str) -> Option<String> {
    deprecated_for(harness).map(|new| {
        format!(
            "flywheel: harness {} is deprecated; use {new} instead",
            canonical(harness)
        )
    })
}

/// `flywheel resume <pin>`: run inside the pane that should hold the seat.
pub fn cmd_resume(cfg: &crate::Config, pin: &str, dry_run: bool) -> Result<()> {
    let pane = std::env::var("REX_BLOCK")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "resume: run this inside the Rex pane that should hold {pin} (REX_BLOCK is unset)"
            )
        })?;
    let (project, seat, archived) = match crate::state::find_seat_by_pin(pin) {
        Some((project, seat)) => (project, seat, false),
        None => crate::state::find_resumable_by_pin(pin)
            .map(|(project, seat)| (project, seat, true))
            .ok_or_else(|| anyhow!("resume: no live or resumable seat for {pin}"))?,
    };
    let harness = seat
        .harness
        .clone()
        .ok_or_else(|| anyhow!("resume: seat {pin} recorded no harness"))?;
    if let Some(line) = deprecation_notice(&harness) {
        eprintln!("{line}");
    }
    let args = resume_launch_args(&harness, seat.harness_session.as_deref())?;
    let mut env = crate::pane::pane_env_for_launch(cfg, seat.mail_identity.as_deref(), None)?;
    let cli = crate::events_tokens::ToronTokenCli {
        bin: std::env::var("TORON_BIN").unwrap_or_else(|_| "toron".to_string()),
    };
    crate::cmd::spawn::prepare_seat_events_before_launch(
        &cli,
        &crate::config::state_dir(),
        &harness,
        &project,
        seat.mail_identity.as_deref(),
        &mut env,
    );
    let command = crate::pane::rex_env_command(&env, &harness, &args);
    if dry_run {
        println!("project: {project}\npane: {pane}\ncommand: {command}");
        return Ok(());
    }
    if archived {
        crate::state::restore_resumable(&project, pin)?;
    }
    crate::cmd::swarm::seat_adopt(&project, Some(pin), &pane, None)?;
    exec_shell(&command)
}

/// Which resumable seats `--all` can bring back, and why it skips the rest.
pub fn resume_all_plan(rows: &[crate::state::SpawnEntry]) -> (Vec<String>, Vec<(String, String)>) {
    let mut ready = Vec::new();
    let mut skipped = Vec::new();
    for row in rows {
        let Some(pin) = row.mail_identity.clone() else {
            continue;
        };
        if pin.is_empty()
            || !pin
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            skipped.push((pin, "pin is not a plain name".to_string()));
            continue;
        }
        let Some(harness) = row.harness.as_deref() else {
            skipped.push((pin, "no harness recorded".to_string()));
            continue;
        };
        match resume_args(harness, row.harness_session.as_deref()) {
            Ok(_) => ready.push(pin),
            Err(e) => skipped.push((pin, format!("{e:#}"))),
        }
    }
    (ready, skipped)
}

/// `flywheel seat resume --all`: one new pane per lost seat in the fleet
/// workspace, each running `flywheel seat resume <pin>`.
pub fn cmd_resume_all(cfg: &crate::Config, project: &str, dry_run: bool) -> Result<()> {
    let backend = crate::backend::factory::multiplexer_backend(cfg)?;
    crate::pane::reap_dead_registry_rows_on(project, &*backend);
    let (ready, skipped) = resume_all_plan(&crate::state::load_resumable(project));
    for (pin, why) in &skipped {
        eprintln!("seat resume --all: skip {pin}: {why}");
    }
    if ready.is_empty() {
        println!("seat resume --all: no resumable seats for {project}");
        return Ok(());
    }
    let label = crate::swarm_label(project);
    let meta = crate::state::load_spawn_meta(project);
    let ws = backend
        .workspace_list()?
        .into_iter()
        .find(|ws| ws.label == label || meta.as_deref() == Some(ws.label.as_str()))
        .ok_or_else(|| {
            anyhow!("seat resume --all: no live fleet workspace for {project} (label {label})")
        })?;
    let mut prev = backend
        .pane_list(Some(&ws.workspace_id))?
        .first()
        .map(|p| p.pane_id.clone())
        .ok_or_else(|| {
            anyhow!(
                "seat resume --all: workspace {} has no pane",
                ws.workspace_id
            )
        })?;
    let dir = crate::utf8_path(&crate::project_dir(cfg, project), "project dir")?;
    for pin in ready {
        let command = format!("flywheel seat resume {pin}");
        if dry_run {
            println!("{pin}: new pane in {} runs: {command}", ws.workspace_id);
            continue;
        }
        let pane = backend.pane_split(&prev, "down", Some(&dir))?.pane_id;
        crate::pane::wait_rex_shell_ready(
            &*backend,
            &pane,
            &pin,
            &pane,
            std::time::Duration::from_secs(15),
        )?;
        backend.pane_run(&pane, &command)?;
        println!("{pin}: resuming in {pane}");
        prev = pane;
    }
    Ok(())
}

#[cfg(unix)]
fn exec_shell(command: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .exec();
    Err(anyhow!("resume: cannot start the harness: {err}"))
}

#[cfg(not(unix))]
fn exec_shell(_command: &str) -> Result<()> {
    bail!("resume: needs a unix shell")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_supported_harness_reopens_its_own_way() {
        assert_eq!(
            resume_args("claude", Some("0b6e3c2a-1f4d-4c55-9a51-2a6d2f0e7c11")).unwrap(),
            ["--resume", "0b6e3c2a-1f4d-4c55-9a51-2a6d2f0e7c11"]
        );
        assert_eq!(
            resume_args("opencode", Some("ses-1-2-3")).unwrap(),
            ["--session", "ses-1-2-3"]
        );
        assert_eq!(resume_args("pi", None).unwrap(), ["--continue"]);
        assert_eq!(
            resume_args("agy", Some("agy:conv-42")).unwrap(),
            ["--conversation", "conv-42"],
            "the hook's harness prefix is stripped"
        );
    }

    #[test]
    fn a_missing_id_or_harness_without_resume_is_a_typed_refusal() {
        assert!(resume_args("claude", None).is_err());
        assert!(resume_args("opencode", Some("  ")).is_err());
        assert!(resume_args("antigravity", None).is_err());
        assert!(resume_args("gemini", Some("x")).is_err());
        assert!(resume_args("codex", Some("x")).is_err());
    }

    #[test]
    fn claude_resume_keeps_the_seat_plugin() {
        let args = resume_launch_args("claude", Some("sid")).unwrap();
        assert_eq!(&args[..2], ["--resume", "sid"]);
        assert!(args.iter().any(|a| a == "--plugin-dir"), "{args:?}");
        assert_eq!(resume_launch_args("pi", None).unwrap(), ["--continue"]);
    }

    fn row(pin: &str, harness: Option<&str>, session: Option<&str>) -> crate::state::SpawnEntry {
        crate::state::SpawnEntry {
            pane_id: "block:gone".to_string(),
            name: format!("crew-{pin}"),
            profile: None,
            harness: harness.map(str::to_string),
            model: None,
            model_pin: None,
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: Some(pin.to_string()),
            skills_injected: false,
            incarnation: 1,
            harness_session: session.map(str::to_string),
        }
    }

    #[test]
    fn resume_all_brings_back_only_seats_that_can_reopen() {
        let rows = [
            row("AmberOwl", Some("claude"), Some("sid")),
            row("BlueFox", Some("pi"), None),
            row("CoralJay", Some("claude"), None),
            row("DuskElk", None, Some("sid")),
            row("bad pin;rm", Some("pi"), None),
        ];
        let (ready, skipped) = resume_all_plan(&rows);
        assert_eq!(ready, ["AmberOwl", "BlueFox"]);
        let names: Vec<&str> = skipped.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, ["CoralJay", "DuskElk", "bad pin;rm"]);
    }

    #[test]
    fn gemini_is_deprecated_for_antigravity() {
        let line = deprecation_notice("gm").expect("gemini is deprecated");
        assert!(
            line.contains("gemini") && line.contains("antigravity"),
            "{line}"
        );
        assert_eq!(deprecation_notice("agy"), None);
        assert_eq!(deprecation_notice("claude"), None);
    }
}
