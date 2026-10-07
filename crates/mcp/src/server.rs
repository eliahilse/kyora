//! Server lifecycle: connect, initialize, list tools, call tools and shut down.
use crate::{
    config::{McpConfig, ServerConfig, validate_name},
    defaults,
    http::HttpClient,
    process::Process,
    tool::{McpTool, render_result},
};
use anyhow::{Context, Result, anyhow, bail};
use kyora_core::{Tool, ToolOutput, ToolSelection, Toolset, ToolsetFactory, runtime::NodeInfo};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    ClientHandler, Peer, RoleClient, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientConfig, ClientRequest, Implementation,
        PaginatedRequestParams, ProtocolVersion, Request,
    },
    service::{
        ClientInitializeError, MaybeSendFuture, NotificationContext, PeerRequestOptions,
        RequestHandle, RunningService,
    },
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    sync::{Mutex, Notify},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// Answers server requests with rmcp's defaults (no sampling, roots or elicitation)
/// and turns `notifications/tools/list_changed` into a refresh.
#[derive(Clone)]
struct Handler {
    changed: Arc<Notify>,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("kyora", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        self.changed.notify_one();
        std::future::ready(())
    }
}

/// The request side of one server connection, shared by its tools.
pub(crate) struct Connection {
    server: String,
    peer: Peer<RoleClient>,
    timeout: Duration,
}

impl Connection {
    pub(crate) fn server(&self) -> &str {
        &self.server
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Calls `tool` until it answers, `cancel` fires or `deadline` passes. The last two
    /// send `notifications/cancelled` for the request.
    pub(crate) async fn call(
        &self,
        tool: &str,
        input: Value,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> ToolOutput {
        let Value::Object(arguments) = input else {
            return ToolOutput::error("tool input must be a JSON object");
        };
        if cancel.is_cancelled() {
            return ToolOutput::error("cancelled");
        }
        let request = ClientRequest::CallToolRequest(Request::new(
            CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments),
        ));
        let handle = match self
            .peer
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => return self.failure(error),
        };
        let mut pending = Pending(Some(handle));
        let response = {
            let response = &mut pending.0.as_mut().expect("pending request").rx;
            tokio::select! {
                biased;
                response = response => Ok(response),
                _ = cancel.cancelled() => Err("cancelled"),
                _ = tokio::time::sleep_until(deadline) => Err("timed out"),
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(reason) => {
                pending.cancel(reason).await;
                return ToolOutput::error(if reason == "cancelled" {
                    reason.to_owned()
                } else {
                    format!("{reason} after {:?}", self.timeout)
                });
            }
        };
        pending.0 = None;
        match response {
            Ok(Ok(result)) => match serde_json::to_value(&result) {
                Ok(value) => render_result(&value),
                Err(error) => ToolOutput::error(format!("invalid tool result: {error}")),
            },
            Ok(Err(error)) => self.failure(error),
            // The connection ended and dropped the pending request.
            Err(_) => self.failure(ServiceError::TransportClosed),
        }
    }

    fn failure(&self, error: ServiceError) -> ToolOutput {
        ToolOutput::error(match error {
            ServiceError::McpError(error) => format!("error {}: {}", error.code.0, error.message),
            ServiceError::TransportClosed => {
                format!("mcp server {} closed the connection", self.server)
            }
            ServiceError::TransportSend(error) => {
                format!("mcp server {}: {}", self.server, chain(&*error.error))
            }
            error => format!("mcp server {}: {error}", self.server),
        })
    }
}

/// rmcp names the transport's Rust type in its errors; keep what happened instead.
fn initialize_error(error: ClientInitializeError) -> anyhow::Error {
    match error {
        ClientInitializeError::TransportError { error, context } => {
            anyhow!("{context}: {}", chain(&*error.error))
        }
        error => error.into(),
    }
}

fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// An in-flight call; dropping it unanswered tells the server to stop.
struct Pending(Option<RequestHandle<RoleClient>>);

impl Pending {
    async fn cancel(&mut self, reason: &str) {
        if let Some(handle) = self.0.take() {
            let notice = handle.cancel(Some(reason.into()));
            let _ = tokio::time::timeout(defaults::CANCEL_NOTICE, notice).await;
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // Reached when the runtime drops a read-only call on cancellation.
        if let Some(handle) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let notice = handle.cancel(Some("cancelled".into()));
                let _ = tokio::time::timeout(defaults::CANCEL_NOTICE, notice).await;
            });
        }
    }
}

