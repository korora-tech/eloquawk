//! HTTP plumbing shared by the Gmail endpoints (SPEC 6.1): the ureq agent, injectable
//! [`Endpoints`], per-request timeouts and body limits, the `get` / `post_*` helpers, and the
//! classification of Google's errors into [`ApiError`] with retry hints.
//!
//! Nothing here logs. Errors carry a status, a kind or Google's own message, never a request
//! URL: a search query in a query string is mail data.

use std::fmt;
use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// The limit for full threads and attachments: a 50 MB attachment is about 67 MB of base64
/// inside JSON.
pub const BODY_LIMIT_LARGE: u64 = 96 * 1024 * 1024;
/// The limit for every other response.
pub const BODY_LIMIT: u64 = 8 * 1024 * 1024;
/// The default request timeout, also the agent's global one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The ceiling for a size-scaled timeout.
const MAX_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// A `RateLimited` hint longer than this is a quota problem, not a burst.
const LONGEST_RATE_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Where the worker sends its requests. Tests point every field at a loopback fake server.
///
/// M9 note: SPEC 6.1 names `base`, `upload` and `token`; `revoke` is added so that signing an
/// account out in a test never reaches Google.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// The Gmail API, up to and including `/users/me`.
    pub base: String,
    /// The Gmail upload API, up to and including `/users/me`.
    pub upload: String,
    /// The OAuth token endpoint (exchange and refresh).
    pub token: String,
    /// The OAuth revocation endpoint.
    pub revoke: String,
}

impl Endpoints {
    pub fn google() -> Self {
        Self {
            base: "https://gmail.googleapis.com/gmail/v1/users/me".into(),
            upload: "https://gmail.googleapis.com/upload/gmail/v1/users/me".into(),
            token: super::oauth::TOKEN_URL.into(),
            revoke: super::oauth::REVOKE_URL.into(),
        }
    }
}

/// What kind of transport failure happened; never the URL or the full error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetKind {
    Timeout,
    Connect,
    Tls,
    Io,
    Other,
}

impl fmt::Display for NetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NetKind::Timeout => "the request timed out",
            NetKind::Connect => "could not connect",
            NetKind::Tls => "a secure connection could not be set up",
            NetKind::Io => "the connection failed",
            NetKind::Other => "the request failed",
        })
    }
}

/// Classifies a ureq transport error.
pub fn net_kind(e: &ureq::Error) -> NetKind {
    match e {
        ureq::Error::Timeout(_) => NetKind::Timeout,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => NetKind::Connect,
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) => NetKind::Tls,
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => NetKind::Timeout,
        ureq::Error::Io(_) => NetKind::Io,
        _ => NetKind::Other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The access token was rejected; refresh and retry once.
    Unauthorized,
    Forbidden(String),
    NotFound,
    /// Slow down; `retry_after` is Google's hint when it gave one.
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// A daily or sending limit, with Google's message.
    QuotaExhausted(String),
    Network(NetKind),
    Http {
        status: u16,
        body: String,
    },
    /// The response exceeded its body limit.
    TooLarge,
    Decode(String),
    #[allow(dead_code)] // consumed in M11
    InvalidRequest(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Google rejected the access token"),
            ApiError::Forbidden(m) => write!(f, "Access denied: {m}"),
            ApiError::NotFound => write!(f, "Not found in Gmail"),
            ApiError::RateLimited { .. } => write!(f, "Gmail asked Eloquawk to slow down"),
            ApiError::QuotaExhausted(m) => write!(f, "Gmail limit reached: {m}"),
            ApiError::Network(kind) => write!(f, "Network error: {kind}"),
            ApiError::Http { status, body } => write!(f, "Gmail returned HTTP {status}: {body}"),
            ApiError::TooLarge => write!(f, "The response was too large to download"),
            ApiError::Decode(m) => write!(f, "Unexpected response from Gmail: {m}"),
            ApiError::InvalidRequest(m) => write!(f, "{m}"),
        }
    }
}

