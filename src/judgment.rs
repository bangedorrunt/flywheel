//! Host-side System One / Jev judgment adapter (W1) + reclassify enricher (W2).
//!
//! governed-by: ADR-0014
//!
//! The adapter answers typed Choice / Score / Noul packs over ephemeral state
//! slices. Code owns Cmd: the table `decide` wins on primitive and topology,
//! Jev only annotates (urgency / confidence) or advises on explicit-`none`.
//! All HTTP lives here, called only from the host expand path
//! (`workspace::expand_reclassify_cmds`); pure `tick` and `decide.rs`
//! classify/pick/build_decision never touch this module (forbidden zone).

use std::collections::BTreeMap;
use std::env;

use crate::authority::RuntimeAuthority;

/// Pinned model id. Never `jev-latest` until pack re-validation CI exists
/// (ADR-0014 §5.1 I5): a moving alias under static packs is silent drift.
pub const JEV_MODEL_PIN: &str = "jev-1.13.0";

/// System One request endpoint.
pub const SYSTEMONE_URL: &str = "https://api.typesafe.ai/v1/systemone";

/// Env flag gating the W2 enricher. Default off: absent or any value other
/// than 1/true/yes means the host expand path behaves exactly as before.
pub const FLAG_ENV: &str = "FLYWHEEL_JEV";

/// Env var carrying the TypeSafe API key. Mikasa-only custody (ADR-0014 §7 I6).
pub const API_KEY_ENV: &str = "TYPESAFE_API_KEY";

/// Max serialized bytes of a state slice before head-cut truncation.
pub const STATE_SLICE_CAP_BYTES: usize = 8192;

/// Field-name substrings redacted from state slices before any POST.
/// Credentials, tokens, private keys and full mail bodies never leave Tailnet.
pub const PII_DENY_SUBSTRINGS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "private_key",
    "privatekey",
    "nsec",
    "passphrase",
    "auth",
    "bearer",
    "credential",
    "ssn",
    "email",
    "mail_body",
    "body_md",
];

/// Max chars kept for unbounded strings entering the audit row.
pub const AUDIT_USAGE_CAP: usize = 512;
pub const AUDIT_NOTE_CAP: usize = 1024;

/// Char-boundary-safe truncation with marker.
pub fn cap_str(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

fn value_secret_patterns() -> [&'static str; 2] {
    [
        r"(?i)\b(api[_-]?key|passwd|password|secret|token|bearer|credential|passphrase)\b\s*[:=]\s*\S+",
        r"\bnsec1[qpzry9x8gf2tvdw0s3jn54khce6mua7l]{20,}",
    ]
}

/// Redact secret-shaped substrings inside VALUES (SEC): key-substring
/// redaction alone misses `goal: "use password=hunter2"`. Assignment matches
/// collapse to `key=[REDACTED]`; bare nsec tokens collapse fully.
pub fn redact_value(v: &str) -> String {
    let mut out = v.to_string();
    for pat in value_secret_patterns() {
        let re = match regex_lite::Regex::new(pat) {
            Ok(r) => r,
            Err(_) => continue,
        };
        out = re
            .replace_all(&out, |caps: &regex_lite::Captures| {
                if caps.len() > 1 {
                    format!("{}=[REDACTED]", &caps[1])
                } else {
                    "[REDACTED]".to_string()
                }
            })
            .to_string();
    }
    out
}

/// Action classes for confidence escalation (ADR-0014 §5.3.5 I1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionClass {
    /// Anything that could move topology or dispatch work.
    #[cfg_attr(not(test), allow(dead_code))]
    // ADR-0014 I1: mail-guardrails + workflow-family surfaces (not yet wired)
    TopologyAdjacent,
    /// Honesty-path gates (reclassify merge, verify verdicts).
    HonestyGate,
    /// Announcements, digests, FYI prompts.
    #[cfg_attr(not(test), allow(dead_code))]
    // ADR-0014 I1/I4: announce-only fallback surface (contract + tests; host wiring pending)
    AnnounceOnly,
}

/// Provisional escalation thresholds: escalate when confidence is BELOW the
/// bound. Initial defaults, captain-tunable, re-validation duty (I1) —
/// constants the code needs, not measured calibration.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub topology_adjacent: f64,
    pub honesty_gate: f64,
    pub announce_only: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            topology_adjacent: 0.85,
            honesty_gate: 0.80,
            announce_only: 0.60,
        }
    }
}

impl Thresholds {
    fn bound(&self, class: ActionClass) -> f64 {
        match class {
            ActionClass::TopologyAdjacent => self.topology_adjacent,
            ActionClass::HonestyGate => self.honesty_gate,
            ActionClass::AnnounceOnly => self.announce_only,
        }
    }
}

/// True when `conf` is too low for `class` and must escalate to
/// muse-spark / human. Confidence is peakedness, never permission to act.
/// Non-finite confidence always escalates (B2).
pub fn escalate_required(conf: f64, class: ActionClass, t: &Thresholds) -> bool {
    !conf.is_finite() || conf < t.bound(class)
}

// ---------------------------------------------------------------------------
// Question packs (pure data)
// ---------------------------------------------------------------------------

/// One typed question inside a pack. `criteria` defines the possible
/// answers (per-option descriptions for Choice/Score, guidance lines for
/// Noul) — the model cannot choose an omitted value, and Score levels must
/// stand on their own as concrete situations (typesafe-ai skill shaping).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct QuestionDef {
    pub id: String,
    pub kind: QuestionKind,
    pub instructions: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub criteria: Vec<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QuestionKind {
    Choice,
    Score,
    Noul,
}

/// Versioned pack: pure data, lives at `.flywheel/judgment-packs/<id>.vN.yaml`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct QuestionPack {
    pub id: String,
    pub version: u32,
    pub model: String,
    pub questions: Vec<QuestionDef>,
}

impl QuestionPack {
    /// Parse + validate: non-empty id, version >= 1, pinned model, at least
    /// one question, every question with an id and instructions, Choice with
    /// >= 2 options. `jev-latest` is rejected (silent drift).
    pub fn parse_yaml(text: &str) -> Result<Self, String> {
        let pack: QuestionPack =
            serde_yaml::from_str(text).map_err(|e| format!("pack yaml: {e}"))?;
        if pack.id.trim().is_empty() {
            return Err("pack id is empty".into());
        }
        if pack.version < 1 {
            return Err("pack version must be >= 1".into());
        }
        if pack.model.trim().is_empty() {
            return Err("pack model is empty".into());
        }
        if pack.model == "jev-latest" {
            return Err("pack model must pin a version, never jev-latest".into());
        }
        if pack.questions.is_empty() {
            return Err("pack has no questions".into());
        }
        for q in &pack.questions {
            if q.id.trim().is_empty() {
                return Err("question with empty id".into());
            }
            if q.instructions.trim().is_empty() {
                return Err(format!("question {} has empty instructions", q.id));
            }
            if q.kind == QuestionKind::Choice && q.options.len() < 2 {
                return Err(format!("choice question {} needs >= 2 options", q.id));
            }
            if q.kind == QuestionKind::Score && q.options.len() < 2 {
                return Err(format!("score question {} needs >= 2 levels", q.id));
            }
            // Criteria, when present, define each option 1:1 (Choice/Score).
            // Noul carries free guidance lines instead (no options to align).
            if !q.criteria.is_empty()
                && matches!(q.kind, QuestionKind::Choice | QuestionKind::Score)
                && q.criteria.len() != q.options.len()
            {
                return Err(format!(
                    "question {} criteria ({} lines) must define each option 1:1 ({} options)",
                    q.id,
                    q.criteria.len(),
                    q.options.len()
                ));
            }
            if q.kind == QuestionKind::Score && q.options.len() < 2 {
                return Err(format!("score question {} needs >= 2 levels", q.id));
            }
        }
        Ok(pack)
    }

    pub fn load_from_file(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read pack: {e}"))?;
        Self::parse_yaml(&text)
    }
}

// ---------------------------------------------------------------------------
// Answers + confidence (code-side math only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ChoiceAnswer {
    pub choice: String,
    /// Full distribution over the pack's closed option set.
    pub probs: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ScoreAnswer {
    /// Zero-based level index into the pack's ordered levels.
    pub level: usize,
    pub levels: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NoulAnswer {
    pub p_yes: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Choice(ChoiceAnswer),
    Score(ScoreAnswer),
    Noul(NoulAnswer),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Answers {
    pub model_id: String,
    pub pack_id: String,
    pub pack_version: u32,
    pub by_question: BTreeMap<String, Answer>,
    pub usage: String,
}

/// Finite-or-zero: any non-finite model number degrades to 0 confidence
/// (escalate), never to NaN passthrough (B2).
fn fin(x: f64) -> f64 {
    if x.is_finite() { x } else { 0.0 }
}

/// Confidence of one answer in [0,1]. Choice = top1−top2 margin; Score/Noul =
/// margin-from-midpoint. Computed in code — never model-side Noul↔Choice
/// arithmetic (jev-1.13 jaggedness).
pub fn confidence_of(a: &Answer) -> f64 {
    match a {
        Answer::Choice(c) => {
            if c.probs.values().any(|p| !p.is_finite()) {
                return 0.0;
            }
            let mut ps: Vec<f64> = c.probs.values().copied().collect();
            if ps.is_empty() {
                return 0.0;
            }
            ps.sort_by(|x, y| y.partial_cmp(x).unwrap_or(std::cmp::Ordering::Equal));
            let top = ps[0].clamp(0.0, 1.0);
            let second = ps.get(1).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            (top - second).clamp(0.0, 1.0)
        }
        Answer::Score(s) => {
            if s.levels < 2 {
                return 0.0;
            }
            let norm = fin(s.level as f64 / (s.levels - 1) as f64);
            (2.0 * (norm - 0.5).abs()).clamp(0.0, 1.0)
        }
        Answer::Noul(n) => {
            if !n.p_yes.is_finite() {
                return 0.0;
            }
            (2.0 * (n.p_yes.clamp(0.0, 1.0) - 0.5).abs()).clamp(0.0, 1.0)
        }
    }
}

/// Minimum confidence across a pack's answers. Empty = 0.0 (escalate, B1).
pub fn pack_confidence(answers: &Answers) -> f64 {
    if answers.by_question.is_empty() {
        return 0.0;
    }
    answers
        .by_question
        .values()
        .map(confidence_of)
        .fold(f64::INFINITY, f64::min)
        .clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// State slices (code-side filter before any POST)
// ---------------------------------------------------------------------------

/// Ephemeral redacted slice. `truncated` marks head-cut at the byte cap.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct StateSlice {
    pub fields: BTreeMap<String, String>,
    pub truncated: bool,
}

fn redacted_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    PII_DENY_SUBSTRINGS.iter().any(|deny| lower.contains(deny))
}

/// Build a slice: redact denylisted keys to `[REDACTED]`, scrub secret-shaped
/// substrings inside values, then drop largest values first until the
/// serialized body fits the cap (keeps the most informative fields, B4).
pub fn build_state_slice(fields: BTreeMap<String, String>) -> StateSlice {
    let mut kept = BTreeMap::new();
    for (k, v) in &fields {
        if redacted_key(k) {
            kept.insert(k.clone(), "[REDACTED]".to_string());
        } else {
            kept.insert(k.clone(), redact_value(v));
        }
    }
    let mut truncated = false;
    while serde_json::to_string(&kept).map(|s| s.len()).unwrap_or(0) > STATE_SLICE_CAP_BYTES
        && !kept.is_empty()
    {
        let biggest = kept
            .iter()
            .max_by_key(|(_, v)| v.len())
            .map(|(k, _)| k.clone());
        if let Some(k) = biggest {
            kept.remove(&k);
            truncated = true;
        } else {
            break;
        }
    }
    StateSlice {
        fields: kept,
        truncated,
    }
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JevError {
    /// No key, timeout, outage — deny shapes.
    Offline(String),
    /// HTTP 429: named so callers escalate fast instead of retry-storming.
    RateLimited(String),
    Unauthorized,
    Malformed(String),
}

impl JevError {
    /// Map judgment transport failures into the shared host taxonomy so Jev
    /// participates in the same retry, audit, and escalation policy as every
    /// other provider. The adapter still preserves its richer error variant.
    pub fn classified(&self) -> crate::failure::ClassifiedFailure {
        let text = match self {
            Self::RateLimited(e) => format!("429 {e}"),
            Self::Unauthorized => "401 unauthorized".to_string(),
            other => other.to_string(),
        };
        crate::failure::classify(&text, None, None)
    }
}

impl std::fmt::Display for JevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JevError::Offline(w) => write!(f, "offline/denied: {w}"),
            JevError::RateLimited(w) => write!(f, "rate-limited (429): {w}"),
            JevError::Unauthorized => write!(f, "unauthorized (key custody)"),
            JevError::Malformed(w) => write!(f, "malformed response: {w}"),
        }
    }
}

/// Effect-layer transport. Blocking by design: called off-tick from the host
/// expand path, never from pure tick.
pub trait Transport {
    fn ask(&self, slice: &StateSlice, pack: &QuestionPack) -> Result<Answers, JevError>;
}

/// Always-deny transport: offline table for tests and keyless hosts.
#[cfg_attr(not(test), allow(dead_code))] // ADR-0014 I4: deny-path contract + tests; host uses UreqTransport
pub struct OfflineTransport;

impl Transport for OfflineTransport {
    fn ask(&self, _slice: &StateSlice, _pack: &QuestionPack) -> Result<Answers, JevError> {
        Err(JevError::Offline("offline transport".into()))
    }
}

/// Live System One POST over ureq (blocking, 5s timeout).
#[derive(Debug, Clone)]
pub struct UreqTransport {
    pub api_key: String,
    pub url: String,
}

impl UreqTransport {
    /// Construct from already-resolved host authority. Network effects must
    /// not re-read process environment after the entrypoint boundary.
    pub fn from_authority(authority: &RuntimeAuthority) -> Result<Self, JevError> {
        let api_key = authority
            .host
            .typesafe_api_key
            .clone()
            .ok_or_else(|| JevError::Offline(format!("{API_KEY_ENV} unset")))?;
        Ok(Self {
            api_key,
            url: SYSTEMONE_URL.to_string(),
        })
    }
}

