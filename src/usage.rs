//! Versioned usage-event ingest.
//!
//! Fleet harnesses push cumulative session snapshots. Two transports share
//! this validator and store: an on-disk inbox drained by `agent-meterd`, and
//! an optional HTTP listener. The quota document `state.json` stays
//! schema version 1. Usage is stored beside it, never inside it.
//!
//! Counts are running totals for one session. A higher `seq` replaces a lower
//! one so a repeated Stop hook does not double count. Unknown counts stay
//! null. This module does not keep prompts, messages, tool payloads, diffs,
//! or provider responses.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const EVENT_SCHEMA: &str = "agent-meter.usage-event/v1";
pub const REPORT_SCHEMA: &str = "agent-meter.usage-report/v1";
pub const TODAY_SCHEMA: &str = "agent-meter.usage-today/v1";
pub const TOKEN_SCHEMA: &str = "agent-meter.ingest-tokens/v1";

pub const EVENTS_FILE_NAME: &str = "usage-events.jsonl";
pub const INDEX_FILE_NAME: &str = "usage-index.json";
pub const TODAY_FILE_NAME: &str = "usage-today.json";
pub const INBOX_DIR_NAME: &str = "inbox";
pub const PROCESSED_DIR_NAME: &str = "processed";
pub const REJECTED_DIR_NAME: &str = "rejected";

/// Largest accepted inbox file or HTTP body.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
pub const MAX_BATCH_EVENTS: usize = 100;
/// Newest session snapshots kept in the rollup. Older ones are dropped,
/// the same way forecast history drops observations past its cap.
pub const MAX_SESSIONS: usize = 2048;
pub const MAX_SEEN_KEYS: usize = 8192;
pub const MAX_JSONL_LINES: u64 = 8192;
pub const MAX_AGE_DAYS: i64 = 30;
pub const MAX_ARCHIVE_FILES: usize = 1024;
const MAX_MODELS: usize = 64;
const INDEX_VERSION: u8 = 1;

const FORBIDDEN_KEYS: &[&str] = &[
    "prompt",
    "prompts",
    "message",
    "messages",
    "content",
    "completion",
    "tool_input",
    "tool_output",
    "tool_use",
    "tool_result",
    "tool_call",
    "tool_calls",
    "diff",
    "diffs",
    "patch",
    "hunk",
    "hunks",
    "file_path",
    "filepath",
    "file",
    "files",
    "path",
    "paths",
    "transcript",
    "transcript_path",
    "cwd",
    "workspace",
];

#[derive(Debug)]
pub enum UsageReject {
    Schema(String),
    TooLarge { size: usize },
}

impl std::fmt::Display for UsageReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsageReject::Schema(msg) => write!(f, "{msg}"),
            UsageReject::TooLarge { size } => {
                write!(
                    f,
                    "payload is {size} bytes; limit is {MAX_EVENT_BYTES} bytes"
                )
            }
        }
    }
}

impl std::error::Error for UsageReject {}

