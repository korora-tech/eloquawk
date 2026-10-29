//! The Google worker (SPEC 7): one thread, `eloquawk-sync`, that owns the credentials, talks
//! to Google and pushes [`SyncEvent`]s to the UI. The UI never blocks on the network or the
//! keyring; it sends [`Command`]s and renders whatever arrives.
//!
//! One OAuth client serves any number of Google accounts. Each account has its own refresh
//! token (in the keyring), access token (in memory only) and problems; a failure in one
//! account does not stop the others.
//!
//! M9 skeleton: the keyring, the client (saved, built-in or missing), browser sign-in with the
//! granular-consent scope check, token refresh, sign-out and sticky problems. The only Gmail
//! call is `profile`, which names a new account and checks the existing ones on start and on
//! Refresh. Listing, opening, changing and sending mail arrive in M10 and M11; until then those
//! commands are answered with an empty list or a "not available yet" error, so the UI never
//! waits for an answer that will not come.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::google::api::{self, ApiError, Endpoints};
use crate::google::gmail;
use crate::google::oauth::{self, AuthError, ClientCredentials, TokenResponse};
use crate::model::AccountId;
use crate::protocol::{AuthState, Command, PageMode, SendError, SyncEvent, SyncHandle};
use crate::secrets::{SecretError, Secrets};

/// How long a browser sign-in may take before it is abandoned.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// An access token this close to expiry is refreshed before use.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);

const NETWORK_PROBLEM: &str = "Couldn't reach Google. Check your connection; Ctrl+R tries again.";
const MISSING_SCOPE_SIGN_IN: &str = concat!(
    "Eloquawk needs permission to read and organise your mail. Sign in again and allow ",
    "Gmail access on Google's consent page.",
);
const NO_REFRESH_TOKEN: &str = concat!(
    "Google did not return a refresh token. Remove Eloquawk's access at ",
    "myaccount.google.com/permissions and sign in again.",
);
const READ_NOT_YET: &str = "Reading Gmail isn't available in this build yet.";
const CHANGE_NOT_YET: &str = "Changing Gmail isn't available in this build yet.";
const SEND_NOT_YET: &str = "Sending isn't available in this build yet.";
const SAVE_NOT_YET: &str = "Saving attachments isn't available in this build yet.";

/// Starts the Google worker with the real endpoints and the compiled-in client.
pub fn start_google(
    secrets: Secrets,
) -> std::io::Result<(SyncHandle, async_channel::Receiver<SyncEvent>)> {
    start_with(secrets, Endpoints::google(), oauth::builtin_client())
}

/// Starts the worker against `endpoints` with `builtin` as the fallback client.
fn start_with(
    secrets: Secrets,
    endpoints: Endpoints,
    builtin: Option<ClientCredentials>,
) -> std::io::Result<(SyncHandle, async_channel::Receiver<SyncEvent>)> {
    let (tx, rx) = mpsc::channel();
    let (etx, erx) = async_channel::unbounded();
    let self_tx = tx.clone();
    std::thread::Builder::new()
        .name("eloquawk-sync".into())
        .spawn(move || Worker::new(secrets, etx, self_tx, endpoints, builtin).run(rx))?;
    Ok((SyncHandle::from_sender(tx), erx))
}

/// The sticky message for an account whose refresh token Google rejected: a short hint says
/// why, depending on whether Google asked for a reauthentication (`invalid_rapt`).
pub fn signed_out_message(email: &str, description: &str) -> String {
    let d = description.to_ascii_lowercase();
    let hint = if d.contains("invalid_rapt") || d.contains("reauth") {
        "(Your organisation requires you to sign in again.)"
    } else {
        "(Test-mode tokens expire after 7 days.)"
    };
    format!("Google signed {email} out. Add it again. {hint}")
}

/// The sticky message for an account whose token lacks the Gmail scope.
fn missing_scope_message(email: &str) -> String {
    format!(
        "Eloquawk needs permission to read and organise mail for {email}. Add the account again."
    )
}

/// Why a Google call failed.
enum CallError {
    /// The account has no usable token (it was just signed out, or the client is rejected).
    Auth,
    /// The account is no longer signed in at all; skip it quietly.
    Gone,
    /// The token endpoint could not be reached (or failed with a 5xx).
    Network,
    Api(ApiError),
}

struct Access {
    token: String,
    expires: Instant,
}

struct AccountState {
    refresh_token: String,
    access: Option<Access>,
}