impl Transport for UreqTransport {
    /// Single POST attempt: 5s timeout lives inside. Never retried inside —
    /// retries re-enter as new reclassifies under the same parent effect
    /// key (ADR-0014 envelope, untouched by ADR-0015).
    fn ask(&self, slice: &StateSlice, pack: &QuestionPack) -> Result<Answers, JevError> {
        let questions: BTreeMap<String, serde_json::Value> = pack
            .questions
            .iter()
            .map(|q| {
                let kind = match q.kind {
                    QuestionKind::Choice => "choice",
                    QuestionKind::Score => "score",
                    QuestionKind::Noul => "noul",
                };
                (
                    q.id.clone(),
                    serde_json::json!({
                        "type": kind,
                        "instructions": q.instructions,
                        "options": q.options,
                        "criteria": q.criteria,
                    }),
                )
            })
            .collect();
        let body = serde_json::json!({
            "model": pack.model,
            "state": slice.fields,
            "questions": questions,
        });
        let resp = ureq::post(&self.url)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .timeout(std::time::Duration::from_secs(5))
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::Status(401, _) => JevError::Unauthorized,
                ureq::Error::Status(429, _) => JevError::RateLimited("http status 429".into()),
                ureq::Error::Status(code, _) => JevError::Offline(format!("http status {code}")),
                ureq::Error::Transport(t) => JevError::Offline(t.to_string()),
            })?;
        let v: serde_json::Value = resp
            .into_json()
            .map_err(|e| JevError::Malformed(e.to_string()))?;
        parse_systemone_response(&v, pack)
    }
}

/// Parse the System One answers envelope. Malformed shapes refuse loudly —
/// never invent an answer from a half-body.
pub fn parse_systemone_response(
    v: &serde_json::Value,
    pack: &QuestionPack,
) -> Result<Answers, JevError> {
    let obj = v
        .get("answers")
        .and_then(|a| a.as_object())
        .ok_or_else(|| JevError::Malformed("missing answers object".into()))?;
    let mut answers = BTreeMap::new();
    for q in &pack.questions {
        let raw = obj
            .get(&q.id)
            .ok_or_else(|| JevError::Malformed(format!("missing answer {}", q.id)))?;
        let answer = match q.kind {
            QuestionKind::Choice => {
                let choice = raw
                    .get("choice")
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| JevError::Malformed(format!("{}.choice", q.id)))?;
                if !q.options.iter().any(|o| o == choice) {
                    return Err(JevError::Malformed(format!(
                        "{}.choice {choice:?} not in pack options",
                        q.id
                    )));
                }
                let probs = raw
                    .get("probs")
                    .and_then(|p| p.as_object())
                    .ok_or_else(|| JevError::Malformed(format!("{}.probs", q.id)))?;
                let mut map = BTreeMap::new();
                let mut sum = 0.0;
                for (k, val) in probs {
                    let p = val
                        .as_f64()
                        .ok_or_else(|| JevError::Malformed(format!("{}.probs.{k}", q.id)))?;
                    if !p.is_finite() || !(0.0..=1.0).contains(&p) {
                        return Err(JevError::Malformed(format!(
                            "{}.probs.{k} out of range",
                            q.id
                        )));
                    }
                    sum += p;
                    map.insert(k.clone(), p);
                }
                if (sum - 1.0).abs() > 1e-3 {
                    return Err(JevError::Malformed(format!(
                        "{}.probs sum {sum} != 1",
                        q.id
                    )));
                }
                Answer::Choice(ChoiceAnswer {
                    choice: choice.to_string(),
                    probs: map,
                })
            }
            QuestionKind::Score => {
                let level = raw
                    .get("level")
                    .and_then(|l| l.as_u64())
                    .ok_or_else(|| JevError::Malformed(format!("{}.level", q.id)))?;
                if level as usize >= q.options.len() {
                    return Err(JevError::Malformed(format!(
                        "{}.level {level} >= {} levels",
                        q.id,
                        q.options.len()
                    )));
                }
                Answer::Score(ScoreAnswer {
                    level: level as usize,
                    levels: q.options.len(),
                })
            }
            QuestionKind::Noul => {
                let p = raw
                    .get("p_yes")
                    .and_then(|p| p.as_f64())
                    .ok_or_else(|| JevError::Malformed(format!("{}.p_yes", q.id)))?;
                if !p.is_finite() {
                    return Err(JevError::Malformed(format!("{}.p_yes non-finite", q.id)));
                }
                Answer::Noul(NoulAnswer { p_yes: p })
            }
        };
        answers.insert(q.id.clone(), answer);
    }
    Ok(Answers {
        model_id: pack.model.clone(),
        pack_id: pack.id.clone(),
        pack_version: pack.version,
        by_question: answers,
        usage: v
            .get("usage")
            .map(|u| u.to_string())
            .unwrap_or_else(|| "{}".into()),
    })
}

// ---------------------------------------------------------------------------
// Adapter outcome + surfaces (offline policy I4)
// ---------------------------------------------------------------------------

/// Which surface is being enriched. Honesty path refuses on deny; only
/// announce-only surfaces may fall back to the table (journaled loudly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Honesty,
    #[cfg_attr(not(test), allow(dead_code))]
    // ADR-0014 I4: announce-only fallback surface (contract + tests; host wiring pending)
    AnnounceOnly,
}

pub enum AdapterOutcome {
    Answered(Answers),
    /// Honesty path deny: escalate, never silent table fallback.
    Refused(JevError),
    /// Announce-only deny: table fallback, journaled with fallback_flag.
    FallbackTable(JevError),
}

pub struct JudgmentAdapter<T: Transport> {
    pub pack: QuestionPack,
    pub transport: T,
}

impl<T: Transport> JudgmentAdapter<T> {
    pub fn ask_on_surface(&self, slice: &StateSlice, surface: Surface) -> AdapterOutcome {
        match self.transport.ask(slice, &self.pack) {
            Ok(a) => AdapterOutcome::Answered(a),
            Err(e) => match surface {
                Surface::Honesty => AdapterOutcome::Refused(e),
                Surface::AnnounceOnly => AdapterOutcome::FallbackTable(e),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// JudgmentEffect (journal child I2)
// ---------------------------------------------------------------------------

/// Journal child of the ReclassifyIntent effect key — one logical reclassify,
/// one parent intent (L3). Emitted as `Cmd::Audit kind=judgment`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct JudgmentEffect {
    pub parent_effect_key: String,
    pub pack_id: String,
    pub pack_version: u32,
    pub model_id: String,
    pub confidence: f64,
    pub usage: String,
    pub fallback_flag: bool,
    pub escalated: bool,
    pub note: String,
    /// Measured System One POST latency in ms (bd-n86n, ADR-0014 S8
    /// measurement duty). 0 = no POST ran (deny before network).
    #[serde(default)]
    pub latency_ms: u64,
}

// ---------------------------------------------------------------------------
// Merge (table wins I3)
// ---------------------------------------------------------------------------

/// Table leaf output entering the merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDecision {
    pub primitive: String,
    pub topology: String,
    pub argv: Vec<String>,
    pub reason: String,
}

/// Result of folding Jev answers beside the table decision.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeResult {
    /// Always the table primitive — Jev never selects it.
    pub primitive: String,
    /// Always the table topology — Jev never flips it.
    pub topology: String,
    pub argv: Vec<String>,
    pub reason: String,
    /// Advisory text for the escalate Prompt (none-path suggestion,
    /// low-confidence note, sticky-violation alarm). None = quiet merge.
    pub advisory: Option<String>,
    pub escalate: bool,
    pub effect: JudgmentEffect,
}

/// Fold answers beside the table leaf. Rules: table wins on primitive and
/// topology; low confidence escalates; explicit-`none` suggestions attach as
/// advisory (never auto-adopt); topology drift without explicit_topology
/// escalates with a LoudMiss note.
#[allow(clippy::too_many_arguments)]
pub fn merge(
    table: &TableDecision,
    answers: Option<&Answers>,
    conf: f64,
    class: ActionClass,
    thresholds: &Thresholds,
    sticky_topology: Option<&str>,
    explicit_topology: Option<&str>,
    parent_effect_key: &str,
    pack_id: &str,
    pack_version: u32,
    model_id: &str,
    usage: &str,
    fallback_flag: bool,
    latency_ms: u64,
    deny_note: Option<&str>,
) -> MergeResult {
    let mut advisory_parts: Vec<String> = Vec::new();
    let mut escalate = false;

    if let Some(note) = deny_note {
        advisory_parts.push(format!("jev denied: {note}"));
        escalate = true;
    }
    if escalate_required(conf, class, thresholds) {
        advisory_parts.push(format!("low confidence {conf:.2} for {class:?}"));
        escalate = true;
    }
    if table.primitive == "none" && answers.is_some() {
        advisory_parts
            .push("table undecided (none); jev suggestion attached, human confirms".into());
        escalate = true;
    }
    let sticky_violated = match (sticky_topology, explicit_topology) {
        (Some(sticky), None) => table.topology != sticky,
        _ => false,
    };
    if sticky_violated {
        advisory_parts.push(format!(
            "LoudMiss: table topology {} != sticky {} without explicit_topology",
            table.topology,
            sticky_topology.unwrap_or("?")
        ));
        escalate = true;
    }

    let advisory = if advisory_parts.is_empty() {
        None
    } else {
        Some(advisory_parts.join("; "))
    };
    let mut reason = table.reason.clone();
    if let Some(ad) = &advisory {
        reason = format!("{reason} [jev: {ad}]");
    }

    MergeResult {
        primitive: table.primitive.clone(),
        topology: table.topology.clone(),
        argv: table.argv.clone(),
        reason,
        advisory,
        escalate,
        effect: JudgmentEffect {
            parent_effect_key: parent_effect_key.to_string(),
            pack_id: pack_id.to_string(),
            pack_version,
            model_id: model_id.to_string(),
            confidence: conf,
            usage: cap_str(usage, AUDIT_USAGE_CAP),
            fallback_flag,
            escalated: escalate,
            note: cap_str(deny_note.unwrap_or("ok"), AUDIT_NOTE_CAP),
            latency_ms,
        },
    }
}

// ---------------------------------------------------------------------------
// Flag + W2 host entry point
// ---------------------------------------------------------------------------

/// Gate for the W2 enricher (bd-0glq; P7 operator order: Jev on by default).
/// - Explicit `0`/`false`/`no` (or any other set non-truthy value) → `Off`:
///   operator override, byte-identical passthrough, no marker.
/// - Explicit `1`/`true`/`yes` → `On`: configured lane; deny escalates,
///   even keyless (never silent-skip a configured lane).
/// - Unset → `On` iff `TYPESAFE_API_KEY` is present non-empty, else
///   `Unconfigured`: table passthrough with a loud `jev: unconfigured`
///   reason marker, never escalate, never audit (no escalate spam).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevGate {
    Off,
    Unconfigured,
    On,
}

/// Resolve the gate from env. Pure read, no I/O beyond env.
pub fn jev_gate() -> JevGate {
    match env::var(FLAG_ENV) {
        Ok(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => JevGate::On,
            _ => JevGate::Off,
        },
        Err(_) => match env::var(API_KEY_ENV) {
            Ok(k) if !k.trim().is_empty() => JevGate::On,
            _ => JevGate::Unconfigured,
        },
    }
}

// W2 host entry point.
// ---------------------------------------------------------------------------

const PACK_DIR_REL: &str = ".flywheel/judgment-packs";
const PACK_SUFFIX: &str = ".yaml";

/// Env knob for the provisional thresholds (I1): `FLYWHEEL_JEV_THRESHOLDS`
/// as `topology,honesty,announce` floats. Unset or malformed → provisional
/// defaults (malformed logs loudly, never silently half-applies).
pub const THRESHOLDS_ENV: &str = "FLYWHEEL_JEV_THRESHOLDS";

pub fn active_thresholds() -> Thresholds {
    let raw = match env::var(THRESHOLDS_ENV) {
        Ok(v) => v,
        Err(_) => return Thresholds::default(),
    };
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() != 3 {
        eprintln!("flywheel: {THRESHOLDS_ENV} wants 3 floats, keeping defaults");
        return Thresholds::default();
    }
    let mut nums = Vec::with_capacity(3);
    for p in parts {
        match p.parse::<f64>() {
            Ok(f) if f.is_finite() && (0.0..=1.0).contains(&f) => nums.push(f),
            _ => {
                eprintln!("flywheel: {THRESHOLDS_ENV} bad value, keeping defaults");
                return Thresholds::default();
            }
        }
    }
    Thresholds {
        topology_adjacent: nums[0],
        honesty_gate: nums[1],
        announce_only: nums[2],
    }
}

/// Best-effort pack discovery (B5): absolute project path first, then the cwd
/// and its ancestors (flag-on outside a repo root no longer denies every
/// reclassify). Picks the highest `<id>.vN.yaml` version in the first
/// directory that holds any. Missing everywhere with the flag on is a loud
/// deny (escalate + audit), never a silent skip.
/// Newest `<id>.vN.yaml` pack for one id (`reclassify`, `doorbell-screen`,
/// …) — same ancestor-walk discovery as before, generalized (bd-lpn6).
/// `find_pack` stays the reclassify entry point.
pub fn find_pack_for(project: &str, id: &str) -> Result<QuestionPack, String> {
    let mut bases: Vec<std::path::PathBuf> = Vec::new();
    let p = std::path::PathBuf::from(project);
    if p.is_absolute() {
        bases.push(p);
    }
    let mut cursor = env::current_dir().ok();
    while let Some(dir) = cursor {
        bases.push(dir.clone());
        cursor = dir.parent().map(|q| q.to_path_buf());
    }
    // P3: pack loads ride a (dir, id) cache with mtime invalidation —
    // repeat reclassifies in one process skip re-read + re-parse.
    static PACK_CACHE: std::sync::OnceLock<std::sync::Mutex<PackCache>> =
        std::sync::OnceLock::new();
    let mut tried: Vec<std::path::PathBuf> = Vec::new();
    for base in &bases {
        let dir = base.join(PACK_DIR_REL);
        tried.push(dir.clone());
        let hit = PACK_CACHE
            .get_or_init(|| std::sync::Mutex::new(PackCache::default()))
            .lock()
            .expect("pack cache")
            .get_or_load(&dir, id);
        if let Some(pack) = hit {
            return pack;
        }
    }
    Err(format!(
        "{id} pack not found (tried {tried:?}); add {PACK_DIR_REL}/{id}.vN.yaml or unset {FLAG_ENV}"
    ))
}

/// Reclassify entry point (bd-n86n path). Passes the pack id through.
pub fn find_pack(project: &str) -> Result<QuestionPack, String> {
    find_pack_for(project, "reclassify")
}

/// Pack cache keyed by (dir, id) with mtime invalidation (P3).
/// ponytail: std HashMap, no TTL, mtime is the invalidation.
#[derive(Debug, Default)]
pub struct PackCache {
    entries: std::collections::HashMap<(String, String), (u64, QuestionPack)>,
}

