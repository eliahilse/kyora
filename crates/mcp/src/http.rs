//! Streamable HTTP for rmcp on the workspace's reqwest client.
//!
//! rmcp's bundled client is written against a newer reqwest with a different TLS
//! stack. This adapter implements the transport's small client trait instead, so
//! the binary keeps one HTTP and TLS implementation. It also owns every request it
//! sends: all of them can be cancelled at once, wait at most a bounded time for a
//! response, and the session the server assigned is remembered until it is deleted.
use crate::defaults;
use futures::{StreamExt, stream::BoxStream};
use reqwest::{
    RequestBuilder, Response, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE},
};
use rmcp::{
    model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage},
    transport::streamable_http_client::{
        AuthRequiredError, SseError, StreamableHttpClient, StreamableHttpError,
        StreamableHttpPostResponse,
    },
};
use sse_stream::{Sse, SseStream};
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const SESSION_ID: &str = "mcp-session-id";
const LAST_EVENT_ID: &str = "last-event-id";
const EVENT_STREAM: &str = "text/event-stream";
const JSON: &str = "application/json";
const ACCEPTS: &str = "text/event-stream, application/json";
/// Characters of an error body kept in an error message.
const BODY_EXCERPT: usize = 200;

type Error = StreamableHttpError<reqwest::Error>;
type Events = BoxStream<'static, Result<Sse, SseError>>;

#[derive(Clone)]
pub(crate) struct HttpClient {
    http: reqwest::Client,
    shared: Arc<Shared>,
}

struct Shared {
    /// Ends every request in flight, including those rmcp sends during startup.
    cancel: CancellationToken,
    /// Bounds the wait for response headers and for a whole non-streaming body.
    timeout: Duration,
    /// Set when a body or event outgrew its limit, to explain the failure.
    oversized: Arc<AtomicBool>,
    /// What a DELETE of a leftover session needs.
    uri: Arc<str>,
    auth_header: Option<String>,
    headers: HashMap<HeaderName, HeaderValue>,
    /// The session the server assigned and nobody has deleted yet.
    session: Mutex<Option<Arc<str>>>,
}

impl HttpClient {
    pub(crate) fn new(
        http: reqwest::Client,
        timeout: Duration,
        oversized: Arc<AtomicBool>,
        uri: Arc<str>,
        auth_header: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Self {
        let shared = Shared {
            cancel: CancellationToken::new(),
            timeout,
            oversized,
            uri,
            auth_header,
            headers,
            session: Mutex::new(None),
        };
        Self {
            http,
            shared: Arc::new(shared),
        }
    }

    /// Ends every request and stream in flight, then deletes a session that is still
    /// open, for example after a failed startup that rmcp abandoned.
    pub(crate) async fn close(&self) {
        self.shared.cancel.cancel();
        let Some(session) = self.take_session() else {
            return;
        };
        let request = self.http.delete(self.shared.uri.as_ref());
        let request = headers(
            request,
            Some(session),
            self.shared.auth_header.clone(),
            self.shared.headers.clone(),
        );
        let _ = tokio::time::timeout(defaults::DELETE_TIMEOUT, request.send()).await;
    }

    fn take_session(&self) -> Option<Arc<str>> {
        self.shared.session.lock().expect("session poisoned").take()
    }

    fn remember(&self, session: &str) {
        *self.shared.session.lock().expect("session poisoned") = Some(session.into());
    }

    fn forget(&self, session: &str) {
        let mut current = self.shared.session.lock().expect("session poisoned");
        if current.as_deref() == Some(session) {
            *current = None;
        }
    }

    /// Runs `work` unless the client is closed or the request timeout passes.
    async fn bounded<T>(
        &self,
        work: impl Future<Output = Result<T, reqwest::Error>>,
    ) -> Result<T, Error> {
        tokio::select! {
            biased;
            _ = self.shared.cancel.cancelled() => Err(io_error(std::io::ErrorKind::Interrupted, "request cancelled")),
            done = tokio::time::timeout(self.shared.timeout, work) => match done {
                Ok(done) => done.map_err(client_error),
                Err(_) => Err(io_error(std::io::ErrorKind::TimedOut, "request timed out")),
            },
        }
    }

    /// Reads at most `limit` bytes of the body; the flag is false when there was more.
    async fn body(&self, mut response: Response, limit: usize) -> Result<(Vec<u8>, bool), Error> {
        self.bounded(async move {
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if body.len() + chunk.len() > limit {
                    body.extend_from_slice(&chunk[..limit - body.len()]);
                    return Ok((body, false));
                }
                body.extend_from_slice(&chunk);
            }
            Ok((body, true))
        })
        .await
    }

