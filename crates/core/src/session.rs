//! Private session files, exclusive locks, a single writer and session discovery.
use crate::{
    defaults,
    trace::{TraceRecord, TraceSink, WriteCommand},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

/// A newly created private session directory and its trace sink.
pub struct SessionStore {
    /// UUIDv7 session identifier.
    pub id: String,
    /// Session directory.
    pub path: PathBuf,
    /// Single writer and live broadcast.
    pub trace: TraceSink,
}
impl SessionStore {
    /// Creates sessions/uuidv7/events.jsonl with an exclusive lock.
    pub fn create(home: &Path) -> Result<Self> {
        let id = Uuid::now_v7().to_string();
        let path = home.join("sessions").join(&id);
        private_dir(&path)?;
        let file = locked_file(&path.join("events.jsonl"), true)?;
        let (live, _) = broadcast::channel(defaults::TRACE_CAPACITY);
        let (writer, mut rx) = mpsc::channel(defaults::WRITER_CAPACITY);
        let broadcast = live.clone();
        let directory = path.clone();
        let join = tokio::spawn(async move {
            let mut file = file;
            let mut seq = 0;
            let mut failed: Option<String> = None;
            while let Some(command) = rx.recv().await {
                match command {
                    WriteCommand::Event(event, ack) => {
                        let record = TraceRecord::new(*event, Some(seq));
                        let result = if let Some(error) = &failed {
                            Err(anyhow::anyhow!(error.clone()))
                        } else {
                            write_record(&mut file, &directory, &record)
                        };
                        if let Err(error) = &result {
                            failed = Some(error.to_string());
                        }
                        if result.is_ok() {
                            seq += 1;
                            let _ = broadcast.send(record);
                        }
                        let _ = ack.send(result);
                    }
                    WriteCommand::Finish(ack) => {
                        let _ = ack.send(file.flush().map_err(Into::into));
                        break;
                    }
                }
            }
            // Dropping file releases the lock, even when all producers disappear.
        });
        Ok(Self {
            id,
            path,
            trace: TraceSink::writer(live, writer, join),
        })
    }
}
/// Opens the session event file and fails fast if another writer owns it.
/// `create_new` prevents accidental replacement of an existing transcript.
pub fn locked_file(path: &Path, create_new: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).append(true).create_new(create_new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock()
        .context("session is locked by another writer")?;
    Ok(file)
}
fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}
fn write_record(file: &mut File, dir: &Path, record: &TraceRecord) -> Result<()> {
    let mut value = serde_json::to_value(record)?;
    // Messages stay inline for append-only replay. Large leaf prompts use blobs.
    if let Some(prompt) = value.get_mut("prompt") {
        let bytes = serde_json::to_vec(prompt)?;
        if bytes.len() > defaults::BLOB_THRESHOLD {
            let hash = format!("{:x}", Sha256::digest(&bytes));
            let blobs = dir.join("blobs");
            private_dir(&blobs)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(blobs.join(&hash)) {
                Ok(mut blob) => {
                    blob.write_all(&bytes)?;
                    blob.flush()?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            *prompt = serde_json::json!({"blob": format!("sha256:{hash}"), "bytes": bytes.len()});
        }
    }
    serde_json::to_writer(&mut *file, &value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}
/// A session summary for list views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session directory name.
    pub id: String,
    /// Start timestamp.
    pub start_time: String,
    /// Final status, or interrupted if no session_end was flushed.
    pub status: String,
    /// First user task, bounded to the configured preview length.
    pub task_preview: String,
}
/// Lists sessions without creating directories. Malformed completed lines are errors.
/// An incomplete last line is ignored, making crashed runs readable.
pub fn list(home: &Path) -> Result<Vec<SessionSummary>> {
    list_with_preview(home, defaults::SESSION_PREVIEW_CHARS)
}
/// Lists sessions with an explicit task-preview character cap.
pub fn list_with_preview(home: &Path, preview_chars: usize) -> Result<Vec<SessionSummary>> {
    let directory = home.join("sessions");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut summaries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("events.jsonl");
        if !path.exists() {
            continue;
        }
        let mut summary = SessionSummary {
            id: entry.file_name().to_string_lossy().into_owned(),
            start_time: String::new(),
            status: "interrupted".into(),
            task_preview: String::new(),
        };
        let mut reader = BufReader::new(File::open(path)?);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if !line.ends_with(b"\n") {
                break;
            }
            let record: serde_json::Value = serde_json::from_slice(&line)?;
            match record["type"].as_str() {
                Some("session_start") => {
                    summary.start_time = record["ts"].as_str().unwrap_or_default().into()
                }
                Some("session_end") => {
                    summary.status = record["status"].as_str().unwrap_or("failed").into()
                }
                Some("message") if record["node"] == 0 && summary.task_preview.is_empty() => {
                    let message: kyora_protocol::Message =
                        serde_json::from_value(record["message"].clone())?;
                    if message.role == kyora_protocol::Role::User {
                        summary.task_preview = message.text().chars().take(preview_chars).collect();
                    }
                }
                _ => {}
            }
        }
        summaries.push(summary);
    }
    summaries.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(summaries)
}