struct SignIn {
    seq: u64,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum Revoke {
    WithGoogle,
    LocalOnly,
}

struct Worker {
    events: async_channel::Sender<SyncEvent>,
    self_tx: mpsc::Sender<Command>,
    agent: ureq::Agent,
    endpoints: Endpoints,
    secrets: Secrets,
    /// The compiled-in client, used when none is saved.
    builtin: Option<ClientCredentials>,
    /// The client in use: the saved one, else the built-in one.
    client: Option<ClientCredentials>,
    auth: AuthState,
    accounts: BTreeMap<AccountId, AccountState>,
    sign_in: Option<SignIn>,
    sign_in_seq: u64,
    /// Sticky messages for the banner, until dismissed or resolved. Keys: an account's email,
    /// "client", "tokens" or "network".
    problems: BTreeMap<String, String>,
    /// The last keyring read failed (locked, no Secret Service); retried on Refresh.
    keyring_failed: bool,
}

impl Worker {
    fn new(
        secrets: Secrets,
        events: async_channel::Sender<SyncEvent>,
        self_tx: mpsc::Sender<Command>,
        endpoints: Endpoints,
        builtin: Option<ClientCredentials>,
    ) -> Self {
        Self {
            events,
            self_tx,
            agent: api::agent(),
            endpoints,
            secrets,
            builtin,
            client: None,
            auth: AuthState::Starting,
            accounts: BTreeMap::new(),
            sign_in: None,
            sign_in_seq: 0,
            problems: BTreeMap::new(),
            keyring_failed: false,
        }
    }

    fn emit(&self, ev: SyncEvent) {
        let _ = self.events.send_blocking(ev);
    }

    fn set_auth(&mut self, auth: AuthState) {
        if self.auth != auth {
            self.auth = auth;
            self.emit(SyncEvent::Auth(auth));
        }
    }

    /// Sets the banner to the sticky problems.
    fn report(&self) {
        let all: Vec<&str> = self.problems.values().map(String::as_str).collect();
        self.emit(SyncEvent::Problem(
            (!all.is_empty()).then(|| all.join(" · ")),
        ));
    }

    fn emit_accounts(&self) {
        self.emit(SyncEvent::Accounts(self.accounts.keys().cloned().collect()));
    }

    fn run(mut self, rx: mpsc::Receiver<Command>) {
        self.load_from_keyring();
        while let Ok(cmd) = rx.recv() {
            if matches!(cmd, Command::Shutdown) {
                break;
            }
            self.handle(cmd);
        }
        self.cancel_sign_in();
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::SaveClient {
                client_id,
                client_secret,
            } => self.save_client(client_id, client_secret),
            Command::ClearClient => self.clear_client(),
            Command::AddAccount => self.begin_sign_in(),
            Command::CancelSignIn => self.cancel_sign_in(),
            Command::SignInFinished(seq, result) => self.finish_sign_in(seq, result),
            Command::RemoveAccount(email) => {
                if self.remove_account(&email, Revoke::WithGoogle) {
                    self.emit(SyncEvent::Notice(format!("Signed out of {email}")));
                }
            }
            Command::Refresh => {
                if self.keyring_failed {
                    self.load_from_keyring();
                } else {
                    self.check_accounts();
                }
            }
            Command::DismissProblems => {
                self.problems.clear();
                self.emit(SyncEvent::Problem(None));
            }
            Command::Want { view, more } => {
                // Nothing is listed until M10: answer page 1 with an empty list, so the view
                // shows as empty rather than loading forever.
                if !more {
                    self.emit(SyncEvent::ThreadPage {
                        view,
                        ids: vec![],
                        threads: vec![],
                        mode: PageMode::Replace,
                        has_more: false,
                    });
                }
            }
            Command::OpenThread(key) => self.emit(SyncEvent::ThreadFailed {
                key,
                error: READ_NOT_YET.into(),
            }),
            Command::Modify { op, .. }
            | Command::Trash { op, .. }
            | Command::Untrash { op, .. } => self.emit(SyncEvent::MutationFinished {
                op,
                error: Some(CHANGE_NOT_YET.into()),
            }),
            Command::Send { op, .. } => self.emit(SyncEvent::SendFinished {
                op,
                error: Some(SendError {
                    message: SEND_NOT_YET.into(),
                    maybe_sent: false,
                }),
            }),
            Command::SaveAttachment { .. } => self.emit(SyncEvent::Notice(SAVE_NOT_YET.into())),
            Command::Shutdown => {}
        }
    }

    // ----- keyring --------------------------------------------------------------------------

