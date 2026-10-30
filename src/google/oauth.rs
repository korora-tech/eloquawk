//! OAuth 2.0 for installed apps: authorization code + PKCE with a loopback redirect
//! (<https://developers.google.com/identity/protocols/oauth2/native-app>).
//!
//! The browser is sent to Google, Google redirects to `http://127.0.0.1:<port>/`, and the one
//! request carrying `code` (or `error`) ends the wait. Nothing here persists anything. The
//! token and revocation endpoints are passed in, so tests can point them at a fake server.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::api::{NetKind, net_kind};

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
/// Read, labels, archive, trash, drafts and send; not permanent delete.
pub const GMAIL_MODIFY: &str = "https://www.googleapis.com/auth/gmail.modify";
/// Everything Eloquawk asks for.
pub const SCOPES: &[&str] = &[GMAIL_MODIFY];

/// Eloquawk's own Desktop client, used unless a build or Settings substitutes another.
const BUILTIN_CLIENT_ID: &str =
    "399765172362-7rbgg9gperlq92l3d5qqlpqveqbs00fp.apps.googleusercontent.com";
/// The built-in client's secret. While it is empty there is no built-in client and the app
/// asks for one (SPEC 6.3, M9 note).
const BUILTIN_CLIENT_SECRET: &str = "";

/// Whether a token response grants the Gmail scope. With granular consent the user can untick
/// it; OAuth omits `scope` only when the grant is exactly what was requested.
pub fn scope_granted(granted: Option<&str>) -> bool {
    granted.is_none_or(|g| g.split_whitespace().any(|s| s == GMAIL_MODIFY))
}

/// A Google "Desktop app" OAuth client. Google requires the secret for desktop clients even
/// though it cannot be kept confidential; it is still treated as a secret locally.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

/// Eloquawk's own Desktop client, used unless a client is saved in Settings. A desktop
/// client's secret ships in every binary and is public by nature (Google says as much); PKCE
/// protects the sign-in. A build can substitute its own with `ELOQUAWK_GOOGLE_CLIENT_ID` and
/// `ELOQUAWK_GOOGLE_CLIENT_SECRET`; setting either to an empty string builds without a
/// client, which makes the app ask for one ("Set up a Google client").
pub fn builtin_client() -> Option<ClientCredentials> {
    client_from(
        (BUILTIN_CLIENT_ID, BUILTIN_CLIENT_SECRET),
        option_env!("ELOQUAWK_GOOGLE_CLIENT_ID"),
        option_env!("ELOQUAWK_GOOGLE_CLIENT_SECRET"),
    )
}