impl ApiError {
    /// Whether the token lacks the Gmail scope (granular consent unticked, or an old grant).
    pub fn missing_scope(&self) -> bool {
        matches!(
            self,
            ApiError::Forbidden(message)
                if {
                    let message = message.to_ascii_lowercase();
                    message.contains("insufficient authentication scopes")
                        || message.contains("insufficientpermissions")
                        || message.contains("access_token_scope_insufficient")
                }
        )
    }
}

/// The agent every Gmail and OAuth request goes through: statuses are returned, not raised,
/// and the global timeout is [`DEFAULT_TIMEOUT`].
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(DEFAULT_TIMEOUT))
        .user_agent(concat!("eloquawk/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// How long to wait for, and how much to read of, one response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub body: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            body: BODY_LIMIT,
        }
    }
}

impl Limits {
    /// Sends, attachments and full fetches: 30 s plus 1 s per 100 KiB of `expected_bytes`,
    /// capped at 10 minutes, with the large body limit.
    pub fn large(expected_bytes: u64) -> Self {
        Self {
            timeout: scaled_timeout(expected_bytes),
            body: BODY_LIMIT_LARGE,
        }
    }
}

/// 30 s plus 1 s per 100 KiB, capped at 10 minutes.
pub fn scaled_timeout(bytes: u64) -> Duration {
    (DEFAULT_TIMEOUT + Duration::from_secs(bytes / (100 * 1024))).min(MAX_TIMEOUT)
}

/// Google's error envelope message, with the first reason when there is one; otherwise the
/// start of the body.
pub fn error_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct Envelope {
        error: Inner,
    }
    #[derive(Deserialize)]
    struct Inner {
        message: String,
        #[serde(default)]
        errors: Vec<Detail>,
    }
    #[derive(Deserialize)]
    struct Detail {
        reason: Option<String>,
    }
    serde_json::from_str::<Envelope>(body)
        .map(|e| {
            let reason = e.error.errors.into_iter().find_map(|detail| detail.reason);
            match reason {
                Some(reason) => format!("{} ({reason})", e.error.message),
                None => e.error.message,
            }
        })
        .unwrap_or_else(|_| body.chars().take(200).collect())
}

/// A `Retry-After` header: delta-seconds or an HTTP-date.
pub fn parse_retry_after(value: &str, now: Timestamp) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(value)
        .ok()?;
    Some(until(at, now))
}

/// `Retry after <RFC 3339 timestamp>` inside an error envelope (Gmail's per-user limits).
pub fn envelope_retry_after(body: &str, now: Timestamp) -> Option<Duration> {
    const NEEDLE: &str = "retry after ";
    // ASCII lowercasing keeps byte offsets, so the match indexes the original text.
    let at = body.to_ascii_lowercase().find(NEEDLE)? + NEEDLE.len();
    let token: String = body[at..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '"' && *c != ',')
        .collect();
    let token = token.trim_end_matches(['.', ')', ';']);
    let ts: Timestamp = token.to_ascii_uppercase().parse().ok()?;
    Some(until(ts, now))
}

fn until(at: Timestamp, now: Timestamp) -> Duration {
    let secs = at.as_second().saturating_sub(now.as_second()).max(0);
    Duration::from_secs(secs.unsigned_abs())
}

/// Maps a non-success status (with its `Retry-After` header and body) to an [`ApiError`].
pub fn classify(status: u16, retry_after: Option<&str>, body: &str, now: Timestamp) -> ApiError {
    let lower = body.to_ascii_lowercase();
    let rate_limited = || {
        let hint = retry_after
            .and_then(|v| parse_retry_after(v, now))
            .or_else(|| envelope_retry_after(body, now));
        match hint {
            Some(d) if d > LONGEST_RATE_LIMIT => ApiError::QuotaExhausted(error_message(body)),
            _ => ApiError::RateLimited { retry_after: hint },
        }
    };
    match status {
        401 => ApiError::Unauthorized,
        404 | 410 => ApiError::NotFound,
        429 => rate_limited(),
        403 if lower.contains("ratelimitexceeded") => rate_limited(),
        403 if lower.contains("dailylimitexceeded")
            || lower.contains("sending limit")
            || lower.contains("sending quota") =>
        {
            ApiError::QuotaExhausted(error_message(body))
        }
        403 => ApiError::Forbidden(error_message(body)),
        _ => ApiError::Http {
            status,
            body: error_message(body),
        },
    }
}