    fn load_from_keyring(&mut self) {
        self.keyring_failed = false;
        let saved = match self.secrets.client() {
            Ok(saved) => saved,
            Err(e) => {
                self.locked(e);
                return;
            }
        };
        self.emit(SyncEvent::Client(
            saved.as_ref().map(|c| c.client_id.clone()),
        ));
        self.client = saved.or_else(|| self.builtin.clone());
        if self.client.is_none() {
            self.set_auth(AuthState::NeedsClient);
            self.report();
            return;
        }
        let tokens = match self.secrets.tokens() {
            Ok(t) => t,
            // Unreadable, so unusable: start afresh rather than failing forever.
            Err(SecretError::Corrupt(e)) => {
                let _ = self.secrets.clear_tokens();
                self.problems.insert(
                    "tokens".into(),
                    format!(
                        "Stored sign-ins were unreadable ({e}) and were reset; add your \
                         accounts again."
                    ),
                );
                BTreeMap::new()
            }
            Err(e) => {
                self.locked(e);
                return;
            }
        };
        // Accounts added while the keyring was unavailable live for this session only; a
        // reload must not forget them.
        for (email, refresh_token) in tokens {
            self.accounts.entry(email).or_insert(AccountState {
                refresh_token,
                access: None,
            });
        }
        self.emit_accounts();
        self.set_auth(AuthState::Ready);
        self.report();
        self.check_accounts();
    }

    fn locked(&mut self, e: SecretError) {
        self.keyring_failed = true;
        self.emit(SyncEvent::Problem(Some(format!(
            "{e}. Eloquawk keeps Google credentials only in the system keyring; unlock it and \
             press Ctrl+R to retry."
        ))));
        self.set_auth(AuthState::Locked);
    }

    // ----- client ---------------------------------------------------------------------------

    fn save_client(&mut self, client_id: String, client_secret: Option<String>) {
        let client_id = client_id.trim().to_string();
        if client_id.is_empty() {
            self.emit(SyncEvent::Problem(Some("Enter a client ID.".into())));
            return;
        }
        let secret = client_secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let client_secret = match (secret, &self.client) {
            (Some(s), _) => s,
            (None, Some(existing)) if existing.client_id == client_id => {
                existing.client_secret.clone()
            }
            _ => {
                self.emit(SyncEvent::Problem(Some(
                    "Enter the client secret for this client ID.".into(),
                )));
                return;
            }
        };
        let client = ClientCredentials {
            client_id,
            client_secret,
        };
        let saved = matches!(self.secrets.client(), Ok(Some(ref c)) if *c == client);
        if saved {
            self.emit(SyncEvent::Notice("OAuth client unchanged".into()));
            return;
        }
        let same_id = self
            .client
            .as_ref()
            .is_some_and(|c| c.client_id == client.client_id);
        if !same_id {
            // Refresh tokens belong to the client id that issued them; a rotated secret for
            // the same client keeps every session.
            self.remove_all_accounts(Revoke::WithGoogle);
        }
        match self.secrets.set_client(&client) {
            Ok(()) => {
                if same_id {
                    // Saving the client in use (say, the built-in one) keeps its accounts,
                    // but the keyring dropped their tokens when no client was stored there.
                    for (email, account) in &self.accounts {
                        let _ = self.secrets.set_token(email, &account.refresh_token);
                    }
                }
                self.emit(SyncEvent::Client(Some(client.client_id.clone())));
                self.client = Some(client);
                self.problems.remove("client");
                self.report();
                self.emit(SyncEvent::Notice(
                    "OAuth client saved to the keyring".into(),
                ));
                self.set_auth(AuthState::Ready);
            }
            Err(e) => self.emit(SyncEvent::Problem(Some(e.to_string()))),
        }
    }

    fn clear_client(&mut self) {
        self.remove_all_accounts(Revoke::WithGoogle);
        match self.secrets.clear_client() {
            Ok(()) => {
                self.client = self.builtin.clone();
                self.problems.remove("client");
                self.report();
                self.emit(SyncEvent::Client(None));
                if self.client.is_some() {
                    self.emit(SyncEvent::Notice(
                        "Your OAuth client was removed; using the built-in client".into(),
                    ));
                    self.set_auth(AuthState::Ready);
                } else {
                    self.emit(SyncEvent::Notice("Your OAuth client was removed".into()));
                    self.set_auth(AuthState::NeedsClient);
                }
            }
            Err(e) => self.emit(SyncEvent::Problem(Some(e.to_string()))),
        }
    }

    // ----- sign-in --------------------------------------------------------------------------

