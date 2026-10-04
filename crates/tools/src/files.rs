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
    sync::{Arc, Mutex, Weak},
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
/// Exact-match editor with shared per-file-identity locks.
#[derive(Clone, Default)]
pub struct EditFile {
    locks: LockRegistry,
    #[cfg(test)]
    observe: Option<Arc<dyn Fn(EditPhase, FileIdentity) + Send + Sync>>,
}
#[cfg(test)]
#[derive(Clone, Copy)]
enum EditPhase {
    Identified,
    Locked,
    Replaced,
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
fn path(input: &Value, root: &Path) -> Result<PathBuf> {
    let p = input["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let supplied = Path::new(p);
    // Only trusted root paths may be canonicalized. Reject unrelated absolute
    // paths lexically, without probing model-supplied ancestors or descendants.
    let absolute_relative;
    let supplied = if supplied.is_absolute() {
        absolute_relative = match supplied.strip_prefix(root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => {
                let canonical = std::fs::canonicalize(root)?;
                supplied
                    .strip_prefix(canonical)
                    .map_err(|_| {
                        anyhow::anyhow!("absolute path must be inside the workspace root")
                    })?
                    .to_path_buf()
            }
        };
        absolute_relative.as_path()
    } else {
        supplied
    };
    let mut relative = PathBuf::new();
    for component in supplied.components() {
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
    mutating: bool,
    work: impl FnOnce() -> Result<String> + Send + 'static,
) -> ToolOutput {
    blocking_work(cx.cancel, cx.node.deadline, mutating, work).await
}
async fn blocking_work(
    cancel: tokio_util::sync::CancellationToken,
    deadline: tokio::time::Instant,
    mutating: bool,
    work: impl FnOnce() -> Result<String> + Send + 'static,
) -> ToolOutput {
    if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
        return ToolOutput::error("cancelled");
    }
    let work_cancel = cancel.clone();
    let task = tokio::task::spawn_blocking(move || {
        if work_cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
            bail!("cancelled");
        }
        work()
    });
    if mutating {
        return output(task.await.map_err(anyhow::Error::from).and_then(|r| r));
    }
    tokio::select! {
        biased;
        _ = cancel.cancelled() => ToolOutput::error("cancelled"),
        _ = tokio::time::sleep_until(deadline) => ToolOutput::error("cancelled"),
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
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    device: nix::libc::dev_t,
    inode: nix::libc::ino_t,
}
type LockRegistry = Arc<Mutex<HashMap<FileIdentity, Weak<Mutex<()>>>>>;

// Each holder and waiter owns a strong reference. Remove all expired identities
// when an attempt leaves, including the old and replacement inode's entries.
struct FileLock {
    lock: Option<Arc<Mutex<()>>>,
    registry: LockRegistry,
}
impl FileLock {
    fn acquire(registry: &LockRegistry, identity: FileIdentity) -> Self {
        let mut entries = registry.lock().expect("file registry poisoned");
        let lock = entries
            .get(&identity)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let lock = Arc::new(Mutex::new(()));
                entries.insert(identity, Arc::downgrade(&lock));
                lock
            });
        Self {
            lock: Some(lock),
            registry: registry.clone(),
        }
    }
    fn lock(&self) -> &Arc<Mutex<()>> {
        self.lock.as_ref().expect("active file lock")
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        let mut entries = self.registry.lock().expect("file registry poisoned");
        drop(self.lock.take());
        entries.retain(|_, lock| lock.strong_count() > 0);
    }
}
fn file_identity(dir: &File, name: &std::ffi::OsStr) -> Result<FileIdentity> {
    let stat = fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    regular(stat.st_mode)?;
    Ok(FileIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
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
            json!({"path":{"type":"string","description":"Path relative to the workspace. Absolute paths must be inside the workspace."},"offset":{"type":"integer"},"limit":{"type":"integer"}}),
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
        blocking(cx, false, move || reader.read(&input, &cwd)).await
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
        let p = path(input, cwd)?;
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
            json!({"path":{"type":"string","description":"Path relative to the workspace. Absolute paths must be inside the workspace."},"content":{"type":"string"}}),
            json!(["path", "content"]),
            true,
        )
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        let cwd = cx.cwd.clone();
        blocking(cx, true, move || {
            let p = path(&input, &cwd)?;
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
            json!({"path":{"type":"string","description":"Path relative to the workspace. Absolute paths must be inside the workspace."},"old":{"type":"string"},"new":{"type":"string"},"replace_all":{"type":"boolean"}}),
            json!(["path", "old", "new"]),
            true,
        )
    }
    fn effect(&self) -> Effect {
        Effect::Mutating
    }
    async fn call(&self, input: Value, cx: ToolCx) -> ToolOutput {
        let editor = self.clone();
        let cwd = cx.cwd.clone();
        blocking(cx, true, move || editor.edit(&input, &cwd)).await
    }
}
impl EditFile {
    #[cfg(test)]
    fn observe(&self, phase: EditPhase, identity: FileIdentity) {
        if let Some(observe) = &self.observe {
            observe(phase, identity);
        }
    }
    fn edit(&self, input: &Value, cwd: &Path) -> Result<String> {
        let locks = &self.locks;
        let p = path(input, cwd)?;
        let dir = parent(cwd, &p, false)?;
        let name = p.file_name().expect("file name");
        // Atomic replacement changes the inode. Revalidate after waiting and
        // publish the new identity under the same lock before releasing it.
        loop {
            let identity = file_identity(&dir, name)?;
            let lease = FileLock::acquire(locks, identity);
            let lock = lease.lock();
            #[cfg(test)]
            self.observe(EditPhase::Identified, identity);
            let _guard = lock.lock().expect("file lock poisoned");
            if file_identity(&dir, name)? != identity {
                continue;
            }
            #[cfg(test)]
            self.observe(EditPhase::Locked, identity);

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
            let mut registry = locks.lock().expect("file registry poisoned");
            atomic_write(&dir, name, text.replace(old, new).as_bytes())?;
            registry.insert(file_identity(&dir, name)?, Arc::downgrade(lock));
            drop(registry);
            #[cfg(test)]
            self.observe(EditPhase::Replaced, identity);
            return Ok(format!("replaced {count} matches"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn staggered_edits_revalidate_waiters_and_serialize_replacement_inode() {
        use std::{
            sync::atomic::{AtomicUsize, Ordering},
            time::{Duration, Instant},
        };
        let root = tempfile::tempdir().unwrap();
        let initial = (0..12).map(|i| format!("old{i:02} ")).collect::<String>();
        std::fs::write(root.path().join("file"), initial).unwrap();
        let dir = Arc::new(parent(root.path(), Path::new("file"), false).unwrap());
        let identified = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let locked = Arc::new(AtomicUsize::new(0));
        let replaced = Arc::new(AtomicUsize::new(0));
        let (published, wait_for_publish) = std::sync::mpsc::channel();
        let editor = EditFile {
            locks: LockRegistry::default(),
            observe: Some(Arc::new(move |phase, identity| {
                let wait_for = |count: usize| {
                    let until = Instant::now() + Duration::from_secs(5);
                    while identified.load(Ordering::SeqCst) < count {
                        assert!(Instant::now() < until, "edits did not reach the barrier");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                };
                match phase {
                    EditPhase::Identified => {
                        identified.fetch_add(1, Ordering::SeqCst);
                    }
                    EditPhase::Locked => {
                        // A waiter must validate the inode it is about to edit,
                        // rather than enter using an identity read before waiting.
                        assert!(
                            file_identity(&dir, "file".as_ref()).unwrap() == identity,
                            "waiter entered with a stale file identity"
                        );
                        assert_eq!(
                            active.fetch_add(1, Ordering::SeqCst),
                            0,
                            "edits on the replacement inode overlapped a lock holder"
                        );
                        if locked.fetch_add(1, Ordering::SeqCst) == 0 {
                            // Queue five waiters on the original inode.
                            wait_for(6);
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    EditPhase::Replaced => {
                        if replaced.fetch_add(1, Ordering::SeqCst) == 0 {
                            published.send(()).unwrap();
                            // Admit six more edits after replacement, while this
                            // edit still holds the lock. They must all wait too.
                            wait_for(12);
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        assert_eq!(active.fetch_sub(1, Ordering::SeqCst), 1);
                    }
                }
            })),
        };
        let mut tasks = Vec::new();
        for i in 0..12 {
            if i == 6 {
                wait_for_publish
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            let editor = editor.clone();
            let cwd = root.path().to_path_buf();
            tasks.push(std::thread::spawn(move || {
                editor.edit(
                    &json!({"path":"file","old":format!("old{i:02}"),"new":format!("new{i:02}")}),
                    &cwd,
                )
            }));
            std::thread::sleep(Duration::from_millis(1));
        }
        for task in tasks {
            task.join().unwrap().unwrap();
        }
        let expected = (0..12).map(|i| format!("new{i:02} ")).collect::<String>();
        assert_eq!(
            std::fs::read_to_string(root.path().join("file")).unwrap(),
            expected
        );
        assert!(editor.locks.lock().unwrap().is_empty());
    }

    #[test]
    fn completed_edits_release_all_inode_registry_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "a").unwrap();
        let editor = EditFile::default();
        for i in 0..2000 {
            let (old, new) = if i % 2 == 0 { ("a", "b") } else { ("b", "a") };
            editor
                .edit(&json!({"path":"file","old":old,"new":new}), root.path())
                .unwrap();
            assert!(editor.locks.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn absolute_paths_outside_both_root_spellings_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let alias = outside.path().join("alias");
        symlink(root.path(), &alias).unwrap();
        // Even an existing alias to the root is outside the lexical boundary.
        // Accepting it would require probing an untrusted filesystem path.
        for supplied in [alias.join("file"), outside.path().join("missing/file")] {
            let error = path(&json!({"path":supplied}), root.path()).unwrap_err();
            assert_eq!(
                error.to_string(),
                "absolute path must be inside the workspace root"
            );
        }
    }

    #[tokio::test]
    async fn started_blocking_mutations_report_their_real_success_or_failure() {
        for fails in [false, true] {
            let cancel = tokio_util::sync::CancellationToken::new();
            let worker_cancel = cancel.clone();
            let (entered, ready) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("file");
            let worker_target = target.clone();
            let task = tokio::spawn(blocking_work(
                worker_cancel,
                tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                true,
                move || {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    if fails {
                        bail!("write failed");
                    }
                    std::fs::write(worker_target, "finished")?;
                    Ok("wrote file".into())
                },
            ));
            ready.await.unwrap();
            cancel.cancel();
            tokio::task::yield_now().await;
            assert!(
                !task.is_finished(),
                "mutation returned before blocking work finished"
            );
            release.send(()).unwrap();
            let result = task.await.unwrap();
            assert_eq!(result.is_error, fails);
            assert_eq!(
                result.text_content(),
                if fails { "write failed" } else { "wrote file" }
            );
            assert_eq!(target.exists(), !fails);
        }
    }

    #[tokio::test]
    async fn read_wait_can_be_cancelled_while_blocking_work_is_still_running() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(blocking_work(
            cancel.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            false,
            move || {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok("read finished".into())
            },
        ));
        ready.await.unwrap();
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        release.send(()).unwrap();
        assert!(result.is_error);
        assert_eq!(result.text_content(), "cancelled");
    }

    #[test]
    fn file_identity_matches_hard_link_aliases() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "text").unwrap();
        std::fs::hard_link(root.path().join("file"), root.path().join("alias")).unwrap();
        let dir = parent(root.path(), Path::new("file"), false).unwrap();
        assert!(
            file_identity(&dir, "file".as_ref()).unwrap()
                == file_identity(&dir, "alias".as_ref()).unwrap()
        );
    }

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
