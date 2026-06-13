//! Credential storage for the CLI.
//!
//! The `sk_live_` API key is read from `SMARTSBIO_API_KEY` first (CI/headless),
//! otherwise from the OS keychain. Browser-login OAuth tokens will be stored
//! here too in a later phase under separate keychain entries.

use crate::error::{Error, Result};

const SERVICE: &str = "smarts-bio-cli";
const API_KEY_ENTRY: &str = "api-key";

/// API key from the environment, if set and non-empty.
pub fn api_key_from_env() -> Option<String> {
    std::env::var("SMARTSBIO_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Resolved API key: env var first, then the OS keychain.
pub fn api_key() -> Option<String> {
    if let Some(key) = api_key_from_env() {
        return Some(key);
    }
    keyring::Entry::new(SERVICE, API_KEY_ENTRY)
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .filter(|s| !s.is_empty())
}

/// True when the resolved key (env or keychain) came from the environment.
pub fn api_key_is_from_env() -> bool {
    api_key_from_env().is_some()
}

/// Store an API key in the OS keychain.
pub fn set_api_key(key: &str) -> Result<()> {
    let entry =
        keyring::Entry::new(SERVICE, API_KEY_ENTRY).map_err(|e| Error::Config(e.to_string()))?;
    entry
        .set_password(key)
        .map_err(|e| Error::Config(e.to_string()))
}

/// Remove the stored API key from the OS keychain (no error if absent).
pub fn clear_api_key() -> Result<()> {
    if let Ok(entry) = keyring::Entry::new(SERVICE, API_KEY_ENTRY) {
        let _ = entry.delete_credential();
    }
    Ok(())
}
