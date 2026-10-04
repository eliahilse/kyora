//! Anthropic Messages API, with one HTTP attempt per stream call.

use std::{
    collections::HashMap,
    fmt,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use async_trait::async_trait;
use kyora_protocol::{ModelInfo, ModelRequest};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{EventStream, ModelProvider, ProviderError};

mod error;
mod models;
mod request;
mod sse;
mod stream;

pub use models::{ModelCaps, default_model_caps};
pub use request::build_request;

/// Transport, thinking, and model capability settings.
/// All product defaults are defined by [`AnthropicConfig::new`] and
/// [`default_model_caps`]; every setting can be overridden before construction.
#[derive(Clone)]
pub struct AnthropicConfig {
    /// API credential. Debug output always redacts it.
    pub api_key: String,
    /// API origin, including an optional proxy path prefix.
    pub base_url: String,
    /// Anthropic API version header.
    pub anthropic_version: String,
    /// Maximum wait between complete SSE events, including pings.
    pub idle_timeout: Duration,
    /// Maximum duration of the whole HTTP attempt, including streaming.
    pub request_timeout: Duration,
    /// Maximum duration for connection setup.
    pub connect_timeout: Duration,
    /// Additional beta names, deduplicated after required betas.
    pub extra_betas: Vec<String>,
    /// Thinking prefix mismatch policy, normally `error` or `drop_block`.
    pub prefix_mismatch_behavior: String,
    /// Whether to send thinking block binding and its required beta.
    pub enable_block_binding: bool,
    /// Capability and fallback limit overrides. The longest matching prefix wins.
    pub model_caps: HashMap<String, ModelCaps>,
}

impl AnthropicConfig {
    /// Creates default settings with an explicit credential.
    ///
    /// Defaults: `https://api.anthropic.com`, version `2023-06-01`, idle
    /// timeout 300 seconds, whole attempt 30 minutes, connection 30 seconds,
    /// no extra betas, prefix mismatch `error`, and block binding enabled.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
            anthropic_version: "2023-06-01".into(),
            idle_timeout: Duration::from_secs(300),
            request_timeout: Duration::from_secs(30 * 60),
            connect_timeout: Duration::from_secs(30),
            extra_betas: Vec::new(),
            prefix_mismatch_behavior: "error".into(),
            enable_block_binding: true,
            model_caps: default_model_caps(),
        }
    }

    /// Reads required `ANTHROPIC_API_KEY` and optional `ANTHROPIC_BASE_URL`.
    /// Missing, empty, or non-Unicode credentials are rejected without printing them.
    pub fn from_env() -> Result<Self, ProviderError> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| ProviderError::Other("ANTHROPIC_API_KEY is required".into()))?;
        let mut config = Self::new(key);
        match std::env::var("ANTHROPIC_BASE_URL") {
            Ok(url) => config.base_url = url,
            Err(std::env::VarError::NotPresent) => {}
            Err(_) => {
                return Err(ProviderError::Other(
                    "ANTHROPIC_BASE_URL must be Unicode".into(),
                ));
            }
        }
        Ok(config)
    }
}

impl fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redact every string setting too, in case a credential was copied there.
        f.debug_struct("AnthropicConfig")
            .field("api_key", &"[redacted]")
            .field("base_url", &error::redact(&self.base_url, &self.api_key))
            .field(
                "anthropic_version",
                &error::redact(&self.anthropic_version, &self.api_key),
            )
            .field("idle_timeout", &self.idle_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field(
                "extra_betas",
                &self
                    .extra_betas
                    .iter()
                    .map(|beta| error::redact(beta, &self.api_key))
                    .collect::<Vec<_>>(),
            )
            .field(
                "prefix_mismatch_behavior",
                &error::redact(&self.prefix_mismatch_behavior, &self.api_key),
            )
            .field("enable_block_binding", &self.enable_block_binding)
            .finish_non_exhaustive()
    }
}

/// Streaming Anthropic backend. Dropping its event stream drops the connection.
/// No retries or background tasks are started by this provider.
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: Client,
}

