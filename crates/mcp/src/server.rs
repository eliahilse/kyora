//! Server lifecycle: connect, initialize, list tools, call tools and shut down.
use crate::{
    config::{McpConfig, ServerConfig, validate_name},
    defaults,
    http::HttpClient,
    process::Process,
    secrets::Secrets,
    tool::{McpTool, render_result},
};
use anyhow::{Context, Result, anyhow, bail};
use kyora_core::{Tool, ToolOutput, ToolSelection, Toolset, ToolsetFactory, runtime::NodeInfo};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    ClientHandler, Peer, RoleClient, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientConfig, ClientRequest, Implementation,
        ListToolsRequest, PaginatedRequestParams, ProtocolVersion, Request, ServerResult,
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
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::Path,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
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
    secrets: Secrets,
    oversized: Arc<AtomicBool>,
}

impl Connection {
    pub(crate) fn server(&self) -> &str {
        &self.server
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn secrets(&self) -> &Secrets {
        &self.secrets
    }

    /// Calls `tool` until it answers, `cancel` fires or `deadline` passes. The last two
    /// send `notifications/cancelled` for the request. Credential values are redacted
    /// from the output, whether it is a result or an error.
    pub(crate) async fn call(
        &self,
        tool: &str,
        input: Value,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> ToolOutput {
        let output = self.call_unredacted(tool, input, cancel, deadline).await;
        self.secrets.output(output)
    }

    async fn call_unredacted(
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
        match self.request(request, cancel, deadline).await {
            Ok(Ok(result)) => match serde_json::to_value(&result) {
                Ok(mut value) => {
                    // Redact decoded strings: once rendered, structured content is
                    // JSON text in which a value may appear only in escaped form.
                    self.secrets.redact_json(&mut value);
                    render_result(&value)
                }
                Err(error) => ToolOutput::error(format!("invalid tool result: {error}")),
            },
            Ok(Err(error)) => ToolOutput::error(self.describe(error)),
            Err(Stopped::Cancelled) => ToolOutput::error("cancelled"),
            Err(Stopped::TimedOut) => {
                ToolOutput::error(format!("timed out after {:?}", self.timeout))
            }
        }
    }

    /// Sends `request` and waits for its response until `cancel` fires or `deadline`
    /// passes. A request stopped that way is cancelled on the server with
    /// `notifications/cancelled`, which also ends its HTTP request; so is one whose
    /// future is dropped.
    async fn request(
        &self,
        request: ClientRequest,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<Result<ServerResult, ServiceError>, Stopped> {
        let handle = match self
            .peer
            .send_request_with_option(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => return Ok(Err(error)),
        };
        let mut pending = Pending(Some(handle));
        let response = {
            let response = &mut pending.0.as_mut().expect("pending request").rx;
            tokio::select! {
                biased;
                response = response => Ok(response),
                _ = cancel.cancelled() => Err(Stopped::Cancelled),
                _ = tokio::time::sleep_until(deadline) => Err(Stopped::TimedOut),
            }
        };
        match response {
            Ok(response) => {
                pending.0 = None;
                // A dropped responder means the connection ended.
                Ok(response.unwrap_or(Err(ServiceError::TransportClosed)))
            }
            Err(stopped) => {
                pending
                    .cancel(match stopped {
                        Stopped::Cancelled => "cancelled",
                        Stopped::TimedOut => "timed out",
                    })
                    .await;
                Err(stopped)
            }
        }
    }

    fn describe(&self, error: ServiceError) -> String {
        match error {
            ServiceError::McpError(error) => format!("error {}: {}", error.code.0, error.message),
            ServiceError::TransportClosed => format!(
                "mcp server {} closed the connection{}",
                self.server,
                limit_note(&self.oversized)
            ),
            ServiceError::TransportSend(error) => {
                format!("mcp server {}: {}", self.server, chain(&*error.error))
            }
            error => format!("mcp server {}: {error}", self.server),
        }
    }
}

/// Why a request ended without a response.
enum Stopped {
    Cancelled,
    TimedOut,
}

/// Ends the HTTP requests of a startup whose future is dropped, for example by a
/// caller that gives up on it, and tries to delete a session it opened.
struct StartupGuard(Option<HttpClient>);

impl Drop for StartupGuard {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            client.cancel();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move { client.close().await });
            }
        }
    }
}

