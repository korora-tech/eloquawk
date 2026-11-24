use super::*;

impl Worker {
    // ----- keyring --------------------------------------------------------------------------

    pub(super) fn load_from_keyring(&mut self) {
        self.keyring_failed = false;
        let saved = match self.secrets.client() {
            Ok(saved) => saved,
            // Unlocking cannot fix an unreadable client, so drop it (and the tokens it issued)
            // and say where to set it again.
            Err(SecretError::Corrupt(_)) => {
                let _ = self.secrets.clear_client();
                self.problems.insert(
                    "client".into(),
                    "Your saved OAuth client was unreadable and was removed; set it again in \
                     Settings and add your accounts again."
                        .into(),
                );
                None
            }
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
            // The parse error is left out: it could quote a token.
            Err(SecretError::Corrupt(_)) => {
                let _ = self.secrets.clear_tokens();
                self.problems.insert(
                    "tokens".into(),
                    "Stored sign-ins were unreadable and were reset; add your accounts again."
                        .into(),
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
            self.accounts
                .entry(email)
                .or_insert_with(|| AccountState::new(refresh_token, None));
        }
        self.emit_accounts();
        self.set_auth(AuthState::Ready);
        self.report();
        let emails: Vec<AccountId> = self.accounts.keys().cloned().collect();
        for email in emails {
            self.restore(&email);
            self.queue.push(Job::InitialSync(email));
        }
    }

    pub(super) fn locked(&mut self, e: SecretError) {
        self.keyring_failed = true;
        self.emit(SyncEvent::Problem(Some(format!(
            "{e}. Eloquawk keeps Google credentials only in the system keyring; unlock it and \
             press Ctrl+R to retry."
        ))));
        self.set_auth(AuthState::Locked);
    }

    // ----- client ---------------------------------------------------------------------------

    pub(super) fn save_client(&mut self, client_id: String, client_secret: Option<String>) {
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
        // The keyring write comes first: if it fails, the old client and its accounts stay
        // as they were.
        match self.secrets.set_client(&client) {
            Ok(()) => {
                if !same_id {
                    // Refresh tokens belong to the client id that issued them; a rotated
                    // secret for the same client keeps every session.
                    self.remove_all_accounts(Revoke::WithGoogle);
                } else {
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
            Err(e) => {
                // `set_client` may have dropped the stored tokens before failing; put back
                // those of the accounts still in use.
                for (email, account) in &self.accounts {
                    let _ = self.secrets.set_token(email, &account.refresh_token);
                }
                self.emit(SyncEvent::Problem(Some(e.to_string())));
            }
        }
    }

    pub(super) fn clear_client(&mut self) {
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

    pub(super) fn cancel_sign_in(&mut self) {
        if let Some(sign_in) = self.sign_in.take() {
            sign_in.cancel.store(true, Ordering::Relaxed);
            self.emit(SyncEvent::SigningIn(None));
        }
    }

    pub(super) fn begin_sign_in(&mut self) {
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

    pub(super) fn finish_sign_in(&mut self, seq: u64, result: Result<TokenResponse, AuthError>) {
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
        // that would revoke the whole grant, including the new token. The caches stay.
        let again = match self.accounts.get_mut(&email) {
            Some(account) => {
                account.refresh_token = refresh_token;
                account.access = access;
                account.backoff.reset();
                true
            }
            None => {
                self.accounts
                    .insert(email.clone(), AccountState::new(refresh_token, access));
                false
            }
        };
        self.queue.push(Job::InitialSync(email.clone()));
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
    pub(super) fn remove_account(&mut self, email: &str, revoke: Revoke) -> bool {
        let Some(account) = self.accounts.remove(email) else {
            return false;
        };
        self.queue.forget(email);
        if let Some(dir) = &self.cache_dir
            && let Err(e) = crate::cache::remove(dir, email)
        {
            eprintln!("eloquawk: could not delete the summary cache: {}", e.kind());
        }
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

    pub(super) fn remove_all_accounts(&mut self, revoke: Revoke) {
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
    pub(super) fn access_token(&mut self, email: &str) -> Result<String, CallError> {
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
            // Anything else (a 400 other than invalid_grant, an unreadable answer) keeps the
            // account but must not fail silently. The message carries no URL or token.
            Err(e) => {
                self.problems
                    .insert(email.to_string(), format!("{email}: {e}"));
                self.report();
                Err(CallError::Auth)
            }
        }
    }

    /// Runs `f` with a valid token for `email`, refreshing once if Google says it is stale.
    pub(super) fn with_token<T>(
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
}
