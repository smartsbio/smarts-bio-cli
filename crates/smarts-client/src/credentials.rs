//! Credential storage for the CLI.
//!
//! All secrets live in a **single** OS-keychain item (a small JSON blob), so a
//! cold start reads the keychain exactly once — one macOS prompt instead of one
//! per credential. Auth precedence (highest first):
//!   1. `SMARTSBIO_API_KEY` env var (CI / headless)
//!   2. OAuth tokens from `smarts login` (access + refresh)
//!   3. an `sk_live_` API key stored via `smarts auth set-key`

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const SERVICE: &str = "smarts-bio-cli";
const ENTRY: &str = "credentials";

/// Everything stored in the one keychain item.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Stored {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl Stored {
    fn is_empty(&self) -> bool {
        self.api_key.is_none() && self.access_token.is_none() && self.refresh_token.is_none()
    }
}

fn entry() -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ENTRY).map_err(|e| Error::Config(e.to_string()))
}

/// Read the credential blob from the keychain (one access). Missing/empty/
/// unparseable → defaults.
pub fn load() -> Stored {
    keyring::Entry::new(SERVICE, ENTRY)
        .ok()
        .and_then(|e| e.get_password().ok())
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write the blob (deleting the item entirely when empty).
fn save(stored: &Stored) -> Result<()> {
    let e = entry()?;
    if stored.is_empty() {
        let _ = e.delete_credential();
        return Ok(());
    }
    let json = serde_json::to_string(stored).map_err(|e| Error::Config(e.to_string()))?;
    e.set_password(&json).map_err(|e| Error::Config(e.to_string()))
}

/// API key from the environment, if set and non-empty.
pub fn api_key_from_env() -> Option<String> {
    std::env::var("SMARTSBIO_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
}

/// True when an API key is present via the environment.
pub fn api_key_is_from_env() -> bool {
    api_key_from_env().is_some()
}

// ---- mutators (load → modify → save) -------------------------------------

pub fn set_api_key(key: &str) -> Result<()> {
    let mut s = load();
    s.api_key = Some(key.to_string());
    save(&s)
}

pub fn set_tokens(access: &str, refresh: &str) -> Result<()> {
    let mut s = load();
    s.access_token = Some(access.to_string());
    s.refresh_token = Some(refresh.to_string());
    save(&s)
}

pub fn clear_tokens() -> Result<()> {
    let mut s = load();
    s.access_token = None;
    s.refresh_token = None;
    save(&s)
}

pub fn clear_api_key() -> Result<()> {
    let mut s = load();
    s.api_key = None;
    save(&s)
}

/// Remove every stored credential (single keychain item is deleted).
pub fn clear_all() -> Result<()> {
    save(&Stored::default())
}
