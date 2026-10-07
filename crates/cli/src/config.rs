//! Optional user configuration and credential resolution.

use anyhow::{Context, Result, bail};
use kyora_core::defaults;
use kyora_protocol::Effort;
use kyora_providers::anthropic::AnthropicConfig;
use serde::Deserialize;
use std::{io::Read, path::Path};

/// User config filename relative to the resolved kyora home.
pub const FILE_NAME: &str = "config.toml";
/// Default environment variable holding the Anthropic credential.
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: Option<String>,
    pub llm_model: Option<String>,
    pub effort: Option<Effort>,
    pub providers: Providers,
    pub mcp: kyora_mcp::McpConfig,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Providers {
    pub anthropic: Anthropic,
}

// Intentionally no Debug or Serialize implementation for credential-bearing types.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Anthropic {
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let mut input = String::new();
        file.read_to_string(&mut input)
            .with_context(|| format!("read {}", path.display()))?;
        let config: Self = toml::from_str(&input).map_err(|error: toml::de::Error| {
            // Only structured data and source offsets are inspected. Parser and
            // deserializer messages, excerpts, values and unknown names are never output.
            let category = toml::from_str::<toml::Value>(&input)
                .ok()
                .and_then(|value| diagnostic_category(&value, ""))
                .unwrap_or_else(|| "syntax error".into());
            let prefix = input
                .get(..error.span().map_or(0, |span| span.start))
                .unwrap_or("");
            let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
            let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            anyhow::anyhow!(
                "invalid config {} at line {line}, column {column}: {category}",
                path.display()
            )
        })?;
        config
            .mcp
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid config {}: {error}", path.display()))?;
        #[cfg(unix)]
        if config.providers.anthropic.api_key.is_some() {
            use std::os::unix::fs::PermissionsExt;
            // Inspect the opened file, not a second path lookup.
            if file.metadata()?.permissions().mode() & 0o077 != 0 {
                bail!(
                    "config {} contains api_key and is readable by group or others; run chmod 600 {}",
                    path.display(),
                    path.display()
                );
            }
        }
        Ok(config)
    }

    /// Refuses credentials copied into resolved references before parsing or output.
    pub fn reject_model_credentials(&self, references: &[&str]) -> Result<()> {
        let settings = &self.providers.anthropic;
        let selected = std::env::var(settings.api_key_env.as_deref().unwrap_or(API_KEY_ENV)).ok();
        for key in [settings.api_key.as_deref(), selected.as_deref()]
            .into_iter()
            .flatten()
            .filter(|key| !key.is_empty())
        {
            if references.iter().any(|value| value.contains(key)) {
                bail!("model settings must not contain API credentials");
            }
        }
        Ok(())
    }

    pub fn anthropic(&self, path: &Path, base_url: Option<&str>) -> Result<AnthropicConfig> {
        let settings = &self.providers.anthropic;
        let variable = settings.api_key_env.as_deref().unwrap_or(API_KEY_ENV);
        let key = std::env::var(variable)
            .ok()
            .filter(|key| !key.is_empty())
            .or_else(|| settings.api_key.clone().filter(|key| !key.is_empty()))
            .ok_or_else(|| anyhow::anyhow!(
                "no API key for provider anthropic: set {variable} or providers.anthropic.api_key in {}",
                path.display()
            ))?;
        let mut config = AnthropicConfig::new(key);
        let url = match base_url {
            Some(url) => Some(url.to_owned()),
            None => env("ANTHROPIC_BASE_URL")?.or_else(|| settings.base_url.clone()),
        };
        if let Some(url) = url {
            config.base_url = url;
        }
        Ok(config)
    }
}

