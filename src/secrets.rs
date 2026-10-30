//! Secrets live in the desktop keyring (freedesktop Secret Service: GNOME Keyring, KWallet,
//! KeePassXC…) and nowhere else: an OAuth client the user saved in Settings, one refresh token
//! per Google account (SPEC 6.4) and, from M16, the OpenRouter API key (SPEC 16.4). Access
//! tokens are only ever held in memory.
//!
//! Debug builds honour `ELOQUAWK_SECRETS=memory`, which swaps the keyring for a [`MemoryStore`]
//! (optionally seeded from the JSON file named by `ELOQUAWK_SECRETS_SEED`), so development
//! runs never touch the user's keyring. Release builds ignore both variables.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ai::openrouter::ApiKey;
use crate::google::oauth::ClientCredentials;

/// Secret Service name used for all Eloquawk keyring items.
pub const SERVICE: &str = "korora.tech.Eloquawk";
pub(crate) const CLIENT_USER: &str = "google-oauth-client";
/// JSON map of account email → refresh token, in one item: one unlock, one atomic write.
pub(crate) const TOKENS_USER: &str = "google-refresh-tokens";
/// The OpenRouter API key for the AI message checks (SPEC 16.4).
pub(crate) const OPENROUTER_USER: &str = "openrouter-api-key";

/// Failure to read or update a stored credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// No Secret Service on the session bus, or it refused access (locked, denied).
    Unavailable(String),
    /// A keyring value exists but cannot be decoded.
    Corrupt(String),
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::Unavailable(m) => write!(f, "Keyring unavailable: {m}"),
            SecretError::Corrupt(m) => write!(f, "Keyring entry is unreadable: {m}"),
        }
    }
}

/// Backend for storing credentials, allowing tests to use an isolated implementation.
pub trait SecretStore: Send + Sync {
    /// Reads a value, returning `None` when no item exists for `user`.
    fn get(&self, user: &str) -> Result<Option<String>, SecretError>;
    /// Stores or replaces the value associated with `user`.
    fn set(&self, user: &str, value: &str) -> Result<(), SecretError>;
    /// Removes the item associated with `user`; succeeds if the item is already absent.
    fn delete(&self, user: &str) -> Result<(), SecretError>;
}

#[derive(Serialize, Deserialize)]
struct StoredClient {
    client_id: String,
    client_secret: String,
}

/// Typed access to Eloquawk's entries on top of any [`SecretStore`].
#[derive(Clone)]
pub struct Secrets {
    store: Arc<dyn SecretStore>,
}

