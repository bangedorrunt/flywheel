//! governed-by: ADR-0010 D4 (bd-8j7m) — reservation write-liveness before respawn.
//!
//! Functional core for the drain-expiry about-to-respawn branch
//! (durable.rs executor leaf): a bounded meta-only walk over the bead's
//! toron reservation globs. A file newer than the quiet window defers the
//! respawn (write evidence = a file mtime, not a renderer poll); a
//! failed/timed-out walk fails toward existing behavior (respawn).
//! The leaf is the imperative shell; everything here is pure except
//! [`walk`], which only stats files.
//!
//! It also carries the ADR-0010 D4/D12 declared-`paused:` lane: the parse for
//! an agent's `paused: <owner>` declaration ([`parse_paused_declaration`]), the
//! resurface-window knob (`FLYWHEEL_PAUSE_RESURFACE_SECS`, default 3600 — the
//! `pending_wait` TTL twin), and the [`paused_resurface_due`] predicate the
//! drain leaf's Tick calls. The paused-since clock itself lives on
//! `durable::DrainState::paused`; the shell stamps the window onto the
//! declaration, so the reducer reads no environment and a journaled
//! declaration replays to the same decision.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Quiet + not-writing + not exact-Busy → inspect (secs).
pub(crate) const STALE_ESCALATE_DEFAULT_SECS: u64 = 240;
/// Exact-Busy older than this → same inspect path (secs).
pub(crate) const BUSY_TURN_MAX_DEFAULT_SECS: u64 = 3600;
/// Consecutive write-evidence defers that add `demand-deep-inspection`.
pub(crate) const WEDGE_INSPECT_DEFAULT_COUNT: u32 = 3;
/// Declared-`paused:` resurface window (secs) — the same 3600s as the
/// `pending_wait` TTL (ADR-0010 D4/D12): a declared wait names its owner and
/// gets a bounded re-surface path, never an indefinite silent park.
pub const PAUSE_RESURFACE_DEFAULT_SECS: u64 = 3600;
/// Walk no deeper than this many levels below the project root.
pub(crate) const WALK_MAXDEPTH_DEFAULT: usize = 8;
/// Walk budget; breach fails toward respawn, never hangs the leaf.
pub const WALK_TIMEOUT_DEFAULT_SECS: u64 = 10;
/// Directory names never descended into.
pub(crate) const WALK_PRUNE_DEFAULT: &[&str] = &["target", ".git", "node_modules", ".hg", ".svn"];

/// Walk configuration, overridable via env.
///
/// - `FLYWHEEL_WRITE_WALK_PRUNE` — comma-separated dir names (default above).
/// - `FLYWHEEL_WRITE_WALK_MAXDEPTH` — default 8.
/// - `FLYWHEEL_WRITE_WALK_TIMEOUT` — seconds, default 10.
/// - `FLYWHEEL_STALE_ESCALATE_SECS` — quiet window, default 240.
/// - `FLYWHEEL_BUSY_TURN_MAX_SECS` — default 3600.
/// - `FLYWHEEL_WEDGE_INSPECT_COUNT` — default 3.
///
/// The declared-pause resurface window is loop-params data now
/// (`LoopParams.pause_resurface_secs`, ADR-0016) — env no longer tunes it.
#[derive(Debug, Clone)]
pub(crate) struct WalkOpts {
    pub prune: Vec<String>,
    pub maxdepth: usize,
    pub timeout_secs: u64,
    pub quiet_secs: u64,
    pub wedge_inspect_count: u32,
    pub busy_turn_max_secs: u64,
    /// Kept for API symmetry with the declaration stamp; `walk` never reads it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub pause_resurface_secs: u64,
}

