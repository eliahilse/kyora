//! Anthropic Messages API, with one HTTP attempt per stream call.

use std::{
    collections::HashMap,
    fmt,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
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
            .map_err(|err| error::classify_dispatch(error::transport(err), false))?;
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
            return Err(error::classify_dispatch(ProviderError::Cancelled, false));
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
    let send_started = Arc::new(AtomicBool::new(false));
    let result = async {
        let (client, request) = request.build_split();
        let mut request = request.map_err(error::transport)?;
        let body = request.body_mut().take().unwrap_or_default();
        *request.body_mut() = Some(reqwest::Body::wrap(SendTrackedBody {
            inner: body,
            send_started: send_started.clone(),
        }));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => Err(ProviderError::Transport("request timeout".into())),
            response = client.execute(request) => response.map_err(error::transport),
        }
    }
    .await;
    result.map_err(|err| error::classify_dispatch(err, send_started.load(Ordering::Acquire)))
}

/// Track HTTP dispatch rather than polling the connection setup future.
struct SendTrackedBody {
    inner: reqwest::Body,
    send_started: Arc<AtomicBool>,
}

impl http_body::Body for SendTrackedBody {
    type Data = <reqwest::Body as http_body::Body>::Data;
    type Error = reqwest::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.send_started.store(true, Ordering::Release);
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        // Reqwest/Hyper query this when dispatching the HTTP request on an
        // established connection, immediately before writing its headers.
        // Track headers too: body polling alone can miss a sent HTTP/2 HEADERS
        // frame or HTTP/1 headers flushed before the body is polled.
        // DNS, TCP, proxy CONNECT and TLS setup do not query this body.
        self.send_started.store(true, Ordering::Release);
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;
    use crate::AttemptCharge;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    async fn read_client_hello(socket: &mut TcpStream) {
        assert_eq!(socket.read_u8().await.unwrap(), 0x16, "expected TLS");
        assert_eq!(socket.read_u16().await.unwrap() >> 8, 3);
        let length = socket.read_u16().await.unwrap();
        let mut hello = vec![0; usize::from(length)];
        socket.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello[0], 1, "expected ClientHello");
    }

    #[tokio::test]
    async fn timeouts_during_tls_are_not_sent() {
        let short = Duration::from_millis(150);
        let long = Duration::from_secs(2);
        // Exercise the provider deadline, reqwest's whole-request timeout,
        // and reqwest's connect timeout independently, then the review case.
        for (deadline, request_timeout, connect_timeout) in [
            (short, long, long),
            (long, short, long),
            (long, long, short),
            (short, short, long),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("https://{}/v1/messages", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_client_hello(&mut socket).await;
                // Stall TLS and verify the timeout closes before HTTP dispatch.
                let mut remaining = Vec::new();
                socket.read_to_end(&mut remaining).await.unwrap();
                assert!(remaining.is_empty(), "only ClientHello should be sent");
            });
            let request = Client::builder()
                .no_proxy()
                .timeout(request_timeout)
                .connect_timeout(connect_timeout)
                .build()
                .unwrap()
                .post(url)
                .body("test-only-body");
            let error = send_request(
                request,
                &CancellationToken::new(),
                Instant::now() + deadline,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(&error, ProviderError::NotSent(message) if message == "request timeout"),
                "{error:?}"
            );
            assert_eq!(error.charge(), AttemptCharge::Zero);
            tokio::time::timeout(TEST_TIMEOUT, server)
                .await
                .expect("timed out connection should close")
                .unwrap();
        }
    }

    #[tokio::test]
    async fn tls_failure_is_not_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}/v1/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_client_hello(&mut socket).await;
            // A fatal TLS handshake_failure alert, without accepting HTTP.
            socket.write_all(&[21, 3, 3, 0, 2, 2, 40]).await.unwrap();
            let mut remaining = Vec::new();
            socket.read_to_end(&mut remaining).await.unwrap();
            assert!(remaining.is_empty(), "only ClientHello should be sent");
        });
        let request = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(url)
            .body("test-only-body");
        let error = send_request(
            request,
            &CancellationToken::new(),
            Instant::now() + TEST_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ProviderError::NotSent(_)), "{error:?}");
        assert_eq!(error.charge(), AttemptCharge::Zero);
        tokio::time::timeout(TEST_TIMEOUT, server)
            .await
            .expect("failed TLS connection should close")
            .unwrap();
    }

    struct FailingDns;

    impl reqwest::dns::Resolve for FailingDns {
        fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            assert_eq!(name.as_str(), "dispatch-test.invalid");
            Box::pin(async {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "test DNS failure").into())
            })
        }
    }

    #[tokio::test]
    async fn dns_failure_is_not_sent() {
        let request = Client::builder()
            .no_proxy()
            .dns_resolver(Arc::new(FailingDns))
            .build()
            .unwrap()
            .post("http://dispatch-test.invalid/v1/messages")
            .body("test-only-body");
        let error = send_request(
            request,
            &CancellationToken::new(),
            Instant::now() + TEST_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ProviderError::NotSent(_)), "{error:?}");
        assert_eq!(error.charge(), AttemptCharge::Zero);
    }

    #[tokio::test]
    async fn reqwest_timeout_after_dispatch_reserves_usage() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(socket.read_u8().await.unwrap(), b'P', "expected POST");
            let mut remaining = Vec::new();
            socket.read_to_end(&mut remaining).await.unwrap();
        });
        let request = Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(150))
            .build()
            .unwrap()
            .post(url)
            .body("test-only-body");
        let error = send_request(
            request,
            &CancellationToken::new(),
            Instant::now() + TEST_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, ProviderError::Transport(message) if message == "request timeout"),
            "{error:?}"
        );
        assert_eq!(error.charge(), AttemptCharge::Reserved);
        tokio::time::timeout(TEST_TIMEOUT, server)
            .await
            .expect("timed out connection should close")
            .unwrap();
    }

    async fn cancel_pending_connect<F: Future>(
        connecting: F,
        cancel: CancellationToken,
    ) -> F::Output {
        tokio::pin!(connecting);
        // Start a real loopback connection, then hold the connector at its
        // first Pending poll so the cancellation cannot race HTTP dispatch.
        let pending =
            futures::future::poll_fn(|cx| Poll::Ready(connecting.as_mut().poll(cx).is_pending()))
                .await;
        assert!(pending, "connection setup should initially be pending");
        cancel.cancel();
        std::future::pending::<()>().await;
        connecting.await
    }

    #[tokio::test]
    async fn cancellation_during_connect_is_not_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cancel = CancellationToken::new();
        let connector_cancel = cancel.clone();
        let client = Client::builder()
            .no_proxy()
            .connector_layer(tower::util::MapFutureLayer::new(move |connecting| {
                cancel_pending_connect(connecting, connector_cancel.clone())
            }))
            .build()
            .unwrap();
        let request = client
            .post(format!(
                "http://{}/v1/messages",
                listener.local_addr().unwrap()
            ))
            .body("test-only-body");
        let error = send_request(request, &cancel, Instant::now() + TEST_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::NotSent(_)), "{error:?}");
        assert_eq!(error.charge(), AttemptCharge::Zero);
    }

    async fn cancel_after_first_byte(tls: bool) -> ProviderError {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "{}://{}/v1/messages",
            if tls { "https" } else { "http" },
            listener.local_addr().unwrap()
        );
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            if tls {
                read_client_hello(&mut socket).await;
                // Do not reply, leaving the client's TLS handshake pending.
            } else {
                assert_eq!(socket.read_u8().await.unwrap(), b'P', "expected POST");
            }
            server_cancel.cancel();
            // Keep the socket alive until the cancelled attempt closes it.
            let mut remaining = Vec::new();
            socket.read_to_end(&mut remaining).await.unwrap();
            if tls {
                assert!(
                    remaining.is_empty(),
                    "no HTTP bytes should follow ClientHello"
                );
            }
        });
        let request = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(url)
            .body("test-only-body");
        let error = send_request(request, &cancel, Instant::now() + TEST_TIMEOUT)
            .await
            .unwrap_err();
        tokio::time::timeout(TEST_TIMEOUT, server)
            .await
            .expect("cancelled connection should close")
            .unwrap();
        error
    }

    #[tokio::test]
    async fn cancellation_during_tls_is_not_sent() {
        let error = cancel_after_first_byte(true).await;
        assert!(matches!(error, ProviderError::NotSent(_)), "{error:?}");
        assert_eq!(error.charge(), AttemptCharge::Zero);
    }

    #[tokio::test]
    async fn cancellation_after_first_request_byte_reserves_usage() {
        let error = cancel_after_first_byte(false).await;
        assert!(matches!(error, ProviderError::Cancelled), "{error:?}");
        assert_eq!(error.charge(), AttemptCharge::Reserved);
    }

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
