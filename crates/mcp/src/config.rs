//! The `[mcp.servers.<name>]` configuration tables and their validation.
use crate::defaults;
use anyhow::{Result, anyhow, bail};
use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

/// The `[mcp]` table.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Servers keyed by name. The name is part of every tool name the server contributes.
    pub servers: BTreeMap<String, ServerConfig>,
}

// Intentionally no Debug or Serialize implementation: env and headers may hold
// values a user would not want echoed.
/// One server. Exactly one of `command` (stdio) and `url` (streamable HTTP) is set.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Executable of a stdio server, looked up on PATH.
    pub command: Option<String>,
    /// Arguments passed to `command`.
    pub args: Vec<String>,
    /// Literal environment values for a stdio server. Credentials belong in `env_vars`.
    pub env: BTreeMap<String, String>,
    /// Variables copied from kyora's environment into a stdio server's environment.
    pub env_vars: Vec<String>,
    /// Working directory of a stdio server, relative to the workspace (default: the workspace).
    pub cwd: Option<PathBuf>,
    /// Endpoint of a streamable HTTP server.
    pub url: Option<String>,
    /// Environment variable whose value is sent as `Authorization: Bearer <value>`.
    pub bearer_token_env: Option<String>,
    /// Literal HTTP headers. Credentials belong in `env_headers` or `bearer_token_env`.
    pub headers: BTreeMap<String, String>,
    /// HTTP headers whose values are read from the named environment variables.
    pub env_headers: BTreeMap<String, String>,
    /// Seconds allowed for spawn or connect, the handshake and the tool listing.
    pub startup_timeout_s: Option<f64>,
    /// Seconds allowed for one tool call.
    pub tool_timeout_s: Option<f64>,
    /// When set, only these server tool names are exposed.
    pub allow_tools: Option<Vec<String>>,
    /// Server tool names that are never exposed, applied after `allow_tools`.
    pub deny_tools: Vec<String>,
    /// False keeps the entry without starting the server.
    pub enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            env_vars: Vec::new(),
            cwd: None,
            url: None,
            bearer_token_env: None,
            headers: BTreeMap::new(),
            env_headers: BTreeMap::new(),
            startup_timeout_s: None,
            tool_timeout_s: None,
            allow_tools: None,
            deny_tools: Vec::new(),
            enabled: true,
        }
    }
}

impl McpConfig {
    /// Rejects invalid server names, ambiguous transports and credentials written into
    /// the file. Errors name the server but never echo values from the file.
    pub fn validate(&self) -> Result<()> {
        for (name, server) in &self.servers {
            validate_name(name)?;
            server
                .validate()
                .map_err(|error| anyhow!("mcp server {name}: {error}"))?;
        }
        Ok(())
    }
}

impl ServerConfig {
    /// Checks one server entry; see [`McpConfig::validate`].
    pub fn validate(&self) -> Result<()> {
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => bail!("set either command or url, not both"),
            (None, None) => bail!("set command (stdio) or url (streamable HTTP)"),
            (Some(command), None) => {
                if command.is_empty() {
                    bail!("command is empty");
                }
                if self.bearer_token_env.is_some()
                    || !self.headers.is_empty()
                    || !self.env_headers.is_empty()
                {
                    bail!("bearer_token_env, headers and env_headers apply only to url servers");
                }
                if self.env.keys().any(|name| credential(name))
                    || self.env.values().any(|value| url_credentials(value))
                {
                    bail!("env must not hold credentials; name the variable in env_vars instead");
                }
                if self.args.iter().any(|arg| url_credentials(arg)) || flag_credential(&self.args) {
                    bail!("args must not hold credentials; pass them through env_vars");
                }
            }
            (None, Some(url)) => {
                let parsed = reqwest::Url::parse(url).map_err(|_| anyhow!("url is not valid"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("url must use http or https");
                }
                if !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.query_pairs().any(|(name, _)| credential(&name))
                {
                    bail!("url must not embed credentials; use bearer_token_env or env_headers");
                }
                if !self.args.is_empty()
                    || !self.env.is_empty()
                    || !self.env_vars.is_empty()
                    || self.cwd.is_some()
                {
                    bail!("args, env, env_vars and cwd apply only to command servers");
                }
                if self.headers.keys().any(|name| credential(name))
                    || self.headers.values().any(|value| url_credentials(value))
                {
                    bail!("headers must not hold credentials; use bearer_token_env or env_headers");
                }
                for name in self.headers.keys().chain(self.env_headers.keys()) {
                    let valid = HeaderName::from_bytes(name.as_bytes())
                        .is_ok_and(|header| !RESERVED_HEADERS.contains(&header.as_str()));
                    if !valid {
                        bail!("invalid or reserved header name");
                    }
                }
                if self
                    .headers
                    .values()
                    .any(|value| HeaderValue::from_str(value).is_err())
                {
                    bail!("invalid header value");
                }
            }
        }
        for seconds in [self.startup_timeout_s, self.tool_timeout_s]
            .into_iter()
            .flatten()
        {
            if seconds <= 0.0 || Duration::try_from_secs_f64(seconds).is_err() {
                bail!("timeouts must be positive numbers of seconds");
            }
        }
        Ok(())
    }