impl Default for WalkOpts {
    fn default() -> Self {
        Self {
            prune: WALK_PRUNE_DEFAULT.iter().map(|s| s.to_string()).collect(),
            maxdepth: WALK_MAXDEPTH_DEFAULT,
            timeout_secs: WALK_TIMEOUT_DEFAULT_SECS,
            quiet_secs: STALE_ESCALATE_DEFAULT_SECS,
            wedge_inspect_count: WEDGE_INSPECT_DEFAULT_COUNT,
            busy_turn_max_secs: BUSY_TURN_MAX_DEFAULT_SECS,
            pause_resurface_secs: PAUSE_RESURFACE_DEFAULT_SECS,
        }
    }
}

impl WalkOpts {
    /// Env override seam — currently unused by the loop (ADR-0016: walk
    /// knobs resolve into `LoopParams` at loop start); kept for operator
    /// tooling that wants the full env-tuned config in one call.
    #[allow(dead_code)]
    pub(crate) fn from_env() -> Self {
        let mut o = Self::default();
        if let Ok(v) = std::env::var("FLYWHEEL_WRITE_WALK_PRUNE") {
            let ps: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !ps.is_empty() {
                o.prune = ps;
            }
        }
        if let Ok(v) = std::env::var("FLYWHEEL_WRITE_WALK_MAXDEPTH")
            .unwrap_or_default()
            .parse()
        {
            o.maxdepth = v;
        }
        if let Ok(v) = std::env::var("FLYWHEEL_WRITE_WALK_TIMEOUT")
            .unwrap_or_default()
            .parse()
        {
            o.timeout_secs = v;
        }
        if let Ok(v) = std::env::var("FLYWHEEL_STALE_ESCALATE_SECS")
            .unwrap_or_default()
            .parse()
        {
            o.quiet_secs = v;
        }
        if let Ok(v) = std::env::var("FLYWHEEL_BUSY_TURN_MAX_SECS")
            .unwrap_or_default()
            .parse()
        {
            o.busy_turn_max_secs = v;
        }
        if let Ok(v) = std::env::var("FLYWHEEL_WEDGE_INSPECT_COUNT")
            .unwrap_or_default()
            .parse()
        {
            o.wedge_inspect_count = v;
        }
        // The declared-pause window is resolved once at loop start
        // (LoopParams.pause_resurface_secs) — env no longer tunes it here.
        o
    }
}

/// Segment-wise glob match over `/`-separated relative paths.
/// Supports `**` (zero or more segments), `*` (within one segment),
/// `?` (one char), and literals. A trailing `/**` also matches the dir
/// itself, so `src/**` covers `src/a.rs` and `src/a/b.rs`.
pub(crate) fn glob_matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pat, &segs)
}

fn match_segments(pat: &[&str], segs: &[&str]) -> bool {
    if pat.is_empty() {
        return segs.is_empty();
    }
    if pat[0] == "**" {
        // `**` eats zero or more segments.
        for i in 0..=segs.len() {
            if match_segments(&pat[1..], &segs[i..]) {
                return true;
            }
        }
        return false;
    }
    if segs.is_empty() {
        return false;
    }
    if !match_segment(pat[0], segs[0]) {
        return false;
    }
    match_segments(&pat[1..], &segs[1..])
}

fn match_segment(pat: &str, seg: &str) -> bool {
    let (mut p, mut s) = (pat.as_bytes(), seg.as_bytes());
    // `*` backtracking over one segment.
    let (mut star, mut ss) = (None, 0);
    let (mut pi, mut si) = (0, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            ss = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            ss += 1;
            si = ss;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    let _ = &mut p;
    let _ = &mut s;
    pi == p.len()
}

/// What the bounded walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WalkOutcome {
    /// A reserved file written within the quiet window.
    Fresh { path: String, age_secs: u64 },
    /// Walk completed; nothing reserved is fresh (or nothing reserved).
    Quiet { freshest_age_secs: Option<u64> },
    /// Walk could not complete (missing root, timeout). Fails toward
    /// existing behavior: respawn, never hang the leaf.
    WalkFailed { reason: String },
}