    fn cancel_sign_in(&mut self) {
        if let Some(sign_in) = self.sign_in.take() {
            sign_in.cancel.store(true, Ordering::Relaxed);
            self.emit(SyncEvent::SigningIn(None));
        }
    }

    fn begin_sign_in(&mut self) {
        let Some(client) = self.client.clone() else {
            self.emit(SyncEvent::Problem(Some(
                "Set up a Google client in Settings first.".into(),
            )));
            return;
        };
        self.cancel_sign_in();
        let loopback = match oauth::Loopback::bind() {
            Ok(l) => l,
            Err(e) => {
                self.emit(SyncEvent::Problem(Some(e.to_string())));
                return;
            }
        };
        let pkce = oauth::new_pkce();
        let state = oauth::random_token(24);
        let url = oauth::authorize_url(
            &client.client_id,
            &loopback.redirect_uri,
            &pkce.challenge,
            &state,
            None,
        );
        self.sign_in_seq += 1;
        let seq = self.sign_in_seq;
        let cancel = Arc::new(AtomicBool::new(false));
        self.sign_in = Some(SignIn {
            seq,
            cancel: cancel.clone(),
        });
        self.report();
        self.emit(SyncEvent::SigningIn(Some(url.clone())));
        if let Err(e) = open::that_detached(&url) {
            eprintln!("eloquawk: could not open a browser: {}", e.kind());
        }
        let tx = self.self_tx.clone();
        let agent = self.agent.clone();
        let token_url = self.endpoints.token.clone();
        let spawned = std::thread::Builder::new()
            .name("eloquawk-sign-in".into())
            .spawn(move || {
                let result = loopback
                    .wait_for_code(&state, &cancel, SIGN_IN_TIMEOUT)
                    .and_then(|code| {
                        oauth::exchange_code(
                            &agent,
                            &token_url,
                            &client,
                            &code,
                            &pkce.verifier,
                            &loopback.redirect_uri,
                        )
                    });
                let _ = tx.send(Command::SignInFinished(seq, result));
            });
        if let Err(error) = spawned {
            self.sign_in = None;
            self.emit(SyncEvent::SigningIn(None));
            self.emit(SyncEvent::Problem(Some(format!(
                "Could not start the sign-in: {error}"
            ))));
        }
    }

    fn finish_sign_in(&mut self, seq: u64, result: Result<TokenResponse, AuthError>) {
        // Superseded or cancelled: drop whatever Google issued. Never revoke a token whose
        // account is unknown: revoking removes the app's whole grant for that user, which
        // would also sign out an account that is already here.
        if self.sign_in.as_ref().map(|s| s.seq) != Some(seq) {
            return;
        }
        self.sign_in = None;
        self.emit(SyncEvent::SigningIn(None));
        let tokens = match result {
            Ok(t) => t,
            Err(AuthError::Cancelled) => return,
            Err(e) => {
                self.emit(SyncEvent::Problem(Some(e.to_string())));
                return;
            }
        };
        // Granular consent: the user may have unticked Gmail. Check before saving anything.
        if !oauth::scope_granted(tokens.scope.as_deref()) {
            // Which account this is cannot be known without the scope (profile needs it).
            // With no account here it cannot be a known one, so the useless grant is revoked;
            // otherwise it is left alone, since revoking could sign out an existing account.
            if self.accounts.is_empty() {
                let token = tokens
                    .refresh_token
                    .as_ref()
                    .unwrap_or(&tokens.access_token);
                oauth::revoke(&self.agent, &self.endpoints.revoke, token);
            }
            self.emit(SyncEvent::Problem(Some(MISSING_SCOPE_SIGN_IN.into())));
            return;
        }
        let email = match gmail::profile(&self.agent, &self.endpoints, &tokens.access_token) {
            Ok(profile) => profile.email_address,
            Err(e) => {
                self.emit(SyncEvent::Problem(Some(format!(
                    "Signed in, but could not read the Gmail profile: {e}"
                ))));
                return;
            }
        };
        let Some(refresh_token) = tokens
            .refresh_token
            .clone()
            .or_else(|| self.accounts.get(&email).map(|a| a.refresh_token.clone()))
        else {
            self.emit(SyncEvent::Problem(Some(NO_REFRESH_TOKEN.into())));
            return;
        };
        self.problems.remove(&email);
        if let Err(e) = self.secrets.set_token(&email, &refresh_token) {
            self.problems.insert(
                email.clone(),
                format!("{e}. {email} is signed in for this session only."),
            );
        }
        let access = Some(Access {
            token: tokens.access_token,
            expires: Instant::now() + Duration::from_secs(tokens.expires_in),
        });
        // Signing in again replaces the refresh token; the old one is not revoked, because
        // that would revoke the whole grant, including the new token.
        let again = self
            .accounts
            .insert(
                email.clone(),
                AccountState {
                    refresh_token,
                    access,
                },
            )
            .is_some();
        self.emit(SyncEvent::Notice(if again {
            format!("{email} signed in again")
        } else {
            format!("Added {email}")
        }));
        self.report();
        self.emit_accounts();
    }

