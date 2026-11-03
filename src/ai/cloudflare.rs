//! Cloudflare Workers AI plumbing for checks with Clef and Clef-flash: the request body, the
//! call, the response envelope, the cost and the classification of Cloudflare's errors into
//! [`AiError`].
//!
//! From Cloudflare's model docs (2026-11-03; the envelope is not yet live-checked): `POST
//! /client/v4/accounts/{account_id}/ai/run/@cf/cloudflare/clef` (or `clef-flash`) with
//! `{"model": "clef", "state", "questions"}`, answering `{"model", "answers", "usage":
//! {"input_tokens", "output_tokens"}}`, which the REST API wraps in `{"result", "success",
//! "errors", "messages"}`. Answers use the same `noul` shape as Jev. Input costs $0.24 per
//! million tokens and output is free.
//!
//! Logging follows SPEC 16.2: only the HTTP status, Cloudflare's `errors[].code` and a network
//! error kind. Never `errors[].message`, a body, the token, the account ID or a host.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::AiBackend;
use super::jev::{DecisionsRequest, DecisionsResponse, Question};
use super::openrouter::{AiEndpoints, AiError, ApiKey, read_with};
use crate::google::api::parse_retry_after;

/// Dollars per input token, for Clef and (unpublished, so at most this) Clef-flash.
pub const PRICE_PER_INPUT_TOKEN: f64 = 0.24 / 1_000_000.0;

/// The inline error Settings shows for an account ID that fails [`valid_account_id`].
pub const ACCOUNT_INVALID: &str =
    "That doesn't look like a Cloudflare account ID (32 hexadecimal characters)";

/// Cloudflare error codes for a missing, malformed or unknown token.
const AUTH_CODES: [i64; 5] = [6003, 6111, 9106, 9109, 10000];

/// Whether `id` is a Cloudflare account ID: 32 lowercase hexadecimal characters. Only such an
/// ID is ever put in a URL path, so it cannot add segments or change the host.
pub fn valid_account_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The Workers AI body: the Decisions body's state and questions, without routing options.
#[derive(Serialize)]
struct ClefRequest<'a> {
    model: &'a str,
    state: &'a serde_json::Value,
    questions: &'a BTreeMap<String, Question>,
}

/// The serialised Clef body for `req`, or `None` when `backend` is not Clef.
pub fn body(req: &DecisionsRequest, backend: AiBackend) -> Option<Vec<u8>> {
    let model = backend.clef_selector()?;
    serde_json::to_vec(&ClefRequest {
        model,
        state: &req.state,
        questions: &req.questions,
    })
    .ok()
}

/// Parses a 200 body, bare or wrapped in `result`, and fills `usage.cost` from the input
/// tokens.
pub fn parse(bytes: &[u8]) -> Result<DecisionsResponse, AiError> {
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| AiError::Decode)?;
    if let Some(result) = value.get_mut("result").filter(|r| r.is_object()) {
        value = result.take();
    }
    let mut resp: DecisionsResponse = serde_json::from_value(value).map_err(|_| AiError::Decode)?;
    if let Some(usage) = resp.usage.as_mut() {
        usage.cost = Some(usage.input_tokens as f64 * PRICE_PER_INPUT_TOKEN);
    }
    Ok(resp)
}

/// Cloudflare's error envelope: `{"success": false, "errors": [{"code", "message"}]}`.
#[derive(Debug, Default, Deserialize)]
struct Envelope {
    #[serde(default)]
    errors: Vec<ErrorItem>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorItem {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: String,
}

impl Envelope {
    fn parse(body: &str) -> Self {
        serde_json::from_str(body).unwrap_or_default()
    }

    fn has_code(&self, codes: &[i64]) -> bool {
        self.errors
            .iter()
            .any(|e| e.code.is_some_and(|c| codes.contains(&c)))
    }