impl Secrets {
    /// Creates a typed credential interface over `store`.
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self { store }
    }

    /// Creates an interface backed by the desktop Secret Service keyring.
    ///
    /// The keyring connection is opened lazily when a credential is accessed.
    pub fn keyring() -> Self {
        Self::new(Arc::new(KeyringStore))
    }

    /// The keyring, or in a debug build with `ELOQUAWK_SECRETS=memory` an in-memory store
    /// seeded from `ELOQUAWK_SECRETS_SEED`.
    pub fn from_env() -> Self {
        #[cfg(debug_assertions)]
        if std::env::var_os("ELOQUAWK_SECRETS").is_some_and(|v| v == "memory") {
            eprintln!("eloquawk: ELOQUAWK_SECRETS=memory: credentials are kept in memory only");
            let secrets = Self::memory();
            if let Some(path) = std::env::var_os("ELOQUAWK_SECRETS_SEED") {
                let result = std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|raw| secrets.seed(&raw));
                if let Err(e) = result {
                    eprintln!("eloquawk: ignoring ELOQUAWK_SECRETS_SEED: {e}");
                }
            }
            return secrets;
        }
        Self::keyring()
    }

    /// An isolated in-memory backend (tests, and `ELOQUAWK_SECRETS=memory`).
    #[cfg(any(test, debug_assertions))]
    pub fn memory() -> Self {
        Self::new(Arc::new(MemoryStore::default()))
    }

    /// Fills the store from a seed: `{"client": {"client_id", "client_secret"}, "tokens":
    /// {"email": "refresh token"}, "openrouter_key": "sk-or-…"}`, every key optional.
    #[cfg(any(test, debug_assertions))]
    pub fn seed(&self, raw: &str) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Seed {
            client: Option<StoredClient>,
            #[serde(default)]
            tokens: BTreeMap<String, String>,
            openrouter_key: Option<String>,
        }
        let seed: Seed = serde_json::from_str(raw).map_err(|e| describe(&e))?;
        if let Some(c) = seed.client {
            self.set_client(&ClientCredentials {
                client_id: c.client_id,
                client_secret: c.client_secret,
            })
            .map_err(|e| e.to_string())?;
        }
        for (email, token) in seed.tokens {
            self.set_token(&email, &token).map_err(|e| e.to_string())?;
        }
        if let Some(raw) = seed.openrouter_key {
            let key = ApiKey::parse(&raw).map_err(|e| e.to_string())?;
            self.set_openrouter_key(&key).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Loads the OpenRouter API key, `None` when none is saved. A stored value that is not a
    /// valid key is [`SecretError::Corrupt`], and the error never quotes it.
    pub fn openrouter_key(&self) -> Result<Option<ApiKey>, SecretError> {
        match self.store.get(OPENROUTER_USER)? {
            None => Ok(None),
            Some(raw) => ApiKey::parse(&raw)
                .map(Some)
                .map_err(|_| SecretError::Corrupt("the OpenRouter key is not a valid key".into())),
        }
    }

    /// Saves or replaces the OpenRouter API key.
    pub fn set_openrouter_key(&self, key: &ApiKey) -> Result<(), SecretError> {
        self.store.set(OPENROUTER_USER, key.expose())
    }

    /// Removes the OpenRouter API key; succeeds if it was already absent.
    pub fn clear_openrouter_key(&self) -> Result<(), SecretError> {
        self.store.delete(OPENROUTER_USER)
    }

    /// Loads OAuth client credentials, returning `None` if no client has been saved.
    ///
    /// Returns [`SecretError::Corrupt`] if a stored entry cannot be decoded.
    pub fn client(&self) -> Result<Option<ClientCredentials>, SecretError> {
        let Some(raw) = self.store.get(CLIENT_USER)? else {
            return Ok(None);
        };
        let c: StoredClient =
            serde_json::from_str(&raw).map_err(|e| SecretError::Corrupt(describe(&e)))?;
        Ok(Some(ClientCredentials {
            client_id: c.client_id,
            client_secret: c.client_secret,
        }))
    }

    /// Stores the client. A different client id invalidates the refresh tokens (Google ties
    /// refresh tokens to the client that obtained them), so they are removed first: a failure
    /// can then never leave a new client paired with an old token.
    pub fn set_client(&self, client: &ClientCredentials) -> Result<(), SecretError> {
        let previous = self.client().ok().flatten();
        if previous.as_ref().map(|c| &c.client_id) != Some(&client.client_id) {
            self.clear_tokens()?;
        }
        let raw = serde_json::to_string(&StoredClient {
            client_id: client.client_id.clone(),
            client_secret: client.client_secret.clone(),
        })
        .expect("StoredClient contains only JSON-serializable strings");
        self.store.set(CLIENT_USER, &raw)
    }

    /// Deletes the OAuth client and all refresh tokens associated with it.
    pub fn clear_client(&self) -> Result<(), SecretError> {
        self.clear_tokens()?;
        self.store.delete(CLIENT_USER)
    }

    /// Loads saved refresh tokens keyed by account email.
    ///
    /// Returns an empty map if no accounts have been saved.
    /// Returns [`SecretError::Corrupt`] if the stored map cannot be decoded.
    pub fn tokens(&self) -> Result<BTreeMap<String, String>, SecretError> {
        match self.store.get(TOKENS_USER)? {
            None => Ok(BTreeMap::new()),
            Some(raw) => serde_json::from_str(&raw).map_err(|e| SecretError::Corrupt(describe(&e))),
        }
    }

    fn write_tokens(&self, tokens: &BTreeMap<String, String>) -> Result<(), SecretError> {
        if tokens.is_empty() {
            self.store.delete(TOKENS_USER)
        } else {
            self.store.set(
                TOKENS_USER,
                &serde_json::to_string(tokens)
                    .expect("refresh-token maps contain only JSON-serializable strings"),
            )
        }
    }

    /// Saves or replaces the refresh token for `account` without changing other accounts.
    pub fn set_token(&self, account: &str, token: &str) -> Result<(), SecretError> {
        let mut tokens = self.tokens()?;
        tokens.insert(account.to_string(), token.to_string());
        self.write_tokens(&tokens)
    }

    /// Removes `account`'s refresh token; succeeds if it was already absent.
    pub fn remove_token(&self, account: &str) -> Result<(), SecretError> {
        let mut tokens = self.tokens()?;
        if tokens.remove(account).is_some() {
            self.write_tokens(&tokens)?;
        }
        Ok(())
    }

    /// Forgets every account.
    pub fn clear_tokens(&self) -> Result<(), SecretError> {
        self.store.delete(TOKENS_USER)
    }
}