    // ----- accounts and tokens --------------------------------------------------------------

    /// Forgets one account. `Revoke::WithGoogle` also invalidates its refresh token with
    /// Google. Returns whether the account existed.
    fn remove_account(&mut self, email: &str, revoke: Revoke) -> bool {
        let Some(account) = self.accounts.remove(email) else {
            return false;
        };
        if matches!(revoke, Revoke::WithGoogle)
            && !oauth::revoke(&self.agent, &self.endpoints.revoke, &account.refresh_token)
        {
            self.emit(SyncEvent::Notice(format!(
                "Signed out of {email} here, but Google could not be told. Remove access at \
                 myaccount.google.com/permissions."
            )));
        }
        if let Err(e) = self.secrets.remove_token(email) {
            self.problems.insert(email.to_string(), e.to_string());
            self.report();
        }
        self.emit_accounts();
        true
    }

    fn remove_all_accounts(&mut self, revoke: Revoke) {
        self.cancel_sign_in();
        let emails: Vec<String> = self.accounts.keys().cloned().collect();
        for email in emails {
            self.remove_account(&email, revoke);
        }
        if let Err(e) = self.secrets.clear_tokens() {
            self.emit(SyncEvent::Problem(Some(e.to_string())));
        }
    }

    /// A valid access token for `email`, refreshed when less than a minute remains.
    fn access_token(&mut self, email: &str) -> Result<String, CallError> {
        let client = self.client.clone().ok_or(CallError::Gone)?;
        let account = self.accounts.get(email).ok_or(CallError::Gone)?;
        if let Some(a) = &account.access
            && a.expires > Instant::now() + TOKEN_MARGIN
        {
            return Ok(a.token.clone());
        }
        let rt = account.refresh_token.clone();
        match oauth::refresh(&self.agent, &self.endpoints.token, &client, &rt) {
            Ok(t) => {
                if self.problems.remove("network").is_some() {
                    self.report();
                }
                let account = self.accounts.get_mut(email).ok_or(CallError::Gone)?;
                if let Some(new_rt) = t.refresh_token
                    && new_rt != rt
                {
                    let _ = self.secrets.set_token(email, &new_rt);
                    account.refresh_token = new_rt;
                }
                account.access = Some(Access {
                    token: t.access_token.clone(),
                    expires: Instant::now() + Duration::from_secs(t.expires_in),
                });
                Ok(t.access_token)
            }
            Err(AuthError::InvalidGrant(description)) => {
                self.remove_account(email, Revoke::LocalOnly);
                let msg = signed_out_message(email, &description);
                self.problems.insert(email.to_string(), msg);
                self.report();
                Err(CallError::Auth)
            }
            Err(AuthError::InvalidClient(m)) => {
                // No account can work with a rejected client.
                self.remove_all_accounts(Revoke::LocalOnly);
                let msg = format!(
                    "Google rejected the OAuth client ({m}). If you saved your own in \
                     Settings, check it; then add your accounts again."
                );
                self.problems.insert("client".into(), msg);
                self.report();
                Err(CallError::Auth)
            }
            Err(AuthError::Network(_) | AuthError::Server(_)) => {
                self.problems
                    .insert("network".into(), NETWORK_PROBLEM.into());
                self.report();
                Err(CallError::Network)
            }
            Err(_) => Err(CallError::Auth),
        }
    }

    /// Runs `f` with a valid token for `email`, refreshing once if Google says it is stale.
    fn with_token<T>(
        &mut self,
        email: &str,
        f: impl Fn(&ureq::Agent, &Endpoints, &str) -> Result<T, ApiError>,
    ) -> Result<T, CallError> {
        let token = self.access_token(email)?;
        match f(&self.agent, &self.endpoints, &token) {
            Err(ApiError::Unauthorized) => {
                if let Some(a) = self.accounts.get_mut(email) {
                    a.access = None;
                }
                let token = self.access_token(email)?;
                f(&self.agent, &self.endpoints, &token).map_err(CallError::Api)
            }
            other => other.map_err(CallError::Api),
        }
    }