    fn codes(&self) -> String {
        self.errors
            .iter()
            .filter_map(|e| e.code)
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Whether a 429 says the account's allocation is used up rather than "slow down". The
    /// message is only matched, never logged.
    fn allocation_spent(&self) -> bool {
        self.errors.iter().any(|e| {
            let m = e.message.to_ascii_lowercase();
            ["daily", "allocation", "neurons", "quota"]
                .iter()
                .any(|w| m.contains(w))
        })
    }
}

/// Maps a non-2xx response to an [`AiError`]. Pure.
pub fn classify(status: u16, retry_after: Option<&str>, body: &str, now: Timestamp) -> AiError {
    let env = Envelope::parse(body);
    let retry_after = retry_after.and_then(|v| parse_retry_after(v, now));
    match status {
        300..=399 | 404 => AiError::Unavailable,
        401 => AiError::KeyRejected,
        400 if env.has_code(&AUTH_CODES) => AiError::KeyRejected,
        403 => AiError::Forbidden(env.codes()),
        413 => AiError::TooLarge,
        429 if env.allocation_spent() => AiError::Credits,
        429 | 500..=599 => AiError::Busy { retry_after },
        _ => AiError::Invalid,
    }
}

/// The log line for a failed request: status and error codes only.
fn log_failure(status: u16, body: &str) {
    let codes = Envelope::parse(body).codes();
    if codes.is_empty() {
        eprintln!("eloquawk: jev: HTTP {status}");
    } else {
        eprintln!("eloquawk: jev: HTTP {status} code {codes}");
    }
}

/// `POST /accounts/{account_id}/ai/run/@cf/cloudflare/{clef|clef-flash}` with a body from
/// [`body`]; returns the response body. `Invalid` without a valid account ID or a Clef
/// backend, before anything is sent.
pub fn post_run(
    agent: &ureq::Agent,
    endpoints: &AiEndpoints,
    account_id: &str,
    backend: AiBackend,
    key: &ApiKey,
    body: &[u8],
) -> Result<Vec<u8>, AiError> {
    let Some(model) = backend.clef_selector() else {
        return Err(AiError::Invalid);
    };
    if !valid_account_id(account_id) {
        return Err(AiError::Invalid);
    }
    let url = format!(
        "{}/accounts/{account_id}/ai/run/@cf/cloudflare/{model}",
        endpoints.cloudflare
    );
    read_with(
        agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", key.expose()))
            .header("Content-Type", "application/json")
            .send(body),
        classify,
        log_failure,
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    fn c(status: u16, body: &str) -> AiError {
        classify(status, None, body, "2026-11-03T12:00:00Z".parse().unwrap())
    }

    #[test]
    fn account_ids_are_32_lowercase_hex() {
        assert!(valid_account_id("0123456789abcdef0123456789abcdef"));
        for bad in [
            "",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "../../evil/0123456789abcdef01234",
            "0123456789abcdef0123456789abcde/",
        ] {
            assert!(!valid_account_id(bad), "{bad}");
        }
    }

    #[test]
    fn the_body_drops_routing_and_sets_the_selector() {
        let mut questions = BTreeMap::new();
        questions.insert("test".to_string(), Question::noul("Q?", "yes", "no"));
        let req = DecisionsRequest::new(json!({"subject": "hi"}), questions);
        let flash: serde_json::Value =
            serde_json::from_slice(&body(&req, AiBackend::ClefFlash).unwrap()).unwrap();
        assert_eq!(
            flash,
            json!({
                "model": "clef-flash",
                "state": {"subject": "hi"},
                "questions": {"test": {"type": "noul", "instructions": "Q?",
                    "criteria": {"true": "yes", "false": "no"}}}
            })
        );
        let clef: serde_json::Value =
            serde_json::from_slice(&body(&req, AiBackend::Clef).unwrap()).unwrap();
        assert_eq!(clef["model"], "clef");
        assert!(body(&req, AiBackend::Jev).is_none());
    }

    #[test]
    fn responses_parse_wrapped_or_bare_with_a_cost() {
        let inner = json!({
            "model": "clef",
            "answers": {"phishing": {"type": "noul", "noul": 0.93}},
            "usage": {"input_tokens": 1000, "output_tokens": 3}
        });
        let wrapped = json!({"result": inner, "success": true, "errors": [], "messages": []});
        for v in [wrapped, inner] {
            let resp = parse(v.to_string().as_bytes()).unwrap();
            assert_eq!(resp.answers["phishing"].noul, Some(0.93));
            let cost = resp.usage.unwrap().cost.unwrap();
            assert!((cost - 0.00024).abs() < 1e-12, "{cost}");
        }
        let no_usage = parse(br#"{"result":{"model":"clef","answers":{}}}"#).unwrap();
        assert!(no_usage.usage.is_none());
        assert_eq!(parse(b"not json").unwrap_err(), AiError::Decode);
    }

    #[test]
    fn classify_covers_every_row() {
        let auth =
            r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}]}"#;
        assert_eq!(c(401, auth), AiError::KeyRejected);
        assert_eq!(
            c(
                400,
                r#"{"errors":[{"code":6003,"message":"Invalid request headers"}]}"#
            ),
            AiError::KeyRejected
        );
        assert_eq!(
            c(400, r#"{"errors":[{"code":5006,"message":"m"}]}"#),
            AiError::Invalid
        );
        assert_eq!(c(403, auth), AiError::Forbidden("10000".into()));
        assert_eq!(c(404, "{}"), AiError::Unavailable);
        assert_eq!(c(302, ""), AiError::Unavailable);
        assert_eq!(c(413, ""), AiError::TooLarge);
        assert_eq!(
            c(
                429,
                r#"{"errors":[{"code":3036,"message":"You have used up your daily free allocation of 10,000 neurons"}]}"#
            ),
            AiError::Credits
        );
        assert_eq!(
            classify(429, Some("2"), "{}", Timestamp::now()),
            AiError::Busy {
                retry_after: Some(Duration::from_secs(2))
            }
        );
        for s in [500, 502, 503, 524] {
            assert_eq!(c(s, "not json"), AiError::Busy { retry_after: None }, "{s}");
        }
        assert_eq!(c(422, "{}"), AiError::Invalid);
    }

    #[test]
    fn post_run_refuses_a_bad_account_before_sending() {
        let key = ApiKey::parse_cloudflare("AbCdEfGh0123456789_-abcdefghWXYZ").unwrap();
        let agent = super::super::openrouter::agent();
        let endpoints = AiEndpoints::production();
        assert_eq!(
            post_run(&agent, &endpoints, "../x", AiBackend::Clef, &key, b"{}"),
            Err(AiError::Invalid)
        );
        assert_eq!(
            post_run(
                &agent,
                &endpoints,
                "0123456789abcdef0123456789abcdef",
                AiBackend::Jev,
                &key,
                b"{}"
            ),
            Err(AiError::Invalid)
        );
    }
}
