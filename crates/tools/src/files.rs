use crate::defaults::FileConfig;
use anyhow::{Result, bail};
use async_trait::async_trait;
use kyora_core::{Effect, Tool, ToolCx, ToolOutput, tool::truncate};
use kyora_protocol::ToolSpec;
use nix::{
    errno::Errno,
    fcntl::{AtFlags, OFlag, open, openat, renameat},
    sys::stat::{Mode, SFlag, fstat, fstatat, mkdirat},
    unistd::{UnlinkatFlags, unlinkat},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
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
    locks: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
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
// Normalize before opening so no tool-supplied component can climb above the root.
fn path(input: &Value) -> Result<PathBuf> {
    let p = input["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let mut relative = PathBuf::new();
    for component in Path::new(p).components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            Component::ParentDir if relative.pop() => {}
            _ => bail!("path must remain relative to the workspace root"),
        }
    }
    if relative.as_os_str().is_empty() {
        bail!("path must name a file");
    }
    Ok(relative)
}
fn output(result: Result<String>) -> ToolOutput {
    match result {
        Ok(s) => ToolOutput::text(s),
        Err(e) => ToolOutput::error(e.to_string()),
    }
}
async fn blocking(
    cx: ToolCx,
    work: impl FnOnce() -> Result<String> + Send + 'static,
) -> ToolOutput {
    if cx.cancel.is_cancelled() {
        return ToolOutput::error("cancelled");
    }
    let task = tokio::task::spawn_blocking(work);
    tokio::select! {
        biased;
        _ = cx.cancel.cancelled() => ToolOutput::error("cancelled"),
        _ = tokio::time::sleep_until(cx.node.deadline) => ToolOutput::error("cancelled"),
        result = task => output(result.map_err(anyhow::Error::from).and_then(|r| r)),
    }
}
// Hold each directory open and resolve only one component at a time. Renames and
// symlink swaps cannot redirect later operations through a different parent.
fn parent(root: &Path, relative: &Path, create: bool) -> Result<File> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW;
    // The workspace root is trusted caller configuration, including root aliases.
    let root = std::fs::canonicalize(root)?;
    let mut dir = File::from(open(&root, flags, Mode::empty())?);
    for name in relative.parent().expect("relative file path").components() {
        let name = name.as_os_str();
        if create {
            match mkdirat(&dir, name, Mode::from_bits_truncate(0o755)) {
                Ok(()) | Err(Errno::EEXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        dir = File::from(openat(&dir, name, flags, Mode::empty())?);
    }
    Ok(dir)
}
fn regular(mode: nix::libc::mode_t) -> Result<()> {
    if SFlag::from_bits_truncate(mode) & SFlag::S_IFMT != SFlag::S_IFREG {
        bail!("non-regular file refused");
    }
    Ok(())
}
fn read_file(dir: &File, name: &std::ffi::OsStr) -> Result<File> {
    regular(fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW)?.st_mode)?;
    // NONBLOCK prevents a replacement FIFO from blocking between stat and open.
    let file = File::from(openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        Mode::empty(),
    )?);
    regular(fstat(&file)?.st_mode)?;
    Ok(file)
}
fn atomic_write(dir: &File, name: &std::ffi::OsStr, content: &[u8]) -> Result<()> {
    let mode = match fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            regular(stat.st_mode)?;
            Mode::from_bits_truncate(stat.st_mode & 0o777)
        }
        Err(Errno::ENOENT) => Mode::from_bits_truncate(0o600),
        Err(e) => return Err(e.into()),
    };
    let tmp_name = format!(".kyora-{}", uuid::Uuid::now_v7());
    let mut tmp = File::from(openat(
        dir,
        tmp_name.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )?);
    struct Cleanup<'a>(&'a File, &'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = unlinkat(self.0, self.1, UnlinkatFlags::NoRemoveDir);
        }
    }
    let _cleanup = Cleanup(dir, &tmp_name);
    tmp.write_all(content)?;
    tmp.flush()?;
    nix::sys::stat::fchmod(&tmp, mode)?;
    // renameat replaces the directory entry itself, never a symlink target.
    renameat(dir, tmp_name.as_str(), dir, name)?;
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
        let reader = Self::new(self.config.clone());
        let cwd = cx.cwd.clone();
        blocking(cx, move || reader.read(&input, &cwd)).await
    }
}
impl ReadFile {
    fn read(&self, input: &Value, cwd: &Path) -> Result<String> {
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
        let p = path(input)?;
        let dir = parent(cwd, &p, false)?;
        read_file(&dir, p.file_name().expect("file name"))?
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
        let cwd = cx.cwd.clone();
        blocking(cx, move || {
            let p = path(&input)?;
            let dir = parent(&cwd, &p, true)?;
            let content = input["content"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing content"))?;
            atomic_write(&dir, p.file_name().expect("file name"), content.as_bytes())?;
            Ok(format!("wrote {} bytes", content.len()))
        })
        .await
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
        let locks = self.locks.clone();
        let cwd = cx.cwd.clone();
        blocking(cx, move || {
            let p = path(&input)?;
            let dir = parent(&cwd, &p, false)?;
            let lock = locks
                .lock()
                .expect("path registry poisoned")
                .entry(std::fs::canonicalize(&cwd)?.join(&p))
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
            let mut text = String::new();
            read_file(&dir, p.file_name().expect("file name"))?.read_to_string(&mut text)?;
            let count = text.matches(old).count();
            if count == 0 || (count != 1 && input["replace_all"] != true) {
                bail!("expected exactly one match, found {count}");
            }
            atomic_write(
                &dir,
                p.file_name().expect("file name"),
                text.replace(old, new).as_bytes(),
            )?;
            Ok(format!("replaced {count} matches"))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn parent_symlink_swap_cannot_redirect_reads_or_atomic_writes() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("parent")).unwrap();
        std::fs::write(root.path().join("parent/file"), "inside").unwrap();
        std::fs::write(outside.path().join("file"), "outside").unwrap();
        let dir = parent(root.path(), Path::new("parent/file"), false).unwrap();
        std::fs::rename(root.path().join("parent"), root.path().join("original")).unwrap();
        symlink(outside.path(), root.path().join("parent")).unwrap();
        let mut text = String::new();
        read_file(&dir, "file".as_ref())
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "inside");
        atomic_write(&dir, "file".as_ref(), b"changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("original/file")).unwrap(),
            "changed"
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("file")).unwrap(),
            "outside"
        );
        assert!(parent(root.path(), Path::new("parent/file"), true).is_err());
    }
}
