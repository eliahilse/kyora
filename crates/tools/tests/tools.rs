#![cfg(unix)]
use kyora_core::{AgentSpec, Limits, Runtime, RuntimeConfig, Tool, Toolset, TraceEvent, TraceSink};
use kyora_protocol::{ContentBlock, ModelRequest, ModelResponse, StopReason, Usage};
use kyora_providers::{RetryPolicy, fake::FnProvider};
use kyora_tools::{EditFile, ReadFile, Shell, ShellConfig, WriteFile, defaults::FileConfig};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

async fn invoke(tool: Arc<dyn Tool>, input: Value, cwd: &std::path::Path) -> (String, bool) {
    let name = tool.spec().name;
    let provider = FnProvider::new(move |request: &ModelRequest| {
        Ok(ModelResponse {
            id: None,
            model: String::new(),
            content: if request.messages.len() == 1 {
                vec![ContentBlock::ToolUse {
                    id: "a".into(),
                    name: name.clone(),
                    input: input.clone(),
                }]
            } else {
                vec![ContentBlock::Text {
                    text: "done".into(),
                }]
            },
            stop_reason: if request.messages.len() == 1 {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage: Usage::default(),
            usage_iterations: vec![],
        })
    });
    let trace = TraceSink::ephemeral();
    let mut rx = trace.subscribe();
    let rt = Runtime::new(RuntimeConfig {
        providers: BTreeMap::from([(
            "fake".into(),
            Arc::new(provider) as Arc<dyn kyora_providers::ModelProvider>,
        )]),
        toolsets: Arc::new(Toolset::new(vec![tool]).unwrap()),
        limits: Limits {
            tool_output_chars: 50_000,
            ..Limits::default()
        },
        retry: RetryPolicy::default(),
        llm_model: "fake/leaf".parse().unwrap(),
        trace,
        session: "test".into(),
    })
    .unwrap();
    let mut spec = AgentSpec::new("task", cwd.into());
    spec.model = "fake/test".parse().unwrap();
    rt.run(spec).await.unwrap();
    while let Ok(record) = rx.try_recv() {
        if let TraceEvent::ToolResult {
            content, is_error, ..
        } = record.event
        {
            return (content, is_error);
        }
    }
    panic!("tool result missing")
}
#[tokio::test]
async fn file_round_trip_pages_edits_and_atomic_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (text, error) = invoke(
        Arc::new(WriteFile),
        json!({"path":"nested/file.txt","content":"one\ntwo\nthree\n"}),
        dir.path(),
    )
    .await;
    assert!(!error, "{text}");
    let (text, error) = invoke(
        Arc::new(ReadFile::new(FileConfig::default())),
        json!({"path":"nested/file.txt","offset":2,"limit":1}),
        dir.path(),
    )
    .await;
    assert!(!error);
    assert_eq!(text, "2: two\n");
    let editor = Arc::new(EditFile::default());
    let (_, error) = invoke(
        editor.clone(),
        json!({"path":"nested/file.txt","old":"two","new":"TWO"}),
        dir.path(),
    )
    .await;
    assert!(!error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("nested/file.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
    let (_, error) = invoke(
        editor,
        json!({"path":"nested/file.txt","old":"missing","new":"x"}),
        dir.path(),
    )
    .await;
    assert!(error);
    assert_eq!(
        std::fs::read_dir(dir.path().join("nested"))
            .unwrap()
            .count(),
        1
    );
}
#[tokio::test]
async fn edit_exactly_once_or_replace_all_and_alias_locks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file"), "same same").unwrap();
    let edit = Arc::new(EditFile::default());
    let (_, error) = invoke(
        edit.clone(),
        json!({"path":"file","old":"same","new":"other"}),
        dir.path(),
    )
    .await;
    assert!(error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        "same same"
    );
    let (_, error) = invoke(
        edit.clone(),
        json!({"path":"./file","old":"same","new":"other","replace_all":true}),
        dir.path(),
    )
    .await;
    assert!(!error);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        "other other"
    );
    let (_, error) = invoke(
        edit,
        json!({"path":"file","old":"","new":"x","replace_all":true}),
        dir.path(),
    )
    .await;
    assert!(error);
}
#[tokio::test]
async fn reads_refuse_binary_clip_long_lines_and_bound_bytes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("binary"), b"a\0b").unwrap();
    let reader = Arc::new(ReadFile::new(FileConfig {
        max_bytes: 100,
        line_chars: 40,
        lines: 2,
    }));
    let (_, error) = invoke(reader.clone(), json!({"path":"binary"}), dir.path()).await;
    assert!(error);
    std::fs::write(dir.path().join("large"), "x".repeat(1_000_000)).unwrap();
    let (text, error) = invoke(reader.clone(), json!({"path":"large"}), dir.path()).await;
    assert!(!error);
    assert!(text.contains("characters omitted"));
    assert!(text.contains("read byte limit"));
    assert!(text.len() < 100);
    let (_, error) = invoke(reader, json!({"path":"large","offset":0}), dir.path()).await;
    assert!(error);
}
#[tokio::test]
async fn shell_flood_is_drained_into_bounded_capture() {
    let dir = tempfile::tempdir().unwrap();
    let (output, error) = invoke(
        Arc::new(Shell::new(ShellConfig::default())),
        json!({"command":"yes | head -c 100000000"}),
        dir.path(),
    )
    .await;
    assert!(!error, "{output}");
    assert!(output.len() < 40_000);
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["exit_code"], 0);
    assert!(value["stdout"].as_str().unwrap().len() < 21_000);
    assert!(
        value["stdout"]
            .as_str()
            .unwrap()
            .contains("99980000 bytes omitted")
    );
}
fn running(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        if stat
            .split_once(") ")
            .is_some_and(|(_, tail)| tail.starts_with('Z'))
        {
            return false;
        }
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}
#[tokio::test]
async fn timeout_kills_background_process_group_and_reaps_shell() {
    let dir = tempfile::tempdir().unwrap();
    let (output, error) = invoke(
        Arc::new(Shell::new(ShellConfig::default())),
        json!({"command":"sleep 60 & echo $! > child.pid; wait","timeout_s":0.2}),
        dir.path(),
    )
    .await;
    assert!(error);
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap()["error"],
        "timeout"
    );
    let pid = std::fs::read_to_string(dir.path().join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for _ in 0..100 {
        if !running(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!running(pid), "background child survived timeout");
}
#[tokio::test]
async fn missing_bash_falls_back_to_sh_and_stdin_is_null() {
    let dir = tempfile::tempdir().unwrap();
    let config = ShellConfig {
        shells: vec![dir.path().join("absent"), "/bin/sh".into()],
        ..ShellConfig::default()
    };
    let (output, error) = invoke(
        Arc::new(Shell::new(config)),
        json!({"command":"read value; printf 'stdin=%s' \"$?\""}),
        dir.path(),
    )
    .await;
    assert!(!error);
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap()["stdout"],
        "stdin=1"
    );
}
#[test]
fn scrubbed_env_subprocess() {
    if std::env::var_os("KYORA_TOOLS_ENV_TEST").is_some() {
        let dir = tempfile::tempdir().unwrap();
        let config = ShellConfig {
            env_extras: vec![
                "ANTHROPIC_API_KEY".into(),
                "OPENAI_API_KEY".into(),
                "KYORA_ALLOWED_TEST".into(),
            ],
            ..ShellConfig::default()
        };
        let (output,error)=tokio::runtime::Runtime::new().unwrap().block_on(invoke(Arc::new(Shell::new(config)),json!({"command":"printf '%s/%s/%s/%s' \"${ANTHROPIC_API_KEY-unset}\" \"${OPENAI_API_KEY-unset}\" \"${KYORA_NOT_ALLOWED-unset}\" \"${KYORA_ALLOWED_TEST-unset}\""}),dir.path()));
        assert!(!error);
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap()["stdout"],
            "unset/unset/unset/allowed"
        );
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "scrubbed_env_subprocess", "--nocapture"])
        .env("HOME", home.path())
        .env("KYORA_TOOLS_ENV_TEST", "1")
        .env("ANTHROPIC_API_KEY", "fake-test-value")
        .env("OPENAI_API_KEY", "fake-test-value")
        .env("KYORA_NOT_ALLOWED", "denied")
        .env("KYORA_ALLOWED_TEST", "allowed")
        .spawn()
        .unwrap();
    assert!(child.wait().unwrap().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_edits_preserve_both_changes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file"), "alpha beta").unwrap();
    let editor = Arc::new(EditFile::default());
    let path = dir.path().to_path_buf();
    let other_path = path.clone();
    let other = editor.clone();
    let first = tokio::spawn(async move {
        invoke(
            editor,
            json!({"path":"file","old":"alpha","new":"ALPHA"}),
            &path,
        )
        .await
    });
    let second = tokio::spawn(async move {
        invoke(
            other,
            json!({"path":"./file","old":"beta","new":"BETA"}),
            &other_path,
        )
        .await
    });
    assert!(!first.await.unwrap().1);
    assert!(!second.await.unwrap().1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        "ALPHA BETA"
    );
}