type ModelCache = Mutex<HashMap<(String, String), ModelInfo>>;
static MODEL_CACHE: OnceLock<ModelCache> = OnceLock::new();

impl AnthropicProvider {
    /// Builds a rustls HTTP client using the supplied settings.
    /// Redirects are disabled so credentials cannot be forwarded to another origin.
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        let mut headers = HeaderMap::new();
        let mut key = HeaderValue::from_str(&config.api_key)
            .map_err(|_| ProviderError::NotSent("invalid API credential".into()))?;
        key.set_sensitive(true);
        headers.insert("x-api-key", key);
        headers.insert(
            "anthropic-version",
            HeaderValue::from_str(&config.anthropic_version)
                .map_err(|_| ProviderError::NotSent("invalid API version".into()))?,
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let client = Client::builder()
            .default_headers(headers)
            .user_agent(concat!("kyora/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(error::transport)?;
        Ok(Self { config, client })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/v1/{path}", self.config.base_url.trim_end_matches('/'))
    }

    async fn discover_model(&self, model: &str) -> Option<ModelInfo> {
        // Append the id as one encoded path segment, never as a query or path.
        let mut url = reqwest::Url::parse(&self.endpoint("models/")).ok()?;
        url.path_segments_mut().ok()?.pop_if_empty().push(model);
        let response = self
            .client
            .get(url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let value: serde_json::Value = response.json().await.ok()?;
        Some(ModelInfo {
            id: model.into(),
            context_window: Some(value["max_input_tokens"].as_u64()?),
            max_output_tokens: Some(u32::try_from(value["max_tokens"].as_u64()?).ok()?),
        })
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled(false));
        }
        let deadline = Instant::now() + self.config.request_timeout;
        let caps = models::resolve(&self.config.model_caps, &req.model);
        let (body, betas) = build_request(&req, &caps, &self.config);
        let mut request = self.client.post(self.endpoint("messages")).json(&body);
        if !betas.is_empty() {
            request = request.header("anthropic-beta", betas.join(","));
        }
        let response = send_request(request, &cancel, deadline).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                _ = tokio::time::sleep_until(deadline) => None,
                _ = tokio::time::sleep(self.config.idle_timeout) => None,
                bytes = response.bytes() => bytes.ok(),
            };
            // The body only enriches an error whose status and headers are known.
            return Err(error::http(
                status,
                &headers,
                bytes.as_deref().unwrap_or_default(),
                &self.config.api_key,
            ));
        }
        Ok(stream::response_stream(
            response,
            cancel,
            self.config.idle_timeout,
            deadline,
            self.config.api_key.clone(),
        ))
    }

    async fn model_info(&self, model: &str) -> Result<ModelInfo, ProviderError> {
        let key = (
            self.config.base_url.trim_end_matches('/').to_owned(),
            model.to_owned(),
        );
        let cache = MODEL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(info) = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
        {
            return Ok(info);
        }
        if let Some(info) = self.discover_model(model).await {
            cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, info.clone());
            return Ok(info);
        }
        Ok(models::resolve(&self.config.model_caps, model).info(model))
    }
}

async fn send_request(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<reqwest::Response, ProviderError> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let send_started = AtomicBool::new(false);
    let sending = async {
        // Before the first poll no bytes can have been sent. Once polled,
        // reqwest may send them, so cancellation must reserve unknown usage.
        send_started.store(true, Ordering::Relaxed);
        request.send().await
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ProviderError::cancelled(send_started.load(Ordering::Relaxed))),
        _ = tokio::time::sleep_until(deadline) => Err(ProviderError::Transport("request timeout".into())),
        response = sending => response.map_err(error::transport),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AttemptCharge;

    #[tokio::test]
    async fn cancellation_after_precheck_before_send_poll_is_not_sent() {
        let cancel = CancellationToken::new();
        assert!(!cancel.is_cancelled());
        let request = Client::new().post("http://127.0.0.1:1/v1/messages");
        cancel.cancel();
        let error = send_request(request, &cancel, Instant::now() + Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::NotSent(_)));
        assert_eq!(error.charge(), AttemptCharge::Zero);
    }
}