/// Bounded meta-only walk (stats only, never reads file contents).
/// Skips symlinks (loop risk), pruned dirs, and entries past maxdepth;
/// breaches the timeout into [`WalkOutcome::WalkFailed`].
pub(crate) fn walk(root: &Path, globs: &[String], opts: &WalkOpts) -> WalkOutcome {
    if !root.is_dir() {
        return WalkOutcome::WalkFailed {
            reason: format!("root not a dir: {}", root.display()),
        };
    }
    if globs.is_empty() {
        return WalkOutcome::Quiet {
            freshest_age_secs: None,
        };
    }
    let now = SystemTime::now();
    let deadline = Instant::now() + Duration::from_secs(opts.timeout_secs.max(1));
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut freshest: Option<(String, u64)> = None;
    let mut checked: usize = 0;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            checked += 1;
            if checked.is_multiple_of(256) && Instant::now() > deadline {
                return WalkOutcome::WalkFailed {
                    reason: "timeout".into(),
                };
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if depth >= opts.maxdepth {
                    continue;
                }
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && opts.prune.iter().any(|p| p == name)
                {
                    continue;
                }
                stack.push((path, depth + 1));
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !globs.iter().any(|g| glob_matches(g, &rel)) {
                continue;
            }
            let age = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX);
            if age <= opts.quiet_secs {
                return WalkOutcome::Fresh {
                    path: rel,
                    age_secs: age,
                };
            }
            match &freshest {
                Some((_, a)) if age < *a => freshest = Some((rel, age)),
                None => freshest = Some((rel, age)),
                _ => {}
            }
        }
    }
    WalkOutcome::Quiet {
        freshest_age_secs: freshest.map(|(_, a)| a),
    }
}

/// Leaf decision over a finished walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LivenessVerdict {
    /// Skip the kill, re-arm grace, emit the write-evidence digest.
    Defer,
    /// Proceed with the respawn. `inspect` recommends a closer look
    /// (quiet + not-writing + not exact-Busy); it never blocks the kill.
    Respawn { inspect: bool },
}

pub(crate) fn decide(outcome: &WalkOutcome, exact_busy: bool, opts: &WalkOpts) -> LivenessVerdict {
    match outcome {
        WalkOutcome::Fresh { .. } => LivenessVerdict::Defer,
        WalkOutcome::WalkFailed { .. } => LivenessVerdict::Respawn { inspect: false },
        WalkOutcome::Quiet { freshest_age_secs } => LivenessVerdict::Respawn {
            inspect: inspect_due(*freshest_age_secs, exact_busy, opts),
        },
    }
}

/// Inspect cadence predicate (D4 table): exact-Busy older than the busy
/// turn max, or quiet + not-writing + not exact-Busy older than the
/// quiet window → inspect. No files at all → nothing to judge → false.
/// NOTE (bd-8j7m): the drain leaf passes `exact_busy=false` — no
/// busy-since clock exists in LoopModel — so the busy-turn arm is
/// exercised by tests and future lanes, not the leaf.
pub(crate) fn inspect_due(
    freshest_age_secs: Option<u64>,
    exact_busy: bool,
    opts: &WalkOpts,
) -> bool {
    match (freshest_age_secs, exact_busy) {
        (Some(age), true) => age >= opts.busy_turn_max_secs,
        (Some(age), false) => age >= opts.quiet_secs,
        (None, _) => false,
    }
}

/// Declared `paused:` resurface: a pause older than the resurface window
/// (default 3600s, the `pending_wait` TTL twin) is due for resurface. Called
/// by the drain leaf's Tick arm, where the paused-since clock lives
/// (`durable::DrainState::paused`). A zero window is never "due": an
/// unset/empty knob must not make every tick a resurface.
pub(crate) fn paused_resurface_due(paused_age_secs: u64, resurface_secs: u64) -> bool {
    resurface_secs > 0 && paused_age_secs >= resurface_secs
}

/// A declared `paused:` wait, parsed from the declaring agent's own text.
/// `owner` is the human named by the declaration — D12 requires every waiting
/// state to name its owner, so an unnamed wait is not a declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclaredPause {
    pub owner: String,
}