impl PackCache {
    pub fn get_or_load(
        &mut self,
        dir: &std::path::Path,
        id: &str,
    ) -> Option<Result<QuestionPack, String>> {
        let prefix = format!("{id}.v");
        let path = newest_pack_path(dir, &prefix)?;
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let key = (dir.to_string_lossy().into_owned(), id.to_string());
        if let Some((cached_mtime, pack)) = self.entries.get(&key)
            && *cached_mtime == mtime
        {
            return Some(Ok(pack.clone()));
        }
        let loaded = QuestionPack::load_from_file(&path);
        if let Ok(pack) = &loaded {
            self.entries.insert(key, (mtime, pack.clone()));
        }
        Some(loaded)
    }
}

fn newest_pack_path(dir: &std::path::Path, prefix: &str) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u32, std::path::PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(prefix) && name.ends_with(PACK_SUFFIX)) {
            continue;
        }
        let mid = &name[prefix.len()..name.len() - PACK_SUFFIX.len()];
        if let Ok(v) = mid.parse::<u32>()
            && best.as_ref().is_none_or(|(bv, _)| v > *bv)
        {
            best = Some((v, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// Arguments for the host expand hook — everything `expand_reclassify_cmds`
/// already holds from the journaled intent plus the table leaf output.
pub struct ExpandArgs {
    pub project: String,
    pub goal: String,
    pub pin: String,
    pub reason: String,
    pub explicit_topology: Option<String>,
    pub parent_effect_key: String,
    pub sticky_topology: Option<String>,
    pub table_primitive: String,
    pub table_topology: String,
    pub table_argv: Vec<String>,
    pub table_reason: String,
}

pub struct ExpandOut {
    pub primitive: String,
    pub topology: String,
    pub argv: Vec<String>,
    pub reason: String,
    /// Table-leaf primitive/topology entering the merge (bd-lpn6 ledger).
    /// `primitive`/`topology` above always equal these (Jev never flips).
    pub table_primitive: String,
    pub table_topology: String,
    /// Whether the merge escalated (low conf, deny, sticky-violation, screen).
    pub escalated: bool,
    /// Some(pin, detail) → caller pushes `Cmd::Audit kind=judgment`.
    pub audit: Option<(String, serde_json::Value)>,
    /// Battery row (bd-lpn6): second `Cmd::Audit kind=judgment` when the
    /// doorbell screen ran. None when skipped or flag off.
    pub screen_audit: Option<(String, serde_json::Value)>,
    /// Battery summary for the miss-ledger row. None when skipped.
    pub screen: Option<ScreenSummary>,
}

/// Split of the enrich path across the journal tx boundary (bd-n86n): the
/// blocking System One POST must never run inside `commit_reclassify`'s
/// closure (it holds the journal tx). `prepare_post` (pack discovery, slice
/// build) and `run_post` (timed network) run OUTSIDE the tx; only
/// `finish_plan` (merge) runs inside, beside `tick`.
#[derive(Debug)]
pub struct PostPlan {
    pub table: TableDecision,
    pub pin: String,
    pub explicit_topology: Option<String>,
    pub parent_effect_key: String,
    pub sticky_topology: Option<String>,
    pub stage: PostStage,
}

#[derive(Debug)]
pub enum PostStage {
    /// Flag off: table passes through untouched, no audit.
    Off,
    /// Flag on but undeployable (missing pack / no key): deny merged now,
    /// no POST ever runs. `latency_ms` records 0 (no network happened).
    Deny {
        note: String,
        pack_id: String,
        pack_version: u32,
        model_id: String,
    },
    /// Flag on and deployable: POST outside the tx, then finish.
    /// `screen` is the doorbell battery (bd-lpn6): a second pack asked over
    /// inbound material in the same outside-tx window; detections annotate
    /// routing only, never flip the table leaf.
    Ready {
        slice: StateSlice,
        pack: QuestionPack,
        transport: UreqTransport,
        screen: Option<ScreenAsk>,
    },
}

/// One doorbell-screen battery ask: pack + redacted inbound-material slice.
/// Built in prepare (no network); asked beside the reclassify POST.
#[derive(Debug, Clone)]
pub struct ScreenAsk {
    pub slice: StateSlice,
    pub pack: QuestionPack,
}

fn table_of(a: &ExpandArgs) -> TableDecision {
    TableDecision {
        primitive: a.table_primitive.clone(),
        topology: a.table_topology.clone(),
        argv: a.table_argv.clone(),
        reason: a.table_reason.clone(),
    }
}

fn plan_of(a: ExpandArgs, stage: PostStage) -> PostPlan {
    PostPlan {
        table: table_of(&a),
        pin: a.pin,
        explicit_topology: a.explicit_topology,
        parent_effect_key: a.parent_effect_key,
        sticky_topology: a.sticky_topology,
        stage,
    }
}

/// Phase 1 (no network): flag check, pack discovery, key read, slice build.
/// Returns `Ready` iff a POST should run; the caller runs `run_post`
/// outside the journal tx, then `finish_plan` inside. `Unconfigured`
/// (keyless default) skips loud with a reason marker — table passthrough,
/// no escalate, no audit.
pub fn prepare_post_with_authority(a: ExpandArgs, authority: &RuntimeAuthority) -> PostPlan {
    prepare_post_with_authority_and_gate(a, authority, jev_gate())
}

fn prepare_post_with_authority_and_gate(
    a: ExpandArgs,
    authority: &RuntimeAuthority,
    gate: JevGate,
) -> PostPlan {
    match gate {
        JevGate::Off => return plan_of(a, PostStage::Off),
        JevGate::Unconfigured => {
            let mut table = table_of(&a);
            table.reason = format!(
                "{} [jev: unconfigured — set TYPESAFE_API_KEY to enable judgment]",
                table.reason
            );
            let plan = PostPlan {
                table,
                pin: a.pin,
                explicit_topology: a.explicit_topology,
                parent_effect_key: a.parent_effect_key,
                sticky_topology: a.sticky_topology,
                stage: PostStage::Off,
            };
            return plan;
        }
        JevGate::On => {}
    }
    let pack = match find_pack(&a.project) {
        Ok(p) => p,
        Err(e) => {
            return plan_of(
                a,
                PostStage::Deny {
                    note: e,
                    pack_id: "reclassify".into(),
                    pack_version: 0,
                    model_id: JEV_MODEL_PIN.into(),
                },
            );
        }
    };
    let transport = match UreqTransport::from_authority(authority) {
        Ok(t) => t,
        Err(e) => {
            return plan_of(
                a,
                PostStage::Deny {
                    note: e.to_string(),
                    pack_id: pack.id.clone(),
                    pack_version: pack.version,
                    model_id: pack.model.clone(),
                },
            );
        }
    };
    let mut slice_fields = BTreeMap::new();
    slice_fields.insert("goal".to_string(), a.goal.clone());
    slice_fields.insert("reason".to_string(), a.reason.clone());
    slice_fields.insert("pin".to_string(), a.pin.clone());
    slice_fields.insert("table_primitive".to_string(), a.table_primitive.clone());
    slice_fields.insert("table_topology".to_string(), a.table_topology.clone());
    let slice = build_state_slice(slice_fields);
    // Doorbell battery (bd-lpn6): screen inbound material with a second
    // pack in the same outside-tx window. A missing screen pack skips the
    // battery loudly without touching the reclassify verdict (auxiliary).
    let screen = match find_pack_for(&a.project, "doorbell-screen") {
        Ok(screen_pack) => {
            let mut screen_fields = BTreeMap::new();
            screen_fields.insert("material.text".to_string(), a.goal.clone());
            screen_fields.insert("material.reason".to_string(), a.reason.clone());
            screen_fields.insert("table_primitive".to_string(), a.table_primitive.clone());
            Some(ScreenAsk {
                slice: build_state_slice(screen_fields),
                pack: screen_pack,
            })
        }
        Err(e) => {
            eprintln!("flywheel: doorbell-screen pack absent ({e}) — battery skipped");
            None
        }
    };
    plan_of(
        a,
        PostStage::Ready {
            slice,
            pack,
            transport,
            screen,
        },
    )
}

/// Phase 2 (network, timed): run the POST outside the journal tx and
/// return the outcome + POST latency in ms for the audit row (bd-n86n,
/// ADR-0014 S8 measurement duty). Takes the adapter by value — one POST
/// per plan, never retried inside (retries re-enter as new reclassifies
/// under the same parent effect key).
pub fn run_post<T: Transport>(
    adapter: JudgmentAdapter<T>,
    slice: &StateSlice,
    surface: Surface,
) -> (AdapterOutcome, u64) {
    let start = std::time::Instant::now();
    let outcome = adapter.ask_on_surface(slice, surface);
    (outcome, start.elapsed().as_millis() as u64)
}

/// Phase 3 (merge only, no network): fold the POST outcome beside the table
/// leaf. `posted=None` on a `Ready` plan is a programming error — deny
/// loud, never quiet-merge. `screen_posted` carries the doorbell-battery
/// outcome (None = battery skipped in prepare); detections annotate routing
/// only — the table leaf still wins on primitive and topology.
pub fn finish_plan(
    plan: PostPlan,
    posted: Option<(AdapterOutcome, u64)>,
    screen_posted: Option<(AdapterOutcome, u64)>,
) -> ExpandOut {
    let PostPlan {
        table,
        pin,
        explicit_topology,
        parent_effect_key,
        sticky_topology,
        stage,
    } = plan;
    match stage {
        PostStage::Off => ExpandOut {
            primitive: table.primitive.clone(),
            topology: table.topology.clone(),
            argv: table.argv.clone(),
            reason: table.reason.clone(),
            table_primitive: table.primitive,
            table_topology: table.topology,
            escalated: false,
            audit: None,
            screen_audit: None,
            screen: None,
        },
        PostStage::Deny {
            note,
            pack_id,
            pack_version,
            model_id,
        } => {
            let merged = merge(
                &table,
                None,
                0.0,
                ActionClass::HonestyGate,
                &active_thresholds(),
                sticky_topology.as_deref(),
                explicit_topology.as_deref(),
                &parent_effect_key,
                &pack_id,
                pack_version,
                &model_id,
                "{}",
                false,
                0,
                Some(&note),
            );
            ExpandOut {
                primitive: merged.primitive,
                topology: merged.topology,
                argv: merged.argv,
                reason: merged.reason,
                table_primitive: table.primitive,
                table_topology: table.topology,
                escalated: merged.escalate,
                audit: Some((pin, serde_json::json!(&merged.effect))),
                screen_audit: None,
                screen: None,
            }
        }
        PostStage::Ready {
            slice: _,
            pack,
            transport: _,
            screen,
        } => {
            // (answers, deny, fallback_flag, latency). Honesty never falls
            // back, but the arm is defensive rather than unreachable: a
            // fallback here still escalates.
            let (answers_opt, deny_opt, fallback_flag, latency_ms) = match posted {
                Some((AdapterOutcome::Answered(a), ms)) => (Some(a), None, false, ms),
                Some((AdapterOutcome::Refused(e), ms)) => (
                    None,
                    Some(format!("{} [{}]", e, e.classified().kind)),
                    false,
                    ms,
                ),
                Some((AdapterOutcome::FallbackTable(e), ms)) => (
                    None,
                    Some(format!("{} [{}]", e, e.classified().kind)),
                    true,
                    ms,
                ),
                None => (
                    None,
                    Some("post skipped: Ready plan finished without a POST".to_string()),
                    false,
                    0,
                ),
            };
            let (pack_id, pack_version, model_id, usage, conf) = match &answers_opt {
                Some(a) => (
                    a.pack_id.clone(),
                    a.pack_version,
                    a.model_id.clone(),
                    a.usage.clone(),
                    pack_confidence(a),
                ),
                None => (
                    pack.id.clone(),
                    pack.version,
                    pack.model.clone(),
                    "{}".to_string(),
                    0.0,
                ),
            };
            let mut merged = merge(
                &table,
                answers_opt.as_ref(),
                conf,
                ActionClass::HonestyGate,
                &active_thresholds(),
                sticky_topology.as_deref(),
                explicit_topology.as_deref(),
                &parent_effect_key,
                &pack_id,
                pack_version,
                &model_id,
                &usage,
                fallback_flag,
                latency_ms,
                deny_opt.as_deref(),
            );
            // Doorbell battery fold (bd-lpn6): detections ride the reason +
            // escalate flag (routing only); the table primitive/topology above
            // are untouched. The battery gets its own audit row.
            let (screen_parts, screen_escalate, screen_summary, screen_effect) =
                fold_screen(screen.as_ref(), screen_posted, &parent_effect_key);
            if !screen_parts.is_empty() {
                let extra = screen_parts.join("; ");
                merged.reason = format!("{} [screen: {extra}]", merged.reason);
                merged.advisory = Some(match merged.advisory {
                    Some(a) => format!("{a}; {extra}"),
                    None => extra,
                });
            }
            if screen_escalate {
                merged.escalate = true;
                merged.effect.escalated = true;
            }
            let screen_audit =
                screen_effect.map(|effect| (pin.clone(), serde_json::json!(&effect)));
            ExpandOut {
                primitive: merged.primitive,
                topology: merged.topology,
                argv: merged.argv,
                reason: merged.reason,
                table_primitive: table.primitive,
                table_topology: table.topology,
                escalated: merged.escalate,
                audit: Some((pin, serde_json::json!(&merged.effect))),
                screen_audit,
                screen: screen_summary,
            }
        }
    }
}

/// Fold one doorbell-battery outcome into routing advisory (bd-lpn6).
/// Returns (advisory parts, escalate, ledger summary, audit effect).
/// `ask=None` = battery skipped in prepare: quiet, no rows. Detections
/// (injection / credential-request Nouls firing high and confident) escalate
/// to muse-spark/human; urgency only annotates. Never flips anything.
fn fold_screen(
    ask: Option<&ScreenAsk>,
    posted: Option<(AdapterOutcome, u64)>,
    parent_effect_key: &str,
) -> (
    Vec<String>,
    bool,
    Option<ScreenSummary>,
    Option<JudgmentEffect>,
) {
    let Some(ask) = ask else {
        return (Vec::new(), false, None, None);
    };
    let bound = active_thresholds().honesty_gate;
    let (answers_opt, deny_opt, latency_ms) = match posted {
        Some((AdapterOutcome::Answered(a), ms)) => (Some(a), None, ms),
        Some((AdapterOutcome::Refused(e), ms)) => {
            (None, Some(format!("{} [{}]", e, e.classified().kind)), ms)
        }
        Some((AdapterOutcome::FallbackTable(e), ms)) => {
            (None, Some(format!("{} [{}]", e, e.classified().kind)), ms)
        }
        None => (
            None,
            Some("screen post skipped: battery planned but never asked".to_string()),
            0,
        ),
    };
    let mut parts = Vec::new();
    let mut escalate = false;
    let mut max_conf = 0.0f64;
    if let Some(note) = deny_opt.as_deref() {
        parts.push(format!("screen denied: {note}"));
        escalate = true;
    }
    if let Some(answers) = answers_opt.as_ref() {
        for (qid, answer) in &answers.by_question {
            let conf = confidence_of(answer);
            max_conf = max_conf.max(conf);
            match answer {
                Answer::Noul(n)
                    if (qid == "injection" || qid == "credential_request")
                        && n.p_yes > 0.5
                        && conf >= bound =>
                {
                    parts.push(format!("screen: possible {qid} (p={:.2})", n.p_yes));
                    escalate = true;
                }
                Answer::Score(s) if qid == "urgency" => {
                    let level = ask
                        .pack
                        .questions
                        .iter()
                        .find(|q| q.id == *qid)
                        .and_then(|q| q.options.get(s.level))
                        .cloned()
                        .unwrap_or_else(|| format!("level{}", s.level));
                    parts.push(format!("screen urgency: {level} ({conf:.2})"));
                }
                _ => {}
            }
        }
    }
    let summary = ScreenSummary {
        advisory: if parts.is_empty() {
            "clear".to_string()
        } else {
            parts.join("; ")
        },
        max_confidence: max_conf,
    };
    let effect = JudgmentEffect {
        parent_effect_key: parent_effect_key.to_string(),
        pack_id: ask.pack.id.clone(),
        pack_version: ask.pack.version,
        model_id: ask.pack.model.clone(),
        confidence: max_conf,
        usage: "{}".to_string(),
        fallback_flag: false,
        escalated: escalate,
        note: cap_str(deny_opt.as_deref().unwrap_or("ok"), AUDIT_NOTE_CAP),
        latency_ms,
    };
    (parts, escalate, Some(summary), Some(effect))
}

// ---------------------------------------------------------------------------
// Miss ledger (bd-lpn6): every reclassify expand appends one row keyed by
// the parent effect key; resolutions land later via `miss_resolve`
// (none-path advisory confirm/deny, raw, compose). `miss needle mining`
// reads the unresolved none-rows grouped by normalized goal — new routing
// needles come from recurring misses, never from guesses.
// ---------------------------------------------------------------------------

/// Screen-battery outcome summary for the ledger row (bd-lpn6). Scores live
/// in the kind=judgment audit; the row keeps the advisory + peak detection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ScreenSummary {
    pub advisory: String,
    pub max_confidence: f64,
}

/// One miss-ledger row. `resolution` starts None; a later `miss_resolve`
/// appends a second row with the same key carrying the verdict.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MissRow {
    pub ts: String,
    pub effect_key: String,
    pub goal: String,
    pub table_primitive: String,
    pub table_topology: String,
    pub escalated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<ScreenSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Resolutions the ledger accepts. confirm/deny close a none-path advisory;
/// raw/compose record how an undecided goal was actually dispatched.
pub const MISS_RESOLUTIONS: &[&str] = &["confirm", "deny", "raw", "compose"];

/// Max goal chars kept per row (goals can be whole briefs).
pub const MISS_GOAL_CAP: usize = 512;

pub fn miss_ledger_path() -> std::path::PathBuf {
    crate::config::state_dir().join("miss-ledger.jsonl")
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Append one row (JSON line). The caller owns durability policy: the
/// expand path treats failure as a loud warn, never a dispatch failure.
pub fn miss_append(row: &MissRow) -> Result<(), String> {
    use std::io::Write;
    let path = miss_ledger_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("miss ledger mkdir: {e}"))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("miss ledger open: {e}"))?;
    let line = serde_json::to_string(row).map_err(|e| format!("miss ledger encode: {e}"))?;
    writeln!(f, "{line}").map_err(|e| format!("miss ledger write: {e}"))?;
    Ok(())
}

/// Read all valid rows, oldest first. Corrupt lines are skipped with one
/// loud line on stderr (a torn tail must never poison mining).
pub fn miss_read() -> Vec<MissRow> {
    let path = miss_ledger_path();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut rows = Vec::new();
    let mut skipped = 0;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<MissRow>(line) {
            Ok(r) => rows.push(r),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        eprintln!(
            "flywheel: miss ledger skipped {skipped} corrupt line(s) in {}",
            path.display()
        );
    }
    rows
}

/// Fold rows to latest-per-key (a resolution row supersedes its expand row).
fn miss_fold_latest(rows: Vec<MissRow>) -> Vec<MissRow> {
    let mut order: Vec<String> = Vec::new();
    let mut by_key: BTreeMap<String, MissRow> = BTreeMap::new();
    for r in rows {
        if !by_key.contains_key(&r.effect_key) {
            order.push(r.effect_key.clone());
        }
        by_key.insert(r.effect_key.clone(), r);
    }
    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .collect()
}

/// Record a resolution for a known effect key. Fails loud on an unknown key
/// or a bad verdict (never invents ledger history).
pub fn miss_resolve(
    effect_key: &str,
    resolution: &str,
    note: Option<&str>,
) -> Result<MissRow, String> {
    if !MISS_RESOLUTIONS.contains(&resolution) {
        return Err(format!(
            "bad resolution {resolution:?}; want one of {}",
            MISS_RESOLUTIONS.join(",")
        ));
    }
    let mut base = miss_read()
        .into_iter()
        .rfind(|r| r.effect_key == effect_key)
        .ok_or_else(|| format!("unknown effect key {effect_key:?} (no expand row)"))?;
    base.resolution = Some(resolution.to_string());
    base.resolved_ts = Some(now_rfc3339());
    base.note = note.map(str::to_string);
    miss_append(&base)?;
    Ok(base)
}

/// Latest-per-key rows still awaiting a resolution.
pub fn miss_unresolved() -> Vec<MissRow> {
    miss_fold_latest(miss_read())
        .into_iter()
        .filter(|r| r.resolution.is_none())
        .collect()
}

/// Normalize a goal for grouping: lowercase, collapse whitespace, cap.
fn miss_norm_goal(goal: &str) -> String {
    let mut out = String::with_capacity(goal.len().min(120));
    let mut prev_space = true;
    for c in goal.to_lowercase().chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
        if out.len() >= 120 {
            break;
        }
    }
    out.trim().to_string()
}