/// A running MCP server and the tools it currently offers.
pub struct Server {
    name: String,
    config: ServerConfig,
    connection: Arc<Connection>,
    tools: RwLock<Vec<Arc<dyn Tool>>>,
    service: Mutex<Option<RunningService<RoleClient, Handler>>>,
    process: Mutex<Option<Process>>,
    closed: CancellationToken,
}

impl Server {
    /// Starts one server: spawn or connect, initialize, then list its tools, all within
    /// the startup timeout. `env` is the environment stdio servers and HTTP credential
    /// lookups draw from. On failure nothing is left running.
    pub async fn start(
        name: &str,
        config: &ServerConfig,
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> Result<Arc<Self>> {
        validate_name(name)?;
        config.validate()?;
        let changed = Arc::new(Notify::new());
        let handler = Handler {
            changed: changed.clone(),
        };
        let mut process = None;
        let startup = config.startup_timeout();
        let started = tokio::time::timeout(startup, async {
            let service = if let Some(command) = &config.command {
                let dir = config
                    .cwd
                    .as_ref()
                    .map_or_else(|| cwd.to_path_buf(), |dir| cwd.join(dir));
                let (child, stdout, stdin) = Process::spawn(config, command, &dir, env)?;
                process = Some(child);
                handler
                    .serve((stdout, stdin))
                    .await
                    .map_err(initialize_error)?
            } else {
                handler
                    .serve(http_transport(config, env)?)
                    .await
                    .map_err(initialize_error)?
            };
            let info = service
                .peer_info()
                .ok_or_else(|| anyhow!("no initialize result"))?;
            let supported = ProtocolVersion::known_up_to(&ProtocolVersion::LATEST_WITH_INITIALIZE);
            if !supported.contains(&info.protocol_version) {
                bail!("unsupported protocol version {}", info.protocol_version);
            }
            let connection = Arc::new(Connection {
                server: name.to_owned(),
                peer: service.peer().clone(),
                timeout: config.tool_timeout(),
            });
            // Servers without the tools capability contribute nothing.
            let tools = if info.capabilities.tools.is_some() {
                list(&connection, config).await?
            } else {
                Vec::new()
            };
            Ok((service, connection, tools))
        })
        .await;
        let (service, connection, tools) = match started {
            Ok(Ok(started)) => started,
            failed => {
                let error = match failed {
                    Ok(Err(error)) => error,
                    _ => anyhow!("startup timed out after {startup:?}"),
                };
                let stderr = match process {
                    Some(process) => process.kill().await,
                    None => String::new(),
                };
                if stderr.is_empty() {
                    return Err(error);
                }
                return Err(anyhow!("{error:#} (stderr: {stderr})"));
            }
        };
        let server = Arc::new(Self {
            name: name.to_owned(),
            config: config.clone(),
            connection,
            tools: RwLock::new(tools),
            service: Mutex::new(Some(service)),
            process: Mutex::new(process),
            closed: CancellationToken::new(),
        });
        let weak = Arc::downgrade(&server);
        let closed = server.closed.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = closed.cancelled() => break,
                    _ = changed.notified() => {}
                }
                let Some(server) = weak.upgrade() else { break };
                server.refresh().await;
            }
        });
        Ok(server)
    }

    /// The configured name, used in every tool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The tools offered now. Nodes freeze the set they receive when they start.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().expect("tool list poisoned").clone()
    }

    /// Calls a tool by its server-side name with the configured timeout.
    pub async fn call(&self, tool: &str, input: Value, cancel: &CancellationToken) -> ToolOutput {
        let deadline = Instant::now() + self.connection.timeout;
        self.connection.call(tool, input, cancel, deadline).await
    }

    /// Lists the tools again after `notifications/tools/list_changed`. A failed refresh
    /// keeps the previous list.
    async fn refresh(&self) {
        let listing = list(&self.connection, &self.config);
        if let Ok(Ok(tools)) = tokio::time::timeout(self.config.startup_timeout(), listing).await {
            *self.tools.write().expect("tool list poisoned") = tools;
        }
    }

    /// Closes the connection. A stdio server gets end of file on stdin, then SIGTERM and
    /// SIGKILL for its process group if it does not exit in time.
    pub async fn shutdown(&self) {
        self.closed.cancel();
        if let Some(mut service) = self.service.lock().await.take() {
            let _ = service.close_with_timeout(defaults::EXIT_GRACE).await;
        }
        if let Some(process) = self.process.lock().await.take() {
            process.stop().await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Ends the refresh task when a server is dropped without shutdown.
        self.closed.cancel();
    }
}

