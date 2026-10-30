//! OpenRouter plumbing for the AI checks (SPEC 16.5, 16.6.6, 16.14): the ureq agent, the
//! endpoints, the API key type, the two calls (`post_decisions`, `get_key`) and the pure
//! classification of OpenRouter's errors into [`AiError`].
//!
//! Logging follows SPEC 16.2: only the HTTP status, OpenRouter's `error.code`,
//! `error.metadata.reason` / `limit_source` and a network error kind. Never `error.message`
//! (it can echo input), a body, the key or a host.

use std::fmt;
use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;

use crate::google::api::{NetKind, net_kind, parse_retry_after};

/// The request timeout, also the agent's global one.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// The most of a response body that is read.
pub const BODY_LIMIT: u64 = 1024 * 1024;
/// The attribution headers' values (SPEC 16.6.6).
const REFERER: &str = "https://github.com/korora-tech/eloquawk";
const TITLE: &str = "Eloquawk";

/// Where the AI worker sends its requests. Only [`AiEndpoints::openrouter`] exists outside
/// tests: no variable, pref or flag can redirect the key or the mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiEndpoints {
    decisions: String,
    key: String,
}

impl AiEndpoints {
    pub fn openrouter() -> Self {
        Self {
            decisions: "https://openrouter.ai/api/alpha/decisions".into(),
            key: "https://openrouter.ai/api/v1/key".into(),
        }
    }

    /// Endpoints on a loopback test server.
    #[cfg(test)]
    pub fn new(decisions: String, key: String) -> Self {
        Self { decisions, key }
    }
}

/// An OpenRouter API key. `Debug` and `Display` show only its last four characters, it is not
/// `Clone`, and its bytes are overwritten with zeros when it is dropped (best effort: earlier
/// reallocations of the input are not covered).
pub struct ApiKey(String);

/// Why a typed key was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    Prefix,
    Length,
    Characters,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(KEY_INVALID)
    }
}

/// The inline error Settings shows for a key that fails [`ApiKey::parse`].
pub const KEY_INVALID: &str =
    "That doesn't look like an OpenRouter key (it should start with sk-or-)";

impl ApiKey {
    /// Validates a typed or stored key: trimmed, starting `sk-or-`, 20 to 256 characters, with
    /// no whitespace or control characters.
    pub fn parse(raw: &str) -> Result<Self, KeyError> {
        let key = raw.trim();
        if !key.starts_with("sk-or-") {
            return Err(KeyError::Prefix);
        }
        if !(20..=256).contains(&key.chars().count()) {
            return Err(KeyError::Length);
        }
        if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(KeyError::Characters);
        }
        Ok(Self(key.to_string()))
    }

    /// `sk-or-…abcd`: all the UI ever holds of a key.
    pub fn hint(&self) -> String {
        format!("sk-or-…{}", self.last4())
    }

    fn last4(&self) -> &str {
        let n = self.0.len();
        let mut start = n.saturating_sub(4);
        while !self.0.is_char_boundary(start) {
            start -= 1;
        }
        &self.0[start..]
    }

    /// The key itself, for the `Authorization` header and the keyring only.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ApiKey(…{})", self.last4())
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ApiKey(…{})", self.last4())
    }
}

impl Drop for ApiKey {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        // Keeps the compiler from eliding the writes to a buffer about to be freed.
        std::hint::black_box(&mut bytes);
    }
}

/// What went wrong with a request (SPEC 16.14).
#[derive(Clone, Debug, PartialEq)]
pub enum AiError {
    /// A 200 whose answers are missing or out of range.
    Decode,
    /// 400, 415 or 422: the request was refused as it is.
    Invalid,
    /// 401.
    KeyRejected,
    /// Slow down and retry, with OpenRouter's `Retry-After` when it sent one.
    Busy {
        retry_after: Option<Duration>,
    },
    /// 402: the account's credit has run out.
    Credits,
    /// 403 for a key, guardrail or workspace budget, or a permission error.
    Forbidden(String),
    /// 403 from moderation, or without a limit source: this message only.
    Flagged,
    /// No endpoint meets the provider constraints, a 404, or a redirect.
    Unavailable,
    /// 413.
    TooLarge,
    Network(NetKind),
}