/// Explains a closed connection caused by an oversized message.
fn limit_note(oversized: &AtomicBool) -> String {
    if oversized.load(Ordering::SeqCst) {
        format!(
            ": a message exceeded the {} byte limit",
            defaults::MAX_MESSAGE_BYTES
        )
    } else {
        String::new()
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
    connection: Arc<Connection>,
    listing: RwLock<Listing>,
    service: Mutex<Option<RunningService<RoleClient, Handler>>>,
    process: Mutex<Option<Process>>,
    http: Option<HttpClient>,
    closed: CancellationToken,
    /// Startup notes, such as credential values too short to redact.
    notes: Vec<String>,
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
        Self::start_until(name, config, cwd, env, &CancellationToken::new()).await
    }

    /// Like [`Self::start`], but gives up when `cancel` fires, after the same cleanup
    /// as any other failure. Dropping the future instead still ends HTTP requests,
    /// but deleting a session is then only attempted in the background.
    pub async fn start_until(
        name: &str,
        config: &ServerConfig,
        cwd: &Path,
        env: &[(OsString, OsString)],
        cancel: &CancellationToken,
    ) -> Result<Arc<Self>> {
        validate_name(name)?;
        config.validate()?;
        let secrets = Secrets::resolve(config, env);
        let changed = Arc::new(Notify::new());
        let handler = Handler {
            changed: changed.clone(),
        };
        let mut process = None;
        let oversized = Arc::new(AtomicBool::new(false));
        let http = match &config.url {
            Some(_) => Some(http_client(
                config,
                env,
                oversized.clone(),
                secrets.clone(),
            )?),
            None => None,
        };
        let mut guard = StartupGuard(http.as_ref().map(|(client, _)| client.clone()));
        let startup = config.startup_timeout();
        let started = tokio::time::timeout(startup, async {
            let service = if let Some(command) = &config.command {
                let dir = config
                    .cwd
                    .as_ref()
                    .map_or_else(|| cwd.to_path_buf(), |dir| cwd.join(dir));
                let (child, stdout, stdin) =
                    Process::spawn(config, command, &dir, env, oversized.clone(), &secrets)?;
                process = Some(child);
                handler
                    .serve((stdout, stdin))
                    .await
                    .map_err(initialize_error)?
            } else {
                let (client, transport) = http.clone().expect("http client for a url server");
                handler
                    .serve(StreamableHttpClientTransport::with_client(
                        client, transport,
                    ))
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
                secrets: secrets.clone(),
                oversized: oversized.clone(),
            });
            // Servers without the tools capability contribute nothing.
            let listing = if info.capabilities.tools.is_some() {
                // The startup timeout bounds this too; the deadline lets the listing
                // cancel itself on the server first.
                let deadline = Instant::now() + startup;
                list(&connection, config, &CancellationToken::new(), deadline).await?
            } else {
                Listing {
                    tools: Vec::new(),
                    warnings: Vec::new(),
                }
            };
            Ok((service, connection, listing))
        });
        let started = tokio::select! {
            started = started => match started {
                Ok(started) => started,
                Err(_) => Err(anyhow!("startup timed out after {startup:?}")),
            },
            _ = cancel.cancelled() => Err(anyhow!("startup cancelled")),
        };
        // From here on, cleanup is awaited below rather than left to the guard.
        guard.0 = None;
        let (service, connection, listing) = match started {
            Ok(started) => started,
            Err(error) => {
                let stderr = match process {
                    Some(process) => process.kill(&secrets).await,
                    None => String::new(),
                };
                // rmcp leaves startup requests running and sessions open on failure.
                if let Some((client, _)) = &http {
                    client.close().await;
                }
                let mut message = format!("{error:#}{}", limit_note(&oversized));
                if !stderr.is_empty() {
                    message.push_str(&format!(" (stderr: {stderr})"));
                }
                // Bodies, JSON-RPC errors and stderr can echo a credential back.
                return Err(anyhow!(secrets.redact(&message)));
            }
        };
        let server = Arc::new(Self {
            name: name.to_owned(),
            connection,
            listing: RwLock::new(listing),
            service: Mutex::new(Some(service)),
            process: Mutex::new(process),
            http: http.map(|(client, _)| client),
            notes: secrets.warnings(),
            closed: CancellationToken::new(),
        });
        tokio::spawn(refresh(
            Arc::downgrade(&server),
            server.connection.clone(),
            config.clone(),
            changed,
            server.closed.clone(),
        ));
        Ok(server)
    }

    /// The configured name, used in every tool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The tools offered now. Nodes freeze the set they receive when they start.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.listing
            .read()
            .expect("tool list poisoned")
            .tools
            .clone()
    }

    /// Problems worth reporting: credential values too short to redact, and tools
    /// left out of the current list, for example because their names collide.
    pub fn warnings(&self) -> Vec<String> {
        let listing = self.listing.read().expect("tool list poisoned");
        self.notes
            .iter()
            .chain(&listing.warnings)
            .cloned()
            .collect()
    }

    /// Calls a tool by its server-side name with the configured timeout.
    pub async fn call(&self, tool: &str, input: Value, cancel: &CancellationToken) -> ToolOutput {
        let deadline = Instant::now() + self.connection.timeout;
        self.connection.call(tool, input, cancel, deadline).await
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
        if let Some(http) = &self.http {
            http.close().await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Ends the refresh task when a server is dropped without shutdown.
        self.closed.cancel();
    }
}

