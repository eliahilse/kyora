use crate::defaults::FileConfig;
use anyhow::{Result, bail};
use async_trait::async_trait;
use kyora_core::{Effect, Tool, ToolCx, ToolOutput, tool::truncate};
use kyora_protocol::ToolSpec;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Bounded, line-numbered UTF-8 file reader.
pub struct ReadFile {
    config: FileConfig,
}
impl ReadFile {
    /// Creates a reader with configurable bounds.
    pub fn new(config: FileConfig) -> Self {
        Self { config }
    }
}
/// Atomically replaces a file after creating its parent directories.
pub struct WriteFile;
/// Exact-match editor with shared per-path locks.
#[derive(Default)]
pub struct EditFile {
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}
fn spec(
    name: &str,
    description: &str,
    properties: Value,
    required: Value,
    large_input: bool,
) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        large_input,
    }
}
fn path(cx: &ToolCx, input: &Value) -> Result<PathBuf> {
    let p = input["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing path"))?;
    if p.is_empty() {
        bail!("path must not be empty");
    }
    Ok(cx.cwd.join(p))
}
fn output(result: Result<String>) -> ToolOutput {
    match result {
        Ok(s) => ToolOutput::text(s),
        Err(e) => ToolOutput::error(e.to_string()),
    }
}
fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(content)?;
    tmp.flush()?;
    if let Ok(metadata) = std::fs::metadata(path) {
        tmp.as_file().set_permissions(metadata.permissions())?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_file",
            "Read UTF-8 text with 1-based line numbers and bounded input. Offset is a 1-based line number.",
            json!({"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}}),
            json!(["path"]),
            false,
        )
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        if cx.cancel.is_cancelled() {
            return ToolOutput::error("cancelled");
        }
        output(self.read(&input, &cx))
    }
}
impl ReadFile {
    fn read(&self, input: &Value, cx: &ToolCx) -> Result<String> {
        let offset = input
            .get("offset")
            .map_or(Some(1), Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("offset must be positive"))?;
        let limit = input
            .get("limit")
            .map_or(Some(self.config.lines as u64), Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("limit must be positive"))?;
        if offset == 0 || limit == 0 || self.config.max_bytes == 0 || self.config.line_chars == 0 {
            bail!("file bounds must be positive");
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path(cx, input)?)?
            .take(self.config.max_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        let clipped = bytes.len() > self.config.max_bytes;
        bytes.truncate(self.config.max_bytes);
        if bytes.contains(&0) {
            bail!("binary file refused");
        }
        // A bounded read may end inside UTF-8. Remove only that incomplete suffix.
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(e) if clipped && e.error_len().is_none() => {
                std::str::from_utf8(&bytes[..e.valid_up_to()])?
            }
            Err(_) => bail!("binary or non-UTF-8 file refused"),
        };
        let mut result = String::new();
        for (index, line) in text
            .lines()
            .enumerate()
            .skip(usize::try_from(offset - 1)?)
            .take(usize::try_from(limit)?)
        {
            result.push_str(&format!(
                "{}: {}\n",
                index + 1,
                truncate(line, self.config.line_chars)
            ));
        }
        if clipped {
            result.push_str("[read byte limit reached]\n");
        }
        Ok(result)
    }
}
#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "write_file",
            "Atomically write UTF-8 contents, creating parent directories.",
            json!({"path":{"type":"string"},"content":{"type":"string"}}),
            json!(["path", "content"]),
            true,
        )
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        if cx.cancel.is_cancelled() {
            return ToolOutput::error("cancelled");
        }
        output((|| {
            let p = path(&cx, &input)?;
            let content = input["content"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing content"))?;
            atomic_write(&p, content.as_bytes())?;
            Ok(format!("wrote {} bytes", content.len()))
        })())
    }
}
#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "edit_file",
            "Replace an exact nonempty match, exactly once unless replace_all is true.",
            json!({"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"},"replace_all":{"type":"boolean"}}),
            json!(["path", "old", "new"]),
            true,
        )
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        if cx.cancel.is_cancelled() {
            return ToolOutput::error("cancelled");
        }
        output((|| {
            let p = std::fs::canonicalize(path(&cx, &input)?)?;
            let lock = self
                .locks
                .lock()
                .expect("path registry poisoned")
                .entry(p.clone())
                .or_default()
                .clone();
            let _guard = lock.lock().expect("path lock poisoned");
            let old = input["old"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing old"))?;
            let new = input["new"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing new"))?;
            if old.is_empty() {
                bail!("old must not be empty");
            }
            let text = std::fs::read_to_string(&p)?;
            let count = text.matches(old).count();
            if count == 0 || (count != 1 && input["replace_all"] != true) {
                bail!("expected exactly one match, found {count}");
            }
            atomic_write(&p, text.replace(old, new).as_bytes())?;
            Ok(format!("replaced {count} matches"))
        })())
    }
}