/// The `defaults` client with each compile-time override applied; `None` when either part
/// ends up empty.
fn client_from(
    defaults: (&str, &str),
    id: Option<&str>,
    secret: Option<&str>,
) -> Option<ClientCredentials> {
    let id = id.unwrap_or(defaults.0).trim();
    let secret = secret.unwrap_or(defaults.1).trim();
    (!id.is_empty() && !secret.is_empty()).then(|| ClientCredentials {
        client_id: id.into(),
        client_secret: secret.into(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// Google rejected the client id or secret.
    InvalidClient(String),
    /// The refresh token was revoked or expired; the user has to sign in again.
    InvalidGrant(String),
    /// The user declined consent, or Google returned an `error` on the redirect.
    Denied(String),
    Cancelled,
    TimedOut,
    /// The token endpoint could not be reached.
    Network(NetKind),
    /// The token endpoint answered with a 5xx; handled like [`AuthError::Network`].
    Server(u16),
    Other(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::InvalidClient(m) => write!(f, "Google rejected the OAuth client: {m}"),
            AuthError::InvalidGrant(m) => write!(f, "Sign-in expired or was revoked: {m}"),
            AuthError::Denied(m) => write!(f, "Sign-in was not completed: {m}"),
            AuthError::Cancelled => write!(f, "Sign-in cancelled"),
            AuthError::TimedOut => write!(f, "Sign-in timed out"),
            AuthError::Network(kind) => write!(f, "Couldn't reach Google: {kind}"),
            AuthError::Server(status) => {
                write!(f, "Google's sign-in service failed (HTTP {status})")
            }
            AuthError::Other(m) => write!(f, "{m}"),
        }
    }
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)
        .expect("the OS must provide cryptographic randomness for OAuth state and PKCE");
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn new_pkce() -> Pkce {
    // 64 random bytes → 86 base64url characters (RFC 7636 allows 43–128).
    let verifier = random_token(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

pub fn authorize_url(
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    login_hint: Option<&str>,
) -> String {
    let mut url = url::Url::parse(AUTH_URL)
        .expect("AUTH_URL is the fixed Google OAuth authorization endpoint");
    let mut query = url.query_pairs_mut();
    query
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        // A refresh token is only issued with offline access, and only reliably with consent.
        // select_account: always show the picker, so "Add account" can add a second account
        // instead of silently re-using the one signed in to the browser.
        .append_pair("access_type", "offline")
        .append_pair("prompt", "select_account consent");
    if let Some(email) = login_hint {
        query.append_pair("login_hint", email);
    }
    drop(query);
    url.into()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    Code {
        code: String,
        state: String,
    },
    Error {
        error: String,
        state: Option<String>,
    },
    /// Not the redirect (favicon, probes): answer and keep waiting.
    Ignore,
}

/// Parses the request line of an HTTP request, e.g. `GET /?code=…&state=… HTTP/1.1`.
pub fn parse_callback(request_line: &str) -> Callback {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Callback::Ignore;
    };
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignore;
    };
    if url.path() != "/" {
        return Callback::Ignore;
    }
    let get = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    match (get("code"), get("error"), get("state")) {
        (_, Some(error), state) => Callback::Error { error, state },
        (Some(code), None, Some(state)) => Callback::Code { code, state },
        _ => Callback::Ignore,
    }
}

pub struct Loopback {
    listener: TcpListener,
    pub redirect_uri: String,
}

impl Loopback {
    pub fn bind() -> Result<Self, AuthError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| AuthError::Other(format!("cannot listen on 127.0.0.1: {e}")))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| AuthError::Other(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| AuthError::Other(e.to_string()))?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}"),
        })
    }

    /// Waits for the redirect carrying `expected_state` and returns the authorization code.
    pub fn wait_for_code(
        &self,
        expected_state: &str,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> Result<String, AuthError> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(AuthError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(AuthError::TimedOut);
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(result) = handle_connection(stream, expected_state) {
                        return result;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(AuthError::Other(format!("loopback accept failed: {e}"))),
            }
        }
    }
}

/// `None` means "not the redirect, keep waiting".
fn handle_connection(
    mut stream: TcpStream,
    expected_state: &str,
) -> Option<Result<String, AuthError>> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next().unwrap_or_default();
    let (result, body) = match parse_callback(line) {
        Callback::Ignore => {
            respond(&mut stream, "404 Not Found", "");
            return None;
        }
        Callback::Code { state, .. }
        | Callback::Error {
            state: Some(state), ..
        } if state != expected_state => {
            // A stale or forged redirect: refuse it but keep waiting for the real one.
            respond(
                &mut stream,
                "400 Bad Request",
                &page("This sign-in link is not the one Eloquawk is waiting for."),
            );
            return None;
        }
        Callback::Code { code, .. } => (
            Ok(code),
            page("Eloquawk is connected to Gmail. You can close this tab."),
        ),
        // Without our state an error redirect could come from anyone; ignore it.
        Callback::Error { state: None, .. } => {
            respond(
                &mut stream,
                "400 Bad Request",
                &page("This sign-in link is not the one Eloquawk is waiting for."),
            );
            return None;
        }
        Callback::Error { error, .. } => (
            Err(AuthError::Denied(error.clone())),
            page(&format!(
                "Sign-in was not completed ({error}). You can close this tab."
            )),
        ),
    };
    respond(&mut stream, "200 OK", &body);
    Some(result)
}

fn page(message: &str) -> String {
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html><meta charset=utf-8><title>Eloquawk</title>\
         <body style=\"font:16px system-ui;background:#0d0f14;color:#f2f3f6;\
         display:grid;place-items:center;height:90vh\">\
         <p>{escaped}</p></body>"
    )
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Never printed: `Debug` is implemented by hand without the tokens.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field("expires_in", &self.expires_in)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

