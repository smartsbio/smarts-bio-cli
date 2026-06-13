use thiserror::Error;

/// Errors returned by the smarts.bio client.
#[derive(Debug, Error)]
pub enum Error {
    /// The gateway returned a non-2xx response. `code`/`message` come from the
    /// gateway's `{ status, error, message }` error envelope when present.
    #[error("API error {status} ({code}): {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },

    /// No usable credentials were found for an authenticated request.
    #[error("{0}")]
    Auth(String),

    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid response: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// True if the failure was an HTTP 401 from the gateway (used to decide
    /// whether to attempt a token refresh once browser-login lands).
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Error::Api { status: 401, .. })
    }
}

pub type Result<T> = std::result::Result<T, Error>;
