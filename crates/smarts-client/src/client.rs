use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::Value;

use crate::config::Config;
use crate::credentials;
use crate::error::{Error, Result};

/// Where the active credential came from (for `auth status` + refresh logic).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenSource {
    None,
    EnvApiKey,
    KeychainApiKey,
    Login,
}

/// Thin async client over the `bioinformatics-api` gateway (`/v1` surface),
/// plus the unauthenticated `/connect/cli/device/*` device-flow endpoints.
///
/// Auth precedence: `SMARTSBIO_API_KEY` env > `smarts login` JWT > stored
/// `sk_live_` key. When authed via login, a 401 transparently refreshes the
/// access token (rotating the refresh token) and retries once.
#[derive(Clone)]
pub struct SmartsClient {
    http: Client,
    base_url: String,
    token: Arc<Mutex<Option<String>>>,
    refresh: Arc<Mutex<Option<String>>>,
    source: TokenSource,
}

impl SmartsClient {
    pub fn new(config: &Config) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("smarts-cli/", env!("CARGO_PKG_VERSION")))
            .build()?;

        // One keychain read for the whole credential blob.
        let stored = credentials::load();
        let (token, refresh, source) = if let Some(k) = credentials::api_key_from_env() {
            (Some(k), None, TokenSource::EnvApiKey)
        } else if let Some(access) = stored.access_token {
            (Some(access), stored.refresh_token, TokenSource::Login)
        } else if let Some(k) = stored.api_key {
            (Some(k), None, TokenSource::KeychainApiKey)
        } else {
            (None, None, TokenSource::None)
        };

        Ok(Self {
            http,
            base_url: config
                .resolved_base_url()
                .trim_end_matches('/')
                .to_string(),
            token: Arc::new(Mutex::new(token)),
            refresh: Arc::new(Mutex::new(refresh)),
            source,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn has_credentials(&self) -> bool {
        self.token.lock().unwrap().is_some()
    }

    pub fn token_source(&self) -> TokenSource {
        self.source
    }

    fn current_token(&self) -> Option<String> {
        self.token.lock().unwrap().clone()
    }

    pub(crate) fn raw(&self) -> &Client {
        &self.http
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Start an authenticated request (auth header is added by [`Self::send`]).
    pub(crate) fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.request(method, self.url(path)))
    }

    /// Send a built request with the current bearer token. On a 401 while logged
    /// in, refresh once and retry. Parses the JSON body / maps non-2xx to errors.
    pub(crate) async fn send(&self, rb: RequestBuilder) -> Result<Value> {
        let token = self.current_token().ok_or_else(|| {
            Error::Auth(
                "not authenticated — run `smarts login`, or set SMARTSBIO_API_KEY / `smarts auth set-key`"
                    .into(),
            )
        })?;

        let retry = rb.try_clone();
        let resp = rb.bearer_auth(&token).send().await?;

        if resp.status().as_u16() == 401 && self.source == TokenSource::Login {
            if let (Some(retry), Ok(new_token)) = (retry, self.refresh_access().await) {
                let resp2 = retry.bearer_auth(&new_token).send().await?;
                return Self::parse(resp2).await;
            }
        }
        Self::parse(resp).await
    }

    async fn parse(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let text = resp.text().await?;
        let body: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()))
        };

        if !status.is_success() {
            let code = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string();
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if text.is_empty() {
                        status.canonical_reason().unwrap_or("request failed").into()
                    } else {
                        text.clone()
                    }
                });
            return Err(Error::Api {
                status: status.as_u16(),
                code,
                message,
            });
        }
        Ok(body)
    }

    /// Exchange the stored refresh token for a new access token (rotating the
    /// refresh token), updating both the in-memory state and the keychain.
    async fn refresh_access(&self) -> Result<String> {
        let refresh = self
            .refresh
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Error::Auth("session expired — run `smarts login` again".into()))?;

        let resp = self
            .http
            .post(self.url("/connect/cli/device/refresh"))
            .json(&serde_json::json!({ "refresh_token": refresh }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Auth("session expired — run `smarts login` again".into()));
        }
        let body: Value = resp.json().await?;
        let access = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Other("no access_token in refresh response".into()))?
            .to_string();
        let new_refresh = body
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or(refresh);

        *self.token.lock().unwrap() = Some(access.clone());
        *self.refresh.lock().unwrap() = Some(new_refresh.clone());
        let _ = credentials::set_tokens(&access, &new_refresh);
        Ok(access)
    }

    // ---- unauthenticated POST (device-flow endpoints) --------------------

    /// POST a public endpoint without auth, returning `(status, body)` so the
    /// caller can interpret device-flow error codes (authorization_pending, …).
    pub(crate) async fn post_public_raw(&self, path: &str, body: Value) -> Result<(u16, Value)> {
        let resp = self.http.post(self.url(path)).json(&body).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Ok((status, value))
    }

    /// Store freshly issued tokens (after a successful device login).
    pub(crate) fn adopt_tokens(&self, access: &str, refresh: &str) {
        *self.token.lock().unwrap() = Some(access.to_string());
        *self.refresh.lock().unwrap() = Some(refresh.to_string());
    }

    // ---- convenience -----------------------------------------------------

    pub(crate) async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let rb = self.request(Method::GET, path)?.query(query);
        self.send(rb).await
    }

    pub(crate) async fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let rb = self.request(Method::POST, path)?.json(&body);
        self.send(rb).await
    }

    pub(crate) async fn put_json(&self, path: &str, body: Value) -> Result<Value> {
        let rb = self.request(Method::PUT, path)?.json(&body);
        self.send(rb).await
    }

    /// Stream an SSE endpoint, invoking `on_event` with each decoded `data:` JSON
    /// object as soon as a complete frame arrives.
    pub(crate) async fn stream_sse<F>(&self, path: &str, body: Value, mut on_event: F) -> Result<()>
    where
        F: FnMut(Value),
    {
        let token = self.current_token().ok_or_else(|| {
            Error::Auth(
                "not authenticated — run `smarts login`, or set SMARTSBIO_API_KEY / `smarts auth set-key`"
                    .into(),
            )
        })?;

        let resp = self
            .request(Method::POST, path)?
            .bearer_auth(&token)
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await?;
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let code = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string();
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| if text.is_empty() { "stream request failed".into() } else { text });
            return Err(Error::Api {
                status: status.as_u16(),
                code,
                message,
            });
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find("\n\n") {
                let frame: String = buf.drain(..pos + 2).collect();
                for line in frame.lines() {
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(data) {
                        on_event(value);
                    }
                }
            }
        }
        Ok(())
    }
}