/// The Secret Service via `keyring-core` + zbus. Items go into the default (login) collection,
/// labelled so they are recognisable in Seahorse/KWallet.
pub struct KeyringStore;

/// Connects on first use; a failure is not cached, so a keyring daemon that starts later
/// (or gets unlocked) is picked up on the next attempt.
fn ensure_default_store() -> Result<(), SecretError> {
    static READY: Mutex<bool> = Mutex::new(false);
    let mut ready = READY
        .lock()
        .map_err(|_| SecretError::Unavailable("keyring initialization lock was poisoned".into()))?;
    if !*ready {
        let store = zbus_secret_service_keyring_store::Store::new()
            .map_err(|e| SecretError::Unavailable(e.to_string()))?;
        keyring_core::set_default_store(store);
        *ready = true;
    }
    Ok(())
}

fn entry(user: &str) -> Result<keyring_core::Entry, SecretError> {
    ensure_default_store()?;
    let label = match user {
        CLIENT_USER => "Eloquawk: OAuth client",
        TOKENS_USER => "Eloquawk: Google accounts",
        OPENROUTER_USER => "Eloquawk: OpenRouter API key",
        _ => "Eloquawk",
    };
    keyring_core::Entry::new_with_modifiers(SERVICE, user, &HashMap::from([("label", label)]))
        .map_err(map_err)
}

/// Where and how a stored entry failed to parse, without serde's message: that echoes the
/// offending value, which can be a refresh token or client secret.
fn describe(e: &serde_json::Error) -> String {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "read error",
        serde_json::error::Category::Syntax => "invalid JSON",
        serde_json::error::Category::Data => "unexpected shape",
        serde_json::error::Category::Eof => "truncated JSON",
    };
    format!("{kind} at line {} column {}", e.line(), e.column())
}

fn map_err(e: keyring_core::Error) -> SecretError {
    match e {
        keyring_core::Error::BadEncoding(_) | keyring_core::Error::BadDataFormat(..) => {
            SecretError::Corrupt(e.to_string())
        }
        other => SecretError::Unavailable(other.to_string()),
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, user: &str) -> Result<Option<String>, SecretError> {
        match entry(user)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_err(e)),
        }
    }

    fn set(&self, user: &str, value: &str) -> Result<(), SecretError> {
        entry(user)?.set_password(value).map_err(map_err)
    }

    fn delete(&self, user: &str) -> Result<(), SecretError> {
        match entry(user)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    }
}

/// Process-local store: tests, and debug runs with `ELOQUAWK_SECRETS=memory`.
#[cfg(any(test, debug_assertions))]
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, String>>);