    /// Time allowed for startup, handshake and listing.
    pub fn startup_timeout(&self) -> Duration {
        seconds(self.startup_timeout_s).unwrap_or(defaults::STARTUP_TIMEOUT)
    }

    /// Time allowed per tool call.
    pub fn tool_timeout(&self) -> Duration {
        seconds(self.tool_timeout_s).unwrap_or(defaults::TOOL_TIMEOUT)
    }

    /// Whether a server-side tool name passes `allow_tools` and `deny_tools`.
    pub fn exposes(&self, tool: &str) -> bool {
        self.allow_tools
            .as_ref()
            .is_none_or(|allow| allow.iter().any(|name| name == tool))
            && !self.deny_tools.iter().any(|name| name == tool)
    }
}

/// Headers the transport sets itself.
const RESERVED_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// Server names become part of tool names, so they are restricted to characters every
/// provider accepts and must keep the `mcp__<server>__` prefix unambiguous.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= defaults::MAX_SERVER_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !name.contains("__")
        && !name.ends_with('_');
    if !valid {
        // The name itself is not echoed; it may be anything the file contains.
        bail!(
            "invalid mcp server name: use 1 to {} ASCII letters, digits, '-' or '_', without '__' or a trailing '_'",
            defaults::MAX_SERVER_NAME
        );
    }
    Ok(())
}

fn seconds(value: Option<f64>) -> Option<Duration> {
    value.and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
}

/// Names that look like credentials or carry them (cookies, sessions): never inherited
/// by default and never written as literals.
pub(crate) fn credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PASSPHRASE",
        "CREDENTIAL",
        "AUTH",
        "COOKIE",
        "SESSION",
        "BEARER",
        "JWT",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

/// Whether `value` contains a URL with user information, as in
/// `postgresql://user:password@host/db`, or with a credential-like query parameter,
/// as in `https://host/?api_key=...`.
fn url_credentials(value: &str) -> bool {
    value.match_indices("://").any(|(start, scheme)| {
        let rest = &value[start + scheme.len()..];
        let url = rest.split(char::is_whitespace).next().unwrap_or_default();
        let authority = url.split(['/', '?', '#']).next().unwrap_or_default();
        let query = url
            .split_once('?')
            .map_or("", |(_, query)| query.split('#').next().unwrap_or_default());
        authority.contains('@')
            || query.split('&').any(|pair| {
                credential(&percent_decoded(pair.split('=').next().unwrap_or_default()))
            })
    })
}

/// A query key with `%XX` escapes and `+` decoded, as servers read it.
fn percent_decoded(key: &str) -> String {
    let bytes = key.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[at], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                at += 1;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `args` pass a credential-looking flag a literal value, as in
/// `--api-key VALUE` or `--password=VALUE`. Flags that name where a credential is
/// kept, such as `--token-file` or `--api-key-env`, are fine.
fn flag_credential(args: &[String]) -> bool {
    args.iter().enumerate().any(|(index, arg)| {
        if !arg.starts_with('-') {
            return false;
        }
        let (flag, value) = match arg.trim_start_matches('-').split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (arg.trim_start_matches('-'), None),
        };
        let upper = flag.to_ascii_uppercase();
        let reference = ["FILE", "PATH", "DIR", "ENV", "VAR"]
            .iter()
            .any(|suffix| upper.ends_with(suffix));
        if !credential(flag) || reference {
            return false;
        }
        match value {
            Some(value) => !value.is_empty(),
            None => args
                .get(index + 1)
                .is_some_and(|next| !next.starts_with('-')),
        }
    })
}