impl AiError {
    /// The text the reader and Settings show (SPEC 16.14).
    pub fn user_text(&self) -> String {
        match self {
            AiError::KeyRejected => "OpenRouter rejected this key".into(),
            AiError::Credits => "Your OpenRouter credit has run out".into(),
            AiError::Forbidden(_) => "OpenRouter refused the request (key or budget limit)".into(),
            AiError::Unavailable => {
                "Jev isn't available with zero data retention within Eloquawk's price limit \
                 right now"
                    .into()
            }
            AiError::Network(_) => "Couldn't reach OpenRouter".into(),
            AiError::Decode
            | AiError::Invalid
            | AiError::Busy { .. }
            | AiError::Flagged
            | AiError::TooLarge => "Couldn't check this message".into(),
        }
    }

    /// Whether this error stops every check until the user acts (SPEC 16.14).
    pub fn pauses(&self) -> bool {
        matches!(
            self,
            AiError::KeyRejected | AiError::Credits | AiError::Forbidden(_) | AiError::Unavailable
        )
    }

    /// Whether the same request may succeed later.
    pub fn retryable(&self) -> bool {
        matches!(self, AiError::Busy { .. } | AiError::Network(_))
    }
}

/// OpenRouter's error envelope: `{"error": {"code", "message", "metadata"?}}`. `code` is a
/// number on the live endpoint (checked on 2026-10-30) and a string in some documentation,
/// so either is accepted.
#[derive(Debug, Default, Deserialize)]
struct Envelope {
    #[serde(default)]
    error: ErrorBody,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: serde_json::Value,
    #[serde(default)]
    message: String,
    #[serde(default)]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ErrorBody {
    fn meta_str(&self, key: &str) -> Option<&str> {
        self.metadata.as_ref()?.get(key)?.as_str()
    }

    fn has_meta(&self, key: &str) -> bool {
        self.metadata.as_ref().is_some_and(|m| m.contains_key(key))
    }

    fn code_text(&self) -> String {
        match &self.code {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    }
}

/// The 403 limit sources that pause every check.
const BUDGET_SOURCES: [&str; 3] = [
    "openrouter_key_limit",
    "openrouter_guardrail_budget",
    "openrouter_workspace_budget",
];

/// Whether an error message says no endpoint meets the provider constraints (ZDR, data
/// collection or `max_price`). The exact body is not documented (SPEC 15 item 25), so this
/// matches the words such a message must use. The message is only matched, never logged.
fn no_endpoint(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "no endpoints",
        "no endpoint",
        "no allowed providers",
        "no providers",
    ]
    .iter()
    .any(|w| m.contains(w))
        || [
            "max_price",
            "zdr",
            "data_collection",
            "data policy",
            "zero data retention",
        ]
        .iter()
        .any(|w| m.contains(w))
}

/// Maps a non-2xx response to an [`AiError`]. Pure.
pub fn classify(status: u16, retry_after: Option<&str>, body: &str, now: Timestamp) -> AiError {
    let env: Envelope = serde_json::from_str(body).unwrap_or_default();
    let e = &env.error;
    let retry_after = retry_after.and_then(|v| parse_retry_after(v, now));
    match status {
        300..=399 => AiError::Unavailable,
        400 | 404 if no_endpoint(&e.message) => AiError::Unavailable,
        404 => AiError::Unavailable,
        401 => AiError::KeyRejected,
        402 if e.meta_str("limit_source") == Some("openrouter_in_flight_budget") => {
            AiError::Busy { retry_after }
        }
        402 => AiError::Credits,
        403 => match e.meta_str("limit_source") {
            Some(src) if BUDGET_SOURCES.contains(&src) => AiError::Forbidden(src.to_string()),
            _ if e.code_text().to_ascii_lowercase().contains("permission") => {
                AiError::Forbidden(e.code_text())
            }
            _ => AiError::Flagged,
        },
        413 => AiError::TooLarge,
        429 | 500..=599 => AiError::Busy { retry_after },
        _ => AiError::Invalid,
    }
}

/// The log line for a failed request: status, code, reason and limit source only.
fn log_failure(status: u16, body: &str) {
    let env: Envelope = serde_json::from_str(body).unwrap_or_default();
    let e = &env.error;
    let mut line = format!("eloquawk: jev: HTTP {status}");
    let code = e.code_text();
    if !code.is_empty() {
        line.push_str(&format!(" code {code}"));
    }
    if let Some(reason) = e.meta_str("reason") {
        line.push_str(&format!(" reason {reason}"));
    }
    if let Some(src) = e.meta_str("limit_source") {
        line.push_str(&format!(" limit_source {src}"));
    }
    if e.has_meta("flagged_input") || e.has_meta("reasons") {
        line.push_str(" (moderation)");
    }
    eprintln!("{line}");
}

