use futures_util::StreamExt;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::Value;

use crate::config::Config;
use crate::credentials;
use crate::error::{Error, Result};

/// Thin async client over the `bioinformatics-api` gateway (`/v1` surface).
///
/// Authenticates with an `sk_live_` API key (env or keychain) as a Bearer
/// token. Browser-login JWTs will plug in here later without changing callers.
#[derive(Clone)]
pub struct SmartsClient {
    http: Client,
    base_url: String,
    api_key: Option<String>,
}

impl SmartsClient {
    /// Build a client from config, resolving credentials eagerly.
    pub fn new(config: &Config) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("smarts-cli/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base_url: config
                .resolved_base_url()
                .trim_end_matches('/')
                .to_string(),
            api_key: credentials::api_key(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether any credentials are available for authenticated requests.
    pub fn has_credentials(&self) -> bool {
        self.api_key.is_some()
    }

    /// Shared reqwest client (used for unauthenticated S3 presigned PUTs).
    pub(crate) fn raw(&self) -> &Client {
        &self.http
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Start an authenticated request, erroring early if no credentials exist.
    pub(crate) fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let key = self.api_key.as_ref().ok_or_else(|| {
            Error::Auth(
                "not authenticated — run `smarts auth set-key sk_live_...` or set \
                 SMARTSBIO_API_KEY (browser login arrives in a later phase)"
                    .into(),
            )
        })?;
        Ok(self.http.request(method, self.url(path)).bearer_auth(key))
    }

    /// Send a built request and parse the JSON body, mapping non-2xx responses
    /// to [`Error::Api`] using the gateway's `{ error, message }` envelope.
    pub(crate) async fn send(&self, rb: RequestBuilder) -> Result<Value> {
        let resp = rb.send().await?;
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
    pub(crate) async fn stream_sse<F>(
        &self,
        path: &str,
        body: Value,
        mut on_event: F,
    ) -> Result<()>
    where
        F: FnMut(Value),
    {
        let resp = self
            .request(Method::POST, path)?
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await?;
            // Stream errors come back as a normal JSON error envelope, not SSE.
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
                .unwrap_or_else(|| {
                    if text.is_empty() {
                        "stream request failed".into()
                    } else {
                        text
                    }
                });
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
            // SSE frames are separated by a blank line.
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
