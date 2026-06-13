//! OAuth 2.0 Device Authorization Grant client (the `smarts login` flow).
//!
//! Calls the gateway's `/connect/cli/device/*` proxy. The CLI drives the UX:
//! [`SmartsClient::start_device_login`] to get the code, then poll
//! [`SmartsClient::poll_device_token`] at the returned interval until it
//! resolves. On success the tokens are stored in the keychain automatically.

use serde_json::json;

use crate::client::SmartsClient;
use crate::credentials;
use crate::error::{Error, Result};

/// What the user needs to do to approve a login.
#[derive(Debug, Clone)]
pub struct DeviceCodeInfo {
    /// Secret the CLI polls with (do not display).
    pub device_code: String,
    /// Short code the user types on the website.
    pub user_code: String,
    /// Page the user visits to approve.
    pub verification_uri: String,
    /// Same page with the code pre-filled (open this directly if present).
    pub verification_uri_complete: Option<String>,
    /// Seconds between polls.
    pub interval: u64,
    /// Seconds until the request expires.
    pub expires_in: u64,
}

/// Result of a single poll of the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePoll {
    /// Not approved yet — keep polling.
    Pending,
    /// Polling too fast — back off, then keep polling.
    SlowDown,
    /// The user denied the request.
    Denied,
    /// The request expired.
    Expired,
    /// Approved — tokens have been stored.
    Approved,
}

impl SmartsClient {
    /// Begin the device-authorization flow.
    pub async fn start_device_login(&self) -> Result<DeviceCodeInfo> {
        let (status, body) = self
            .post_public_raw("/connect/cli/device/code", json!({ "scope": "cli" }))
            .await?;
        if status != 200 {
            let msg = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("could not start login");
            return Err(Error::Other(msg.to_string()));
        }
        let get = |k: &str| body.get(k).and_then(|v| v.as_str()).map(str::to_string);
        Ok(DeviceCodeInfo {
            device_code: get("device_code")
                .ok_or_else(|| Error::Other("no device_code in response".into()))?,
            user_code: get("user_code")
                .ok_or_else(|| Error::Other("no user_code in response".into()))?,
            verification_uri: get("verification_uri")
                .ok_or_else(|| Error::Other("no verification_uri in response".into()))?,
            verification_uri_complete: get("verification_uri_complete"),
            interval: body.get("interval").and_then(|v| v.as_u64()).unwrap_or(5),
            expires_in: body.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(900),
        })
    }

    /// Poll once for the token. On `Approved`, the access + refresh tokens are
    /// persisted to the keychain and adopted by this client.
    pub async fn poll_device_token(&self, device_code: &str) -> Result<DevicePoll> {
        let (status, body) = self
            .post_public_raw("/connect/cli/device/token", json!({ "device_code": device_code }))
            .await?;

        if status == 200 {
            let access = body
                .get("access_token")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Other("no access_token in response".into()))?;
            let refresh = body
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Other("no refresh_token in response".into()))?;
            credentials::set_tokens(access, refresh)?;
            self.adopt_tokens(access, refresh);
            return Ok(DevicePoll::Approved);
        }

        match body.get("error").and_then(|v| v.as_str()).unwrap_or("") {
            "authorization_pending" => Ok(DevicePoll::Pending),
            "slow_down" => Ok(DevicePoll::SlowDown),
            "access_denied" => Ok(DevicePoll::Denied),
            "expired_token" => Ok(DevicePoll::Expired),
            _ => {
                let msg = body
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("login failed");
                Err(Error::Other(msg.to_string()))
            }
        }
    }
}