#[derive(Debug)]
pub enum AcceptError {
    Schema(String),
    TooLarge { size: usize },
    Unauthorized,
    Store(String),
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcceptError::Schema(msg) => write!(f, "{msg}"),
            AcceptError::TooLarge { size } => {
                write!(
                    f,
                    "payload is {size} bytes; limit is {MAX_EVENT_BYTES} bytes"
                )
            }
            AcceptError::Unauthorized => write!(f, "unauthorized"),
            AcceptError::Store(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for AcceptError {}

impl From<UsageReject> for AcceptError {
    fn from(value: UsageReject) -> Self {
        match value {
            UsageReject::Schema(msg) => AcceptError::Schema(msg),
            UsageReject::TooLarge { size } => AcceptError::TooLarge { size },
        }
    }
}

fn schema_err(msg: impl Into<String>) -> UsageReject {
    let msg = msg.into();
    let msg = if msg.len() > 240 {
        format!("{}…", &msg[..240])
    } else {
        msg
    };
    UsageReject::Schema(msg)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostSource {
    Harness,
    Estimated,
    None,
}

/// One model's cumulative counts inside a session snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelUsage {
    pub model: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub requests: Option<u64>,
    pub cost_usd: Option<f64>,
    pub cost_source: CostSource,
}

/// A validated usage event. Every field is required on the wire; null means
/// the reporter does not know the value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UsageEvent {
    pub schema: String,
    pub idempotency_key: String,
    pub kind: String,
    pub host: String,
    pub harness: String,
    pub harness_version: String,
    pub session_id: String,
    pub seq: u64,
    pub observed_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub models: Vec<ModelUsage>,
    pub source: String,
    pub dispatch_run_id: Option<String>,
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestResult {
    pub accepted: u32,
    pub duplicates: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainStats {
    pub seen: u32,
    pub accepted: u32,
    pub duplicates: u32,
    pub rejected: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Harness,
    Model,
    Host,
}

impl GroupBy {
    pub fn parse(spec: &str) -> std::result::Result<Self, String> {
        match spec {
            "harness" => Ok(Self::Harness),
            "model" => Ok(Self::Model),
            "host" => Ok(Self::Host),
            _ => Err("--by must be harness, model, or host".into()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Model => "model",
            Self::Host => "host",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TokenTotals {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub requests: Option<u64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    /// Fields where some values were known and at least one was null.
    /// A field that is entirely unknown is null and is not listed here.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UsageGroup {
    pub key: String,
    pub sessions: u64,
    pub totals: TokenTotals,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UsageReport {
    pub schema: String,
    pub generated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    pub sessions: u64,
    pub totals: TokenTotals,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<UsageGroup>,
}

#[derive(Debug, Clone)]
pub struct UsagePaths {
    root: PathBuf,
}

impl UsagePaths {
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$XDG_STATE_HOME/agent-meter`, matching the stable quota state file.
    pub fn from_env() -> Self {
        let root = crate::stable_state_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("agent-meter"));
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn inbox(&self) -> PathBuf {
        self.root.join(INBOX_DIR_NAME)
    }

    pub fn processed(&self) -> PathBuf {
        self.root.join(PROCESSED_DIR_NAME)
    }

    pub fn rejected(&self) -> PathBuf {
        self.root.join(REJECTED_DIR_NAME)
    }

    pub fn events_log(&self) -> PathBuf {
        self.root.join(EVENTS_FILE_NAME)
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE_NAME)
    }

    pub fn today_path(&self) -> PathBuf {
        self.root.join(TODAY_FILE_NAME)
    }
}

/// Lowercase hex SHA-256 of `host|harness|session_id|seq`.
///
/// `seq` is rendered in decimal without leading zeros. The three string
/// fields are the validated values and do not contain `|`.
pub fn canonical_idempotency_key(host: &str, harness: &str, session_id: &str, seq: u64) -> String {
    let material = format!("{host}|{harness}|{session_id}|{seq}");
    token_sha256_hex(&material)
}

/// Lowercase hex SHA-256 of the bearer token's UTF-8 bytes. No newline.
pub fn token_sha256_hex(token: &str) -> String {
    hex_encode(&Sha256::digest(token.as_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn ct_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// `24h`, `7d`, `30m`, `90s`, `2w`, or an RFC3339 timestamp.
pub fn parse_since(spec: &str, now: DateTime<Utc>) -> std::result::Result<DateTime<Utc>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("--since needs a duration such as 24h or an RFC3339 timestamp".into());
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(spec) {
        return Ok(parsed.with_timezone(&Utc));
    }
    let split = spec
        .find(|c: char| c.is_ascii_alphabetic())
        .ok_or_else(|| format!("--since {spec} is not a duration or RFC3339 timestamp"))?;
    let (number, unit) = spec.split_at(split);
    let number: i64 = number
        .parse()
        .map_err(|_| format!("--since {spec} is not a duration or RFC3339 timestamp"))?;
    if number < 0 {
        return Err("--since duration must be zero or positive".into());
    }
    let duration = match unit.to_ascii_lowercase().as_str() {
        "s" => ChronoDuration::seconds(number),
        "m" => ChronoDuration::minutes(number),
        "h" => ChronoDuration::hours(number),
        "d" => ChronoDuration::days(number),
        "w" => ChronoDuration::weeks(number),
        _ => return Err(format!("--since unit in {spec} must be s, m, h, d, or w")),
    };
    Ok(now - duration)
}

pub fn parse_payload(bytes: &[u8]) -> std::result::Result<Vec<UsageEvent>, UsageReject> {
    if bytes.len() > MAX_EVENT_BYTES {
        return Err(UsageReject::TooLarge { size: bytes.len() });
    }
    if bytes.is_empty() {
        return Err(schema_err("empty payload"));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|err| schema_err(err.to_string()))?;
    reject_forbidden(&value)?;
    let events = match &value {
        Value::Object(map) if map.contains_key("events") => {
            if map.len() != 1 {
                return Err(schema_err("a batch object may only contain the events key"));
            }
            match map.get("events") {
                Some(Value::Array(items)) => {
                    if items.len() > MAX_BATCH_EVENTS {
                        return Err(schema_err(format!(
                            "batch has {} events; limit is {MAX_BATCH_EVENTS}",
                            items.len()
                        )));
                    }
                    items.clone()
                }
                _ => return Err(schema_err("events must be an array")),
            }
        }
        Value::Object(map) if map.contains_key("schema") => vec![Value::Object(map.clone())],
        Value::Array(_) => {
            return Err(schema_err(
                "payload must be one event object or {\"events\":[...]}",
            ))
        }
        _ => return Err(schema_err("payload must be a JSON object")),
    };
    events.into_iter().map(parse_event_value).collect()
}

fn reject_forbidden(value: &Value) -> std::result::Result<(), UsageReject> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if FORBIDDEN_KEYS
                    .iter()
                    .any(|name| key.eq_ignore_ascii_case(name))
                {
                    return Err(schema_err(format!("forbidden field `{key}`")));
                }
                reject_forbidden(child)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                reject_forbidden(item)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn parse_event_value(value: Value) -> std::result::Result<UsageEvent, UsageReject> {
    let event: UsageEvent =
        serde_json::from_value(value).map_err(|err| schema_err(err.to_string()))?;
    validate_event(&event)?;
    Ok(event)
}

fn validate_event(event: &UsageEvent) -> std::result::Result<(), UsageReject> {
    if event.schema != EVENT_SCHEMA {
        return Err(schema_err(format!(
            "schema must be {EVENT_SCHEMA}, got {}",
            event.schema
        )));
    }
    if event.kind != "session_snapshot" {
        return Err(schema_err(
            "kind must be session_snapshot in usage-event v1",
        ));
    }
    if !is_host_id(&event.host) {
        return Err(schema_err(
            "host must be 1-128 characters of letters, digits, '.', '_' or '-'",
        ));
    }
    if !is_harness(&event.harness) {
        return Err(schema_err(
            "harness must be a lowercase id such as claude, codex, agy, grok, muse, omp, copilot, or cursor",
        ));
    }
    if !is_version(&event.harness_version) {
        return Err(schema_err(
            "harness_version must be 1-64 letters, digits, '.', '_', '+', or '-'",
        ));
    }
    if !is_session_id(&event.session_id) {
        return Err(schema_err(
            "session_id must be 1-200 letters, digits, or . _ : @ + - and must not be a path",
        ));
    }
    if !is_idempotency_hex(&event.idempotency_key) {
        return Err(schema_err(
            "idempotency_key must be 64 lowercase hex characters",
        ));
    }
    let expected =
        canonical_idempotency_key(&event.host, &event.harness, &event.session_id, event.seq);
    if !ct_eq(event.idempotency_key.as_bytes(), expected.as_bytes()) {
        return Err(schema_err(
            "idempotency_key must be sha256 hex of host|harness|session_id|seq",
        ));
    }
    parse_rfc3339(&event.observed_at).map_err(|_| schema_err("observed_at must be RFC3339"))?;
    if let Some(started) = &event.started_at {
        parse_rfc3339(started).map_err(|_| schema_err("started_at must be RFC3339 or null"))?;
    }
    if let Some(ended) = &event.ended_at {
        parse_rfc3339(ended).map_err(|_| schema_err("ended_at must be RFC3339 or null"))?;
    }
    if event.models.len() > MAX_MODELS {
        return Err(schema_err(format!(
            "models has {} entries; limit is {MAX_MODELS}",
            event.models.len()
        )));
    }
    let mut model_names = HashSet::new();
    for model in &event.models {
        validate_model(model)?;
        if !model_names.insert(model.model.as_str()) {
            return Err(schema_err(format!("duplicate model `{}`", model.model)));
        }
    }
    if !is_source(&event.source) {
        return Err(schema_err(
            "source must be 1-128 ASCII characters without control characters",
        ));
    }
    if let Some(run) = &event.dispatch_run_id {
        if !is_session_id(run) || run.len() > 128 {
            return Err(schema_err(
                "dispatch_run_id must be a short id or null, not a path",
            ));
        }
    }
    if let Some(repo) = &event.repo {
        if !is_repo(repo) {
            return Err(schema_err(
                "repo must be owner/name or null, not a filesystem path",
            ));
        }
    }
    Ok(())
}

fn validate_model(model: &ModelUsage) -> std::result::Result<(), UsageReject> {
    if !is_model_name(&model.model) {
        return Err(schema_err(
            "model must be 1-128 characters and must not be a filesystem path",
        ));
    }
    match (model.cost_usd, model.cost_source) {
        (None, CostSource::None) => Ok(()),
        (Some(cost), CostSource::Harness | CostSource::Estimated)
            if cost.is_finite() && cost >= 0.0 =>
        {
            Ok(())
        }
        (None, _) => Err(schema_err("cost_source must be none when cost_usd is null")),
        (Some(_), CostSource::None) => Err(schema_err(
            "cost_source must be harness or estimated when cost_usd is set",
        )),
        (Some(_), _) => Err(schema_err(
            "cost_usd must be a non-negative finite number or null",
        )),
    }
}

fn parse_rfc3339(value: &str) -> std::result::Result<DateTime<FixedOffset>, ()> {
    DateTime::parse_from_rfc3339(value).map_err(|_| ())
}

fn is_host_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_harness(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    })
}

fn is_version(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

fn is_session_id(value: &str) -> bool {
    (1..=200).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'+' | b'-')
        })
}

fn is_model_name(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && !value.contains("..")
        && !value.starts_with('/')
        && !value.contains('\\')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b':' | b'@' | b'+' | b'-' | b'/')
        })
}

fn is_source(value: &str) -> bool {
    (1..=128).contains(&value.len()) && value.chars().all(|ch| ch.is_ascii() && !ch.is_control())
}

fn is_repo(value: &str) -> bool {
    if value.is_empty() || value.len() > 200 {
        return false;
    }
    let mut parts = value.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let name = parts.next();
    if parts.next().is_some() {
        return false;
    }
    repo_segment(owner) && name.map(repo_segment).unwrap_or(true)
}

fn repo_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

fn is_idempotency_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Clone)]
struct Limits {
    max_sessions: usize,
    max_seen_keys: usize,
    max_jsonl_lines: u64,
    max_age: ChronoDuration,
    max_archive_files: usize,
}

fn default_limits() -> Limits {
    Limits {
        max_sessions: MAX_SESSIONS,
        max_seen_keys: MAX_SEEN_KEYS,
        max_jsonl_lines: MAX_JSONL_LINES,
        max_age: ChronoDuration::days(MAX_AGE_DAYS),
        max_archive_files: MAX_ARCHIVE_FILES,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSession {
    event: UsageEvent,
}

#[derive(Debug, Serialize, Deserialize)]
struct UsageIndexFile {
    version: u8,
    sessions: Vec<StoredSession>,
    seen_keys: Vec<String>,
}

enum Outcome {
    Accepted,
    Duplicate,
}

pub struct UsageStore {
    paths: UsagePaths,
    sessions: Vec<StoredSession>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    jsonl_lines: u64,
    limits: Limits,
    extra_today: Vec<PathBuf>,
}

impl UsageStore {
    pub fn open(paths: UsagePaths) -> Result<Self> {
        let mut store = UsageStore {
            paths,
            sessions: Vec::new(),
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            jsonl_lines: 0,
            limits: default_limits(),
            extra_today: Vec::new(),
        };
        store.load_index()?;
        let dirty = store.replay_log()?;
        if dirty {
            store.save_index()?;
        }
        store.compact_log_if_needed()?;
        Ok(store)
    }

    pub fn inbox_dir(&self) -> PathBuf {
        self.paths.inbox()
    }

    pub fn set_extra_today_paths(&mut self, paths: Vec<PathBuf>) {
        self.extra_today = paths;
    }

    pub fn accept_payload(
        &mut self,
        bytes: &[u8],
        expected_host: Option<&str>,
    ) -> std::result::Result<IngestResult, AcceptError> {
        let events = parse_payload(bytes)?;
        if let Some(host) = expected_host {
            if events.iter().any(|event| event.host != host) {
                return Err(AcceptError::Unauthorized);
            }
        }
        self.ingest_events(&events)
            .map_err(|err| AcceptError::Store(err.to_string()))
    }

    fn ingest_events(&mut self, events: &[UsageEvent]) -> Result<IngestResult> {
        let mut accepted = 0u32;
        let mut duplicates = 0u32;
        for event in events {
            match self.commit_event(event.clone(), true)? {
                Outcome::Accepted => accepted += 1,
                Outcome::Duplicate => duplicates += 1,
            }
        }
        self.enforce_bounds();
        self.compact_log_if_needed()?;
        self.save_index()?;
        Ok(IngestResult {
            accepted,
            duplicates,
        })
    }

    fn commit_event(&mut self, event: UsageEvent, append: bool) -> Result<Outcome> {
        if self.seen.contains(&event.idempotency_key) {
            return Ok(Outcome::Duplicate);
        }
        let position = self.sessions.iter().position(|session| {
            session.event.host == event.host
                && session.event.harness == event.harness
                && session.event.session_id == event.session_id
        });
        if let Some(position) = position {
            if event.seq <= self.sessions[position].event.seq {
                self.remember_key(event.idempotency_key);
                return Ok(Outcome::Duplicate);
            }
            if append {
                self.append_log(&event)?;
            }
            self.remember_key(event.idempotency_key.clone());
            self.sessions[position].event = event;
            return Ok(Outcome::Accepted);
        }
        if append {
            self.append_log(&event)?;
        }
        self.remember_key(event.idempotency_key.clone());
        self.sessions.push(StoredSession { event });
        Ok(Outcome::Accepted)
    }

    fn remember_key(&mut self, key: String) {
        if self.seen.insert(key.clone()) {
            self.seen_order.push_back(key);
        }
    }

    fn append_log(&mut self, event: &UsageEvent) -> Result<()> {
        let path = self.paths.events_log();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        file.write_all(&line)
            .with_context(|| format!("appending {}", path.display()))?;
        file.sync_all().ok();
        self.jsonl_lines += 1;
        Ok(())
    }

    fn enforce_bounds(&mut self) {
        let cutoff = Utc::now() - self.limits.max_age;
        self.sessions
            .retain(|session| event_time(&session.event) >= cutoff);
        if self.sessions.len() > self.limits.max_sessions {
            self.sessions.sort_by(|left, right| {
                event_time(&left.event)
                    .cmp(&event_time(&right.event))
                    .then(left.event.session_id.cmp(&right.event.session_id))
            });
            let overflow = self.sessions.len() - self.limits.max_sessions;
            self.sessions.drain(0..overflow);
        }
        self.trim_seen_keys();
    }

    fn trim_seen_keys(&mut self) {
        let live: HashSet<&str> = self
            .sessions
            .iter()
            .map(|session| session.event.idempotency_key.as_str())
            .collect();
        let mut rotated = 0usize;
        while self.seen_order.len() > self.limits.max_seen_keys {
            let Some(key) = self.seen_order.pop_front() else {
                break;
            };
            if live.contains(key.as_str()) {
                self.seen_order.push_back(key);
                rotated += 1;
                if rotated > self.seen_order.len() {
                    break;
                }
                continue;
            }
            self.seen.remove(&key);
            rotated = 0;
        }
    }

    fn compact_log_if_needed(&mut self) -> Result<()> {
        if self.jsonl_lines <= self.limits.max_jsonl_lines {
            return Ok(());
        }
        let path = self.paths.events_log();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("jsonl.tmp");
        {
            let mut file = File::create(&temporary)
                .with_context(|| format!("creating {}", temporary.display()))?;
            for session in &self.sessions {
                let mut line = serde_json::to_vec(&session.event)?;
                line.push(b'\n');
                file.write_all(&line)?;
            }
            file.sync_all().ok();
        }
        fs::rename(&temporary, &path).with_context(|| format!("replacing {}", path.display()))?;
        self.jsonl_lines = self.sessions.len() as u64;
        Ok(())
    }

    fn load_index(&mut self) -> Result<()> {
        let path = self.paths.index_path();
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let index: UsageIndexFile = serde_json::from_str(&contents)
            .with_context(|| format!("parsing {}", path.display()))?;
        if index.version != INDEX_VERSION {
            bail!("unsupported usage index version {}", index.version);
        }
        self.sessions = index.sessions;
        self.seen_order = index.seen_keys.into();
        self.seen = self.seen_order.iter().cloned().collect();
        Ok(())
    }

    fn replay_log(&mut self) -> Result<bool> {
        let path = self.paths.events_log();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let reader = BufReader::new(file);
        let mut dirty = false;
        let mut lines = 0u64;
        for line in reader.lines() {
            let line = line.with_context(|| format!("reading {}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            lines += 1;
            let Ok(event) = serde_json::from_str::<UsageEvent>(&line) else {
                eprintln!("agent-meterd: skipping unreadable usage log line");
                continue;
            };
            if validate_event(&event).is_err() {
                eprintln!("agent-meterd: skipping invalid usage log line");
                continue;
            }
            if self.seen.contains(&event.idempotency_key) {
                continue;
            }
            self.commit_event(event, false)?;
            dirty = true;
        }
        self.jsonl_lines = lines;
        if dirty {
            self.enforce_bounds();
        }
        Ok(dirty)
    }

    fn save_index(&self) -> Result<()> {
        let path = self.paths.index_path();
        let index = UsageIndexFile {
            version: INDEX_VERSION,
            sessions: self.sessions.clone(),
            seen_keys: self.seen_order.iter().cloned().collect(),
        };
        let encoded = serde_json::to_vec_pretty(&index)?;
        atomic_write(&path, &encoded)
    }

    pub fn drain_inbox(&mut self) -> Result<DrainStats> {
        let inbox = self.paths.inbox();
        if !inbox.exists() {
            return Ok(DrainStats::default());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(&inbox).with_context(|| format!("reading {}", inbox.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if !meta.file_type().is_file() {
                continue;
            }
            paths.push(entry.path());
        }
        paths.sort();
        let mut stats = DrainStats::default();
        for path in paths {
            stats.seen += 1;
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    self.archive(&path, &self.paths.rejected(), Some(&err.to_string()))?;
                    stats.rejected += 1;
                    continue;
                }
            };
            match self.accept_payload(&bytes, None) {
                Ok(result) => {
                    stats.accepted += result.accepted;
                    stats.duplicates += result.duplicates;
                    self.archive(&path, &self.paths.processed(), None)?;
                }
                Err(err) => {
                    self.archive(&path, &self.paths.rejected(), Some(&err.to_string()))?;
                    stats.rejected += 1;
                }
            }
        }
        self.trim_archive(&self.paths.processed())?;
        self.trim_archive(&self.paths.rejected())?;
        Ok(stats)
    }

    fn archive(&self, src: &Path, dir: &Path, reason: Option<&str>) -> Result<()> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let name = src.file_name().context("inbox file is missing a name")?;
        let dest = unique_dest(dir, name);
        fs::rename(src, &dest)
            .with_context(|| format!("moving {} to {}", src.display(), dest.display()))?;
        if let Some(reason) = reason {
            let reason_path = dest.with_extension("reason");
            fs::write(&reason_path, reason)
                .with_context(|| format!("writing {}", reason_path.display()))?;
        }
        Ok(())
    }

    fn trim_archive(&self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            files.push((modified, entry.path()));
        }
        if files.len() <= self.limits.max_archive_files {
            return Ok(());
        }
        files.sort_by_key(|(modified, _)| *modified);
        let overflow = files.len() - self.limits.max_archive_files;
        for (_, path) in files.into_iter().take(overflow) {
            let _ = fs::remove_file(path.with_extension("reason"));
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    }

    pub fn query(&self, since: Option<DateTime<Utc>>, group_by: Option<GroupBy>) -> UsageReport {
        let mut included = Vec::new();
        for session in &self.sessions {
            let observed = match event_time_result(&session.event) {
                Some(observed) => observed,
                None => continue,
            };
            if let Some(since) = since {
                if observed < since {
                    continue;
                }
            }
            included.push(&session.event);
        }
        let totals = totals_for_events(&included);
        let groups = match group_by {
            Some(group) => groups_for(group, &included),
            None => Vec::new(),
        };
        UsageReport {
            schema: REPORT_SCHEMA.to_owned(),
            generated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            since: since.map(|stamp| stamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            group_by: group_by.map(GroupBy::as_str).map(str::to_owned),
            sessions: included.len() as u64,
            totals,
            groups,
        }
    }

    pub fn publish_today(&mut self, now: DateTime<FixedOffset>) -> Result<()> {
        let day = now.date_naive();
        let mut included = Vec::new();
        for session in &self.sessions {
            if event_on_day(&session.event, now) {
                included.push(&session.event);
            }
        }
        let totals = totals_for_events(&included);
        let sessions = included.len() as u64;
        let label = today_label(sessions, &totals);
        let document = serde_json::json!({
            "schema": TODAY_SCHEMA,
            "day": day.format("%Y-%m-%d").to_string(),
            "sessions": sessions,
            "label": label,
            "totals": totals,
        });
        let encoded = serde_json::to_vec_pretty(&document)?;
        let mut destinations = vec![self.paths.today_path()];
        destinations.extend(self.extra_today.iter().cloned());
        for path in destinations {
            atomic_write(&path, &encoded)?;
        }
        Ok(())
    }

    pub fn publish_today_now(&mut self) -> Result<()> {
        let local = chrono::Local::now().to_rfc3339();
        let now = DateTime::parse_from_rfc3339(&local).unwrap_or_else(|_| {
            FixedOffset::east_opt(0)
                .expect("utc offset")
                .from_utc_datetime(&Utc::now().naive_utc())
        });
        self.publish_today(now)
    }
}

fn event_time(event: &UsageEvent) -> DateTime<Utc> {
    event_time_result(event).unwrap_or_else(Utc::now)
}

fn event_time_result(event: &UsageEvent) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&event.observed_at)
        .ok()
        .map(|stamp| stamp.with_timezone(&Utc))
}

fn event_on_day(event: &UsageEvent, now: DateTime<FixedOffset>) -> bool {
    let Ok(stamp) = DateTime::parse_from_rfc3339(&event.observed_at) else {
        return false;
    };
    stamp.with_timezone(now.offset()).date_naive() == now.date_naive()
}

fn today_label(sessions: u64, totals: &TokenTotals) -> String {
    if sessions == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    if let Some(input) = totals.input_tokens {
        parts.push(format!("{} in", group_digits(input)));
    }
    if let Some(output) = totals.output_tokens {
        parts.push(format!("{} out", group_digits(output)));
    }
    let noun = if sessions == 1 { "session" } else { "sessions" };
    parts.push(format!("{sessions} {noun}"));
    if totals
        .incomplete
        .iter()
        .any(|field| field == "input_tokens" || field == "output_tokens")
    {
        parts.push("partial".into());
    }
    format!("Today: {}", parts.join(" · "))
}

fn group_digits(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[derive(Default)]
struct CountAcc {
    sum: u64,
    known: bool,
    missing: bool,
}

impl CountAcc {
    fn add(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.sum = self.sum.saturating_add(value);
                self.known = true;
            }
            None => self.missing = true,
        }
    }

    fn finish(&self, name: &str, incomplete: &mut Vec<String>) -> Option<u64> {
        if !self.known {
            return None;
        }
        if self.missing {
            incomplete.push(name.to_owned());
        }
        Some(self.sum)
    }
}

struct CostAcc {
    sum: f64,
    known: bool,
    missing: bool,
}

impl Default for CostAcc {
    fn default() -> Self {
        Self {
            sum: 0.0,
            known: false,
            missing: false,
        }
    }
}

impl CostAcc {
    fn add(&mut self, value: Option<f64>) {
        match value {
            Some(value) => {
                self.sum += value;
                self.known = true;
            }
            None => self.missing = true,
        }
    }

    fn finish(&self, incomplete: &mut Vec<String>) -> Option<f64> {
        if !self.known {
            return None;
        }
        if self.missing {
            incomplete.push("cost_usd".into());
        }
        Some(self.sum)
    }
}

struct TotalsAcc {
    input_tokens: CountAcc,
    output_tokens: CountAcc,
    cache_read_tokens: CountAcc,
    cache_write_tokens: CountAcc,
    reasoning_tokens: CountAcc,
    requests: CountAcc,
    duration_ms: CountAcc,
    cost_usd: CostAcc,
}

impl TotalsAcc {
    fn add_model(&mut self, model: &ModelUsage) {
        self.input_tokens.add(model.input_tokens);
        self.output_tokens.add(model.output_tokens);
        self.cache_read_tokens.add(model.cache_read_tokens);
        self.cache_write_tokens.add(model.cache_write_tokens);
        self.reasoning_tokens.add(model.reasoning_tokens);
        self.requests.add(model.requests);
        self.cost_usd.add(model.cost_usd);
    }

    fn add_duration(&mut self, duration_ms: Option<u64>) {
        self.duration_ms.add(duration_ms);
    }

    fn finish(&self) -> TokenTotals {
        let mut incomplete = Vec::new();
        TokenTotals {
            input_tokens: self.input_tokens.finish("input_tokens", &mut incomplete),
            output_tokens: self.output_tokens.finish("output_tokens", &mut incomplete),
            cache_read_tokens: self
                .cache_read_tokens
                .finish("cache_read_tokens", &mut incomplete),
            cache_write_tokens: self
                .cache_write_tokens
                .finish("cache_write_tokens", &mut incomplete),
            reasoning_tokens: self
                .reasoning_tokens
                .finish("reasoning_tokens", &mut incomplete),
            requests: self.requests.finish("requests", &mut incomplete),
            cost_usd: self.cost_usd.finish(&mut incomplete),
            duration_ms: self.duration_ms.finish("duration_ms", &mut incomplete),
            incomplete,
        }
    }
}

impl Default for TotalsAcc {
    fn default() -> Self {
        Self {
            input_tokens: CountAcc::default(),
            output_tokens: CountAcc::default(),
            cache_read_tokens: CountAcc::default(),
            cache_write_tokens: CountAcc::default(),
            reasoning_tokens: CountAcc::default(),
            requests: CountAcc::default(),
            duration_ms: CountAcc::default(),
            cost_usd: CostAcc::default(),
        }
    }
}

fn totals_for_events(events: &[&UsageEvent]) -> TokenTotals {
    let mut acc = TotalsAcc::default();
    for event in events {
        for model in &event.models {
            acc.add_model(model);
        }
        acc.add_duration(event.duration_ms);
    }
    acc.finish()
}

fn groups_for(group_by: GroupBy, events: &[&UsageEvent]) -> Vec<UsageGroup> {
    let mut grouped: BTreeMap<String, (u64, TotalsAcc)> = BTreeMap::new();
    for event in events {
        match group_by {
            GroupBy::Harness => {
                let entry = grouped.entry(event.harness.clone()).or_default();
                entry.0 += 1;
                for model in &event.models {
                    entry.1.add_model(model);
                }
                entry.1.add_duration(event.duration_ms);
            }
            GroupBy::Host => {
                let entry = grouped.entry(event.host.clone()).or_default();
                entry.0 += 1;
                for model in &event.models {
                    entry.1.add_model(model);
                }
                entry.1.add_duration(event.duration_ms);
            }
            GroupBy::Model => {
                let mut seen_in_session = HashSet::new();
                for model in &event.models {
                    let entry = grouped.entry(model.model.clone()).or_default();
                    if seen_in_session.insert(model.model.as_str()) {
                        entry.0 += 1;
                    }
                    entry.1.add_model(model);
                }
            }
        }
    }
    grouped
        .into_iter()
        .map(|(key, (sessions, acc))| UsageGroup {
            key,
            sessions,
            totals: acc.finish(),
        })
        .collect()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("usage");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn unique_dest(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let dest = dir.join(name);
    if !dest.exists() {
        return dest;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    dir.join(format!("{nanos}-{}", name.to_string_lossy()))
}

/// Read a today file and return its label when `day` matches.
pub fn today_label_from_file(path: &Path, today: NaiveDate) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let day = value.get("day")?.as_str()?;
    let parsed = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    if parsed != today {
        return None;
    }
    let label = value.get("label")?.as_str()?;
    if label.is_empty() {
        None
    } else {
        Some(label.to_owned())
    }
}

/// Best-effort label for the desktop card. Missing files and stale days
/// produce no line.
pub fn current_today_label() -> Option<String> {
    let today = chrono::Local::now().date_naive();
    let mut paths = Vec::new();
    if let Some(parent) = crate::runtime_state_path().parent() {
        paths.push(parent.join(TODAY_FILE_NAME));
    }
    if let Some(parent) = crate::stable_state_path().parent() {
        paths.push(parent.join(TODAY_FILE_NAME));
    }
    for path in paths {
        if let Some(label) = today_label_from_file(&path, today) {
            return Some(label);
        }
    }
    None
}

#[derive(Clone, Debug)]
pub struct IngestTokens {
    hosts: Vec<(String, String)>,
}

impl IngestTokens {
    /// Host id whose stored hash matches the bearer token.
    ///
    /// Every configured hash is compared. Zero matches and more than one
    /// match both fail closed.
    pub fn host_for_token(&self, token: &str) -> Option<String> {
        if token.is_empty() || token.len() > 512 {
            return None;
        }
        let hash = token_sha256_hex(token);
        let mut found = None;
        let mut matches = 0u32;
        for (host, host_hash) in &self.hosts {
            if ct_eq(hash.as_bytes(), host_hash.as_bytes()) {
                matches += 1;
                found = Some(host.clone());
            }
        }
        if matches == 1 {
            found
        } else {
            None
        }
    }
}

pub fn load_ingest_tokens(path: &Path) -> Result<IngestTokens> {
    if !path.exists() {
        bail!(
            "ingest token file {} is missing; refusing to listen",
            path.display()
        );
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let Some(map) = value.as_object() else {
        bail!("ingest token file must be a JSON object");
    };
    for key in map.keys() {
        if key != "schema" && key != "hosts" {
            bail!("ingest token file has unknown key {key}");
        }
    }
    if let Some(schema) = map.get("schema") {
        if schema.as_str() != Some(TOKEN_SCHEMA) {
            bail!("ingest token file schema must be {TOKEN_SCHEMA}");
        }
    }
    let Some(hosts) = map.get("hosts").and_then(Value::as_object) else {
        bail!("ingest token file is missing hosts");
    };
    if hosts.is_empty() {
        bail!("ingest token file has no hosts");
    }
    let mut pairs = Vec::with_capacity(hosts.len());
    for (host, hash) in hosts {
        if !is_host_id(host) {
            bail!("ingest token host id {host} is not a valid host id");
        }
        let Some(hash) = hash.as_str() else {
            bail!("ingest token hash for {host} must be a string");
        };
        if !is_idempotency_hex(hash) {
            bail!(
                "ingest token hash for {host} must be lowercase sha256 hex, not the token itself"
            );
        }
        pairs.push((host.clone(), hash.to_owned()));
    }
    Ok(IngestTokens { hosts: pairs })
}

pub fn parse_listen_addr(spec: &str, listen_any: bool) -> Result<SocketAddr> {
    let addr: SocketAddr = spec
        .parse()
        .with_context(|| format!("--listen {spec} must be an ip:port such as 100.64.0.1:8787"))?;
    if addr.ip().is_unspecified() && !listen_any {
        bail!("refusing to bind {} without --listen-any", addr.ip());
    }
    Ok(addr)
}

#[derive(Debug)]
pub struct UsageListener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub addr: SocketAddr,
}

impl Drop for UsageListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Bind the usage listener. Fails before the socket opens when the address is
/// unspecified and `listen_any` is false, or when the token file is missing.
pub fn serve(
    spec: &str,
    listen_any: bool,
    token_path: &Path,
    store: Arc<Mutex<UsageStore>>,
) -> Result<UsageListener> {
    let addr = parse_listen_addr(spec, listen_any)?;
    let tokens = load_ingest_tokens(token_path)?;
    let listener = TcpListener::bind(addr).with_context(|| format!("binding {addr}"))?;
    listener
        .set_nonblocking(true)
        .context("setting listener nonblocking")?;
    let bound = listener.local_addr().context("reading listener address")?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || accept_loop(listener, tokens, store, thread_stop));
    Ok(UsageListener {
        stop,
        thread: Some(thread),
        addr: bound,
    })
}

fn accept_loop(
    listener: TcpListener,
    tokens: IngestTokens,
    store: Arc<Mutex<UsageStore>>,
    stop: Arc<AtomicBool>,
) {
    use std::os::fd::AsRawFd;
    let fd = listener.as_raw_fd();
    while !stop.load(Ordering::SeqCst) {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, 200) };
        if ready <= 0 {
            continue;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                if let Some(request) = read_http_request(&mut stream) {
                    let response = handle_request(&request, &tokens, &store);
                    let _ = write_http_response(&mut stream, &response);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    body: String,
    authenticate: bool,
}

fn handle_request(
    request: &HttpRequest,
    tokens: &IngestTokens,
    store: &Arc<Mutex<UsageStore>>,
) -> HttpResponse {
    if request.method == "GET" && request.path == "/v1/usage/healthz" {
        return HttpResponse {
            status: 200,
            reason: "OK",
            body: serde_json::json!({"ok": true}).to_string(),
            authenticate: false,
        };
    }
    if request.path != "/v1/usage/events" {
        return error_response(404, "Not Found", "not found", false);
    }
    if request.method != "POST" {
        return error_response(405, "Method Not Allowed", "method not allowed", false);
    }
    if let Some(length) = content_length(request) {
        if length > MAX_EVENT_BYTES {
            return error_response(
                400,
                "Bad Request",
                &format!("payload is {length} bytes; limit is {MAX_EVENT_BYTES} bytes"),
                false,
            );
        }
    } else {
        return error_response(400, "Bad Request", "Content-Length is required", false);
    }
    let Some(token) = bearer_token(request) else {
        return error_response(401, "Unauthorized", "unauthorized", true);
    };
    let Some(host) = tokens.host_for_token(token) else {
        return error_response(401, "Unauthorized", "unauthorized", true);
    };
    let mut guard = store.lock().unwrap_or_else(|err| err.into_inner());
    match guard.accept_payload(&request.body, Some(&host)) {
        Ok(result) => {
            if result.accepted > 0 {
                if let Err(err) = guard.publish_today_now() {
                    eprintln!("agent-meterd: could not write usage today: {err}");
                }
            }
            HttpResponse {
                status: 202,
                reason: "Accepted",
                body: serde_json::json!({
                    "accepted": result.accepted,
                    "duplicates": result.duplicates,
                })
                .to_string(),
                authenticate: false,
            }
        }
        Err(AcceptError::Unauthorized) => error_response(401, "Unauthorized", "unauthorized", true),
        Err(AcceptError::TooLarge { size }) => error_response(
            400,
            "Bad Request",
            &format!("payload is {size} bytes; limit is {MAX_EVENT_BYTES} bytes"),
            false,
        ),
        Err(AcceptError::Schema(msg)) => error_response(400, "Bad Request", &msg, false),
        Err(AcceptError::Store(msg)) => {
            eprintln!("agent-meterd: usage store error: {msg}");
            error_response(500, "Internal Server Error", "store failed", false)
        }
    }
}

fn error_response(
    status: u16,
    reason: &'static str,
    message: &str,
    authenticate: bool,
) -> HttpResponse {
    HttpResponse {
        status,
        reason,
        body: serde_json::json!({"error": message}).to_string(),
        authenticate,
    }
}

fn content_length(request: &HttpRequest) -> Option<usize> {
    request
        .headers
        .get("content-length")
        .and_then(|value| value.trim().parse().ok())
}

fn bearer_token(request: &HttpRequest) -> Option<&str> {
    let header = request.headers.get("authorization")?;
    let mut parts = header.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    let token = parts.next()?.trim();
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token)
    } else {
        None
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        if buf.len() > 16 * 1024 {
            return None;
        }
        let read = stream.read(&mut tmp).ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..read]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
    };
    let header_text = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next()?;
    let mut request_parts = request_line.split(' ');
    let method = request_parts.next()?.to_owned();
    let raw_path = request_parts.next()?;
    let path = raw_path.split('?').next()?.to_owned();
    if request_parts.next().is_none() {
        return None;
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let declared = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if declared > MAX_EVENT_BYTES {
        let body = Vec::new();
        return Some(HttpRequest {
            method,
            path,
            headers,
            body,
        });
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < declared {
        let read = stream.read(&mut tmp).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..read]);
    }
    if body.len() < declared {
        return None;
    }
    body.truncate(declared);
    Some(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_http_response(stream: &mut TcpStream, response: &HttpResponse) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        response.status,
        response.reason,
        response.body.len()
    );
    if response.authenticate {
        head.push_str("WWW-Authenticate: Bearer\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event_with(
        host: &str,
        harness: &str,
        session: &str,
        seq: u64,
        observed_at: &str,
        input: Option<u64>,
        output: Option<u64>,
    ) -> UsageEvent {
        UsageEvent {
            schema: EVENT_SCHEMA.into(),
            idempotency_key: canonical_idempotency_key(host, harness, session, seq),
            kind: "session_snapshot".into(),
            host: host.into(),
            harness: harness.into(),
            harness_version: "2.1.321".into(),
            session_id: session.into(),
            seq,
            observed_at: observed_at.into(),
            started_at: None,
            ended_at: None,
            duration_ms: Some(45000),
            models: vec![ModelUsage {
                model: "demo-model".into(),
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: None,
                cache_write_tokens: Some(0),
                reasoning_tokens: None,
                requests: Some(1),
                cost_usd: None,
                cost_source: CostSource::None,
            }],
            source: "hook:Stop+transcript".into(),
            dispatch_run_id: None,
            repo: None,
        }
    }

    fn store_in(dir: &Path) -> UsageStore {
        UsageStore::open(UsagePaths::from_root(dir.join("agent-meter"))).unwrap()
    }

    fn accept(store: &mut UsageStore, event: &UsageEvent) -> IngestResult {
        let bytes = serde_json::to_vec(event).unwrap();
        store.accept_payload(&bytes, None).unwrap()
    }

    #[test]
    fn duplicates_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let event = event_with(
            "homelabdesktop",
            "claude",
            "session-1",
            1,
            "2026-11-12T15:00:00Z",
            Some(10),
            Some(2),
        );
        let first = accept(&mut store, &event);
        let second = accept(&mut store, &event);
        assert_eq!(
            first,
            IngestResult {
                accepted: 1,
                duplicates: 0
            }
        );
        assert_eq!(
            second,
            IngestResult {
                accepted: 0,
                duplicates: 1
            }
        );
        let report = store.query(None, None);
        assert_eq!(report.sessions, 1);
        assert_eq!(report.totals.input_tokens, Some(10));
    }

    #[test]
    fn higher_seq_replaces_lower_seq() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let low = event_with(
            "homelabdesktop",
            "claude",
            "session-1",
            1,
            "2026-11-12T15:00:00Z",
            Some(10),
            Some(2),
        );
        let mut high = event_with(
            "homelabdesktop",
            "claude",
            "session-1",
            4,
            "2026-11-12T15:05:00Z",
            Some(25),
            Some(9),
        );
        high.models[0].model = "claude-opus-5-5".into();
        assert_eq!(accept(&mut store, &low).accepted, 1);
        assert_eq!(accept(&mut store, &high).accepted, 1);
        let again = accept(&mut store, &low);
        assert_eq!(again.duplicates, 1);
        let report = store.query(None, None);
        assert_eq!(report.sessions, 1);
        assert_eq!(report.totals.input_tokens, Some(25));
        assert_eq!(report.totals.output_tokens, Some(9));
        assert_eq!(store.sessions[0].event.seq, 4);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let event = event_with(
            "homelabdesktop",
            "claude",
            "session-1",
            1,
            "2026-11-12T15:00:00Z",
            Some(1),
            Some(1),
        );
        let mut value = serde_json::to_value(&event).unwrap();
        value["prompt"] = json!("secret prompt text");
        let err = store
            .accept_payload(&serde_json::to_vec(&value).unwrap(), None)
            .unwrap_err();
        assert!(
            matches!(err, AcceptError::Schema(ref msg) if msg.contains("prompt") || msg.contains("forbidden") || msg.contains("unknown"))
        );
        value = serde_json::to_value(&event).unwrap();
        value["models"][0]["message"] = json!("hello");
        let err = store
            .accept_payload(&serde_json::to_vec(&value).unwrap(), None)
            .unwrap_err();
        assert!(matches!(err, AcceptError::Schema(_)));
        assert_eq!(store.query(None, None).sessions, 0);
    }

    #[test]
    fn payloads_over_the_size_limit_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let bytes = vec![b'{'; MAX_EVENT_BYTES + 1];
        let err = store.accept_payload(&bytes, None).unwrap_err();
        assert!(matches!(err, AcceptError::TooLarge { size } if size == MAX_EVENT_BYTES + 1));
        assert_eq!(store.query(None, None).sessions, 0);
    }

    #[test]
    fn null_count_is_not_stored_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let event = event_with(
            "homelabdesktop",
            "codex",
            "session-1",
            1,
            "2026-11-12T15:00:00Z",
            None,
            Some(0),
        );
        accept(&mut store, &event);
        let report = store.query(None, None);
        assert_eq!(report.totals.input_tokens, None);
        assert_eq!(report.totals.output_tokens, Some(0));
        assert_eq!(report.totals.reasoning_tokens, None);
        assert!(report.totals.incomplete.is_empty());
    }

    #[test]
    fn partial_sums_mark_incomplete_instead_of_treating_null_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let mut event = event_with(
            "homelabdesktop",
            "claude",
            "session-1",
            1,
            "2026-11-12T15:00:00Z",
            Some(10),
            Some(1),
        );
        event.models.push(ModelUsage {
            model: "other-model".into(),
            input_tokens: None,
            output_tokens: Some(4),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            requests: None,
            cost_usd: None,
            cost_source: CostSource::None,
        });
        accept(&mut store, &event);
        let report = store.query(None, Some(GroupBy::Model));
        assert_eq!(report.totals.input_tokens, Some(10));
        assert!(report
            .totals
            .incomplete
            .iter()
            .any(|field| field == "input_tokens"));
        assert_eq!(report.groups.len(), 2);
    }

    #[test]
    fn idempotency_key_must_match_canonical_formula() {
        let mut event = event_with(
            "homelabdesktop",
            "claude",
            "session-1",
            3,
            "2026-11-12T15:00:00Z",
            Some(1),
            Some(1),
        );
        event.idempotency_key = "ab".repeat(32);
        let err = parse_payload(&serde_json::to_vec(&event).unwrap()).unwrap_err();
        assert!(matches!(err, UsageReject::Schema(ref msg) if msg.contains("idempotency_key")));
    }

    #[test]
    fn cost_source_must_agree_with_cost() {
        let mut event = event_with(
            "host-a",
            "omp",
            "session-1",
            1,
            "2026-11-12T15:00:00Z",
            Some(1),
            Some(1),
        );
        event.models[0].cost_usd = Some(0.25);
        event.models[0].cost_source = CostSource::None;
        assert!(parse_payload(&serde_json::to_vec(&event).unwrap()).is_err());
        event.models[0].cost_source = CostSource::Harness;
        assert!(parse_payload(&serde_json::to_vec(&event).unwrap()).is_ok());
    }

    #[test]
    fn inbox_drain_works_with_the_listener_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let inbox = store.inbox_dir();
        fs::create_dir_all(&inbox).unwrap();
        let good = event_with(
            "homelabdesktop",
            "grok",
            "session-9",
            2,
            "2026-11-12T15:00:00Z",
            Some(80),
            Some(7),
        );
        fs::write(inbox.join("good.json"), serde_json::to_vec(&good).unwrap()).unwrap();
        let mut bad = serde_json::to_value(&good).unwrap();
        bad["diff"] = json!("--- a/src/main.rs");
        fs::write(inbox.join("bad.json"), serde_json::to_vec(&bad).unwrap()).unwrap();
        fs::write(inbox.join("huge.json"), vec![b'x'; MAX_EVENT_BYTES + 8]).unwrap();
        let stats = store.drain_inbox().unwrap();
        assert_eq!(stats.seen, 3);
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.rejected, 2);
        assert!(!inbox.join("good.json").exists());
        assert!(store.paths.processed().join("good.json").exists());
        assert!(store.paths.rejected().join("bad.json").exists());
        assert!(store.paths.rejected().join("bad.reason").exists());
        let report = store.query(None, Some(GroupBy::Harness));
        assert_eq!(report.sessions, 1);
        assert_eq!(report.groups[0].key, "grok");
        assert_eq!(report.groups[0].totals.input_tokens, Some(80));
        let again = store.drain_inbox().unwrap();
        assert_eq!(again.seen, 0);
    }

    #[test]
    fn batch_counts_duplicates_without_double_counting() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let first = event_with(
            "mac-worker",
            "codex",
            "session-1",
            1,
            "2026-11-12T12:00:00Z",
            Some(5),
            Some(1),
        );
        let second = event_with(
            "mac-worker",
            "codex",
            "session-1",
            2,
            "2026-11-12T12:05:00Z",
            Some(9),
            Some(3),
        );
        let payload = json!({"events": [first, second.clone(), second]});
        let result = store
            .accept_payload(&serde_json::to_vec(&payload).unwrap(), None)
            .unwrap();
        assert_eq!(
            result,
            IngestResult {
                accepted: 2,
                duplicates: 1
            }
        );
        assert_eq!(store.query(None, None).totals.input_tokens, Some(9));
    }

    #[test]
    fn schema_error_in_a_batch_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let good = event_with(
            "host-a",
            "claude",
            "session-1",
            1,
            "2026-11-12T12:00:00Z",
            Some(5),
            Some(1),
        );
        let mut bad = serde_json::to_value(&good).unwrap();
        bad["session_id"] = json!("session-2");
        bad["seq"] = json!(1);
        bad["extra"] = json!(true);
        let payload = json!({"events": [good, bad]});
        let err = store
            .accept_payload(&serde_json::to_vec(&payload).unwrap(), None)
            .unwrap_err();
        assert!(matches!(err, AcceptError::Schema(_)));
        assert_eq!(store.query(None, None).sessions, 0);
    }

    #[test]
    fn since_and_group_by_host() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        accept(
            &mut store,
            &event_with(
                "host-a",
                "claude",
                "old",
                1,
                "2026-11-01T00:00:00Z",
                Some(100),
                Some(1),
            ),
        );
        accept(
            &mut store,
            &event_with(
                "host-b",
                "codex",
                "new",
                1,
                "2026-11-12T15:00:00Z",
                Some(4),
                Some(2),
            ),
        );
        let since = parse_since(
            "24h",
            DateTime::parse_from_rfc3339("2026-11-12T16:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .unwrap();
        let report = store.query(Some(since), Some(GroupBy::Host));
        assert_eq!(report.sessions, 1);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].key, "host-b");
        assert_eq!(report.totals.input_tokens, Some(4));
    }

    #[test]
    fn session_cap_drops_the_oldest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        store.limits.max_sessions = 2;
        for (index, session) in ["s-old", "s-mid", "s-new"].iter().enumerate() {
            let stamp = format!("2026-11-12T15:0{index}:00Z");
            accept(
                &mut store,
                &event_with(
                    "host-a",
                    "agy",
                    session,
                    1,
                    &stamp,
                    Some(index as u64),
                    Some(1),
                ),
            );
        }
        let report = store.query(None, None);
        assert_eq!(report.sessions, 2);
        let ids: HashSet<_> = store
            .sessions
            .iter()
            .map(|session| session.event.session_id.as_str())
            .collect();
        assert!(!ids.contains("s-old"));
        assert!(ids.contains("s-new"));
    }

    #[test]
    fn log_compacts_to_the_latest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        store.limits.max_jsonl_lines = 2;
        for seq in 1..=3 {
            accept(
                &mut store,
                &event_with(
                    "host-a",
                    "muse",
                    "only",
                    seq,
                    "2026-11-12T15:00:00Z",
                    Some(seq * 10),
                    Some(1),
                ),
            );
        }
        let log = fs::read_to_string(store.paths.events_log()).unwrap();
        let lines: Vec<_> = log.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"seq\":3"));
        assert_eq!(store.query(None, None).totals.input_tokens, Some(30));
    }

    #[test]
    fn checked_in_schema_example_validates() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/usage-event.v1.json");
        let document: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let example = &document["examples"][0];
        let events = parse_payload(&serde_json::to_vec(example).unwrap()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].host, "homelabdesktop");
        assert_eq!(events[0].models[0].reasoning_tokens, None);
        assert_eq!(events[0].models[0].cache_write_tokens, Some(0));
        assert!(document["additionalProperties"] == json!(false));
    }

    #[test]
    fn today_line_uses_the_callers_calendar_day() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        accept(
            &mut store,
            &event_with(
                "homelabdesktop",
                "claude",
                "session-1",
                1,
                "2026-11-12T15:00:00Z",
                Some(1200),
                Some(340),
            ),
        );
        let now = DateTime::parse_from_rfc3339("2026-11-12T18:00:00Z").unwrap();
        store.publish_today(now).unwrap();
        let label = today_label_from_file(
            &store.paths.today_path(),
            NaiveDate::from_ymd_opt(2026, 11, 12).unwrap(),
        )
        .unwrap();
        assert_eq!(label, "Today: 1,200 in · 340 out · 1 session");
        assert!(today_label_from_file(
            &store.paths.today_path(),
            NaiveDate::from_ymd_opt(2026, 11, 13).unwrap()
        )
        .is_none());
    }

    #[test]
    fn listener_refuses_unspecified_addresses_without_listen_any() {
        let err = parse_listen_addr("0.0.0.0:8787", false).unwrap_err();
        assert!(err.to_string().contains("0.0.0.0"));
        assert!(err.to_string().contains("listen-any"));
        assert!(parse_listen_addr("0.0.0.0:8787", true).is_ok());
        let v6 = parse_listen_addr("[::]:8787", false).unwrap_err();
        assert!(v6.to_string().contains("listen-any"));
        assert!(parse_listen_addr("127.0.0.1:8787", false).is_ok());
    }

    #[test]
    fn listener_refuses_to_start_without_a_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("ingest-tokens.json");
        let store = Arc::new(Mutex::new(store_in(dir.path())));
        let err = serve("127.0.0.1:0", false, &missing, store).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("token") || message.contains("ingest"),
            "{message}"
        );
    }

    fn fake_token(host: &str) -> String {
        format!("unit-test-not-a-secret-{host}")
    }

    fn write_token_file(path: &Path, hosts: &[&str]) {
        let mut map = serde_json::Map::new();
        for host in hosts {
            map.insert(host.to_string(), json!(token_sha256_hex(&fake_token(host))));
        }
        let document = json!({
            "schema": TOKEN_SCHEMA,
            "hosts": Value::Object(map),
        });
        fs::write(path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    }

    fn http_exchange(
        addr: SocketAddr,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(token) = token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
        (status, body)
    }

    #[test]
    fn listener_returns_401_for_a_wrong_host_token_pair() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("ingest-tokens.json");
        write_token_file(&tokens, &["host-a", "host-b"]);
        let store = Arc::new(Mutex::new(store_in(dir.path())));
        let listener = serve("127.0.0.1:0", false, &tokens, Arc::clone(&store)).unwrap();

        let (status, body) = http_exchange(listener.addr, "GET", "/v1/usage/healthz", None, "");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("true"));

        let event_b = event_with(
            "host-b",
            "claude",
            "session-1",
            1,
            "2026-11-12T15:00:00Z",
            Some(12),
            Some(3),
        );
        let raw = serde_json::to_string(&event_b).unwrap();
        let (status, body) = http_exchange(
            listener.addr,
            "POST",
            "/v1/usage/events",
            Some(&fake_token("host-a")),
            &raw,
        );
        assert_eq!(status, 401, "{body}");

        let (status, _) = http_exchange(
            listener.addr,
            "POST",
            "/v1/usage/events",
            Some("unit-test-not-a-secret-unknown"),
            &raw,
        );
        assert_eq!(status, 401);

        let (status, body) = http_exchange(
            listener.addr,
            "POST",
            "/v1/usage/events",
            Some(&fake_token("host-b")),
            &raw,
        );
        assert_eq!(status, 202, "{body}");
        let accepted: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(accepted["accepted"], 1);
        assert_eq!(accepted["duplicates"], 0);

        let (status, body) = http_exchange(
            listener.addr,
            "POST",
            "/v1/usage/events",
            Some(&fake_token("host-b")),
            &raw,
        );
        assert_eq!(status, 202, "{body}");
        let accepted: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(accepted["duplicates"], 1);

        let report = store.lock().unwrap().query(None, Some(GroupBy::Host));
        assert_eq!(report.sessions, 1);
        assert_eq!(report.totals.input_tokens, Some(12));
        drop(listener);
    }

    #[test]
    fn oversized_content_length_is_rejected_before_auth_can_store_it() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("ingest-tokens.json");
        write_token_file(&tokens, &["host-a"]);
        let store = Arc::new(Mutex::new(store_in(dir.path())));
        let listener = serve("127.0.0.1:0", false, &tokens, Arc::clone(&store)).unwrap();
        let (status, body) = http_exchange(
            listener.addr,
            "POST",
            "/v1/usage/events",
            Some(&fake_token("host-a")),
            "",
        );
        // Empty body with Content-Length 0 is a schema error, not a store write.
        // The dedicated oversized header is covered by rewriting Content-Length below.
        assert!(status == 400 || status == 401, "{status} {body}");
        let mut stream = TcpStream::connect(listener.addr).unwrap();
        let request = format!(
            "POST /v1/usage/events HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            MAX_EVENT_BYTES + 1,
            fake_token("host-a")
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert_eq!(store.lock().unwrap().query(None, None).sessions, 0);
        drop(listener);
    }

    #[test]
    fn plaintext_token_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ingest-tokens.json");
        fs::write(
            &path,
            r#"{"schema":"agent-meter.ingest-tokens/v1","hosts":{"host-a":"not-a-hash"}}"#,
        )
        .unwrap();
        let err = load_ingest_tokens(&path).unwrap_err();
        assert!(err.to_string().contains("sha256"));
    }
}