type Response = ureq::http::Response<ureq::Body>;

/// Reads a response: the status, the `Retry-After` header and the body within `limit`.
fn read(result: Result<Response, ureq::Error>, limit: u64) -> Result<(u16, String), ApiError> {
    let mut resp = result.map_err(|e| ApiError::Network(net_kind(&e)))?;
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = match resp.body_mut().with_config().limit(limit).read_to_string() {
        Ok(body) => body,
        Err(ureq::Error::BodyExceedsLimit(_)) => return Err(ApiError::TooLarge),
        Err(e) => return Err(ApiError::Network(net_kind(&e))),
    };
    if (200..300).contains(&status) {
        Ok((status, body))
    } else {
        Err(classify(
            status,
            retry_after.as_deref(),
            &body,
            Timestamp::now(),
        ))
    }
}

fn decode<T: DeserializeOwned>(body: &str) -> Result<T, ApiError> {
    serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
}

/// `url` with `query` appended. An unparseable URL is a programming error in the endpoint
/// table; it is reported as a request error rather than a panic.
fn with_query(url: &str, query: &[(&str, String)]) -> Result<url::Url, ApiError> {
    let mut url = url::Url::parse(url)
        .map_err(|_| ApiError::InvalidRequest("The Gmail address is invalid.".into()))?;
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (k, v) in query {
            pairs.append_pair(k, v);
        }
    }
    Ok(url)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// `GET url?query` with a bearer token, decoding JSON.
pub fn get<T: DeserializeOwned>(
    agent: &ureq::Agent,
    token: &str,
    url: &str,
    query: &[(&str, String)],
    limits: Limits,
) -> Result<T, ApiError> {
    let url = with_query(url, query)?;
    let result = agent
        .get(url.as_str())
        .header("Authorization", &bearer(token))
        .config()
        .timeout_global(Some(limits.timeout))
        .build()
        .call();
    read(result, limits.body).and_then(|(_, body)| decode(&body))
}

/// `POST url` with a JSON body, decoding a JSON response.
#[allow(dead_code)] // consumed in M11
pub fn post_json<B: serde::Serialize, T: DeserializeOwned>(
    agent: &ureq::Agent,
    token: &str,
    url: &str,
    body: &B,
    limits: Limits,
) -> Result<T, ApiError> {
    let result = agent
        .post(url)
        .header("Authorization", &bearer(token))
        .config()
        .timeout_global(Some(limits.timeout))
        .build()
        .send_json(body);
    read(result, limits.body).and_then(|(_, body)| decode(&body))
}

/// `POST url` with a JSON body, accepting any 2xx including 204 with an empty body.
#[allow(dead_code)] // consumed in M11
pub fn post_json_no_content<B: serde::Serialize>(
    agent: &ureq::Agent,
    token: &str,
    url: &str,
    body: &B,
) -> Result<(), ApiError> {
    let result = agent
        .post(url)
        .header("Authorization", &bearer(token))
        .send_json(body);
    read(result, BODY_LIMIT).map(|_| ())
}

/// `POST url` with no body (trash, untrash), decoding a JSON response.
#[allow(dead_code)] // consumed in M11
pub fn post_empty<T: DeserializeOwned>(
    agent: &ureq::Agent,
    token: &str,
    url: &str,
) -> Result<T, ApiError> {
    let result = agent
        .post(url)
        .header("Authorization", &bearer(token))
        .send_empty();
    read(result, BODY_LIMIT).and_then(|(_, body)| decode(&body))
}

