//! Streamable HTTP for rmcp on the workspace's reqwest client.
//!
//! rmcp's bundled client is written against a newer reqwest with a different TLS
//! stack. This adapter implements the transport's small client trait instead, so
//! the binary keeps one HTTP and TLS implementation.
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
use std::{borrow::Cow, collections::HashMap, sync::Arc};

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
pub(crate) struct HttpClient(pub(crate) reqwest::Client);

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
            crate::defaults::MAX_SSE_EVENT_BYTES,
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
            .0
            .post(uri.as_ref())
            .header(ACCEPT, ACCEPTS)
            .json(&message);
        let response = headers(request, session_id, auth_header, custom_headers)
            .send()
            .await
            .map_err(client_error)?;
        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        auth_required(&response)?;
        let session = response
            .headers()
            .get(SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let kind = content_type(&response);
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            // JSON-RPC errors sent with an HTTP error status still answer the request.
            if kind.starts_with(JSON)
                && let Ok(error @ JsonRpcMessage::Error(_)) =
                    serde_json::from_str::<ServerJsonRpcMessage>(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(error, session));
            }
            let excerpt: String = body.chars().take(BODY_EXCERPT).collect();
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {}", excerpt.trim()),
            )));
        }
        if kind.starts_with(EVENT_STREAM) {
            return Ok(StreamableHttpPostResponse::Sse(
                events(response, max_sse_event_size),
                session,
            ));
        }
        if kind.starts_with(JSON) {
            let body = response.bytes().await.map_err(client_error)?;
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
        let request = self.0.delete(uri.as_ref());
        let response = headers(request, Some(session_id), auth_header, custom_headers)
            .send()
            .await
            .map_err(client_error)?;
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
            crate::defaults::MAX_SSE_EVENT_BYTES,
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
        let mut request = self.0.get(uri.as_ref()).header(ACCEPT, ACCEPTS);
        if let Some(id) = last_event_id {
            request = request.header(LAST_EVENT_ID, id);
        }
        let response = headers(request, session_id, auth_header, custom_headers)
            .send()
            .await
            .map_err(client_error)?;
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
        Ok(events(response, max_sse_event_size))
    }
}

/// reqwest errors name the request URL; keep what happened without it.
fn client_error(error: reqwest::Error) -> Error {
    StreamableHttpError::Client(error.without_url())
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

/// Parses the body as SSE, failing once a single event grows past `limit` bytes.
fn events(response: Response, limit: usize) -> Events {
    let mut size = 0usize;
    let mut line_start = true;
    let bytes = response.bytes_stream().map(move |chunk| {
        let chunk = chunk.map_err(std::io::Error::other)?;
        for &byte in chunk.iter() {
            match byte {
                // A blank line ends the event.
                b'\n' if line_start => size = 0,
                b'\n' => line_start = true,
                b'\r' => {}
                _ => {
                    line_start = false;
                    size += 1;
                    if size > limit {
                        return Err(std::io::Error::other("SSE event exceeds the size limit"));
                    }
                }
            }
        }
        Ok(chunk)
    });
    SseStream::from_bytes_stream(bytes).boxed()
}
