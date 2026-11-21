// governed-by: ADR-0039
//! bd-b2kaa: per-seat hook installation and the hook CLI. Each harness gets
//! its own config dir under the flywheel state root — never the operator's —
//! and the hooks report turn state / mail into per-seat files the doorbell
//! reads. The decisions are pure in `turn_state.rs`; this module is the port.

use std::path::{Path, PathBuf};

use crate::turn_state::{ContinuationState, TurnReport};

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

pub fn seat_dir(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    state_dir
        .join("turn-hooks")
        .join(format!("{}-{}", sanitize(project), sanitize(pin)))
}

pub fn turn_path(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    seat_dir(state_dir, project, pin).join("turn.json")
}

pub fn continuation_path(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    seat_dir(state_dir, project, pin).join("continuation.json")
}

pub fn mail_path(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    seat_dir(state_dir, project, pin).join("mail.json")
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string(value).unwrap_or_default())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

pub fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Read a fresh-enough hook report; `turn_state::status_from_turn` decides
/// whether it wins over the plane's screen-derived status.
pub fn read_turn(state_dir: &Path, project: &str, pin: &str) -> Option<TurnReport> {
    read_json(&turn_path(state_dir, project, pin))
}

pub fn write_turn(
    state_dir: &Path,
    project: &str,
    pin: &str,
    running: bool,
) -> std::io::Result<()> {
    write_json(
        &turn_path(state_dir, project, pin),
        &TurnReport {
            running,
            at_us: now_us(),
        },
    )
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailSignal {
    pub waiting: bool,
    pub urgent: bool,
}

/// The doorbell writes this when it queues/digests a wake for a pin; the Stop
/// and PostToolUse hooks read it (bd-2ghz8 signal links replace the file later).
pub fn write_mail(
    state_dir: &Path,
    project: &str,
    pin: &str,
    signal: &MailSignal,
) -> std::io::Result<()> {
    write_json(&mail_path(state_dir, project, pin), signal)
}

fn read_mail(state_dir: &Path, project: &str, pin: &str) -> MailSignal {
    read_json(&mail_path(state_dir, project, pin)).unwrap_or_default()
}

/// One hook invocation. Returns the plain-language line the harness should
/// inject, if any; the caller prints it and the harness continues the turn.
pub fn on_event(state_dir: &Path, project: &str, pin: &str, event: &str) -> Option<String> {
    match event {
        "session-start" | "user-prompt" => {
            let _ = write_turn(state_dir, project, pin, true);
            None
        }
        "session-end" => {
            let _ = write_turn(state_dir, project, pin, false);
            None
        }
        "stop" => {
            let _ = write_turn(state_dir, project, pin, false);
            let mut cont: ContinuationState =
                read_json(&continuation_path(state_dir, project, pin)).unwrap_or_default();
            let signal = read_mail(state_dir, project, pin);
            let line = crate::turn_state::stop_line(signal.waiting, &cont).map(str::to_string);
            cont.count = if line.is_some() { cont.count + 1 } else { 0 };
            let _ = write_json(&continuation_path(state_dir, project, pin), &cont);
            line
        }
        "post-tool" => {
            let _ = write_turn(state_dir, project, pin, true);
            let mut cont: ContinuationState =
                read_json(&continuation_path(state_dir, project, pin)).unwrap_or_default();
            let signal = read_mail(state_dir, project, pin);
            let due = crate::turn_state::urgent_due(signal.urgent, now_us(), &cont);
            if due {
                cont.last_urgent_check_us = now_us();
                let _ = write_json(&continuation_path(state_dir, project, pin), &cont);
            }
            due.then(|| "URGENT mail is waiting — run `toron next` now".to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_state_is_keyed_by_project_then_pin() {
        let dir = std::env::temp_dir().join(format!("b2kaa-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        on_event(&dir, "flywheel", "AmberProbe", "session-start");
        assert!(
            turn_path(&dir, "flywheel", "AmberProbe").is_file(),
            "turn state must live under <project>/<pin>"
        );
        assert!(
            !turn_path(&dir, "AmberProbe", "AmberProbe").is_file(),
            "the pin must never be used as the project key"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_hook_resets_the_cap_when_no_mail_waits() {
        let dir = std::env::temp_dir().join(format!("b2kaa-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cont = ContinuationState {
            count: 2,
            last_urgent_check_us: 0,
        };
        let _ = write_json(&continuation_path(&dir, "p", "Pin"), &cont);
        write_mail(&dir, "p", "Pin", &MailSignal::default()).unwrap();
        assert!(on_event(&dir, "p", "Pin", "stop").is_none());
        cont = read_json(&continuation_path(&dir, "p", "Pin")).unwrap();
        assert_eq!(cont.count, 0, "a quiet stop resets the continuation cap");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