    /// Parses the body as SSE until the client closes, failing once a single event
    /// grows past `limit` bytes.
    fn events(&self, response: Response, limit: usize) -> Events {
        let oversized = self.shared.oversized.clone();
        let mut size = EventSize::new(limit);
        let bytes = response
            .bytes_stream()
            .take_until(self.shared.cancel.clone().cancelled_owned())
            .map(move |chunk| {
                let chunk = chunk.map_err(|error| std::io::Error::other(error.without_url()))?;
                if !size.feed(&chunk) {
                    oversized.store(true, Ordering::SeqCst);
                    return Err(std::io::Error::other("SSE event exceeds the size limit"));
                }
                Ok(chunk)
            });
        SseStream::from_bytes_stream(bytes).boxed()
    }
}

impl StreamableHttpClient for HttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            defaults::MAX_MESSAGE_BYTES,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let attached = session_id.is_some();
        let expects_reply = matches!(message, JsonRpcMessage::Request(_));
        let request = self
            .http
            .post(uri.as_ref())
            .header(ACCEPT, ACCEPTS)
            .json(&message);
        let request = headers(request, session_id, auth_header, custom_headers);
        let response = self.bounded(request.send()).await?;
        let session = response
            .headers()
            .get(SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if let Some(session) = &session {
            self.remember(session);
        }
        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        auth_required(&response)?;
        let kind = content_type(&response);
        if !status.is_success() {
            let (body, whole) = self
                .body(response, defaults::ERROR_BODY_BYTES)
                .await
                .unwrap_or_default();
            // JSON-RPC errors sent with an HTTP error status still answer the request.
            if whole
                && kind.starts_with(JSON)
                && let Ok(error @ JsonRpcMessage::Error(_)) =
                    serde_json::from_slice::<ServerJsonRpcMessage>(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(error, session));
            }
            let body = String::from_utf8_lossy(&body);
            let excerpt: String = body.chars().take(BODY_EXCERPT).collect();
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {}", excerpt.trim()),
            )));
        }
        if kind.starts_with(EVENT_STREAM) {
            return Ok(StreamableHttpPostResponse::Sse(
                self.events(response, max_sse_event_size),
                session,
            ));
        }
        if kind.starts_with(JSON) {
            let (body, whole) = self.body(response, defaults::MAX_MESSAGE_BYTES).await?;
            if !whole {
                self.shared.oversized.store(true, Ordering::SeqCst);
                return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                    format!(
                        "response body exceeds {} bytes",
                        defaults::MAX_MESSAGE_BYTES
                    ),
                )));
            }
            return match serde_json::from_slice(&body) {
                Ok(reply) => Ok(StreamableHttpPostResponse::Json(reply, session)),
                // Notifications and replies need no answer; tolerate a stray body.
                Err(_) if !expects_reply => Ok(StreamableHttpPostResponse::Accepted),
                Err(error) => Err(StreamableHttpError::Deserialize(error)),
            };
        }
        if !expects_reply {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        Err(StreamableHttpError::UnexpectedContentType(
            (!kind.is_empty()).then_some(kind),
        ))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        let request = self.http.delete(uri.as_ref());
        let request = headers(
            request,
            Some(session_id.clone()),
            auth_header,
            custom_headers,
        );
        let response = self.bounded(request.send()).await?;
        // Answered either way; the session needs no second DELETE.
        self.forget(&session_id);
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        response.error_for_status().map_err(client_error)?;
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<Events, Error> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            defaults::MAX_MESSAGE_BYTES,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<Events, Error> {
        let mut request = self.http.get(uri.as_ref()).header(ACCEPT, ACCEPTS);
        if let Some(id) = last_event_id {
            request = request.header(LAST_EVENT_ID, id);
        }
        let request = headers(request, session_id, auth_header, custom_headers);
        let response = self.bounded(request.send()).await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        auth_required(&response)?;
        let response = response.error_for_status().map_err(client_error)?;
        let kind = content_type(&response);
        if !kind.starts_with(EVENT_STREAM) && !kind.starts_with(JSON) {
            return Err(StreamableHttpError::UnexpectedContentType(
                (!kind.is_empty()).then_some(kind),
            ));
        }
        Ok(self.events(response, max_sse_event_size))
    }
}

