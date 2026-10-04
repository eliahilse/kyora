use kyora_core::{
    Limits, TraceEvent,
    session::{SessionStore, list, locked_file},
};
use serde_json::json;

#[tokio::test]
async fn records_are_gapless_private_locked_and_flushed() {
    let home = tempfile::tempdir().unwrap();
    let session = SessionStore::create(home.path()).unwrap();
    session
        .trace
        .emit(TraceEvent::SessionStart {
            session: session.id.clone(),
            cwd: home.path().into(),
            kyora: "test".into(),
            limits: Limits::default(),
        })
        .await
        .unwrap();
    let path = session.path.join("events.jsonl");
    assert!(locked_file(&path, false).is_err());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'));
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(value["seq"], 0);
    assert!(value["ts"].as_str().unwrap().ends_with('Z'));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&session.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    session
        .trace
        .emit(TraceEvent::Message {
            node: 0,
            message: kyora_protocol::Message::user_text("task"),
        })
        .await
        .unwrap();
    session
        .trace
        .emit(TraceEvent::StreamReset { node: 0 })
        .await
        .unwrap();
    session
        .trace
        .emit(TraceEvent::SessionEnd {
            status: kyora_core::Status::Completed,
        })
        .await
        .unwrap();
    session.trace.finish().await.unwrap();
    assert!(locked_file(&path, false).is_ok());
    let records = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3);
    for (seq, r) in records.iter().enumerate() {
        assert_eq!(r["seq"], seq);
    }
    let summaries = list(home.path()).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].status, "completed");
    assert_eq!(summaries[0].task_preview, "task");
}
#[tokio::test]
async fn concurrent_writer_sequences_and_large_prompt_blobs() {
    let home = tempfile::tempdir().unwrap();
    let store = SessionStore::create(home.path()).unwrap();
    let mut tasks = Vec::new();
    for node in 0..20 {
        let trace = store.trace.clone();
        tasks.push(tokio::spawn(async move {
            trace
                .emit(TraceEvent::Error {
                    node,
                    message: "test".into(),
                })
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let prompt = "x".repeat(70_000);
    store
        .trace
        .emit(TraceEvent::NodeStart {
            node: 20,
            parent: Some(0),
            depth: 0,
            kind: "llm".into(),
            name: String::new(),
            model: "fake/test".into(),
            system: None,
            tools: vec![],
            limits: Limits::default(),
            prompt: Some(json!(prompt)),
        })
        .await
        .unwrap();
    store.trace.finish().await.unwrap();
    let records = std::fs::read_to_string(store.path.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .collect::<Vec<_>>();
    for (seq, r) in records.iter().enumerate() {
        assert_eq!(r["seq"], seq);
    }
    let p = &records[20]["prompt"];
    let hash = p["blob"].as_str().unwrap().strip_prefix("sha256:").unwrap();
    let blob = std::fs::read(store.path.join("blobs").join(hash)).unwrap();
    assert_eq!(serde_json::from_slice::<String>(&blob).unwrap(), prompt);
    assert_eq!(p["bytes"], blob.len());
}
#[test]
fn absent_sessions_does_not_create_files_and_crashed_tail_is_readable() {
    let home = tempfile::tempdir().unwrap();
    assert!(list(home.path()).unwrap().is_empty());
    assert!(!home.path().join("sessions").exists());
    let path = home.path().join("sessions/test");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join("events.jsonl"),
        "{\"type\":\"session_start\",\"ts\":\"test\"}\n{partial",
    )
    .unwrap();
    assert_eq!(list(home.path()).unwrap()[0].status, "interrupted");
}