/// Follows `nextCursor` until the listing ends.
async fn list(connection: &Arc<Connection>, config: &ServerConfig) -> Result<Vec<Arc<dyn Tool>>> {
    let mut listed = Vec::new();
    let mut cursor = None;
    for _ in 0..defaults::MAX_LIST_PAGES {
        let page = connection
            .peer
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .context("tools/list")?;
        listed.extend(page.tools);
        match page.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => {
                let mut names = BTreeSet::new();
                return Ok(listed
                    .into_iter()
                    .filter(|tool| config.exposes(&tool.name))
                    .map(|tool| McpTool::new(connection.clone(), tool))
                    // A sanitized name equal to an earlier one keeps the first tool.
                    .filter(|tool| names.insert(tool.name().to_owned()))
                    .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
                    .collect());
            }
        }
    }
    bail!(
        "tools/list did not finish within {} pages",
        defaults::MAX_LIST_PAGES
    )
}

fn http_transport(
    config: &ServerConfig,
    env: &[(OsString, OsString)],
) -> Result<StreamableHttpClientTransport<HttpClient>> {
    let lookup = |variable: &str| {
        env.iter()
            .find(|(name, _)| name == variable)
            .and_then(|(_, value)| value.to_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("environment variable {variable} is not set"))
    };
    let mut headers = HashMap::new();
    for (name, value) in &config.headers {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value)?,
        );
    }
    for (name, variable) in &config.env_headers {
        let mut value = HeaderValue::from_str(&lookup(variable)?)
            .map_err(|_| anyhow!("environment variable {variable} is not a valid header value"))?;
        value.set_sensitive(true);
        headers.insert(HeaderName::from_bytes(name.as_bytes())?, value);
    }
    let url = config.url.as_deref().expect("validated url");
    let mut transport = StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(headers)
        .max_sse_event_size(defaults::MAX_SSE_EVENT_BYTES);
    if let Some(variable) = &config.bearer_token_env {
        transport = transport.auth_header(lookup(variable)?);
    }
    // Redirects are refused: following one would send env-sourced headers, and on a
    // scheme downgrade the bearer token, to a location the config never named.
    let client = HttpClient(
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
    );
    Ok(StreamableHttpClientTransport::with_client(
        client, transport,
    ))
}

/// Every configured server that started. Failures are returned, never fatal.
#[derive(Default)]
pub struct Servers {
    servers: Vec<Arc<Server>>,
}

impl Servers {
    /// Starts every enabled server concurrently with kyora's own environment.
    pub async fn start(config: &McpConfig, cwd: &Path) -> (Self, Vec<anyhow::Error>) {
        let env: Vec<_> = std::env::vars_os().collect();
        Self::start_with_env(config, cwd, &env).await
    }

    /// Starts every enabled server concurrently. Each failure names its server; the
    /// other servers keep running.
    pub async fn start_with_env(
        config: &McpConfig,
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> (Self, Vec<anyhow::Error>) {
        let starts = config
            .servers
            .iter()
            .filter(|(_, server)| server.enabled)
            .map(|(name, server)| async move {
                Server::start(name, server, cwd, env)
                    .await
                    .with_context(|| format!("mcp server {name}"))
            });
        let mut servers = Vec::new();
        let mut failures = Vec::new();
        for result in futures::future::join_all(starts).await {
            match result {
                Ok(server) => servers.push(server),
                Err(error) => failures.push(error),
            }
        }
        (Self { servers }, failures)
    }

    /// The running servers, in name order.
    pub fn servers(&self) -> &[Arc<Server>] {
        &self.servers
    }

    /// The current tools of every server.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.servers
            .iter()
            .flat_map(|server| server.tools())
            .collect()
    }

    /// Shuts every server down concurrently; see [`Server::shutdown`].
    pub async fn shutdown(&self) {
        futures::future::join_all(self.servers.iter().map(|server| server.shutdown())).await;
    }
}

/// Node toolsets made of fixed tools plus the servers' tools at node start.
pub struct McpToolsets {
    base: Vec<Arc<dyn Tool>>,
    servers: Arc<Servers>,
}

impl McpToolsets {
    /// Combines `base` (for example the built-in tools) with MCP tools.
    pub fn new(base: &Toolset, servers: Arc<Servers>) -> Self {
        let base = base
            .specs()
            .iter()
            .filter_map(|spec| base.get(&spec.name))
            .collect();
        Self { base, servers }
    }

    /// Every tool a node would be offered now, before its selection applies.
    pub fn snapshot(&self) -> Result<Toolset> {
        Toolset::new(
            self.base
                .iter()
                .cloned()
                .chain(self.servers.tools())
                .collect(),
        )
    }
}

impl ToolsetFactory for McpToolsets {
    fn toolset(&self, _node: &NodeInfo, selection: &ToolSelection) -> Result<Toolset> {
        self.snapshot()?.select(selection)
    }
}