/// Needle candidates: unresolved `none`-primitive rows grouped by normalized
/// goal, most-recurring first. Recurring misses are routing-table gaps.
pub fn miss_needle_candidates() -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in miss_unresolved() {
        if r.table_primitive == "none" {
            *counts.entry(miss_norm_goal(&r.goal)).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// Verify prescreen fold (bd-lpn6): advisory input to ce-evaluate, never a
/// verdict. Returns (advisory, destructive_flag): the shape Choice value +
/// the destructive Noul, plainly stated for a human to judge.
pub fn fold_prescreen(answers: &Answers) -> (String, bool) {
    let shape = answers
        .by_question
        .get("safe_shape")
        .and_then(|a| match a {
            Answer::Choice(c) => Some(c.choice.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "unknown".to_string());
    let destructive_p = answers
        .by_question
        .get("destructive_risk")
        .and_then(|a| match a {
            Answer::Noul(n) => Some(n.p_yes),
            _ => None,
        })
        .unwrap_or(0.5);
    let conf = pack_confidence(answers);
    (
        format!("prescreen: shape={shape} destructive_p={destructive_p:.2} conf={conf:.2}"),
        destructive_p > 0.5 && conf >= active_thresholds().honesty_gate,
    )
}

/// Verify prescreen ask (bd-lpn6): runs the verify-prescreen pack over
/// {verify.cmd} outside any tx. Deny (no pack/key) errors loud — the caller
/// renders it into the advisory channel, never into a verdict field.
pub fn prescreen_verify(
    project: &str,
    cmd_text: &str,
    authority: &RuntimeAuthority,
) -> Result<(String, bool, f64, u64), String> {
    let pack = find_pack_for(project, "verify-prescreen")?;
    let transport = UreqTransport::from_authority(authority).map_err(|e| e.to_string())?;
    let mut fields = BTreeMap::new();
    fields.insert("verify.cmd".to_string(), cmd_text.to_string());
    let slice = build_state_slice(fields);
    let adapter = JudgmentAdapter { pack, transport };
    let (outcome, latency_ms) = run_post(adapter, &slice, Surface::Honesty);
    match outcome {
        AdapterOutcome::Answered(a) => {
            let conf = pack_confidence(&a);
            let (advisory, destructive) = fold_prescreen(&a);
            Ok((advisory, destructive, conf, latency_ms))
        }
        AdapterOutcome::Refused(e) | AdapterOutcome::FallbackTable(e) => Err(e.to_string()),
    }
}

/// Coordination-annotator surfaces (bd-lpn6). Each maps to one pack; every
/// result is advisory text for the captain — annotators never dispatch,
/// hold, assign, or close anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotateSurface {
    CoalitionIntent,
    WaveReadiness,
    ReviewerRank,
    /// bd-adv4: smart profile pickup — one coder-family candidate per
    /// call, the caller ranks repeated calls; the user decides.
    ProfilePick,
    /// bd-88m4: model+harness tier suggestion — reasoning depth picks the
    /// cost band, harness coupling flags proprietary-harness tasks.
    TierPick,
}

impl AnnotateSurface {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "coalition-intent" => Some(Self::CoalitionIntent),
            "wave-readiness" => Some(Self::WaveReadiness),
            "reviewer-rank" => Some(Self::ReviewerRank),
            "profile-pick" => Some(Self::ProfilePick),
            "tier-pick" => Some(Self::TierPick),
            _ => None,
        }
    }

    fn pack_id(self) -> &'static str {
        match self {
            Self::CoalitionIntent => "coalition-intent",
            Self::WaveReadiness => "wave-readiness",
            Self::ReviewerRank => "reviewer-rank",
            Self::ProfilePick => "profile-pick",
            Self::TierPick => "tier-pick",
        }
    }
}

/// Annotator fold (bd-lpn6): answers → advisory line. Pure; network lives in
/// `annotate`. Coalition unclear and wave unreadiness stay advisory text —
/// the captain (or code gate) acts, never this fold.
pub fn fold_annotate(surface: AnnotateSurface, answers: &Answers, pack: &QuestionPack) -> String {
    let conf = pack_confidence(answers);
    match surface {
        AnnotateSurface::CoalitionIntent => {
            let intent = answers
                .by_question
                .get("intent")
                .and_then(|a| match a {
                    Answer::Choice(c) => Some(c.choice.as_str()),
                    _ => None,
                })
                .unwrap_or("unclear");
            if intent == "unclear" {
                format!("annotate coalition: unclear ({conf:.2}) — ask before forming")
            } else {
                format!("annotate coalition: intent={intent} ({conf:.2})")
            }
        }
        AnnotateSurface::WaveReadiness => {
            let p = answers
                .by_question
                .get("ready_to_release")
                .and_then(|a| match a {
                    Answer::Noul(n) => Some(n.p_yes),
                    _ => None,
                })
                .unwrap_or(0.5);
            format!("annotate wave: ready_p={p:.2} ({conf:.2})")
        }
        AnnotateSurface::ReviewerRank => {
            let (level, lv_conf) = answers
                .by_question
                .get("reviewer_fit")
                .and_then(|a| match a {
                    Answer::Score(s) => pack
                        .questions
                        .iter()
                        .find(|q| q.id == "reviewer_fit")
                        .and_then(|q| q.options.get(s.level))
                        .map(|name| (name.clone(), confidence_of(a))),
                    _ => None,
                })
                .unwrap_or(("unknown".to_string(), 0.0));
            format!("annotate reviewer: fit={level} ({lv_conf:.2})")
        }
        AnnotateSurface::ProfilePick => {
            let (level, lv_conf) = answers
                .by_question
                .get("profile_fit")
                .and_then(|a| match a {
                    Answer::Score(s) => pack
                        .questions
                        .iter()
                        .find(|q| q.id == "profile_fit")
                        .and_then(|q| q.options.get(s.level))
                        .map(|name| (name.clone(), confidence_of(a))),
                    _ => None,
                })
                .unwrap_or(("unknown".to_string(), 0.0));
            format!("annotate profile: fit={level} ({lv_conf:.2})")
        }
        AnnotateSurface::TierPick => {
            // bd-vlvn: reasoning_depth is a Score — the level indexes the
            // pack's ordered options, mirroring the reviewer-rank arm.
            let depth = answers
                .by_question
                .get("reasoning_depth")
                .and_then(|a| match a {
                    Answer::Score(s) => pack
                        .questions
                        .iter()
                        .find(|q| q.id == "reasoning_depth")
                        .and_then(|q| q.options.get(s.level)),
                    _ => None,
                })
                .map(String::as_str)
                .unwrap_or("substantial");
            let coupling = answers
                .by_question
                .get("harness_coupling")
                .and_then(|a| match a {
                    Answer::Choice(c) => Some(c.choice.as_str()),
                    _ => None,
                })
                .unwrap_or("open");
            let conf = pack_confidence(answers);
            format!("annotate tier: depth={depth} coupling={coupling} ({conf:.2})")
        }
    }
}

/// bd-88m4: offline FallbackTable for the tier-pick pack — strong textual
/// cues map to a slot band, everything else returns None, which IS current
/// behavior (the briefing renders no suggestion and today's menus stand).
/// The live path is `annotate(project, AnnotateSurface::TierPick, ...)`;
/// the user always decides — a suggestion never dispatches.
pub fn suggest_tier_pick(objective: &str) -> Option<&'static str> {
    let lower = objective.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let has = |w: &str| tokens.contains(&w);
    if has("typo")
        || has("rename")
        || lower.contains("one-line")
        || lower.contains("one line")
        || has("docs")
        || lower.contains("mechanical")
    {
        return Some("@cheap");
    }
    if has("architecture")
        || has("redesign")
        || has("migration")
        || has("security")
        || lower.contains("perf budget")
        || lower.contains("frontier")
    {
        return Some("@premium");
    }
    None
}

/// bd-vlvn: reject-all confidence gate — a live suggestion renders only
/// when its calibrated confidence clears the announce-only bar (default
/// 0.7, captain-tunable). Below the bar the suggestion is NOTHING (not
/// even the table): a weak live signal is evidence against the table cue,
/// while a deny (no signal at all) keeps it.
pub fn suggestion_gate(conf: f64) -> bool {
    conf >= active_thresholds().announce_only
}

/// One batched candidate score (bd-vlvn): the candidate, its Score level
/// and level name, and the calibrated confidence.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateScore {
    pub candidate: String,
    pub level: usize,
    pub level_name: String,
    pub confidence: f64,
}