/// Parse a declared pause out of a pane/agent text tail: a line whose key is
/// `paused` (case-insensitive), e.g. `paused: CooperSparrow (needs the AC call)`.
///
/// The FIRST declaring line wins (a later line is a newer state, not the
/// declaration being aged). The value is trimmed of list/quote decoration and
/// split into owner + note: the owner is the first token with any leading `@`
/// stripped, which is the name the resurface is addressed to. A bare `paused:`
/// with no owner, or a `paused`-prefixed line with no colon, is NOT a
/// declaration — the caller keeps the ordinary drain path.
pub(crate) fn parse_paused_declaration(text: &str) -> Option<DeclaredPause> {
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches(['-', '*', '>', '#']).trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("paused") {
            continue;
        }
        let value = value.trim().trim_matches('`').trim();
        let owner = value
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches('@');
        if owner.is_empty() {
            continue;
        }
        return Some(DeclaredPause {
            owner: owner.to_string(),
        });
    }
    None
}

/// Write-evidence digest for the defer path. `defers` counts consecutive
/// write-evidence defers including this one; at `wedge_inspect_count` the
/// digest adds `demand-deep-inspection` (text escalation for the captain —
/// the pane is still never stopped here).
pub(crate) fn defer_digest(
    pin: &str,
    bead: &str,
    path: &str,
    age_secs: u64,
    defers: u32,
    opts: &WalkOpts,
) -> String {
    let base = format!(
        "flywheel: write-evidence defer respawn for {pin} (bead {bead}): {path} written {age_secs}s ago, defer #{defers}"
    );
    if defers >= opts.wedge_inspect_count.max(1) {
        format!("{base} — demand-deep-inspection")
    } else {
        base
    }
}

