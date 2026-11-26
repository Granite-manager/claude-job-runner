//! The Granite Manager API, the only place a job gets data from.
//!
//! Before a job runs, the runner trades `AGENT_RUNNER_KEY` for a token bound
//! to that one question (`POST /v1/ai/agentic-questions/{id}/agent-token`).
//! The token acts as the person who asked, in the company they asked in,
//! with their own permissions (read-only), and dies when the question is
//! answered or after 30 minutes. The job reaches the API through its MCP
//! server (`/v1/mcp`) with that token; it never sees the runner key or any
//! database login.

use std::time::Duration;

use serde::Deserialize;

use crate::job::MessageId;

/// How long the token request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Where the API is and the runner's key to it.
#[derive(Debug, Clone)]
pub struct GraniteApi {
    base_url: String,
    runner_key: String,
    http: reqwest::Client,
}

/// A question's token, as the API issued it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentToken {
    /// `agt_...`, sent as `Authorization: Bearer`.
    pub token: String,
    /// Unix seconds after which the API refuses it.
    pub expires_at: i64,
}

/// Why no token came back.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The API will not answer this question: it is already answered, older
    /// than a week, or its asker can no longer act in that company.
    #[error("the question can no longer be answered (its asker may have lost access)")]
    NotIssued,
    /// The API refused the runner key.
    #[error("the API refused AGENT_RUNNER_KEY")]
    BadKey,
    /// Anything else: the API is down, or answered something unexpected.
    #[error("could not get an agent token: {0}")]
    Other(String),
}

impl GraniteApi {
    /// The API at `base_url` (`https://api.granite-manager.com`), with the
    /// runner's `AGENT_RUNNER_KEY`.
    pub fn new(base_url: &str, runner_key: String) -> Result<Self, reqwest::Error> {
        // reqwest is built without a default crypto provider; use ring.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            runner_key,
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()?,
        })
    }

    /// The API's base URL, without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The MCP server's URL.
    #[must_use]
    pub fn mcp_url(&self) -> String {
        format!("{}/v1/mcp", self.base_url)
    }

    /// A token acting as the asker of `question`.
    pub async fn agent_token(&self, question: MessageId) -> Result<AgentToken, TokenError> {
        let url = format!(
            "{}/v1/ai/agentic-questions/{question}/agent-token",
            self.base_url
        );
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.runner_key)
            .send()
            .await
            .map_err(|err| TokenError::Other(err.to_string()))?;
        match response.status().as_u16() {
            200 | 201 => response
                .json::<AgentToken>()
                .await
                .map_err(|err| TokenError::Other(format!("unexpected answer: {err}"))),
            401 | 403 => Err(TokenError::BadKey),
            404 => Err(TokenError::NotIssued),
            status => Err(TokenError::Other(format!("HTTP {status}"))),
        }
    }
}