/// bd-vlvn: batch per-candidate scores into ONE request. Clone the Score
/// question per candidate (`<qid>__<candidate>`, instructions specialized
/// to that candidate), ask once, split the answers back. The transport
/// still sees a single POST; options/criteria ride each clone unchanged.
pub fn batch_pack_for_candidates(
    pack: &QuestionPack,
    qid: &str,
    candidates: &[String],
) -> Result<QuestionPack, String> {
    let template = pack
        .questions
        .iter()
        .find(|q| q.id == qid)
        .ok_or_else(|| format!("batch: question {qid:?} not in pack {}", pack.id))?;
    if !matches!(template.kind, QuestionKind::Score) {
        return Err(format!("batch: question {qid:?} is not a Score"));
    }
    let questions = candidates
        .iter()
        .map(|c| QuestionDef {
            id: format!("{qid}__{c}"),
            kind: template.kind,
            instructions: template
                .instructions
                .replace("`candidate`", &format!("`{c}`")),
            options: template.options.clone(),
            criteria: template.criteria.clone(),
        })
        .collect();
    Ok(QuestionPack {
        id: pack.id.clone(),
        version: pack.version,
        model: pack.model.clone(),
        questions,
    })
}

/// bd-vlvn: split batched answers back per candidate. Missing or
/// non-Score answers drop that candidate (loud skip, never invented).
pub fn split_batched_scores(
    answers: &Answers,
    pack: &QuestionPack,
    qid: &str,
    candidates: &[String],
) -> Vec<CandidateScore> {
    let mut out = Vec::new();
    for c in candidates {
        let key = format!("{qid}__{c}");
        let Some(Answer::Score(s)) = answers.by_question.get(&key) else {
            continue;
        };
        let name = pack
            .questions
            .iter()
            .find(|q| q.id == key)
            .and_then(|q| q.options.get(s.level))
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        out.push(CandidateScore {
            candidate: c.clone(),
            level: s.level,
            level_name: name,
            confidence: confidence_of(&Answer::Score(ScoreAnswer {
                level: s.level,
                levels: s.levels,
            })),
        });
    }
    out
}

/// bd-vlvn: rank batched scores — highest level wins, confidence breaks
/// ties. None when empty.
pub fn rank_candidate_scores(scores: &[CandidateScore]) -> Option<&CandidateScore> {
    scores.iter().max_by(|a, b| {
        a.level.cmp(&b.level).then_with(|| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    })
}

/// bd-vlvn: live profile suggestion — batch-score implement candidates in
/// ONE request (candidate when-scope excerpts ride the state), rank, then
/// gate. Ok(Some) = gated `@profile`; Ok(None) = answered-but-weak
/// (nothing, not even the table) or no candidates; Err = deny (the caller
/// keeps the FallbackTable).
pub fn live_profile_suggestion<T: Transport>(
    transport: &T,
    pack: &QuestionPack,
    goal: &str,
    candidates: &[(String, String)],
) -> Result<Option<String>, JevError> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let names: Vec<String> = candidates.iter().map(|(n, _)| n.clone()).collect();
    let batch =
        batch_pack_for_candidates(pack, "profile_fit", &names).map_err(JevError::Malformed)?;
    let mut fields = BTreeMap::new();
    fields.insert("goal.text".to_string(), goal.to_string());
    for (name, when) in candidates {
        fields.insert(format!("candidate.{name}.when"), when.clone());
    }
    let slice = build_state_slice(fields);
    let answers = transport.ask(&slice, &batch)?;
    let scores = split_batched_scores(&answers, &batch, "profile_fit", &names);
    let best = rank_candidate_scores(&scores);
    match best {
        Some(b) if suggestion_gate(b.confidence) => Ok(Some(format!("@{}", b.candidate))),
        _ => Ok(None),
    }
}

/// bd-vlvn: live tier suggestion — one request over the tier-pick pack
/// (reasoning_depth Score + harness_coupling Choice). routine maps to
/// @cheap, frontier to @premium, substantial (the unsure middle) to
/// nothing; the whole suggestion gates on pack confidence. Err = deny
/// (the caller keeps the FallbackTable).
/// Shared tier-pick ask: one request, confidence gate, depth band name.
/// `None` = gate rejected or no depth answer (caller keeps its default).
/// Err = deny (the caller keeps its table). One site for the ask shape so
/// the slot-band and effort arms cannot drift apart.
fn ask_depth_band<T: Transport>(
    transport: &T,
    pack: &QuestionPack,
    goal: &str,
) -> Result<Option<String>, JevError> {
    let mut fields = BTreeMap::new();
    fields.insert("goal.text".to_string(), goal.to_string());
    let slice = build_state_slice(fields);
    let answers = transport.ask(&slice, pack)?;
    if !suggestion_gate(pack_confidence(&answers)) {
        return Ok(None);
    }
    let depth = answers.by_question.get("reasoning_depth");
    Ok(match depth {
        Some(Answer::Score(s)) => pack
            .questions
            .iter()
            .find(|q| q.id == "reasoning_depth")
            .and_then(|q| q.options.get(s.level))
            .cloned(),
        _ => None,
    })
}

pub fn live_tier_suggestion<T: Transport>(
    transport: &T,
    pack: &QuestionPack,
    goal: &str,
) -> Result<Option<String>, JevError> {
    match ask_depth_band(transport, pack, goal)?.as_deref() {
        Some("routine") => Ok(Some("@cheap".to_string())),
        Some("frontier") => Ok(Some("@premium".to_string())),
        _ => Ok(None),
    }
}

/// governed-by: bd-ulzi — tier-pick reasoning_depth maps to a harness
/// effort level: routine/low, substantial/high, frontier/max. Unknown
/// (or empty) maps to empty, which means the profile default stands —
/// a suggestion never fabricates an effort. Levels are the shared
/// pi/omp/pa vocabulary (off/minimal/low/medium/high/xhigh/max).
pub fn effort_for_depth(depth: &str) -> &'static str {
    match depth.trim() {
        "routine" => "low",
        "substantial" => "high",
        "frontier" => "max",
        _ => "",
    }
}

/// governed-by: bd-ulzi — live effort suggestion: the same tier-pick ask
/// and confidence gate as [`live_tier_suggestion`], but the depth maps to
/// an effort level for the dispatch-spec effort input (not a model slot).
/// Unlike the slot band, the unsure middle IS covered (substantial/high).
/// Err = deny (the caller keeps the profile default).
pub fn live_effort_suggestion<T: Transport>(
    transport: &T,
    pack: &QuestionPack,
    goal: &str,
) -> Result<Option<String>, JevError> {
    match ask_depth_band(transport, pack, goal)?.as_deref() {
        Some(d) if !effort_for_depth(d).is_empty() => Ok(Some(effort_for_depth(d).to_string())),
        _ => Ok(None),
    }
}

/// bd-adv4: offline FallbackTable for the profile-pick pack — deterministic
/// language cues over the objective text. The live path is
/// `annotate(project, AnnotateSurface::ProfilePick, goal, extra, authority)`
/// once per coder-family candidate (the caller ranks by fit level, then
/// confidence); this table is what the steer briefing shows when Jev is
/// unreachable, and `@coder` is the default the compose tokens ship. The
/// user always decides — a suggestion never dispatches, holds, or closes.
pub fn suggest_profile_pick(objective: &str) -> &'static str {
    let lower = objective.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let has = |w: &str| tokens.contains(&w);
    if has("rust")
        || has("cargo")
        || has("rustc")
        || lower.contains(".rs")
        || lower.contains("borrow check")
        || lower.contains("lifetime")
    {
        return "@rust-coder";
    }
    if has("typescript")
        || has("javascript")
        || has("node")
        || has("npm")
        || has("deno")
        || has("tsx")
        || has("tsconfig")
        || lower.contains(".ts")
        || lower.contains("jsx")
    {
        return "@ts-coder";
    }
    if has("zig") || lower.contains(".zig") || lower.contains("comptime") {
        return "@zig-coder";
    }
    "@coder"
}