/// The agent every OpenRouter request goes through: statuses are returned, redirects are never
/// followed (so the bearer key never goes anywhere else), 20 s global timeout.
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("eloquawk/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

type Response = ureq::http::Response<ureq::Body>;

/// Reads a response within [`BODY_LIMIT`]: the body of a 2xx, or the classified error.
fn read(result: Result<Response, ureq::Error>) -> Result<Vec<u8>, AiError> {
    let mut resp = result.map_err(|e| {
        let kind = net_kind(&e);
        eprintln!("eloquawk: jev: network error: {kind}");
        AiError::Network(kind)
    })?;
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = match resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()
    {
        Ok(body) => body,
        Err(ureq::Error::BodyExceedsLimit(_)) => {
            eprintln!("eloquawk: jev: HTTP {status}: the response was too large");
            return Err(AiError::Decode);
        }
        Err(e) => {
            let kind = net_kind(&e);
            eprintln!("eloquawk: jev: network error: {kind}");
            return Err(AiError::Network(kind));
        }
    };
    if (200..300).contains(&status) {
        return Ok(body);
    }
    let text = String::from_utf8_lossy(&body);
    log_failure(status, &text);
    Err(classify(
        status,
        retry_after.as_deref(),
        &text,
        Timestamp::now(),
    ))
}

/// `POST /api/alpha/decisions` with an already serialised body; returns the response body.
pub fn post_decisions(
    agent: &ureq::Agent,
    endpoints: &AiEndpoints,
    key: &ApiKey,
    body: &[u8],
) -> Result<Vec<u8>, AiError> {
    read(
        agent
            .post(&endpoints.decisions)
            .header("Authorization", &format!("Bearer {}", key.expose()))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", REFERER)
            .header("X-OpenRouter-Title", TITLE)
            .send(body),
    )
}

/// `GET /api/v1/key`: the credit left on the key in USD, `None` when it has no limit.
pub fn get_key(
    agent: &ureq::Agent,
    endpoints: &AiEndpoints,
    key: &ApiKey,
) -> Result<Option<f64>, AiError> {
    #[derive(Deserialize)]
    struct KeyInfo {
        data: KeyData,
    }
    #[derive(Deserialize)]
    struct KeyData {
        #[serde(default)]
        limit_remaining: Option<f64>,
    }
    let body = read(
        agent
            .get(&endpoints.key)
            .header("Authorization", &format!("Bearer {}", key.expose()))
            .header("HTTP-Referer", REFERER)
            .header("X-OpenRouter-Title", TITLE)
            .call(),
    )?;
    let info: KeyInfo = serde_json::from_slice(&body).map_err(|_| AiError::Decode)?;
    Ok(info.data.limit_remaining)
}

/// Retry timing (SPEC 16.14), pure: `Retry-After` when given, otherwise
/// `min(1 s · 2^attempt, 30 s)` plus up to a second of jitter.
pub struct Backoff;

/// The longest computed backoff, before jitter.
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

impl Backoff {
    /// The delay before attempt `attempt + 1`, with `jitter_ms` in `0..1000`.
    pub fn delay(attempt: u32, retry_after: Option<Duration>, jitter_ms: u64) -> Duration {
        if let Some(d) = retry_after {
            return d;
        }
        let base = Duration::from_secs(1u64 << attempt.min(5)).min(BACKOFF_CAP);
        base + Duration::from_millis(jitter_ms % 1000)
    }

    /// Random jitter from getrandom, 0 when it is unavailable.
    pub fn jitter() -> u64 {
        let mut b = [0u8; 2];
        match getrandom::fill(&mut b) {
            Ok(()) => u64::from(u16::from_le_bytes(b)) % 1000,
            Err(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-or-v1-0123456789abcdefWXYZ";

    fn now() -> Timestamp {
        "2026-10-30T12:00:00Z".parse().unwrap()
    }

    fn c(status: u16, body: &str) -> AiError {
        classify(status, None, body, now())
    }

    #[test]
    fn classify_covers_every_row() {
        assert_eq!(
            c(
                400,
                r#"{"error":{"message":"[{\"x\":1}]","code":400},"user_id":"u"}"#
            ),
            AiError::Invalid
        );
        assert_eq!(c(422, "{}"), AiError::Invalid);
        assert_eq!(c(415, ""), AiError::Invalid);
        assert_eq!(
            c(401, r#"{"error":{"message":"User not found.","code":401}}"#),
            AiError::KeyRejected
        );
        assert_eq!(
            classify(
                402,
                Some("3"),
                r#"{"error":{"code":402,"message":"x","metadata":{"limit_source":"openrouter_in_flight_budget"}}}"#,
                now()
            ),
            AiError::Busy {
                retry_after: Some(Duration::from_secs(3))
            }
        );
        assert_eq!(
            c(
                402,
                r#"{"error":{"code":402,"message":"Insufficient credits"}}"#
            ),
            AiError::Credits
        );
        for src in BUDGET_SOURCES {
            assert_eq!(
                c(
                    403,
                    &format!(
                        r#"{{"error":{{"code":403,"message":"m","metadata":{{"limit_source":"{src}"}}}}}}"#
                    )
                ),
                AiError::Forbidden(src.into())
            );
        }
        assert_eq!(
            c(
                403,
                r#"{"error":{"code":403,"message":"m","metadata":{"reasons":["x"],"flagged_input":"y"}}}"#
            ),
            AiError::Flagged
        );
        assert_eq!(
            c(403, r#"{"error":{"code":403,"message":"m"}}"#),
            AiError::Flagged
        );
        assert_eq!(
            c(
                403,
                r#"{"error":{"code":"permission_denied","message":"m"}}"#
            ),
            AiError::Forbidden("permission_denied".into())
        );
        assert_eq!(c(404, "{}"), AiError::Unavailable);
        assert_eq!(
            c(
                400,
                r#"{"error":{"code":400,"message":"No endpoints found matching your data policy and max_price"}}"#
            ),
            AiError::Unavailable
        );
        assert_eq!(c(302, ""), AiError::Unavailable);
        assert_eq!(c(413, "{}"), AiError::TooLarge);
        for s in [429, 500, 502, 503, 524, 529] {
            assert_eq!(c(s, "not json"), AiError::Busy { retry_after: None }, "{s}");
        }
        assert!(AiError::Credits.pauses() && AiError::Unavailable.pauses());
        assert!(!AiError::Flagged.pauses() && !AiError::Invalid.pauses());
        assert!(AiError::Network(NetKind::Timeout).retryable());
        assert_eq!(
            AiError::Network(NetKind::Io).user_text(),
            "Couldn't reach OpenRouter"
        );
    }

    #[test]
    fn api_keys_are_validated() {
        assert!(ApiKey::parse(KEY).is_ok());
        assert_eq!(ApiKey::parse(&format!("  {KEY}\n")).unwrap().expose(), KEY);
        assert_eq!(
            ApiKey::parse("sk-proj-0123456789abcdef").unwrap_err(),
            KeyError::Prefix
        );
        assert_eq!(ApiKey::parse("sk-or-short").unwrap_err(), KeyError::Length);
        assert_eq!(
            ApiKey::parse(&format!("sk-or-{}", "a".repeat(260))).unwrap_err(),
            KeyError::Length
        );
        assert_eq!(
            ApiKey::parse("sk-or-v1-0123 456789abcdef").unwrap_err(),
            KeyError::Characters
        );
        assert_eq!(
            ApiKey::parse("sk-or-v1-0123\u{7}456789abcdef").unwrap_err(),
            KeyError::Characters
        );
        assert_eq!(KeyError::Prefix.to_string(), KEY_INVALID);
    }

    #[test]
    fn api_key_debug_never_shows_the_key() {
        let k = ApiKey::parse(KEY).unwrap();
        for s in [format!("{k:?}"), format!("{k}"), k.hint()] {
            assert!(!s.contains("0123456789"), "{s}");
            assert!(s.contains("WXYZ"), "{s}");
        }
        assert_eq!(k.hint(), "sk-or-…WXYZ");
    }

    #[test]
    fn backoff_schedule_cap_and_jitter() {
        assert_eq!(Backoff::delay(0, None, 0), Duration::from_secs(1));
        assert_eq!(Backoff::delay(1, None, 250), Duration::from_millis(2250));
        assert_eq!(Backoff::delay(4, None, 0), Duration::from_secs(16));
        assert_eq!(Backoff::delay(5, None, 0), BACKOFF_CAP);
        assert_eq!(
            Backoff::delay(40, None, 999),
            BACKOFF_CAP + Duration::from_millis(999)
        );
        assert_eq!(
            Backoff::delay(3, Some(Duration::from_secs(7)), 500),
            Duration::from_secs(7)
        );
        for _ in 0..50 {
            assert!(Backoff::jitter() < 1000);
        }
    }

    #[test]
    fn the_agent_never_follows_redirects() {
        let a = agent();
        assert_eq!(a.config().max_redirects(), 0);
        assert!(!a.config().http_status_as_error());
        assert_eq!(
            AiEndpoints::openrouter().decisions,
            "https://openrouter.ai/api/alpha/decisions"
        );
    }
}