    /// Checks every account with `profile`: a token that no longer works, a missing scope or
    /// an unreachable Google shows up in the banner straight away.
    fn check_accounts(&mut self) {
        let emails: Vec<String> = self.accounts.keys().cloned().collect();
        for email in emails {
            match self.with_token(&email, gmail::profile) {
                Ok(_) => {
                    let resolved = self.problems.remove("network").is_some()
                        | self.problems.remove(&email).is_some();
                    if resolved {
                        self.report();
                    }
                }
                Err(CallError::Api(ApiError::Network(_))) => {
                    self.problems
                        .insert("network".into(), NETWORK_PROBLEM.into());
                    self.report();
                }
                Err(CallError::Api(e)) => {
                    let msg = if e.missing_scope() {
                        missing_scope_message(&email)
                    } else {
                        format!("{email}: {e}")
                    };
                    self.problems.insert(email.clone(), msg);
                    self.report();
                }
                // Already reported by `access_token`.
                Err(CallError::Auth | CallError::Gone | CallError::Network) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::fake::{FakeServer, Reply};
    use crate::model::{ThreadKey, ViewKey, ViewKind};

    const PROFILE: &str = r#"{"emailAddress":"me@example.com","historyId":"42"}"#;
    const TOKEN: &str = r#"{"access_token":"at2","expires_in":3599,"token_type":"Bearer"}"#;

    fn client() -> ClientCredentials {
        ClientCredentials {
            client_id: "c.apps.googleusercontent.com".into(),
            client_secret: "cs".into(),
        }
    }

    /// Endpoints on a port nobody listens on, for tests that must not make a request.
    fn nowhere() -> Endpoints {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let base = format!("http://127.0.0.1:{port}");
        Endpoints {
            base: base.clone(),
            upload: base.clone(),
            token: format!("{base}/token"),
            revoke: format!("{base}/revoke"),
        }
    }

    fn fake_endpoints(server: &FakeServer) -> Endpoints {
        Endpoints {
            base: server.url("/gmail"),
            upload: server.url("/upload"),
            token: server.url("/token"),
            revoke: server.url("/revoke"),
        }
    }

    /// A worker that is not running on a thread, and its event receiver.
    fn worker(
        secrets: Secrets,
        endpoints: Endpoints,
    ) -> (Worker, async_channel::Receiver<SyncEvent>) {
        let (etx, erx) = async_channel::unbounded();
        let (tx, _rx) = mpsc::channel();
        let mut w = Worker::new(secrets, etx, tx, endpoints, Some(client()));
        w.client = Some(client());
        w.auth = AuthState::Ready;
        (w, erx)
    }

    fn drain(rx: &async_channel::Receiver<SyncEvent>) -> Vec<SyncEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// Receives events from a running worker until `done` matches one, or 5 s pass.
    fn until(
        rx: &async_channel::Receiver<SyncEvent>,
        done: impl Fn(&SyncEvent) -> bool,
    ) -> Vec<SyncEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) => {
                    let stop = done(&ev);
                    out.push(ev);
                    if stop {
                        return out;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("timed out; got {out:?}");
    }

    fn problem(events: &[SyncEvent]) -> Option<String> {
        events.iter().rev().find_map(|e| match e {
            SyncEvent::Problem(p) => Some(p.clone().unwrap_or_default()),
            _ => None,
        })
    }

    fn signing_in(w: &mut Worker) -> u64 {
        w.sign_in_seq += 1;
        w.sign_in = Some(SignIn {
            seq: w.sign_in_seq,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        w.sign_in_seq
    }

    fn tokens(scope: Option<&str>) -> TokenResponse {
        TokenResponse {
            access_token: "at".into(),
            expires_in: 3599,
            refresh_token: Some("rt".into()),
            scope: scope.map(str::to_string),
        }
    }

    #[test]
    fn no_client_at_all_needs_one_and_pasting_saves_it() {
        let secrets = Secrets::memory();
        let (handle, rx) = start_with(secrets.clone(), nowhere(), None).unwrap();
        let events = until(&rx, |e| {
            matches!(e, SyncEvent::Auth(AuthState::NeedsClient))
        });
        assert!(matches!(events[0], SyncEvent::Client(None)));

        // A secret is required for a client id that is not in use.
        handle.send(Command::SaveClient {
            client_id: "pasted.apps.googleusercontent.com".into(),
            client_secret: None,
        });
        let events = until(&rx, |e| matches!(e, SyncEvent::Problem(Some(_))));
        assert!(problem(&events).unwrap().contains("client secret"));
        assert_eq!(secrets.client().unwrap(), None);

        handle.send(Command::SaveClient {
            client_id: " pasted.apps.googleusercontent.com ".into(),
            client_secret: Some(" GOCSPX-pasted ".into()),
        });
        let events = until(&rx, |e| matches!(e, SyncEvent::Auth(AuthState::Ready)));
        assert!(events.iter().any(|e| matches!(
            e,
            SyncEvent::Client(Some(id)) if id == "pasted.apps.googleusercontent.com"
        )));
        assert_eq!(
            secrets.client().unwrap(),
            Some(ClientCredentials {
                client_id: "pasted.apps.googleusercontent.com".into(),
                client_secret: "GOCSPX-pasted".into(),
            })
        );

        // "Use built-in" without a built-in client goes back to asking for one.
        handle.send(Command::ClearClient);
        until(&rx, |e| {
            matches!(e, SyncEvent::Auth(AuthState::NeedsClient))
        });
        assert_eq!(secrets.client().unwrap(), None);
        handle.send(Command::Shutdown);
    }

    #[test]
    fn builtin_client_starts_ready_without_accounts() {
        let (handle, rx) = start_with(Secrets::memory(), nowhere(), Some(client())).unwrap();
        let events = until(&rx, |e| matches!(e, SyncEvent::Auth(_)));
        assert!(matches!(events[0], SyncEvent::Client(None)));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SyncEvent::Accounts(a) if a.is_empty()))
        );
        assert!(matches!(
            events.last(),
            Some(SyncEvent::Auth(AuthState::Ready))
        ));
        handle.send(Command::Shutdown);
    }

    #[test]
    fn mail_commands_get_an_answer_until_sync_exists() {
        let (mut w, rx) = worker(Secrets::memory(), nowhere());
        let view = ViewKey {
            account: "me@example.com".into(),
            kind: ViewKind::Label("INBOX".into()),
        };
        w.handle(Command::Want { view, more: false });
        w.handle(Command::OpenThread(ThreadKey::new("me@example.com", "t")));
        w.handle(Command::Trash {
            op: 3,
            account: "me@example.com".into(),
            thread_ids: vec!["t".into()],
        });
        let events = drain(&rx);
        assert!(matches!(
            &events[0],
            SyncEvent::ThreadPage { ids, has_more: false, mode: PageMode::Replace, .. } if ids.is_empty()
        ));
        assert!(matches!(&events[1], SyncEvent::ThreadFailed { .. }));
        assert!(matches!(
            &events[2],
            SyncEvent::MutationFinished {
                op: 3,
                error: Some(_)
            }
        ));
    }

    #[test]
    fn sign_in_names_the_account_from_profile_and_saves_its_token() {
        let server = FakeServer::start(vec![Reply::json(200, PROFILE).for_path("/gmail/profile")]);
        let secrets = Secrets::memory();
        let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
        let seq = signing_in(&mut w);
        // A stale result is dropped.
        w.finish_sign_in(seq + 7, Ok(tokens(None)));
        assert!(drain(&rx).is_empty());

        w.finish_sign_in(seq, Ok(tokens(Some(oauth::GMAIL_MODIFY))));
        let events = drain(&rx);
        assert!(events.iter().any(|e| matches!(
            e,
            SyncEvent::Accounts(a) if a == &vec!["me@example.com".to_string()]
        )));
        assert_eq!(secrets.tokens().unwrap()["me@example.com"], "rt");
        let reqs = server.finish();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].header("authorization").as_deref(),
            Some("Bearer at")
        );
    }