/// Coordination annotator ask (bd-lpn6): loads the surface pack, asks over
/// {goal.text + extra state}, folds to advisory text + confidence + latency.
/// Deny errors loud. One candidate per call for reviewer-rank; the caller
/// ranks repeated calls in code.
pub fn annotate(
    project: &str,
    surface: AnnotateSurface,
    goal: &str,
    extra: &BTreeMap<String, String>,
    authority: &RuntimeAuthority,
) -> Result<(String, f64, u64), String> {
    let pack = find_pack_for(project, surface.pack_id())?;
    let transport = UreqTransport::from_authority(authority).map_err(|e| e.to_string())?;
    let mut fields = BTreeMap::new();
    fields.insert("goal.text".to_string(), goal.to_string());
    for (k, v) in extra {
        fields.insert(k.clone(), v.clone());
    }
    let slice = build_state_slice(fields);
    let adapter = JudgmentAdapter {
        pack: pack.clone(),
        transport,
    };
    let (outcome, latency_ms) = run_post(adapter, &slice, Surface::Honesty);
    match outcome {
        AdapterOutcome::Answered(a) => {
            let conf = pack_confidence(&a);
            Ok((fold_annotate(surface, &a, &pack), conf, latency_ms))
        }
        AdapterOutcome::Refused(e) | AdapterOutcome::FallbackTable(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd-gkwu: the judgment transport is built from explicit authority —
    /// no env read at the effect site. A keyed authority constructs; a
    /// keyless one fails loud naming the missing credential.
    #[test]
    fn authority_edge_transport_built_from_explicit_authority() {
        let mut keyed = crate::authority::HostAuthority::for_test("/tmp/fw-home");
        keyed.typesafe_api_key = Some("test-key".into());
        let auth = crate::authority::RuntimeAuthority {
            host: keyed,
            clock: std::sync::Arc::new(crate::authority::FixedClock { now: 1 }),
            monotonic: std::sync::Arc::new(crate::authority::FixedMonotonicClock { now: 1 }),
        };
        assert!(
            UreqTransport::from_authority(&auth).is_ok(),
            "explicit key must construct without env"
        );
        let bare = crate::authority::RuntimeAuthority {
            host: crate::authority::HostAuthority::for_test("/tmp/fw-home"),
            clock: std::sync::Arc::new(crate::authority::FixedClock { now: 1 }),
            monotonic: std::sync::Arc::new(crate::authority::FixedMonotonicClock { now: 1 }),
        };
        let err = UreqTransport::from_authority(&bare).expect_err("keyless must refuse");
        assert!(
            format!("{err:?}").contains("unset"),
            "refusal must name the missing credential: {err:?}"
        );
    }

    fn choice_probs(top: f64, second: f64) -> Answer {
        Answer::Choice(ChoiceAnswer {
            choice: "a".into(),
            probs: BTreeMap::from([
                ("a".to_string(), top),
                ("b".to_string(), second),
                ("c".to_string(), 1.0 - top - second),
            ]),
        })
    }

    fn table(primitive: &str, topology: &str) -> TableDecision {
        TableDecision {
            primitive: primitive.into(),
            topology: topology.into(),
            argv: vec!["x".into()],
            reason: "table".into(),
        }
    }

    fn effect_parent() -> String {
        "reclassify:abc123".into()
    }

    // --- confidence ---

    #[test]
    fn choice_confidence_is_top1_minus_top2_margin() {
        assert!((confidence_of(&choice_probs(0.7, 0.2)) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn choice_confidence_peaked_is_high_flat_is_low() {
        assert!(confidence_of(&choice_probs(0.9, 0.05)) > 0.8);
        assert!(confidence_of(&choice_probs(0.4, 0.35)) < 0.1);
    }

    #[test]
    fn noul_confidence_is_margin_from_midpoint() {
        assert!((confidence_of(&Answer::Noul(NoulAnswer { p_yes: 1.0 })) - 1.0).abs() < 1e-9);
        assert!(confidence_of(&Answer::Noul(NoulAnswer { p_yes: 0.5 })) < 1e-9);
        assert!((confidence_of(&Answer::Noul(NoulAnswer { p_yes: 0.1 })) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn score_confidence_is_margin_from_midpoint() {
        let mid = Answer::Score(ScoreAnswer {
            level: 1,
            levels: 3,
        });
        assert!(confidence_of(&mid) < 1e-9);
        let edge = Answer::Score(ScoreAnswer {
            level: 0,
            levels: 3,
        });
        assert!((confidence_of(&edge) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn thresholds_escalate_below_bound_only() {
        let t = Thresholds::default();
        assert!(escalate_required(0.84, ActionClass::TopologyAdjacent, &t));
        assert!(!escalate_required(0.85, ActionClass::TopologyAdjacent, &t));
        assert!(escalate_required(0.79, ActionClass::HonestyGate, &t));
        assert!(!escalate_required(0.80, ActionClass::HonestyGate, &t));
        assert!(escalate_required(0.59, ActionClass::AnnounceOnly, &t));
        assert!(!escalate_required(0.60, ActionClass::AnnounceOnly, &t));
    }

    // --- packs ---

    const PACK_YAML: &str = r#"
id: reclassify
version: 1
model: jev-1.13.0
questions:
  - id: intent
    kind: choice
    instructions: "Which lane does the goal belong to?"
    options: [loop, swarm, open]
  - id: urgency
    kind: score
    instructions: "How urgent?"
    options: [low, high]
  - id: honest
    kind: noul
    instructions: "Is the request honest?"
"#;

    #[test]
    fn pack_parses_and_validates() {
        let p = QuestionPack::parse_yaml(PACK_YAML).unwrap();
        assert_eq!(p.id, "reclassify");
        assert_eq!(p.version, 1);
        assert_eq!(p.model, JEV_MODEL_PIN);
        assert_eq!(p.questions.len(), 3);
    }

    #[test]
    fn shipped_pack_file_parses_and_pins_model() {
        // .flywheel/ is gitignored (machine state): the pack ships in the
        // working tree, not in git. Skip gracefully where absent (CI).
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".flywheel/judgment-packs/reclassify.v1.yaml");
        if !path.is_file() {
            eprintln!("skip: pack file absent (untracked .flywheel)");
            return;
        }
        let p = QuestionPack::load_from_file(&path).unwrap();
        assert_eq!(p.id, "reclassify");
        assert!(p.version >= 1);
        assert_eq!(p.model, JEV_MODEL_PIN);
    }

    #[test]
    fn pack_rejects_jev_latest() {
        let bad = PACK_YAML.replace("jev-1.13.0", "jev-latest");
        assert!(QuestionPack::parse_yaml(&bad).is_err());
    }

    /// bd-lpn6: every LIVE pack (highest version per id) parses, pins the
    /// model, and carries criteria defining each Choice/Score option 1:1
    /// (typesafe-ai shaping). Older versions stay on disk for audit and are
    /// not held to the current shaping bar. .flywheel/ is untracked machine
    /// state — skip gracefully where absent.
    #[test]
    fn shipped_packs_parse_pin_and_define_options() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".flywheel/judgment-packs");
        if !dir.is_dir() {
            eprintln!("skip: packs dir absent (untracked .flywheel)");
            return;
        }
        // <id>.vN.yaml — test the highest N per id (the loader's live pick;
        // older versions stay on disk for audit).
        let mut best: std::collections::BTreeMap<String, (u32, std::path::PathBuf)> =
            std::collections::BTreeMap::new();
        let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".yaml") {
                continue;
            }
            let stem = name.trim_end_matches(".yaml");
            let (id, ver) = match stem.rsplit_once(".v") {
                Some((id, v)) => (id.to_string(), v.parse::<u32>().unwrap_or(0)),
                None => continue,
            };
            best.entry(id)
                .and_modify(|e| {
                    if ver > e.0 {
                        *e = (ver, entry.path());
                    }
                })
                .or_insert((ver, entry.path()));
        }
        let mut seen = 0;
        for (id, (ver, path)) in &best {
            let name = format!("{id}.v{ver}");
            let p = QuestionPack::load_from_file(path).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(p.model, JEV_MODEL_PIN, "{name} pins the model");
            for q in &p.questions {
                if matches!(q.kind, QuestionKind::Choice | QuestionKind::Score) {
                    assert!(
                        !q.criteria.is_empty() && q.criteria.len() == q.options.len(),
                        "{name}/{qid}: criteria must define each option 1:1",
                        qid = q.id
                    );
                }
            }
            seen += 1;
        }
        assert!(seen >= 5, "reclassify + 4 lpn6 packs, got {seen}");
    }

    #[test]
    fn pack_rejects_misaligned_criteria() {
        let bad = r#"
id: x
version: 1
model: jev-1.13.0
questions:
  - id: q
    kind: choice
    instructions: "?"
    options: [a, b]
    criteria: ["only one"]
"#;
        assert!(QuestionPack::parse_yaml(bad).is_err());
    }

    #[test]
    fn pack_rejects_choice_without_options() {
        let bad = r#"
id: x
version: 1
model: jev-1.13.0
questions:
  - id: q
    kind: choice
    instructions: "?"
    options: [only]
"#;
        assert!(QuestionPack::parse_yaml(bad).is_err());
    }

    // --- slices ---

    #[test]
    fn slice_redacts_pii_keys() {
        let fields = BTreeMap::from([
            ("goal".to_string(), "do work".to_string()),
            ("api_key".to_string(), "sekret".to_string()),
            ("TORON_PASSPHRASE".to_string(), "sekret".to_string()),
        ]);
        let s = build_state_slice(fields);
        assert_eq!(s.fields["goal"], "do work");
        assert_eq!(s.fields["api_key"], "[REDACTED]");
        assert_eq!(s.fields["TORON_PASSPHRASE"], "[REDACTED]");
        assert!(!s.truncated);
    }

    #[test]
    fn slice_truncates_over_cap_with_marker() {
        let big = "x".repeat(STATE_SLICE_CAP_BYTES + 100);
        let fields = BTreeMap::from([("goal".to_string(), big)]);
        let s = build_state_slice(fields);
        assert!(s.truncated);
        let len = serde_json::to_string(&s.fields).unwrap().len();
        assert!(len <= STATE_SLICE_CAP_BYTES);
    }

    // --- surfaces ---

    #[test]
    fn honesty_surface_refuses_on_deny() {
        let pack = QuestionPack::parse_yaml(PACK_YAML).unwrap();
        let a = JudgmentAdapter {
            pack,
            transport: OfflineTransport,
        };
        let slice = build_state_slice(BTreeMap::from([("goal".into(), "x".into())]));
        assert!(matches!(
            a.ask_on_surface(&slice, Surface::Honesty),
            AdapterOutcome::Refused(_)
        ));
    }

    #[test]
    fn announce_surface_falls_back_with_flag() {
        let pack = QuestionPack::parse_yaml(PACK_YAML).unwrap();
        let a = JudgmentAdapter {
            pack,
            transport: OfflineTransport,
        };
        let slice = build_state_slice(BTreeMap::from([("goal".into(), "x".into())]));
        assert!(matches!(
            a.ask_on_surface(&slice, Surface::AnnounceOnly),
            AdapterOutcome::FallbackTable(_)
        ));
    }

    // --- merge ---

    #[test]
    fn merge_table_wins_on_primitive_and_topology() {
        let t = table("loop-work", "loop");
        let m = merge(
            &t,
            None,
            0.99,
            ActionClass::HonestyGate,
            &Thresholds::default(),
            Some("loop"),
            None,
            &effect_parent(),
            "reclassify",
            1,
            JEV_MODEL_PIN,
            "{}",
            false,
            0,
            None,
        );
        assert_eq!(m.primitive, "loop-work");
        assert_eq!(m.topology, "loop");
        assert!(!m.escalate);
        assert!(m.advisory.is_none());
        assert_eq!(m.effect.parent_effect_key, effect_parent());
        assert!(!m.effect.fallback_flag);
    }

    #[test]
    fn merge_low_confidence_escalates_but_keeps_table() {
        let t = table("loop-work", "loop");
        let m = merge(
            &t,
            None,
            0.10,
            ActionClass::HonestyGate,
            &Thresholds::default(),
            Some("loop"),
            None,
            &effect_parent(),
            "reclassify",
            1,
            JEV_MODEL_PIN,
            "{}",
            false,
            0,
            None,
        );
        assert!(m.escalate);
        assert_eq!(m.primitive, "loop-work");
        assert!(m.advisory.unwrap().contains("low confidence"));
    }

    #[test]
    fn merge_none_path_advises_and_escalates_never_adopts() {
        let t = table("none", "none");
        let answers = Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "reclassify".into(),
            pack_version: 1,
            by_question: BTreeMap::from([(
                "honest".into(),
                Answer::Noul(NoulAnswer { p_yes: 0.99 }),
            )]),
            usage: "{}".into(),
        };
        let m = merge(
            &t,
            Some(&answers),
            0.99,
            ActionClass::HonestyGate,
            &Thresholds::default(),
            None,
            None,
            &effect_parent(),
            "reclassify",
            1,
            JEV_MODEL_PIN,
            "{}",
            false,
            0,
            None,
        );
        assert_eq!(m.primitive, "none");
        assert!(m.escalate);
        assert!(m.advisory.unwrap().contains("human confirms"));
    }

    #[test]
    fn merge_sticky_violation_escalates_with_loudmiss() {
        let t = table("wave-swarm", "swarm");
        let m = merge(
            &t,
            None,
            0.99,
            ActionClass::TopologyAdjacent,
            &Thresholds::default(),
            Some("open"),
            None,
            &effect_parent(),
            "reclassify",
            1,
            JEV_MODEL_PIN,
            "{}",
            false,
            0,
            None,
        );
        assert!(m.escalate);
        assert!(m.advisory.unwrap().contains("LoudMiss"));
        assert_eq!(m.topology, "swarm");
    }

    #[test]
    fn merge_explicit_topology_permits_change() {
        let t = table("wave-swarm", "swarm");
        let m = merge(
            &t,
            None,
            0.99,
            ActionClass::TopologyAdjacent,
            &Thresholds::default(),
            Some("open"),
            Some("swarm"),
            &effect_parent(),
            "reclassify",
            1,
            JEV_MODEL_PIN,
            "{}",
            false,
            0,
            None,
        );
        assert!(!m.escalate);
    }

    // --- response parse ---

    #[test]
    fn parse_response_reads_typed_answers() {
        let pack = QuestionPack::parse_yaml(PACK_YAML).unwrap();
        let v = serde_json::json!({
            "answers": {
                "intent": {"choice": "loop", "probs": {"loop": 0.8, "swarm": 0.1, "open": 0.1}},
                "urgency": {"level": 1},
                "honest": {"p_yes": 0.95}
            },
            "usage": {"ms": 42}
        });
        let a = parse_systemone_response(&v, &pack).unwrap();
        assert_eq!(a.pack_id, "reclassify");
        assert!(matches!(a.by_question["intent"], Answer::Choice(_)));
        assert!(matches!(a.by_question["urgency"], Answer::Score(_)));
        assert!(matches!(a.by_question["honest"], Answer::Noul(_)));
    }

    #[test]
    fn parse_response_refuses_half_body() {
        let pack = QuestionPack::parse_yaml(PACK_YAML).unwrap();
        let v = serde_json::json!({"answers": {"intent": {"choice": "loop"}}});
        assert!(parse_systemone_response(&v, &pack).is_err());
    }

    // --- flag ---

    #[test]
    fn flag_defaults_off() {
        let _env = crate::config::lock_env();
        // Edition 2024: env mutation is unsafe (single-threaded test, sound).
        unsafe { env::remove_var(FLAG_ENV) };
        unsafe { env::remove_var(API_KEY_ENV) };
        assert_eq!(jev_gate(), JevGate::Unconfigured);
    }

    /// bd-0glq: P7 default-on — unset flag enriches iff the key is present;
    /// keyless default skips loud (marker, never escalate); explicit values
    /// keep the old contract (truthy = configured lane, else off).
    #[test]
    fn gate_default_on_with_key_unconfigured_without() {
        let _env = crate::config::lock_env();
        unsafe { env::remove_var(FLAG_ENV) };
        unsafe { env::set_var(API_KEY_ENV, "k") };
        assert_eq!(jev_gate(), JevGate::On);
        unsafe { env::remove_var(API_KEY_ENV) };
        assert_eq!(jev_gate(), JevGate::Unconfigured);
        unsafe { env::set_var(FLAG_ENV, "1") };
        assert_eq!(jev_gate(), JevGate::On);
        unsafe { env::set_var(FLAG_ENV, "0") };
        assert_eq!(jev_gate(), JevGate::Off);
        unsafe { env::remove_var(FLAG_ENV) };
    }

    // --- review blockers (PearlTern fresh-eyes) ---

    fn empty_answers() -> Answers {
        Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "reclassify".into(),
            pack_version: 1,
            by_question: BTreeMap::new(),
            usage: "{}".into(),
        }
    }

    #[test]
    fn b1_empty_pack_confidence_is_zero_not_one() {
        assert!((pack_confidence(&empty_answers()) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn b2_nan_confidence_escalates() {
        let nan = Answer::Noul(NoulAnswer { p_yes: f64::NAN });
        assert!((confidence_of(&nan) - 0.0).abs() < 1e-9);
        assert!(escalate_required(
            f64::NAN,
            ActionClass::HonestyGate,
            &Thresholds::default()
        ));
        let nan_probs = Answer::Choice(ChoiceAnswer {
            choice: "a".into(),
            probs: BTreeMap::from([("a".to_string(), f64::NAN), ("b".to_string(), 0.5)]),
        });
        assert!((confidence_of(&nan_probs) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn b3_response_validation_refuses_bad_shapes() {
        let pack = QuestionPack::parse_yaml(PACK_YAML).unwrap();
        // choice outside pack options
        let v = serde_json::json!({
            "answers": {
                "intent": {"choice": "teleport", "probs": {"teleport": 1.0}},
                "urgency": {"level": 0},
                "honest": {"p_yes": 0.5}
            }
        });
        assert!(parse_systemone_response(&v, &pack).is_err());
        // probs sum != 1
        let v = serde_json::json!({
            "answers": {
                "intent": {"choice": "loop", "probs": {"loop": 0.5, "swarm": 0.1, "open": 0.1}},
                "urgency": {"level": 0},
                "honest": {"p_yes": 0.5}
            }
        });
        assert!(parse_systemone_response(&v, &pack).is_err());
        // level out of range
        let v = serde_json::json!({
            "answers": {
                "intent": {"choice": "loop", "probs": {"loop": 0.8, "swarm": 0.1, "open": 0.1}},
                "urgency": {"level": 9},
                "honest": {"p_yes": 0.5}
            }
        });
        assert!(parse_systemone_response(&v, &pack).is_err());
    }

    #[test]
    fn b3_score_pack_without_levels_rejected() {
        let bad = r#"
id: x
version: 1
model: jev-1.13.0
questions:
  - id: q
    kind: score
    instructions: "?"
"#;
        assert!(QuestionPack::parse_yaml(bad).is_err());
    }

    #[test]
    fn sec_value_side_secrets_redacted() {
        assert_eq!(
            redact_value("deploy with password=hunter2 now"),
            "deploy with password=[REDACTED] now"
        );
        assert_eq!(
            redact_value("key: nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq"),
            "key: [REDACTED]"
        );
        assert_eq!(redact_value("nothing secret here"), "nothing secret here");
        let fields = BTreeMap::from([("goal".to_string(), "use token=abc now".to_string())]);
        let s = build_state_slice(fields);
        assert_eq!(s.fields["goal"], "use token=[REDACTED] now");
    }

    #[test]
    fn sec_audit_strings_capped() {
        let big = "u".repeat(4000);
        let t = table("loop-work", "loop");
        let m = merge(
            &t,
            None,
            0.99,
            ActionClass::HonestyGate,
            &Thresholds::default(),
            Some("loop"),
            None,
            "parent",
            "reclassify",
            1,
            JEV_MODEL_PIN,
            &big,
            false,
            0,
            None,
        );
        assert!(m.effect.usage.len() <= AUDIT_USAGE_CAP + 16);
        assert!(m.effect.usage.contains("[truncated]"));
    }

    #[test]
    fn b5_thresholds_knob_parses_or_defaults() {
        unsafe { env::set_var(THRESHOLDS_ENV, "0.9,0.85,0.7") };
        let t = active_thresholds();
        assert!((t.topology_adjacent - 0.9).abs() < 1e-9);
        assert!((t.honesty_gate - 0.85).abs() < 1e-9);
        assert!((t.announce_only - 0.7).abs() < 1e-9);
        unsafe { env::set_var(THRESHOLDS_ENV, "junk") };
        assert!((active_thresholds().honesty_gate - 0.80).abs() < 1e-9);
        unsafe { env::remove_var(THRESHOLDS_ENV) };
    }

    #[test]
    fn b5_find_pack_picks_highest_version() {
        let base = std::env::temp_dir().join(format!("jev-pack-{}", std::process::id()));
        let dir = base.join(".flywheel/judgment-packs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("reclassify.v1.yaml"), PACK_YAML).unwrap();
        std::fs::write(
            dir.join("reclassify.v2.yaml"),
            PACK_YAML.replace("version: 1", "version: 2"),
        )
        .unwrap();
        let found = find_pack(base.to_str().unwrap()).unwrap();
        assert_eq!(found.version, 2);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn p3_pack_cache_hits_then_invalidates_on_mtime() {
        let base = std::env::temp_dir().join(format!("jev-packcache-{}", std::process::id()));
        let dir = base.join(".flywheel/judgment-packs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("reclassify.v1.yaml"), PACK_YAML).unwrap();
        let mut cache = PackCache::default();
        let first = cache.get_or_load(&dir, "reclassify").unwrap().unwrap();
        let second = cache.get_or_load(&dir, "reclassify").unwrap().unwrap();
        assert_eq!(first.version, second.version);
        assert_eq!(cache.entries.len(), 1, "one cached entry");
        let _ = std::fs::remove_dir_all(&base);
    }

    // Superseded by prepare_flag_off_is_passthrough_without_audit (bd-n86n
    // three-phase path); removed with maybe_enrich.

    // --- bd-lpn6: miss ledger ---

    fn miss_row(key: &str, goal: &str, prim: &str) -> MissRow {
        MissRow {
            ts: "2026-09-19T00:00:00Z".into(),
            effect_key: key.into(),
            goal: goal.into(),
            table_primitive: prim.into(),
            table_topology: "none".into(),
            escalated: true,
            screen: None,
            resolution: None,
            resolved_ts: None,
            note: None,
        }
    }

    #[test]
    fn miss_append_read_roundtrip() {
        let _home = crate::config::test_state_home();
        miss_append(&miss_row("k1", "Ship the onboarding", "none")).unwrap();
        let rows = miss_read();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].effect_key, "k1");
        assert_eq!(rows[0].resolution, None);
    }

    #[test]
    fn miss_resolve_records_and_clears_unresolved() {
        let _home = crate::config::test_state_home();
        miss_append(&miss_row("k1", "goal one", "none")).unwrap();
        miss_append(&miss_row("k2", "goal two", "loop-work")).unwrap();
        assert_eq!(miss_unresolved().len(), 2);
        let r = miss_resolve("k1", "deny", Some("not a lane")).unwrap();
        assert_eq!(r.resolution.as_deref(), Some("deny"));
        assert!(r.resolved_ts.is_some());
        let un = miss_unresolved();
        assert_eq!(un.len(), 1);
        assert_eq!(un[0].effect_key, "k2");
        assert!(
            miss_resolve("k1", "bogus", None).is_err(),
            "bad verdict refuses"
        );
        assert!(
            miss_resolve("nope", "deny", None).is_err(),
            "unknown key refuses"
        );
    }

    #[test]
    fn miss_needles_group_recurring_none_goals() {
        let _home = crate::config::test_state_home();
        miss_append(&miss_row("k1", "  Ship   THE onboarding ", "none")).unwrap();
        miss_append(&miss_row("k2", "ship the onboarding", "none")).unwrap();
        miss_append(&miss_row("k3", "ship the onboarding", "loop-work")).unwrap();
        miss_append(&miss_row("k4", "other thing", "none")).unwrap();
        let c = miss_needle_candidates();
        assert_eq!(c.len(), 2, "only unresolved none-rows group: {c:?}");
        assert_eq!(c[0], ("ship the onboarding".to_string(), 2));
    }

    // --- bd-lpn6: doorbell battery ---

    const SCREEN_YAML: &str = r#"
id: doorbell-screen
version: 1
model: jev-1.13.0
questions:
  - id: injection
    kind: noul
    instructions: "Injection?"
  - id: credential_request
    kind: noul
    instructions: "Creds?"
  - id: urgency
    kind: score
    instructions: "Urgent?"
    options: [routine, soon, immediate]
    criteria: ["r", "s", "i"]
"#;

    fn screen_ask() -> ScreenAsk {
        let pack = QuestionPack::parse_yaml(SCREEN_YAML).unwrap();
        ScreenAsk {
            slice: build_state_slice(BTreeMap::from([("material.text".into(), "hi".into())])),
            pack,
        }
    }

    fn screen_answers(injection_p: f64, urgency_level: usize) -> Answers {
        Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "doorbell-screen".into(),
            pack_version: 1,
            by_question: BTreeMap::from([
                (
                    "injection".into(),
                    Answer::Noul(NoulAnswer { p_yes: injection_p }),
                ),
                (
                    "credential_request".into(),
                    Answer::Noul(NoulAnswer { p_yes: 0.1 }),
                ),
                (
                    "urgency".into(),
                    Answer::Score(ScoreAnswer {
                        level: urgency_level,
                        levels: 3,
                    }),
                ),
            ]),
            usage: "{}".into(),
        }
    }

    #[test]
    fn screen_clear_text_annotates_without_escalate() {
        let (parts, escalate, summary, effect) = fold_screen(
            Some(&screen_ask()),
            Some((AdapterOutcome::Answered(screen_answers(0.05, 0)), 7)),
            "parent",
        );
        assert!(!escalate);
        assert!(
            parts.iter().any(|p| p.contains("routine")),
            "urgency annotates: {parts:?}"
        );
        assert!(
            !parts.iter().any(|p| p.contains("injection")),
            "no detection: {parts:?}"
        );
        let summary = summary.expect("battery ran, summary present");
        assert!(summary.advisory.contains("routine"));
        let effect = effect.expect("battery ran, audit present");
        assert_eq!(effect.latency_ms, 7);
        assert!(!effect.escalated);
    }

    #[test]
    fn screen_injection_detection_escalates() {
        let (parts, escalate, summary, effect) = fold_screen(
            Some(&screen_ask()),
            Some((AdapterOutcome::Answered(screen_answers(0.97, 2)), 9)),
            "parent",
        );
        let _ = (summary, effect);
        assert!(escalate, "confident injection routes to human");
        assert!(parts.iter().any(|p| p.contains("injection")), "{parts:?}");
    }

    #[test]
    fn screen_deny_and_skip_are_loud_or_quiet() {
        let (parts, escalate, _, effect) = fold_screen(
            Some(&screen_ask()),
            Some((AdapterOutcome::Refused(JevError::Offline("down".into())), 3)),
            "parent",
        );
        assert!(escalate, "blind spot on honesty path escalates");
        assert!(parts.iter().any(|p| p.contains("denied")), "{parts:?}");
        assert!(effect.expect("deny audits").escalated);
        let (parts, escalate, summary, effect) = fold_screen(None, None, "parent");
        assert!(parts.is_empty() && !escalate);
        assert!(
            summary.is_none() && effect.is_none(),
            "skipped battery emits nothing"
        );
    }

    // --- bd-lpn6: prescreen + annotators ---

    fn prescreen_answers(shape: &str, destructive_p: f64) -> Answers {
        Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "verify-prescreen".into(),
            pack_version: 1,
            by_question: BTreeMap::from([
                (
                    "safe_shape".into(),
                    Answer::Choice(ChoiceAnswer {
                        choice: shape.into(),
                        probs: BTreeMap::from([
                            ("single-command".into(), 0.9),
                            (shape.into(), 0.05),
                        ]),
                    }),
                ),
                (
                    "destructive_risk".into(),
                    Answer::Noul(NoulAnswer {
                        p_yes: destructive_p,
                    }),
                ),
            ]),
            usage: "{}".into(),
        }
    }

    #[test]
    fn prescreen_fold_states_shape_plainly() {
        let (advisory, destructive) = fold_prescreen(&prescreen_answers("single-command", 0.02));
        assert!(advisory.contains("shape=single-command"), "{advisory}");
        assert!(!destructive);
        let (advisory, destructive) = fold_prescreen(&prescreen_answers("destructive", 0.99));
        assert!(destructive, "confident destructive flags: {advisory}");
    }

    #[test]
    fn prescreen_and_annotate_deny_without_key() {
        let _env = crate::config::lock_env();
        unsafe { env::remove_var(API_KEY_ENV) };
        // Packs resolve via ancestor walk from the repo; the failure under
        // test is the missing key, not discovery.
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .to_string_lossy()
            .into_owned();
        let authority = RuntimeAuthority::for_test("/tmp/jev-home", 0);
        assert!(
            prescreen_verify(&repo, "cargo test", &authority).is_err(),
            "no key = loud deny"
        );
        assert!(
            annotate(
                &repo,
                AnnotateSurface::CoalitionIntent,
                "goal",
                &BTreeMap::new(),
                &authority,
            )
            .is_err(),
            "no key = loud deny"
        );
    }

    #[test]
    fn annotate_folds_state_intent_plainly() {
        let pack = QuestionPack::parse_yaml(
            "id: coalition-intent\nversion: 1\nmodel: jev-1.13.0\nquestions:\n  - id: intent\n    kind: choice\n    instructions: \"?\"\n    options: [joint-ownership, review-handoff, no-coalition, unclear]\n    criteria: [\"a\", \"b\", \"c\", \"d\"]\n",
        )
        .unwrap();
        let answers = Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "coalition-intent".into(),
            pack_version: 1,
            by_question: BTreeMap::from([(
                "intent".into(),
                Answer::Choice(ChoiceAnswer {
                    choice: "review-handoff".into(),
                    probs: BTreeMap::from([
                        ("review-handoff".into(), 0.8),
                        ("unclear".into(), 0.1),
                    ]),
                }),
            )]),
            usage: "{}".into(),
        };
        let line = fold_annotate(AnnotateSurface::CoalitionIntent, &answers, &pack);
        assert!(line.contains("intent=review-handoff"), "{line}");
        assert!(AnnotateSurface::parse("wave-readiness") == Some(AnnotateSurface::WaveReadiness));
        assert!(AnnotateSurface::parse("bogus").is_none());
    }

    /// bd-adv4: the profile-pick fold maps the Score level to its option
    /// name, mirroring the reviewer-rank arm.
    #[test]
    fn profile_pick_fold_reports_fit_level() {
        let pack = QuestionPack::parse_yaml(
            r#"
id: profile-pick
version: 1
model: jev-1.13.0
questions:
  - id: profile_fit
    kind: score
    instructions: "fit?"
    options: [poor, adequate, strong]
    criteria: ["p", "a", "s"]
"#,
        )
        .expect("profile-pick test pack parses");
        let answers = Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "profile-pick".into(),
            pack_version: 1,
            by_question: BTreeMap::from([(
                "profile_fit".into(),
                Answer::Score(ScoreAnswer {
                    level: 2,
                    levels: 3,
                }),
            )]),
            usage: "{}".into(),
        };
        let line = fold_annotate(AnnotateSurface::ProfilePick, &answers, &pack);
        assert!(line.contains("fit=strong"), "{line}");
        assert!(AnnotateSurface::parse("profile-pick") == Some(AnnotateSurface::ProfilePick));
    }

    /// bd-88m4: the tier-pick fold combines both questions; parse knows
    /// the surface.
    #[test]
    fn tier_pick_fold_combines_depth_and_coupling() {
        let pack = QuestionPack::parse_yaml(
            r#"
id: tier-pick
version: 1
model: jev-1.13.0
questions:
  - id: reasoning_depth
    kind: score
    instructions: "depth?"
    options: [routine, substantial, frontier]
    criteria: ["r", "s", "f"]
  - id: harness_coupling
    kind: choice
    instructions: "coupling?"
    options: [open, coupled]
    criteria: ["o", "c"]
"#,
        )
        .expect("tier-pick test pack parses");
        let answers = Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "tier-pick".into(),
            pack_version: 1,
            by_question: BTreeMap::from([
                (
                    "reasoning_depth".into(),
                    Answer::Score(ScoreAnswer {
                        level: 2,
                        levels: 3,
                    }),
                ),
                (
                    "harness_coupling".into(),
                    Answer::Choice(ChoiceAnswer {
                        choice: "coupled".into(),
                        probs: BTreeMap::from([("coupled".into(), 0.8), ("open".into(), 0.2)]),
                    }),
                ),
            ]),
            usage: "{}".into(),
        };
        let line = fold_annotate(AnnotateSurface::TierPick, &answers, &pack);
        assert!(line.contains("depth=frontier"), "{line}");
        assert!(line.contains("coupling=coupled"), "{line}");
        assert!(AnnotateSurface::parse("tier-pick") == Some(AnnotateSurface::TierPick));
    }

    /// bd-88m4: the offline tier-pick table fires only on strong cues —
    /// anything else is None, which is current behavior.
    #[test]
    fn tier_pick_suggest_offline_falls_back_to_current() {
        assert_eq!(
            suggest_tier_pick("fix the typo in the docs"),
            Some("@cheap")
        );
        assert_eq!(
            suggest_tier_pick("plan the security migration with a perf budget"),
            Some("@premium")
        );
        assert_eq!(suggest_tier_pick("fix the flake"), None);
        assert_eq!(suggest_tier_pick("sharpen the objective into beads"), None);
    }

    /// bd-vlvn: the reject-all gate clears the announce-only bar and
    /// rejects below it (tracks the captain-tunable bar, default 0.6).
    #[test]
    fn jev_live_suggestion_gate_clears_at_bar() {
        let bar = active_thresholds().announce_only;
        assert!(suggestion_gate(bar));
        assert!(suggestion_gate(1.0));
        assert!(!suggestion_gate(bar - 0.01));
        assert!(!suggestion_gate(0.0));
    }

    /// bd-vlvn: batching clones the Score question per candidate into one
    /// pack; non-Score or unknown questions refuse.
    #[test]
    fn jev_live_batch_pack_clones_score_per_candidate() {
        let pack = QuestionPack::parse_yaml(
            r#"
id: profile-pick
version: 1
model: jev-1.13.0
questions:
  - id: profile_fit
    kind: score
    instructions: "How fit is `candidate`?"
    options: [poor, adequate, strong]
    criteria: ["p", "a", "s"]
"#,
        )
        .expect("batch test pack parses");
        let cands = vec!["coder".to_string(), "rust-coder".to_string()];
        let batch = batch_pack_for_candidates(&pack, "profile_fit", &cands).expect("batch");
        assert_eq!(batch.questions.len(), 2);
        assert_eq!(batch.questions[0].id, "profile_fit__coder");
        assert!(batch.questions[1].instructions.contains("`rust-coder`"));
        assert!(batch_pack_for_candidates(&pack, "missing", &cands).is_err());
    }

    /// bd-vlvn: split drops missing candidates instead of inventing
    /// scores; rank takes the highest level, confidence breaking ties.
    #[test]
    fn jev_live_split_drops_missing_and_ranks_best() {
        let pack = QuestionPack::parse_yaml(
            r#"
id: profile-pick
version: 1
model: jev-1.13.0
questions:
  - id: profile_fit__coder
    kind: score
    instructions: "?"
    options: [poor, adequate, strong]
    criteria: ["p", "a", "s"]
  - id: profile_fit__rust-coder
    kind: score
    instructions: "?"
    options: [poor, adequate, strong]
    criteria: ["p", "a", "s"]
"#,
        )
        .expect("split test pack parses");
        let cands = vec![
            "coder".to_string(),
            "rust-coder".to_string(),
            "ghost".to_string(),
        ];
        let answers = Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "profile-pick".into(),
            pack_version: 1,
            by_question: BTreeMap::from([
                (
                    "profile_fit__coder".into(),
                    Answer::Score(ScoreAnswer {
                        level: 1,
                        levels: 3,
                    }),
                ),
                (
                    "profile_fit__rust-coder".into(),
                    Answer::Score(ScoreAnswer {
                        level: 2,
                        levels: 3,
                    }),
                ),
            ]),
            usage: "{}".into(),
        };
        let scores = split_batched_scores(&answers, &pack, "profile_fit", &cands);
        assert_eq!(scores.len(), 2, "ghost drops, never invented");
        let best = rank_candidate_scores(&scores).expect("best");
        assert_eq!(best.candidate, "rust-coder");
        assert_eq!(best.level_name, "strong");
        assert!(suggestion_gate(best.confidence));
        assert!(rank_candidate_scores(&[]).is_none());
    }

    /// Canned batch transport for the live-suggestion tests: one POST,
    /// per-candidate Score answers from the map.
    struct CannedBatch {
        levels: BTreeMap<String, usize>,
    }

    impl Transport for CannedBatch {
        fn ask(&self, _slice: &StateSlice, pack: &QuestionPack) -> Result<Answers, JevError> {
            let mut by_question = BTreeMap::new();
            for q in &pack.questions {
                let level = self.levels.get(&q.id).copied().unwrap_or(1);
                if !matches!(q.kind, QuestionKind::Score) {
                    // harness_coupling Choice: sharp open (conf 0.9).
                    by_question.insert(
                        q.id.clone(),
                        Answer::Choice(ChoiceAnswer {
                            choice: "open".into(),
                            probs: BTreeMap::from([
                                ("open".into(), 0.95),
                                ("coupled".into(), 0.05),
                            ]),
                        }),
                    );
                    continue;
                }
                by_question.insert(
                    q.id.clone(),
                    Answer::Score(ScoreAnswer {
                        level,
                        levels: q.options.len(),
                    }),
                );
            }
            Ok(Answers {
                model_id: JEV_MODEL_PIN.into(),
                pack_id: pack.id.clone(),
                pack_version: pack.version,
                by_question,
                usage: "{}".into(),
            })
        }
    }

    fn profile_pick_pack_for_live() -> QuestionPack {
        QuestionPack::parse_yaml(
            r#"
id: profile-pick
version: 1
model: jev-1.13.0
questions:
  - id: profile_fit
    kind: score
    instructions: "How fit is `candidate`?"
    options: [poor, adequate, strong]
    criteria: ["p", "a", "s"]
"#,
        )
        .expect("live test pack parses")
    }

    fn tier_pick_pack_for_live() -> QuestionPack {
        QuestionPack::parse_yaml(
            r#"
id: tier-pick
version: 1
model: jev-1.13.0
questions:
  - id: reasoning_depth
    kind: score
    instructions: "depth?"
    options: [routine, substantial, frontier]
    criteria: ["r", "s", "f"]
  - id: harness_coupling
    kind: choice
    instructions: "coupling?"
    options: [open, coupled]
    criteria: ["o", "c"]
"#,
        )
        .expect("live tier test pack parses")
    }

    /// bd-vlvn: live profile suggestion on sample objectives — a strong
    /// candidate clears the gate; all-adequate rejects everything (the
    /// reject-all gate); deny is an Err so the caller keeps the table.
    #[test]
    fn jev_live_profile_suggestion_gates_strong_only() {
        let pack = profile_pick_pack_for_live();
        let cands = vec![
            ("coder".to_string(), "write/fix code".to_string()),
            ("rust-coder".to_string(), "Rust code".to_string()),
        ];
        // Sample 1: strong rust-coder for a borrow-checker objective.
        let strong = CannedBatch {
            levels: BTreeMap::from([
                ("profile_fit__coder".into(), 1),
                ("profile_fit__rust-coder".into(), 2),
            ]),
        };
        assert_eq!(
            live_profile_suggestion(
                &strong,
                &pack,
                "fix the borrow checker error in cargo build",
                &cands
            )
            .expect("answered"),
            Some("@rust-coder".to_string())
        );
        // Sample 2: every candidate merely adequate — reject ALL.
        let weak = CannedBatch {
            levels: BTreeMap::from([
                ("profile_fit__coder".into(), 1),
                ("profile_fit__rust-coder".into(), 1),
            ]),
        };
        assert_eq!(
            live_profile_suggestion(&weak, &pack, "sharpen the objective", &cands)
                .expect("answered"),
            None,
            "adequate everywhere carries no confidence — nothing, not even the table"
        );
        // Deny keeps the table: Err, never a fabricated suggestion.
        assert!(
            live_profile_suggestion(&OfflineTransport, &pack, "fix the flake", &cands).is_err()
        );
        assert!(
            live_profile_suggestion(&strong, &pack, "fix the flake", &[])
                .expect("empty")
                .is_none()
        );
    }

    /// bd-vlvn: live tier suggestion maps edge depths to bands and the
    /// unsure middle to nothing, all under the gate.
    #[test]
    fn jev_live_tier_suggestion_maps_edges_only() {
        let pack = tier_pick_pack_for_live();
        // frontier → @premium (depth conf 1.0, coupling conf 0.9).
        let frontier = CannedBatch {
            levels: BTreeMap::from([("reasoning_depth".into(), 2)]),
        };
        assert_eq!(
            live_tier_suggestion(&frontier, &pack, "plan the security migration")
                .expect("answered"),
            Some("@premium".to_string())
        );
        // routine → @cheap.
        let routine = CannedBatch {
            levels: BTreeMap::from([("reasoning_depth".into(), 0)]),
        };
        assert_eq!(
            live_tier_suggestion(&routine, &pack, "fix the typo").expect("answered"),
            Some("@cheap".to_string())
        );
        // substantial (level 1, conf 0) → gate rejects → None.
        let middle = CannedBatch {
            levels: BTreeMap::from([("reasoning_depth".into(), 1)]),
        };
        assert_eq!(
            live_tier_suggestion(&middle, &pack, "add the feature").expect("answered"),
            None
        );
        assert!(live_tier_suggestion(&OfflineTransport, &pack, "fix the flake").is_err());
    }

    /// governed-by: bd-ulzi — tier-pick reasoning_depth maps to effort:
    /// routine/low, substantial/high, frontier/max. Unknown stays empty
    /// so the profile default stands (never a fabricated effort).
    #[test]
    fn jev_effort_depth_maps_to_effort() {
        assert_eq!(effort_for_depth("routine"), "low");
        assert_eq!(effort_for_depth("substantial"), "high");
        assert_eq!(effort_for_depth("frontier"), "max");
        assert_eq!(effort_for_depth("unknown"), "");
        assert_eq!(effort_for_depth(""), "");
    }

    /// governed-by: bd-ulzi — the live effort suggestion returns an
    /// effort level (not a model slot) under the same confidence gate;
    /// deny is an Err so the caller keeps the profile default.
    #[test]
    fn jev_effort_live_suggestion_returns_effort() {
        let pack = tier_pick_pack_for_live();
        let frontier = CannedBatch {
            levels: BTreeMap::from([("reasoning_depth".into(), 2)]),
        };
        assert_eq!(
            live_effort_suggestion(&frontier, &pack, "plan the security migration")
                .expect("answered"),
            Some("max".to_string())
        );
        let routine = CannedBatch {
            levels: BTreeMap::from([("reasoning_depth".into(), 0)]),
        };
        assert_eq!(
            live_effort_suggestion(&routine, &pack, "fix the typo").expect("answered"),
            Some("low".to_string())
        );
        let middle = CannedBatch {
            levels: BTreeMap::from([("reasoning_depth".into(), 1)]),
        };
        assert_eq!(
            live_effort_suggestion(&middle, &pack, "add the feature").expect("answered"),
            None,
            "weak middle carries no confidence — profile default stands"
        );
        assert!(live_effort_suggestion(&OfflineTransport, &pack, "fix the flake").is_err());
    }

    /// bd-adv4: the offline profile-pick table routes by language cue and
    /// defaults to the generic coder.
    #[test]
    fn profile_pick_suggest_falls_back_by_language() {
        assert_eq!(
            suggest_profile_pick("fix the borrow checker error in cargo build"),
            "@rust-coder"
        );
        assert_eq!(
            suggest_profile_pick("migrate the tsconfig build to npm workspaces"),
            "@ts-coder"
        );
        assert_eq!(
            suggest_profile_pick("rewrite the allocator with comptime"),
            "@zig-coder"
        );
        assert_eq!(
            suggest_profile_pick("sharpen the objective into beads"),
            "@coder"
        );
        // Substring caution: bare "nodes"/"assets" must not cue ts-coder.
        assert_eq!(
            suggest_profile_pick("drain the nodes and rotate assets"),
            "@coder"
        );
    }

    // --- bd-n86n: POST outside the journal tx ---

    fn expand_args(project: &str) -> ExpandArgs {
        ExpandArgs {
            project: project.into(),
            goal: "g".into(),
            pin: "p".into(),
            reason: "r".into(),
            explicit_topology: None,
            parent_effect_key: effect_parent(),
            sticky_topology: Some("loop".into()),
            table_primitive: "loop-work".into(),
            table_topology: "loop".into(),
            table_argv: vec!["x".into()],
            table_reason: "table".into(),
        }
    }

    struct StubTransport {
        answers: Answers,
    }

    impl Transport for StubTransport {
        fn ask(&self, _slice: &StateSlice, _pack: &QuestionPack) -> Result<Answers, JevError> {
            Ok(self.answers.clone())
        }
    }

    fn stub_answers() -> Answers {
        Answers {
            model_id: JEV_MODEL_PIN.into(),
            pack_id: "reclassify".into(),
            pack_version: 1,
            by_question: BTreeMap::from([(
                "honest".into(),
                Answer::Noul(NoulAnswer { p_yes: 0.99 }),
            )]),
            usage: "{}".into(),
        }
    }

    #[test]
    fn prepare_flag_off_is_passthrough_without_audit() {
        let plan = prepare_post_with_authority_and_gate(
            expand_args("/nonexistent-probe"),
            &RuntimeAuthority::for_test("/tmp/jev-home", 0),
            JevGate::Off,
        );
        assert!(matches!(plan.stage, PostStage::Off));
        let out = finish_plan(plan, None, None);
        assert_eq!(out.primitive, "loop-work");
        assert_eq!(out.topology, "loop");
        assert!(out.audit.is_none(), "flag off = byte-identical, no audit");
        assert!(
            !out.reason.contains("jev:"),
            "explicit off carries no marker"
        );
    }

    /// bd-0glq: keyless default is a LOUD skip — table passthrough with the
    /// unconfigured marker, no audit, no escalate (never escalate spam).
    #[test]
    fn prepare_unconfigured_marks_passthrough_without_escalate() {
        let plan = prepare_post_with_authority_and_gate(
            expand_args("/nonexistent-probe"),
            &RuntimeAuthority::for_test("/tmp/jev-home", 0),
            JevGate::Unconfigured,
        );
        assert!(matches!(plan.stage, PostStage::Off));
        let out = finish_plan(plan, None, None);
        assert_eq!(out.primitive, "loop-work", "table wins");
        assert!(out.audit.is_none(), "no POST = no audit");
        assert!(!out.escalated, "unconfigured never escalates");
        assert!(
            out.reason.contains("jev: unconfigured"),
            "loud skip marker: {}",
            out.reason
        );
    }

    #[test]
    fn prepare_missing_pack_is_deny_with_escalate() {
        let base = std::env::temp_dir().join(format!("jev-n86n-nopack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let plan = prepare_post_with_authority_and_gate(
            expand_args(&base.to_string_lossy()),
            &RuntimeAuthority::for_test("/tmp/jev-home", 0),
            JevGate::On,
        );
        assert!(
            matches!(plan.stage, PostStage::Deny { .. }),
            "missing pack = deny, never POST"
        );
        let out = finish_plan(plan, None, None);
        assert_eq!(out.primitive, "loop-work", "table wins even on deny");
        let (_, detail) = out.audit.expect("deny always audits");
        assert!(detail["escalated"].as_bool().unwrap_or(false));
        assert_eq!(
            detail["latency_ms"].as_u64(),
            Some(0),
            "no POST = zero latency"
        );
        unsafe { env::remove_var(FLAG_ENV) };
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn run_post_stub_answered_and_finish_records_latency() {
        let pack = QuestionPack::parse_yaml(PACK_YAML).unwrap();
        let slice = build_state_slice(BTreeMap::from([("goal".into(), "g".into())]));
        let stub = StubTransport {
            answers: stub_answers(),
        };
        let adapter = JudgmentAdapter {
            pack: pack.clone(),
            transport: stub,
        };
        let (outcome, _ms) = run_post(adapter, &slice, Surface::Honesty);
        assert!(matches!(outcome, AdapterOutcome::Answered(_)));
        let plan = PostPlan {
            table: table("loop-work", "loop"),
            pin: "p".into(),
            explicit_topology: None,
            parent_effect_key: effect_parent(),
            sticky_topology: Some("loop".into()),
            stage: PostStage::Ready {
                slice,
                pack,
                transport: UreqTransport {
                    api_key: "k".into(),
                    url: "u".into(),
                },
                screen: None,
            },
        };
        let out = finish_plan(
            plan,
            Some((AdapterOutcome::Answered(stub_answers()), 123)),
            None,
        );
        let (_, detail) = out.audit.expect("ready always audits");
        assert_eq!(
            detail["latency_ms"].as_u64(),
            Some(123),
            "measured POST latency lands in the audit"
        );
        assert!(
            !detail["escalated"].as_bool().unwrap_or(true),
            "high-conf stub must not escalate"
        );
    }

    #[test]
    fn finish_ready_without_post_is_loud_deny() {
        let pack = QuestionPack::parse_yaml(PACK_YAML).unwrap();
        let slice = build_state_slice(BTreeMap::from([("goal".into(), "g".into())]));
        let plan = PostPlan {
            table: table("loop-work", "loop"),
            pin: "p".into(),
            explicit_topology: None,
            parent_effect_key: effect_parent(),
            sticky_topology: Some("loop".into()),
            stage: PostStage::Ready {
                slice,
                pack,
                transport: UreqTransport {
                    api_key: "k".into(),
                    url: "u".into(),
                },
                screen: None,
            },
        };
        // Programming error (POST skipped): fail-closed deny, never quiet merge.
        let out = finish_plan(plan, None, None);
        assert!(out.audit.is_some());
        let reason = out.reason;
        assert!(reason.contains("jev"), "deny attaches advisory: {reason}");
    }
}