// Paths returned here are schema literals, never names read from the file.
fn diagnostic_category(value: &toml::Value, setting: &str) -> Option<String> {
    if matches!(
        setting,
        "" | "providers" | "providers.anthropic" | "mcp" | "mcp.servers" | "mcp.servers.*"
    ) {
        let Some(table) = value.as_table() else {
            return Some(format!("wrong type for {setting}"));
        };
        for (key, value) in table {
            let path = match (setting, key.as_str()) {
                ("", "model") => "model",
                ("", "llm_model") => "llm_model",
                ("", "effort") => "effort",
                ("", "providers") => "providers",
                ("", "mcp") => "mcp",
                ("providers", "anthropic") => "providers.anthropic",
                ("providers.anthropic", "api_key") => "providers.anthropic.api_key",
                ("providers.anthropic", "api_key_env") => "providers.anthropic.api_key_env",
                ("providers.anthropic", "base_url") => "providers.anthropic.base_url",
                ("mcp", "servers") => "mcp.servers",
                // Server names are user data, so all entries share one path.
                ("mcp.servers", _) => "mcp.servers.*",
                ("mcp.servers.*", "command") => "mcp.servers.*.command",
                ("mcp.servers.*", "args") => "mcp.servers.*.args",
                ("mcp.servers.*", "env") => "mcp.servers.*.env",
                ("mcp.servers.*", "env_vars") => "mcp.servers.*.env_vars",
                ("mcp.servers.*", "cwd") => "mcp.servers.*.cwd",
                ("mcp.servers.*", "url") => "mcp.servers.*.url",
                ("mcp.servers.*", "bearer_token_env") => "mcp.servers.*.bearer_token_env",
                ("mcp.servers.*", "headers") => "mcp.servers.*.headers",
                ("mcp.servers.*", "env_headers") => "mcp.servers.*.env_headers",
                ("mcp.servers.*", "startup_timeout_s") => "mcp.servers.*.startup_timeout_s",
                ("mcp.servers.*", "tool_timeout_s") => "mcp.servers.*.tool_timeout_s",
                ("mcp.servers.*", "allow_tools") => "mcp.servers.*.allow_tools",
                ("mcp.servers.*", "deny_tools") => "mcp.servers.*.deny_tools",
                ("mcp.servers.*", "enabled") => "mcp.servers.*.enabled",
                _ => return Some("unknown field".into()),
            };
            if let Some(category) = diagnostic_category(value, path) {
                return Some(category);
            }
        }
        None
    } else {
        let strings = |items: &toml::value::Array| items.iter().all(toml::Value::is_str);
        let valid = match setting {
            "effort" => value.clone().try_into::<Effort>().is_ok(),
            "mcp.servers.*.args"
            | "mcp.servers.*.env_vars"
            | "mcp.servers.*.allow_tools"
            | "mcp.servers.*.deny_tools" => value.as_array().is_some_and(strings),
            "mcp.servers.*.env" | "mcp.servers.*.headers" | "mcp.servers.*.env_headers" => value
                .as_table()
                .is_some_and(|table| table.values().all(toml::Value::is_str)),
            "mcp.servers.*.startup_timeout_s" | "mcp.servers.*.tool_timeout_s" => {
                value.is_float() || value.is_integer()
            }
            "mcp.servers.*.enabled" => value.is_bool(),
            _ => value.is_str(),
        };
        (!valid).then(|| format!("wrong type for {setting}"))
    }
}

/// Reads an optional Unicode setting without displaying its value on error.
pub fn env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => bail!("{name} must be Unicode"),
    }
}

pub fn model(
    flag: Option<String>,
    variable: &'static str,
    configured: Option<&str>,
    fallback: &str,
    path: &Path,
    setting: &'static str,
) -> Result<ModelSetting> {
    let (value, source) = if let Some(value) = flag {
        (value, format!("flag --{}", setting.replace('_', "-")))
    } else if let Some(value) = env(variable)? {
        (value, format!("environment variable {variable}"))
    } else if let Some(value) = configured {
        (
            value.to_owned(),
            format!("config {} key {setting}", path.display()),
        )
    } else {
        (fallback.to_owned(), "built-in default".into())
    };
    Ok(ModelSetting {
        value,
        setting,
        source,
    })
}

/// A winning model setting, kept unparsed until credential checks finish.
pub struct ModelSetting {
    pub value: String,
    setting: &'static str,
    source: String,
}

impl ModelSetting {
    pub fn parse(&self) -> Result<defaults::ModelRef> {
        self.value.parse().map_err(|_| {
            anyhow::anyhow!(
                "invalid {} from {}: expected provider/model",
                self.setting,
                self.source
            )
        })
    }
}