fn token_request(
    agent: &ureq::Agent,
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, AuthError> {
    let mut resp = agent
        .post(token_url)
        .send_form(form.iter().copied())
        .map_err(|e| AuthError::Network(net_kind(&e)))?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| AuthError::Network(net_kind(&e)))?;
    if status == 200 {
        return serde_json::from_str(&body).map_err(|e| {
            // Not `e`'s message: it can echo a token from the body.
            AuthError::Other(format!(
                "Google's sign-in service gave an unexpected answer (line {} column {})",
                e.line(),
                e.column()
            ))
        });
    }
    if status >= 500 {
        return Err(AuthError::Server(status));
    }
    let err: TokenError = serde_json::from_str(&body).unwrap_or(TokenError {
        error: format!("HTTP {status}"),
        error_description: None,
    });
    let msg = err.error_description.unwrap_or_else(|| err.error.clone());
    Err(match err.error.as_str() {
        "invalid_client" | "unauthorized_client" => AuthError::InvalidClient(msg),
        "invalid_grant" => AuthError::InvalidGrant(msg),
        _ => AuthError::Other(msg),
    })
}

pub fn exchange_code(
    agent: &ureq::Agent,
    token_url: &str,
    client: &ClientCredentials,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, AuthError> {
    token_request(
        agent,
        token_url,
        &[
            ("code", code),
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ],
    )
}

pub fn refresh(
    agent: &ureq::Agent,
    token_url: &str,
    client: &ClientCredentials,
    refresh_token: &str,
) -> Result<TokenResponse, AuthError> {
    token_request(
        agent,
        token_url,
        &[
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
    )
}

/// Best effort: revoking a refresh token also revokes the access tokens issued from it, and
/// the user's whole grant to this client.
pub fn revoke(agent: &ureq::Agent, revoke_url: &str, token: &str) -> bool {
    agent
        .post(revoke_url)
        .send_form([("token", token)])
        .is_ok_and(|r| r.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::api::agent;
    use crate::google::fake::{FakeServer, Reply};

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = new_pkce();
        assert_eq!(p.verifier.len(), 86);
        assert!((43..=128).contains(&p.verifier.len()));
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expected);
        // RFC 7636 appendix B test vector.
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorize_url_has_required_params() {
        let u = authorize_url("id.apps", "http://127.0.0.1:5555", "chal", "st", None);
        let url = url::Url::parse(&u).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "id.apps");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5555");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["scope"], "https://www.googleapis.com/auth/gmail.modify");
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "select_account consent");
        assert_eq!(q["state"], "st");
        assert!(!q.contains_key("login_hint"));
    }

    #[test]
    fn authorize_url_hints_the_existing_account() {
        let u = authorize_url(
            "id.apps",
            "http://127.0.0.1:5555",
            "chal",
            "st",
            Some("me@example.com"),
        );
        let url = url::Url::parse(&u).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["login_hint"], "me@example.com");
    }

    #[test]
    fn the_gmail_scope_must_be_granted() {
        assert!(scope_granted(None));
        assert!(scope_granted(Some(GMAIL_MODIFY)));
        assert!(scope_granted(Some(&format!("openid {GMAIL_MODIFY} email"))));
        assert!(!scope_granted(Some("")));
        assert!(!scope_granted(Some(
            "https://www.googleapis.com/auth/gmail.readonly"
        )));
        // A prefix is not the scope.
        assert!(!scope_granted(Some(&format!("{GMAIL_MODIFY}x"))));
    }

    #[test]
    fn builtin_client_is_compiled_in_and_overridable() {
        assert!(BUILTIN_CLIENT_ID.ends_with(".apps.googleusercontent.com"));
        let defaults = ("ours.apps.googleusercontent.com", "GOCSPX-ours");
        let ours = client_from(defaults, None, None).unwrap();
        assert_eq!(ours.client_id, "ours.apps.googleusercontent.com");
        assert_eq!(ours.client_secret, "GOCSPX-ours");
        let own = client_from(defaults, Some("mine.apps"), Some("s3cret")).unwrap();
        assert_eq!(own.client_id, "mine.apps");
        assert_eq!(own.client_secret, "s3cret");
        // Building with an empty variable, or with an empty default, means "no client".
        assert_eq!(client_from(defaults, Some(""), None), None);
        assert_eq!(client_from(defaults, None, Some("  ")), None);
        assert_eq!(client_from(("id", ""), None, None), None);
        // Without overrides the build's client is the compiled-in one.
        if option_env!("ELOQUAWK_GOOGLE_CLIENT_ID").is_none()
            && option_env!("ELOQUAWK_GOOGLE_CLIENT_SECRET").is_none()
        {
            assert_eq!(
                builtin_client(),
                client_from((BUILTIN_CLIENT_ID, BUILTIN_CLIENT_SECRET), None, None)
            );
        }
    }

    #[test]
    fn callback_parsing() {
        assert_eq!(
            parse_callback("GET /?state=s1&code=4%2F0Ab&scope=x HTTP/1.1"),
            Callback::Code {
                code: "4/0Ab".into(),
                state: "s1".into()
            }
        );
        assert_eq!(
            parse_callback("GET /?error=access_denied&state=s1 HTTP/1.1"),
            Callback::Error {
                error: "access_denied".into(),
                state: Some("s1".into())
            }
        );
        assert_eq!(
            parse_callback("GET /favicon.ico HTTP/1.1"),
            Callback::Ignore
        );
        assert_eq!(
            parse_callback("POST /?code=a&state=b HTTP/1.1"),
            Callback::Ignore
        );
        assert_eq!(parse_callback("GET /?code=a HTTP/1.1"), Callback::Ignore);
    }

    #[test]
    fn loopback_accepts_matching_state_only() {
        let lb = Loopback::bind().unwrap();
        let addr = lb.redirect_uri.trim_start_matches("http://").to_string();
        let client = std::thread::spawn(move || {
            let send = |path: &str| {
                let mut s = TcpStream::connect(&addr).unwrap();
                write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
                let mut out = String::new();
                let _ = s.read_to_string(&mut out);
                out
            };
            let a = send("/favicon.ico");
            let b = send("/?code=wrong&state=other");
            let d = send("/?error=access_denied");
            let c = send("/?code=right&state=good");
            (a, b, c, d)
        });
        let cancel = AtomicBool::new(false);
        let code = lb
            .wait_for_code("good", &cancel, Duration::from_secs(10))
            .unwrap();
        assert_eq!(code, "right");
        let (a, b, c, d) = client.join().unwrap();
        assert!(a.starts_with("HTTP/1.1 404"));
        assert!(b.starts_with("HTTP/1.1 400"));
        assert!(
            d.starts_with("HTTP/1.1 400"),
            "error without state is refused"
        );
        assert!(c.starts_with("HTTP/1.1 200"));
        assert!(c.contains("Eloquawk is connected to Gmail."));
    }

    #[test]
    fn loopback_cancel() {
        let lb = Loopback::bind().unwrap();
        let cancel = AtomicBool::new(true);
        assert_eq!(
            lb.wait_for_code("s", &cancel, Duration::from_secs(10)),
            Err(AuthError::Cancelled)
        );
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = ClientCredentials {
            client_id: "id".into(),
            client_secret: "hunter2".into(),
        };
        assert!(!format!("{c:?}").contains("hunter2"));
        let t = TokenResponse {
            access_token: "at-secret".into(),
            expires_in: 1,
            refresh_token: Some("rt-secret".into()),
            scope: None,
        };
        assert!(!format!("{t:?}").contains("secret"));
    }

    #[test]
    fn token_endpoint_errors_map() {
        let server = FakeServer::start(vec![
            Reply::json(
                200,
                r#"{"access_token":"a","expires_in":3599,"scope":"x","token_type":"Bearer"}"#,
            ),
            Reply::json(
                400,
                r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
            ),
            Reply::json(
                401,
                r#"{"error":"invalid_client","error_description":"bad"}"#,
            ),
            Reply::json(503, "unavailable"),
        ]);
        let a = agent();
        let url = server.url("/token");
        let client = ClientCredentials {
            client_id: "cid".into(),
            client_secret: "cs".into(),
        };
        let ok = refresh(&a, &url, &client, "rt").unwrap();
        assert_eq!(ok.access_token, "a");
        assert_eq!(
            refresh(&a, &url, &client, "rt").unwrap_err(),
            AuthError::InvalidGrant("Token has been expired or revoked.".into())
        );
        assert!(matches!(
            refresh(&a, &url, &client, "rt"),
            Err(AuthError::InvalidClient(_))
        ));
        assert_eq!(
            refresh(&a, &url, &client, "rt").unwrap_err(),
            AuthError::Server(503)
        );
        let reqs = server.finish();
        let form = reqs[0].form();
        assert!(form.contains(&("grant_type".into(), "refresh_token".into())));
        assert!(form.contains(&("refresh_token".into(), "rt".into())));
        assert!(form.contains(&("client_id".into(), "cid".into())));
    }
}