#[cfg(any(test, debug_assertions))]
impl MemoryStore {
    fn items(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        // A panic while holding the lock leaves plain strings behind; they are still usable.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(any(test, debug_assertions))]
impl SecretStore for MemoryStore {
    fn get(&self, user: &str) -> Result<Option<String>, SecretError> {
        Ok(self.items().get(user).cloned())
    }
    fn set(&self, user: &str, value: &str) -> Result<(), SecretError> {
        self.items().insert(user.into(), value.into());
        Ok(())
    }
    fn delete(&self, user: &str) -> Result<(), SecretError> {
        self.items().remove(user);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(id: &str) -> ClientCredentials {
        ClientCredentials {
            client_id: id.into(),
            client_secret: "s".into(),
        }
    }

    #[test]
    fn client_roundtrip_and_token_invalidation() {
        let s = Secrets::memory();
        assert_eq!(s.client().unwrap(), None);
        s.set_client(&client("a")).unwrap();
        s.set_token("me@x", "rt").unwrap();
        // Saving the same client, or rotating its secret, keeps the sessions.
        s.set_client(&client("a")).unwrap();
        s.set_client(&ClientCredentials {
            client_id: "a".into(),
            client_secret: "rotated".into(),
        })
        .unwrap();
        assert_eq!(
            s.tokens().unwrap().get("me@x").map(String::as_str),
            Some("rt")
        );
        // A different client drops every account.
        s.set_client(&client("b")).unwrap();
        assert!(s.tokens().unwrap().is_empty());
        assert_eq!(s.client().unwrap(), Some(client("b")));
        s.set_token("me@x", "rt2").unwrap();
        s.clear_client().unwrap();
        assert_eq!(s.client().unwrap(), None);
        assert!(s.tokens().unwrap().is_empty());
    }

    #[test]
    fn tokens_per_account() {
        let s = Secrets::memory();
        s.set_token("a@x", "1").unwrap();
        s.set_token("b@x", "2").unwrap();
        s.set_token("a@x", "3").unwrap();
        let t = s.tokens().unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t["a@x"], "3");
        s.remove_token("a@x").unwrap();
        assert_eq!(s.tokens().unwrap().keys().collect::<Vec<_>>(), vec!["b@x"]);
        s.remove_token("b@x").unwrap();
        assert!(s.tokens().unwrap().is_empty());
    }

    #[test]
    fn corrupt_entries_are_reported() {
        let store = Arc::new(MemoryStore::default());
        store.set(CLIENT_USER, "not json").unwrap();
        store.set(TOKENS_USER, "[1, 2]").unwrap();
        let s = Secrets::new(store);
        assert!(matches!(s.client(), Err(SecretError::Corrupt(_))));
        assert!(matches!(s.tokens(), Err(SecretError::Corrupt(_))));
    }

    #[test]
    fn corrupt_entries_never_quote_their_contents() {
        let store = Arc::new(MemoryStore::default());
        store.set(TOKENS_USER, r#""1//0g-refresh""#).unwrap();
        let s = Secrets::new(store);
        let Err(SecretError::Corrupt(m)) = s.tokens() else {
            panic!("expected Corrupt");
        };
        assert_eq!(m, "unexpected shape at line 1 column 15");
        assert!(
            !s.seed(r#"{"tokens": "1//0g-refresh"}"#)
                .unwrap_err()
                .contains("0g")
        );
    }

    const KEY: &str = "sk-or-v1-0123456789abcdef0123";

    #[test]
    fn openrouter_key_round_trip_is_independent() {
        let s = Secrets::memory();
        assert!(s.openrouter_key().unwrap().is_none());
        s.set_client(&client("a")).unwrap();
        s.set_token("me@x", "rt").unwrap();
        s.set_openrouter_key(&ApiKey::parse(KEY).unwrap()).unwrap();
        assert_eq!(s.openrouter_key().unwrap().unwrap().expose(), KEY);
        // Switching clients drops the tokens, never the key; removing the key keeps the rest.
        s.set_client(&client("b")).unwrap();
        assert!(s.openrouter_key().unwrap().is_some());
        s.set_token("me@x", "rt").unwrap();
        s.clear_openrouter_key().unwrap();
        assert!(s.openrouter_key().unwrap().is_none());
        assert_eq!(s.tokens().unwrap()["me@x"], "rt");
        assert_eq!(s.client().unwrap(), Some(client("b")));
        s.clear_openrouter_key().unwrap();
    }

    #[test]
    fn an_unparseable_stored_key_is_corrupt_and_not_quoted() {
        let store = Arc::new(MemoryStore::default());
        store
            .set(OPENROUTER_USER, "sk-or- has spaces secretpart")
            .unwrap();
        let s = Secrets::new(store);
        let Err(SecretError::Corrupt(m)) = s.openrouter_key() else {
            panic!("expected Corrupt");
        };
        assert!(!m.contains("secretpart") && !m.contains("sk-or"), "{m}");
    }

    #[test]
    fn seeding_fills_the_openrouter_key() {
        let s = Secrets::memory();
        s.seed(&format!(r#"{{"openrouter_key":"{KEY}"}}"#)).unwrap();
        assert_eq!(s.openrouter_key().unwrap().unwrap().expose(), KEY);
        let err = Secrets::memory()
            .seed(r#"{"openrouter_key":"not-a-key-secretpart"}"#)
            .unwrap_err();
        assert!(!err.contains("secretpart"), "{err}");
    }

    #[test]
    fn seeding_fills_client_then_tokens() {
        let s = Secrets::memory();
        s.seed(
            r#"{"client":{"client_id":"c.apps","client_secret":"x"},
                "tokens":{"me@example.com":"rt"}}"#,
        )
        .unwrap();
        assert_eq!(s.client().unwrap().unwrap().client_id, "c.apps");
        assert_eq!(s.tokens().unwrap()["me@example.com"], "rt");
        let empty = Secrets::memory();
        empty.seed("{}").unwrap();
        assert_eq!(empty.client().unwrap(), None);
        assert!(empty.seed("nope").is_err());
    }
}