    #[test]
    fn missing_gmail_scope_saves_nothing_and_revokes_only_an_unknown_grant() {
        // No accounts yet: the new grant cannot belong to a known account, so it is revoked.
        let server = FakeServer::start(vec![Reply::json(200, "{}").for_path("/revoke")]);
        let secrets = Secrets::memory();
        let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
        let seq = signing_in(&mut w);
        w.finish_sign_in(seq, Ok(tokens(Some("openid"))));
        assert!(problem(&drain(&rx)).unwrap().contains("needs permission"));
        assert!(secrets.tokens().unwrap().is_empty());
        assert!(w.accounts.is_empty());
        let reqs = server.finish();
        assert_eq!(reqs.len(), 1, "no profile call, one revoke");
        assert!(reqs[0].form().contains(&("token".into(), "rt".into())));

        // With an account here the grant might be its own: nothing is revoked.
        let (mut w, rx) = worker(secrets.clone(), nowhere());
        w.accounts.insert(
            "old@example.com".into(),
            AccountState {
                refresh_token: "old".into(),
                access: None,
            },
        );
        let seq = signing_in(&mut w);
        w.finish_sign_in(seq, Ok(tokens(Some("openid"))));
        assert!(problem(&drain(&rx)).unwrap().contains("needs permission"));
        assert_eq!(w.accounts.len(), 1);
    }