/// Resolve a project slug to a filesystem root: slug-as-path, then
/// `~/workspace/<slug>`, else the process cwd (precedent: durable.rs
/// project-dir fallback chain).
pub(crate) fn resolve_project_root(slug: &str) -> PathBuf {
    let p = Path::new(slug);
    if p.is_dir() {
        return p.to_path_buf();
    }
    if let Ok(home) = std::env::var("HOME") {
        let ws = PathBuf::from(home).join("workspace").join(slug);
        if ws.is_dir() {
            return ws;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fw-wl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn glob_star_star_covers_nested_and_shallow() {
        assert!(glob_matches("src/**", "src/a.rs"));
        assert!(glob_matches("src/**", "src/a/b.rs"));
        assert!(glob_matches("src/**", "src"));
        assert!(!glob_matches("src/**", "other/a.rs"));
        assert!(glob_matches("src/*.rs", "src/a.rs"));
        assert!(!glob_matches("src/*.rs", "src/a/b.rs"));
        assert!(glob_matches("**/*.rs", "a/b/c.rs"));
        assert!(glob_matches("exact.txt", "exact.txt"));
        assert!(!glob_matches("exact.txt", "exact.txt2"));
        assert!(glob_matches("a?c.rs", "abc.rs"));
        assert!(!glob_matches("a?c.rs", "ac.rs"));
    }

    #[test]
    fn walk_finds_fresh_reserved_file() {
        let d = tmpdir("fresh");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/a.rs"), "x").unwrap();
        let o = walk(&d, &["src/**".to_string()], &WalkOpts::default());
        match o {
            WalkOutcome::Fresh { path, .. } => assert_eq!(path, "src/a.rs"),
            other => panic!("expected Fresh, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn walk_ignores_unreserved_and_pruned() {
        let d = tmpdir("prune");
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join("target/x"), "x").unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        let o = walk(&d, &["target/**".to_string()], &WalkOpts::default());
        // target/ is pruned: never descended, so Quiet with no freshest.
        assert_eq!(
            o,
            WalkOutcome::Quiet {
                freshest_age_secs: None
            }
        );
        let o2 = walk(&d, &["src/**".to_string()], &WalkOpts::default());
        assert_eq!(
            o2,
            WalkOutcome::Quiet {
                freshest_age_secs: None
            }
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn walk_missing_root_fails_toward_respawn() {
        let o = walk(
            Path::new("/nonexistent-fW-root-xyz"),
            &["**".to_string()],
            &WalkOpts::default(),
        );
        assert!(matches!(o, WalkOutcome::WalkFailed { .. }));
        assert_eq!(
            decide(&o, false, &WalkOpts::default()),
            LivenessVerdict::Respawn { inspect: false }
        );
    }

    #[test]
    fn decide_fresh_defers_quiet_may_recommend_inspect() {
        let opts = WalkOpts::default();
        let fresh = WalkOutcome::Fresh {
            path: "a".into(),
            age_secs: 3,
        };
        assert_eq!(decide(&fresh, false, &opts), LivenessVerdict::Defer);
        let stale_quiet = WalkOutcome::Quiet {
            freshest_age_secs: Some(9999),
        };
        assert_eq!(
            decide(&stale_quiet, false, &opts),
            LivenessVerdict::Respawn { inspect: true }
        );
        let recent_quiet = WalkOutcome::Quiet {
            freshest_age_secs: Some(5),
        };
        assert_eq!(
            decide(&recent_quiet, false, &opts),
            LivenessVerdict::Respawn { inspect: false }
        );
        // Exact-busy needs the busy-turn max, not the quiet window.
        assert!(!inspect_due(Some(300), true, &opts));
        assert!(inspect_due(Some(3600), true, &opts));
        assert!(!inspect_due(None, false, &opts));
        // Pause resurface twins the 3600s window.
        assert!(paused_resurface_due(3600, 3600));
        assert!(!paused_resurface_due(3599, 3600));
        // A zero/unset window must not make every tick a resurface.
        assert!(!paused_resurface_due(u64::MAX, 0));
    }

    #[test]
    fn pause_default_window_twins_the_pending_wait_ttl() {
        assert_eq!(WalkOpts::default().pause_resurface_secs, 3600);
        assert_eq!(PAUSE_RESURFACE_DEFAULT_SECS, 3600);
    }

    #[test]
    fn parse_paused_declaration_reads_owner_from_the_first_line() {
        let p = parse_paused_declaration("some work\npaused: CooperSparrow (needs the AC call)\n")
            .expect("declaration");
        assert_eq!(p.owner, "CooperSparrow");
        // Case-insensitive key, list/quote decoration tolerated, `@` stripped.
        let p = parse_paused_declaration("- PAUSED:   @NavyDog  — waiting on sign-off")
            .expect("declaration");
        assert_eq!(p.owner, "NavyDog");
        // The FIRST declaring line wins: the age must not be re-based by a
        // later echo of the same wait.
        let p = parse_paused_declaration("paused: First\npaused: Second").expect("declaration");
        assert_eq!(p.owner, "First");
    }

    #[test]
    fn parse_paused_declaration_refuses_an_unnamed_wait() {
        // D12: every waiting state names its owner — an unnamed wait has no
        // re-surface path, so it is not a declaration (ordinary drain path).
        assert_eq!(parse_paused_declaration("paused:\n"), None);
        assert_eq!(parse_paused_declaration("paused:   `  `"), None);
        assert_eq!(parse_paused_declaration("paused :"), None);
        // A different key, or prose that merely mentions the word.
        assert_eq!(parse_paused_declaration("blocked: someone"), None);
        assert_eq!(parse_paused_declaration("I am paused right now"), None);
        assert_eq!(parse_paused_declaration(""), None);
    }

    #[test]
    fn defer_digest_escalates_text_at_wedge_count() {
        let opts = WalkOpts::default();
        let d1 = defer_digest("P", "bd-1", "src/a.rs", 3, 1, &opts);
        assert!(d1.contains("write-evidence") && !d1.contains("demand-deep-inspection"));
        let d3 = defer_digest("P", "bd-1", "src/a.rs", 3, 3, &opts);
        assert!(d3.contains("demand-deep-inspection"));
    }
}