#[tokio::test]
async fn file_tools_refuse_workspace_escapes_and_symlinks() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("file"), "original").unwrap();
    symlink(&outside, root.join("linked-parent")).unwrap();
    symlink(outside.join("file"), root.join("linked-file")).unwrap();
    for path in [
        outside.join("file").to_string_lossy().into_owned(),
        "../outside/file".into(),
        "nested/../../outside/file".into(),
        "linked-parent/file".into(),
        "linked-file".into(),
    ] {
        for tool in [
            Arc::new(WriteFile) as Arc<dyn Tool>,
            Arc::new(EditFile::default()),
            Arc::new(ReadFile::new(FileConfig::default())),
        ] {
            let input = match tool.spec().name.as_str() {
                "write_file" => json!({"path":path,"content":"changed"}),
                "edit_file" => json!({"path":path,"old":"original","new":"changed"}),
                _ => json!({"path":path}),
            };
            let (text, error) = invoke(tool, input, &root).await;
            assert!(error, "accepted {path}: {text}");
            assert_eq!(
                std::fs::read_to_string(outside.join("file")).unwrap(),
                "original"
            );
        }
    }
    let (_, error) = invoke(
        Arc::new(WriteFile),
        json!({"path":"linked-parent/new/file","content":"escape"}),
        &root,
    )
    .await;
    assert!(error);
    assert!(!outside.join("new").exists());
    let (_, error) = invoke(
        Arc::new(WriteFile),
        json!({"path":"nested/../safe","content":"ok"}),
        &root,
    )
    .await;
    assert!(!error);
    assert_eq!(std::fs::read_to_string(root.join("safe")).unwrap(), "ok");
}