/// A `multipart/related` body from `(content type, bytes)` parts and its boundary. The
/// boundary is random and checked against every part, so content can never end a part early.
fn multipart_body(parts: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = loop {
        let candidate = format!("eloquawk-{}", super::oauth::random_token(18));
        let needle = candidate.as_bytes();
        if !parts
            .iter()
            .any(|(_, bytes)| bytes.windows(needle.len()).any(|w| w == needle))
        {
            break candidate;
        }
    };
    let mut body = Vec::new();
    for (content_type, bytes) in parts {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: {content_type}\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

/// `POST url?query` with a `multipart/related` body (the upload send), decoding JSON.
#[allow(dead_code)] // consumed in M11
pub fn post_multipart<T: DeserializeOwned>(
    agent: &ureq::Agent,
    token: &str,
    url: &str,
    query: &[(&str, String)],
    parts: &[(&str, &[u8])],
    limits: Limits,
) -> Result<T, ApiError> {
    let url = with_query(url, query)?;
    let (boundary, body) = multipart_body(parts);
    let result = agent
        .post(url.as_str())
        .header("Authorization", &bearer(token))
        .header(
            "Content-Type",
            &format!("multipart/related; boundary={boundary}"),
        )
        .config()
        .timeout_global(Some(limits.timeout))
        .build()
        .send(&body[..]);
    read(result, limits.body).and_then(|(_, body)| decode(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::fake::{FakeServer, Reply};

    fn now() -> Timestamp {
        "2026-10-29T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn statuses_classify() {
        assert_eq!(classify(401, None, "", now()), ApiError::Unauthorized);
        assert_eq!(classify(404, None, "", now()), ApiError::NotFound);
        assert_eq!(
            classify(429, None, "", now()),
            ApiError::RateLimited { retry_after: None }
        );
        for status in [500, 502, 503, 504] {
            assert!(matches!(
                classify(status, None, "{\"error\":{\"message\":\"Backend Error\"}}", now()),
                ApiError::Http { status: s, ref body } if s == status && body == "Backend Error"
            ));
        }
        assert!(matches!(
            classify(400, None, "{\"error\":{\"message\":\"Invalid\"}}", now()),
            ApiError::Http { status: 400, .. }
        ));
    }

    #[test]
    fn forbidden_limits_classify_case_insensitively() {
        let user = r#"{"error":{"message":"User-rate limit exceeded.","errors":[{"reason":"userRateLimitExceeded"}]}}"#;
        assert_eq!(
            classify(403, None, user, now()),
            ApiError::RateLimited { retry_after: None }
        );
        let shouty = r#"{"error":{"message":"x","errors":[{"reason":"RATELIMITEXCEEDED"}]}}"#;
        assert!(matches!(
            classify(403, None, shouty, now()),
            ApiError::RateLimited { .. }
        ));
        let daily = r#"{"error":{"message":"Daily Limit Exceeded","errors":[{"reason":"dailyLimitExceeded"}]}}"#;
        assert_eq!(
            classify(403, None, daily, now()),
            ApiError::QuotaExhausted("Daily Limit Exceeded (dailyLimitExceeded)".into())
        );
        let sending = r#"{"error":{"message":"Daily user Sending limit exceeded."}}"#;
        assert!(matches!(
            classify(403, None, sending, now()),
            ApiError::QuotaExhausted(_)
        ));
        assert!(matches!(
            classify(403, None, r#"{"error":{"message":"nope"}}"#, now()),
            ApiError::Forbidden(m) if m == "nope"
        ));
    }

    #[test]
    fn missing_scope_is_told_apart() {
        let e = classify(
            403,
            None,
            r#"{"error":{"message":"Request had insufficient authentication scopes.","errors":[{"reason":"insufficientPermissions"}]}}"#,
            now(),
        );
        assert!(e.missing_scope());
        assert!(!ApiError::Forbidden("Delegation denied".into()).missing_scope());
    }

    #[test]
    fn retry_after_header_seconds_and_http_date() {
        assert_eq!(
            parse_retry_after("120", now()),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Thu, 29 Oct 2026 12:01:30 GMT", now()),
            Some(Duration::from_secs(90))
        );
        // A date in the past means "now".
        assert_eq!(
            parse_retry_after("Thu, 29 Oct 2026 11:00:00 GMT", now()),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now()), None);
        assert_eq!(
            classify(429, Some("7"), "", now()),
            ApiError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
    }

    #[test]
    fn retry_after_from_the_envelope() {
        let body = include_str!("fixtures/error_rate_limited.json");
        assert_eq!(
            envelope_retry_after(body, now()),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            classify(429, None, body, now()),
            ApiError::RateLimited {
                retry_after: Some(Duration::from_secs(45))
            }
        );
        // The header wins over the envelope.
        assert_eq!(
            classify(429, Some("3"), body, now()),
            ApiError::RateLimited {
                retry_after: Some(Duration::from_secs(3))
            }
        );
    }

    #[test]
    fn long_rate_limits_become_quota_exhausted() {
        assert!(matches!(
            classify(429, Some("901"), r#"{"error":{"message":"slow"}}"#, now()),
            ApiError::QuotaExhausted(m) if m == "slow"
        ));
        assert!(matches!(
            classify(429, Some("900"), "", now()),
            ApiError::RateLimited { .. }
        ));
    }

    #[test]
    fn envelope_message_parsing() {
        assert_eq!(
            error_message(r#"{"error":{"code":400,"message":"Bad","errors":[{"reason":"x"}]}}"#),
            "Bad (x)"
        );
        assert_eq!(error_message(r#"{"error":{"message":"Plain"}}"#), "Plain");
        assert_eq!(error_message("not json"), "not json");
        assert_eq!(error_message(&"y".repeat(500)).len(), 200);
    }

    #[test]
    fn timeouts_scale_with_size_and_cap() {
        assert_eq!(scaled_timeout(0), Duration::from_secs(30));
        assert_eq!(scaled_timeout(2 * 1024 * 1024), Duration::from_secs(50));
        assert_eq!(scaled_timeout(u64::MAX / 2), Duration::from_secs(600));
    }

    #[test]
    fn multipart_parts_are_framed() {
        let (boundary, body) = multipart_body(&[
            ("application/json; charset=UTF-8", b"{}"),
            ("message/rfc822", b"Subject: x\r\n\r\nhi"),
        ]);
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{{}}\r\n"
        )));
        assert!(text.contains("Content-Type: message/rfc822\r\n\r\nSubject: x\r\n\r\nhi\r\n"));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn get_sends_bearer_and_query_then_decodes() {
        let server = FakeServer::start(vec![Reply::json(200, r#"{"value":7}"#)]);
        #[derive(Deserialize)]
        struct V {
            value: u32,
        }
        let v: V = get(
            &agent(),
            "tok",
            &server.url("/x"),
            &[("q", "a b".into())],
            Limits::default(),
        )
        .unwrap();
        assert_eq!(v.value, 7);
        let reqs = server.finish();
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].target, "/x?q=a+b");
        assert_eq!(
            reqs[0].header("authorization").as_deref(),
            Some("Bearer tok")
        );
    }

    #[test]
    fn errors_and_limits_over_http() {
        let server = FakeServer::start(vec![
            Reply::json(429, "{}").with_header("Retry-After", "12"),
            Reply::json(200, &format!("\"{}\"", "a".repeat(64))),
            Reply::json(204, ""),
        ]);
        let a = agent();
        let url = server.url("/y");
        assert_eq!(
            get::<serde_json::Value>(&a, "t", &url, &[], Limits::default()),
            Err(ApiError::RateLimited {
                retry_after: Some(Duration::from_secs(12))
            })
        );
        let tight = Limits {
            timeout: DEFAULT_TIMEOUT,
            body: 16,
        };
        assert_eq!(
            get::<serde_json::Value>(&a, "t", &url, &[], tight),
            Err(ApiError::TooLarge)
        );
        assert_eq!(
            post_json_no_content(&a, "t", &url, &serde_json::json!({"a": 1})),
            Ok(())
        );
        let reqs = server.finish();
        assert_eq!(reqs[2].method, "POST");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reqs[2].body).unwrap(),
            serde_json::json!({"a": 1})
        );
    }

    #[test]
    fn network_failures_carry_only_a_kind() {
        // Nothing listens on a port that was just released.
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/secret?q=from:someone");
        let err =
            get::<serde_json::Value>(&agent(), "t", &url, &[], Limits::default()).unwrap_err();
        assert!(matches!(err, ApiError::Network(_)));
        assert!(!err.to_string().contains("secret"));
    }
}