/// Lists the tools again after each `notifications/tools/list_changed`; a failed
/// refresh keeps the previous list. The listing runs on the connection alone, so
/// dropping the server is never held up by it.
async fn refresh(
    server: Weak<Server>,
    connection: Arc<Connection>,
    config: ServerConfig,
    changed: Arc<Notify>,
    closed: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = closed.cancelled() => return,
            _ = changed.notified() => {}
        }
        let deadline = Instant::now() + config.startup_timeout();
        let Ok(listing) = list(&connection, &config, &closed, deadline).await else {
            continue;
        };
        let Some(server) = server.upgrade() else {
            return;
        };
        *server.listing.write().expect("tool list poisoned") = listing;
    }
}

/// A server's tools, without those whose names collide.
struct Listing {
    tools: Vec<Arc<dyn Tool>>,
    warnings: Vec<String>,
}

impl Listing {
    /// Leaves out every tool whose kyora name another tool also maps to, rather than
    /// letting one silently shadow the other, and says so in a warning.
    fn new(tools: Vec<McpTool>, connection: &Connection, mut warnings: Vec<String>) -> Self {
        let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for tool in &tools {
            names
                .entry(tool.name().to_owned())
                .or_default()
                .push(tool.remote_name().to_owned());
        }
        warnings.extend(names.iter().filter(|(_, remotes)| remotes.len() > 1).map(
            |(name, remotes)| {
                let warning = format!(
                    "tools {} all map to {name}; none of them is offered",
                    remotes.join(", ")
                );
                connection.secrets.redact(&warning)
            },
        ));
        let tools = tools
            .into_iter()
            .filter(|tool| names[tool.name()].len() == 1)
            .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
            .collect();
        Self { tools, warnings }
    }
}