#[tokio::test]
async fn file_tools_refuse_fifos_without_a_writer() {
    use nix::{sys::stat::Mode, unistd::mkfifo};
    let dir = tempfile::tempdir().unwrap();
    mkfifo(&dir.path().join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    for tool in [
        Arc::new(ReadFile::new(FileConfig::default())) as Arc<dyn Tool>,
        Arc::new(EditFile::default()),
        Arc::new(WriteFile),
    ] {
        let input = match tool.spec().name.as_str() {
            "write_file" => json!({"path":"pipe","content":"replacement"}),
            "edit_file" => json!({"path":"pipe","old":"a","new":"b"}),
            _ => json!({"path":"pipe"}),
        };
        let (text, error) =
            tokio::time::timeout(Duration::from_secs(2), invoke(tool, input, dir.path()))
                .await
                .unwrap();
        assert!(error);
        assert!(text.contains("non-regular"), "{text}");
    }
}

#[tokio::test]
async fn absolute_workspace_paths_and_root_aliases_are_accepted() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let alias = dir.path().join("alias");
    symlink(&root, &alias).unwrap();
    let canonical = std::fs::canonicalize(&root).unwrap();
    for (cwd, prefix) in [(&root, &root), (&alias, &alias), (&alias, &canonical)] {
        let path = prefix.join("nested/file");
        let (text, error) = invoke(
            Arc::new(WriteFile),
            json!({"path":path,"content":"one"}),
            cwd,
        )
        .await;
        assert!(!error, "{text}");
        let (text, error) = invoke(
            Arc::new(EditFile::default()),
            json!({"path":path,"old":"one","new":"two"}),
            cwd,
        )
        .await;
        assert!(!error, "{text}");
        let (text, error) = invoke(
            Arc::new(ReadFile::new(FileConfig::default())),
            json!({"path":path}),
            cwd,
        )
        .await;
        assert!(!error, "{text}");
        assert_eq!(text, "1: two\n");
    }
    #[cfg(target_os = "macos")]
    {
        let short = std::path::Path::new("/").join(canonical.strip_prefix("/private").unwrap());
        let (text, error) = invoke(
            Arc::new(ReadFile::new(FileConfig::default())),
            json!({"path":short.join("nested/file")}),
            &short,
        )
        .await;
        assert!(!error, "{text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_case_and_unicode_alias_edits_preserve_all_updates() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let editor = Arc::new(EditFile::default());
    for (first, second) in [
        ("Case.txt", "case.txt"),
        ("caf\u{e9}.txt", "cafe\u{301}.txt"),
    ] {
        std::fs::write(dir.path().join(first), "alpha beta gamma").unwrap();
        let a = std::fs::metadata(dir.path().join(first)).unwrap();
        let Ok(b) = std::fs::metadata(dir.path().join(second)) else {
            continue;
        };
        if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
            continue;
        }
        for _ in 0..8 {
            // A long read widens the overlap between concurrent edits.
            let padding = "x".repeat(1024 * 1024);
            std::fs::write(dir.path().join(first), format!("alpha beta gamma{padding}")).unwrap();
            let (a, b, c) = tokio::join!(
                invoke(
                    editor.clone(),
                    json!({"path":first,"old":"alpha","new":"ALPHA"}),
                    dir.path()
                ),
                invoke(
                    editor.clone(),
                    json!({"path":second,"old":"beta","new":"BETA"}),
                    dir.path()
                ),
                invoke(
                    editor.clone(),
                    json!({"path":first,"old":"gamma","new":"GAMMA"}),
                    dir.path()
                ),
            );
            assert!(!a.1, "{}", a.0);
            assert!(!b.1, "{}", b.0);
            assert!(!c.1, "{}", c.0);
            assert_eq!(
                std::fs::read_to_string(dir.path().join(first)).unwrap(),
                format!("ALPHA BETA GAMMA{padding}")
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staggered_concurrent_edits_preserve_every_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let editor = Arc::new(EditFile::default());
    let padding = "x".repeat(8 * 1024 * 1024);
    for _ in 0..4 {
        let initial = (0..12).map(|i| format!("old{i:02} ")).collect::<String>();
        std::fs::write(dir.path().join("file"), format!("{initial}{padding}")).unwrap();
        let mut tasks = Vec::new();
        for i in 0..12 {
            let editor = editor.clone();
            let cwd = dir.path().to_path_buf();
            tasks.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(i * 2)).await;
                invoke(
                    editor,
                    json!({"path":"file", "old":format!("old{i:02}"), "new":format!("new{i:02}")}),
                    &cwd,
                )
                .await
            }));
        }
        for task in tasks {
            let (text, error) = task.await.unwrap();
            assert!(!error, "{text}");
        }
        let expected = (0..12).map(|i| format!("new{i:02} ")).collect::<String>();
        let actual = std::fs::read_to_string(dir.path().join("file")).unwrap();
        assert!(
            actual == format!("{expected}{padding}"),
            "lost a concurrent replacement: {}",
            &actual[..72]
        );
    }
}
