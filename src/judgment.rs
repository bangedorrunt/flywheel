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

/// One typed question inside a pack.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct QuestionDef {
    pub id: String,
    pub kind: QuestionKind,
    pub instructions: String,
    #[serde(default)]
    pub options: Vec<String>,
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
    /// No key, timeout, 429, outage, malformed body — all deny shapes.
    Offline(String),
    Unauthorized,
    Malformed(String),
}

impl std::fmt::Display for JevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JevError::Offline(w) => write!(f, "offline/denied: {w}"),
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
    pub fn from_env() -> Result<Self, JevError> {
        let api_key =
            env::var(API_KEY_ENV).map_err(|_| JevError::Offline(format!("{API_KEY_ENV} unset")))?;
        if api_key.trim().is_empty() {
            return Err(JevError::Offline(format!("{API_KEY_ENV} empty")));
        }
        Ok(Self {
            api_key,
            url: SYSTEMONE_URL.to_string(),
        })
    }
}

impl Transport for UreqTransport {
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

/// True only for explicit opt-in values. Everything else (including unset)
/// is off: the expand path behaves exactly as before.
pub fn jev_enabled() -> bool {
    match env::var(FLAG_ENV) {
        Ok(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

// W2 host entry point.
// ---------------------------------------------------------------------------

const PACK_DIR_REL: &str = ".flywheel/judgment-packs";
const PACK_PREFIX: &str = "reclassify.v";
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
/// reclassify). Picks the highest `reclassify.vN.yaml` version in the first
/// directory that holds any. Missing everywhere with the flag on is a loud
/// deny (escalate + audit), never a silent skip.
pub fn find_pack(project: &str) -> Result<QuestionPack, String> {
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
    let mut tried: Vec<std::path::PathBuf> = Vec::new();
    for base in &bases {
        let dir = base.join(PACK_DIR_REL);
        tried.push(dir.clone());
        if let Some(pack) = newest_pack_in(&dir) {
            return pack;
        }
    }
    Err(format!(
        "reclassify pack not found (tried {tried:?}); add {PACK_DIR_REL}/reclassify.vN.yaml or unset {FLAG_ENV}"
    ))
}

fn newest_pack_in(dir: &std::path::Path) -> Option<Result<QuestionPack, String>> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u32, std::path::PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(PACK_PREFIX) && name.ends_with(PACK_SUFFIX)) {
            continue;
        }
        let mid = &name[PACK_PREFIX.len()..name.len() - PACK_SUFFIX.len()];
        if let Ok(v) = mid.parse::<u32>()
            && best.as_ref().is_none_or(|(bv, _)| v > *bv)
        {
            best = Some((v, entry.path()));
        }
    }
    best.map(|(_, path)| QuestionPack::load_from_file(&path))
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
    /// Some(pin, detail) → caller pushes `Cmd::Audit kind=judgment`.
    pub audit: Option<(String, serde_json::Value)>,
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
    Ready {
        slice: StateSlice,
        pack: QuestionPack,
        transport: UreqTransport,
    },
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
/// outside the journal tx, then `finish_plan` inside.
pub fn prepare_post(a: ExpandArgs) -> PostPlan {
    if !jev_enabled() {
        return plan_of(a, PostStage::Off);
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
    let transport = match UreqTransport::from_env() {
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
    plan_of(
        a,
        PostStage::Ready {
            slice,
            pack,
            transport,
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
/// loud, never quiet-merge.
pub fn finish_plan(plan: PostPlan, posted: Option<(AdapterOutcome, u64)>) -> ExpandOut {
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
            primitive: table.primitive,
            topology: table.topology,
            argv: table.argv,
            reason: table.reason,
            audit: None,
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
                audit: Some((pin, serde_json::json!(&merged.effect))),
            }
        }
        PostStage::Ready {
            slice: _,
            pack,
            transport: _,
        } => {
            // (answers, deny, fallback_flag, latency). Honesty never falls
            // back, but the arm is defensive rather than unreachable: a
            // fallback here still escalates.
            let (answers_opt, deny_opt, fallback_flag, latency_ms) = match posted {
                Some((AdapterOutcome::Answered(a), ms)) => (Some(a), None, false, ms),
                Some((AdapterOutcome::Refused(e), ms)) => (None, Some(e.to_string()), false, ms),
                Some((AdapterOutcome::FallbackTable(e), ms)) => {
                    (None, Some(e.to_string()), true, ms)
                }
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
            let merged = merge(
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
            ExpandOut {
                primitive: merged.primitive,
                topology: merged.topology,
                argv: merged.argv,
                reason: merged.reason,
                audit: Some((pin, serde_json::json!(&merged.effect))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!jev_enabled());
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

    // Superseded by prepare_flag_off_is_passthrough_without_audit (bd-n86n
    // three-phase path); removed with maybe_enrich.

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
        let _env = crate::config::lock_env();
        unsafe { env::remove_var(FLAG_ENV) };
        let plan = prepare_post(expand_args("/nonexistent-probe"));
        assert!(matches!(plan.stage, PostStage::Off));
        let out = finish_plan(plan, None);
        assert_eq!(out.primitive, "loop-work");
        assert_eq!(out.topology, "loop");
        assert!(out.audit.is_none(), "flag off = byte-identical, no audit");
    }

    #[test]
    fn prepare_missing_pack_is_deny_with_escalate() {
        let _env = crate::config::lock_env();
        unsafe { env::set_var(FLAG_ENV, "1") };
        let base = std::env::temp_dir().join(format!("jev-n86n-nopack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let plan = prepare_post(expand_args(&base.to_string_lossy()));
        assert!(
            matches!(plan.stage, PostStage::Deny { .. }),
            "missing pack = deny, never POST"
        );
        let out = finish_plan(plan, None);
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
            },
        };
        let out = finish_plan(plan, Some((AdapterOutcome::Answered(stub_answers()), 123)));
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
            },
        };
        // Programming error (POST skipped): fail-closed deny, never quiet merge.
        let out = finish_plan(plan, None);
        assert!(out.audit.is_some());
        let reason = out.reason;
        assert!(reason.contains("jev"), "deny attaches advisory: {reason}");
    }
}