/// The size of the SSE event being received. As in the SSE parser, a line ends at CR,
/// LF or CRLF, and a blank line ends the event.
struct EventSize {
    limit: usize,
    size: usize,
    line: usize,
    after_cr: bool,
}

impl EventSize {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            size: 0,
            line: 0,
            after_cr: false,
        }
    }

    /// False once the current event exceeds the limit.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        for &byte in bytes {
            match byte {
                // The LF of a CRLF, possibly split across chunks.
                b'\n' if self.after_cr => self.after_cr = false,
                b'\r' | b'\n' => {
                    self.after_cr = byte == b'\r';
                    if self.line == 0 {
                        self.size = 0;
                    }
                    self.line = 0;
                }
                _ => {
                    self.after_cr = false;
                    self.line += 1;
                    self.size += 1;
                    if self.size > self.limit {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// reqwest errors name the request URL; keep what happened without it.
fn client_error(error: reqwest::Error) -> Error {
    StreamableHttpError::Client(error.without_url())
}

fn io_error(kind: std::io::ErrorKind, message: &'static str) -> Error {
    StreamableHttpError::Io(std::io::Error::new(kind, message))
}

fn headers(
    mut request: RequestBuilder,
    session_id: Option<Arc<str>>,
    auth_header: Option<String>,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> RequestBuilder {
    // Includes MCP-Protocol-Version once the handshake has negotiated it.
    for (name, value) in custom_headers {
        request = request.header(name, value);
    }
    if let Some(session) = session_id {
        request = request.header(SESSION_ID, session.as_ref());
    }
    if let Some(token) = auth_header {
        request = request.bearer_auth(token);
    }
    request
}

/// A 401 with a challenge; other failures are reported with their status and body.
fn auth_required(response: &Response) -> Result<(), Error> {
    if response.status() == StatusCode::UNAUTHORIZED
        && let Some(challenge) = response.headers().get(WWW_AUTHENTICATE)
    {
        return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
            String::from_utf8_lossy(challenge.as_bytes()).into_owned(),
        )));
    }
    Ok(())
}

fn content_type(response: &Response) -> String {
    response
        .headers()
        .get(CONTENT_TYPE)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::EventSize;

    #[test]
    fn every_line_ending_separates_events() {
        for ending in ["\n", "\r", "\r\n"] {
            let event = format!("event: message{ending}data: 0123456789{ending}{ending}");
            let mut size = EventSize::new(32);
            for _ in 0..100 {
                assert!(size.feed(event.as_bytes()), "{ending:?}");
            }
            let long = format!("data: 0123456789{ending}").repeat(3);
            assert!(!EventSize::new(32).feed(long.as_bytes()), "{ending:?}");
        }
    }

    #[test]
    fn a_crlf_split_across_chunks_is_one_line_ending() {
        let mut size = EventSize::new(20);
        for chunk in [
            "data: 0123456789\r",
            "\n",
            "\r",
            "\n",
            "data: 0123456789\r\n\r\n",
        ] {
            assert!(size.feed(chunk.as_bytes()));
        }
        // A lone CR then LF is still one line end, so this is one 22 byte event.
        let mut size = EventSize::new(20);
        assert!(size.feed(b"data: 0123456789\r"));
        assert!(!size.feed(b"\ndata: 0"));
    }
}