    #[test]
    fn invalid_grant_signs_the_account_out_with_a_hint() {
        let server = FakeServer::start(vec![Reply::json(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )]);
        let secrets = Secrets::memory();
        secrets.set_token("me@example.com", "rt").unwrap();
        let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
        w.accounts.insert(
            "me@example.com".into(),
            AccountState {
                refresh_token: "rt".into(),
                access: None,
            },
        );
        assert!(matches!(
            w.access_token("me@example.com"),
            Err(CallError::Auth)
        ));
        assert!(w.accounts.is_empty());
        assert!(secrets.tokens().unwrap().is_empty());
        assert_eq!(
            problem(&drain(&rx)).unwrap(),
            "Google signed me@example.com out. Add it again. (Test-mode tokens expire after 7 days.)"
        );
        assert_eq!(server.finish().len(), 1, "a local sign-out revokes nothing");
    }

    #[test]
    fn signed_out_hint_depends_on_reauth() {
        assert!(
            signed_out_message("a@b", "reauth related error (invalid_rapt)")
                .ends_with("(Your organisation requires you to sign in again.)")
        );
        assert!(
            signed_out_message("a@b", "Bad Request")
                .ends_with("(Test-mode tokens expire after 7 days.)")
        );
    }

    #[test]
    fn token_endpoint_outages_are_network_problems_that_keep_the_account() {
        let server = FakeServer::start(vec![Reply::json(503, "down"), Reply::json(200, TOKEN)]);
        let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
        w.accounts.insert(
            "me@example.com".into(),
            AccountState {
                refresh_token: "rt".into(),
                access: None,
            },
        );
        assert!(matches!(
            w.access_token("me@example.com"),
            Err(CallError::Network)
        ));
        assert_eq!(w.accounts.len(), 1);
        assert_eq!(problem(&drain(&rx)).as_deref(), Some(NETWORK_PROBLEM));
        // The next success clears it.
        assert_eq!(
            w.access_token("me@example.com").ok().as_deref(),
            Some("at2")
        );
        assert_eq!(problem(&drain(&rx)).as_deref(), Some(""));
    }

    #[test]
    fn a_rejected_access_token_is_refreshed_once() {
        let server = FakeServer::start(vec![
            Reply::json(401, "{}").for_path("/gmail/profile"),
            Reply::json(200, TOKEN).for_path("/token"),
            Reply::json(200, PROFILE).for_path("/gmail/profile"),
        ]);
        let (mut w, _rx) = worker(Secrets::memory(), fake_endpoints(&server));
        w.accounts.insert(
            "me@example.com".into(),
            AccountState {
                refresh_token: "rt".into(),
                access: Some(Access {
                    token: "stale".into(),
                    expires: Instant::now() + Duration::from_secs(3000),
                }),
            },
        );
        let profile = w.with_token("me@example.com", gmail::profile).ok().unwrap();
        assert_eq!(profile.email_address, "me@example.com");
        let reqs = server.finish();
        assert_eq!(
            reqs[0].header("authorization").as_deref(),
            Some("Bearer stale")
        );
        assert_eq!(
            reqs[2].header("authorization").as_deref(),
            Some("Bearer at2")
        );
    }

    #[test]
    fn saving_the_client_in_use_keeps_its_accounts() {
        let secrets = Secrets::memory();
        let (mut w, rx) = worker(secrets.clone(), nowhere());
        w.accounts.insert(
            "me@example.com".into(),
            AccountState {
                refresh_token: "rt".into(),
                access: None,
            },
        );
        secrets.set_token("me@example.com", "rt").unwrap();
        // The built-in client's id with a secret pasted: same client, so nobody is signed out.
        w.save_client(client().client_id, Some("cs".into()));
        assert_eq!(secrets.client().unwrap(), Some(client()));
        assert_eq!(secrets.tokens().unwrap()["me@example.com"], "rt");
        assert_eq!(w.accounts.len(), 1);
        assert!(
            drain(&rx)
                .iter()
                .any(|e| matches!(e, SyncEvent::Client(Some(_))))
        );
    }
}
