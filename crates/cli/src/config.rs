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
            // Source excerpts and quoted diagnostic values can contain credentials,
            // including in malformed TOML. Retain the error category and location.
            let mut message = String::new();
            let mut quote = None;
            for ch in error.message().chars() {
                match quote {
                    Some(end) if ch == end => quote = None,
                    Some(_) => {}
                    None if ch == '`' || ch == '"' || ch == '\'' => {
                        quote = Some(ch);
                        message.push_str("[redacted]");
                    }
                    None => message.push(ch),
                }
            }
            let line = error
                .span()
                .map(|span| {
                    input
                        .get(..span.start)
                        .unwrap_or(&input)
                        .bytes()
                        .filter(|&b| b == b'\n')
                        .count()
                        + 1
                })
                .map(|line| format!(" at line {line}"))
                .unwrap_or_default();
            anyhow::anyhow!("invalid config {}{line}: {message}", path.display())
        })?;
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

    pub fn anthropic(&self, path: &Path) -> Result<AnthropicConfig> {
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
        if let Some(url) = &settings.base_url {
            config.base_url.clone_from(url);
        }
        if let Some(url) = env("ANTHROPIC_BASE_URL")? {
            config.base_url = url;
        }
        Ok(config)
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
    flag: Option<defaults::ModelRef>,
    variable: &str,
    configured: Option<&str>,
    fallback: &str,
) -> Result<defaults::ModelRef> {
    if let Some(model) = flag {
        return Ok(model);
    }
    env(variable)?
        .as_deref()
        .or(configured)
        .unwrap_or(fallback)
        .parse()
}