/// Follows `nextCursor` until the listing ends, `cancel` fires or `deadline` passes.
async fn list(
    connection: &Arc<Connection>,
    config: &ServerConfig,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Listing> {
    let mut listed = Vec::new();
    let mut bytes = 0;
    let mut cursor = None;
    for _ in 0..defaults::MAX_LIST_PAGES {
        let request = ClientRequest::ListToolsRequest(ListToolsRequest::with_param(
            PaginatedRequestParams::default().with_cursor(cursor),
        ));
        let page = match connection.request(request, cancel, deadline).await {
            Ok(Ok(ServerResult::ListToolsResult(page))) => page,
            Ok(Ok(_)) => bail!("tools/list: unexpected response"),
            Ok(Err(error)) => bail!("tools/list: {}", connection.describe(error)),
            Err(Stopped::Cancelled) => bail!("tools/list cancelled"),
            Err(Stopped::TimedOut) => bail!("tools/list timed out"),
        };
        // Each page is bounded by the message limit. The totals are checked after every
        // page and before the next is requested, so a listing holds at most these
        // limits plus one page.
        bytes += page
            .tools
            .iter()
            .map(|tool| serde_json::to_vec(tool).map_or(0, |json| json.len()))
            .sum::<usize>();
        listed.extend(page.tools);
        if listed.len() > defaults::MAX_TOOLS {
            bail!("the server offers more than {} tools", defaults::MAX_TOOLS);
        }
        if bytes > defaults::MAX_LISTING_BYTES {
            bail!(
                "the server's tool definitions exceed {} bytes",
                defaults::MAX_LISTING_BYTES
            );
        }
        match page.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => {
                let secrets = connection.secrets();
                let mut warnings = Vec::new();
                let tools = listed
                    .into_iter()
                    .filter(|tool| config.exposes(&tool.name))
                    .filter(|tool| {
                        // A renamed tool would be confusing; one that cannot be named
                        // without the credential is left out.
                        let hidden = secrets.found_in(&tool.name);
                        if hidden {
                            warnings.push(format!(
                                "tool {} is left out because its name contains a credential value",
                                secrets.redact(&tool.name)
                            ));
                        }
                        !hidden
                    })
                    .map(|tool| McpTool::new(connection.clone(), tool))
                    .collect();
                return Ok(Listing::new(tools, connection, warnings));
            }
        }
    }
    bail!(
        "tools/list did not finish within {} pages",
        defaults::MAX_LIST_PAGES
    )
}

/// The HTTP client and rmcp transport settings for a url server.
fn http_client(
    config: &ServerConfig,
    env: &[(OsString, OsString)],
    oversized: Arc<AtomicBool>,
    secrets: Secrets,
) -> Result<(HttpClient, StreamableHttpClientTransportConfig)> {
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
    let url: Arc<str> = config.url.as_deref().expect("validated url").into();
    let auth_header = match &config.bearer_token_env {
        Some(variable) => Some(lookup(variable)?),
        None => None,
    };
    let mut transport = StreamableHttpClientTransportConfig::with_uri(url.clone())
        .custom_headers(headers.clone())
        .max_sse_event_size(defaults::MAX_MESSAGE_BYTES);
    if let Some(token) = &auth_header {
        transport = transport.auth_header(token.clone());
    }
    // Redirects are refused: following one would send env-sourced headers, and on a
    // scheme downgrade the bearer token, to a location the config never named.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(config.startup_timeout())
        .build()?;
    // A response may take as long as the slowest call or listing it answers.
    let timeout = config.startup_timeout().max(config.tool_timeout());
    let client = HttpClient::new(http, timeout, oversized, secrets, url, auth_header, headers);
    Ok((client, transport))
}

/// Every configured server that started. Failures are returned, never fatal.
#[derive(Default)]
pub struct Servers {
    servers: Vec<Arc<Server>>,
}

impl Servers {
    /// Starts every enabled server concurrently with kyora's own environment.
    pub async fn start(config: &McpConfig, cwd: &Path) -> (Self, Vec<anyhow::Error>) {
        Self::start_until(config, cwd, &CancellationToken::new()).await
    }

    /// Like [`Self::start`], but servers still starting give up when `cancel` fires,
    /// after cleaning up. Servers that already started are returned.
    pub async fn start_until(
        config: &McpConfig,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> (Self, Vec<anyhow::Error>) {
        let env: Vec<_> = std::env::vars_os().collect();
        Self::start_with_env_until(config, cwd, &env, cancel).await
    }

    /// Starts every enabled server concurrently. Each failure names its server; the
    /// other servers keep running.
    pub async fn start_with_env(
        config: &McpConfig,
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> (Self, Vec<anyhow::Error>) {
        Self::start_with_env_until(config, cwd, env, &CancellationToken::new()).await
    }

    async fn start_with_env_until(
        config: &McpConfig,
        cwd: &Path,
        env: &[(OsString, OsString)],
        cancel: &CancellationToken,
    ) -> (Self, Vec<anyhow::Error>) {
        let starts = config
            .servers
            .iter()
            .filter(|(_, server)| server.enabled)
            .map(|(name, server)| async move {
                Server::start_until(name, server, cwd, env, cancel)
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
