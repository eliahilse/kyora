use assert_cmd::Command;
use serde_json::{Value, json};
fn command(dir: &tempfile::TempDir) -> Command {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kyora");
    cmd.env("KYORA_HOME", dir.path().join("home"))
        .env("HOME", dir.path())
        .env_remove("KYORA_FAKE_SCRIPT")
        .env_remove("KYORA_MODEL")
        .env_remove("KYORA_LLM_MODEL")
        .env_remove("KYORA_EFFORT")
        .env_remove("ANTHROPIC_API_KEY");
    cmd
}
fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"))
}
fn run(dir: &tempfile::TempDir, name: &str) -> Command {
    let mut cmd = command(dir);
    cmd.args(["run", "task", "--fake-script", &fixture(name), "-C"])
        .arg(dir.path());
    cmd
}
fn records(dir: &tempfile::TempDir) -> (std::path::PathBuf, Vec<Value>) {
    let session = std::fs::read_dir(dir.path().join("home/sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let path = session.join("events.jsonl");
    let records = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (path, records)
}
#[test]
fn version_and_help() {
    let dir = tempfile::tempdir().unwrap();
    command(&dir).arg("--version").assert().success();
    let out = command(&dir).arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().contains("sessions"));
}
#[cfg(unix)]
#[test]
fn logical_workspace_spelling_can_write_with_cd_or_pwd() {
    use std::os::unix::fs::symlink;
    for spelling in ["absolute", "relative", "pwd"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let alias = dir.path().join("logical");
        symlink(&root, &alias).unwrap();
        let script = dir.path().join("script.json");
        std::fs::write(
            &script,
            json!({"rules":[{"responses":[
                {"content":[{"type":"tool_use","id":"write","name":"write_file",
                    "input":{"path":alias.join("file"),"content":"logical write"}},
                    {"type":"tool_use","id":"cwd","name":"shell",
                    "input":{"command":"pwd -P"}}],"stop_reason":"tool_use"},
                {"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}
            ]}]})
            .to_string(),
        )
        .unwrap();
        let mut cmd = command(&dir);
        cmd.args(["run", "task", "--fake-script"]).arg(&script);
        match spelling {
            "absolute" => {
                cmd.arg("-C").arg(&alias).env("PWD", dir.path());
            }
            "relative" => {
                cmd.current_dir(dir.path())
                    .arg("-C")
                    .arg("./logical/")
                    .env("PWD", dir.path());
            }
            _ => {
                cmd.current_dir(&root).env("PWD", &alias);
            }
        }
        cmd.assert().success().stdout("done\n");
        assert_eq!(
            std::fs::read_to_string(root.join("file")).unwrap(),
            "logical write"
        );
        let (_, events) = records(&dir);
        let canonical = std::fs::canonicalize(&root).unwrap();
        assert_eq!(events[0]["cwd"], canonical.to_str().unwrap());
        for call in ["write", "cwd"] {
            let result = events
                .iter()
                .find(|e| e["type"] == "tool_result" && e["call"] == call)
                .unwrap();
            assert_eq!(result["is_error"], false);
            if call == "cwd" {
                let output: Value =
                    serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
                assert_eq!(output["stdout"], format!("{}\n", canonical.display()));
            }
        }
    }
}
#[test]
fn missing_api_key_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    command(&dir)
        .args(["run", "task"])
        .assert()
        .code(1)
        .stderr(format!("error: no API key for provider anthropic: set ANTHROPIC_API_KEY or providers.anthropic.api_key in {}\n", dir.path().join("home/config.toml").display()));
    assert!(!dir.path().join("home").exists());
}
#[test]
fn tool_round_trip_and_session_records_order_permissions_and_list() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&dir, "round-trip").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"done\n");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "hello\n"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("tool write_file"));
    assert!(stderr.contains("tool read_file"));
    let (path, events) = records(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let types = events
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        vec![
            "session_start",
            "node_start",
            "message",
            "attempt_start",
            "attempt_end",
            "message",
            "tool_call",
            "tool_result",
            "message",
            "attempt_start",
            "attempt_end",
            "message",
            "tool_call",
            "tool_result",
            "message",
            "attempt_start",
            "attempt_end",
            "message",
            "node_end",
            "session_end"
        ]
    );
    for (seq, e) in events.iter().enumerate() {
        assert_eq!(e["seq"], seq);
        assert_eq!(e["v"], 1);
        assert!(e["ts"].as_str().unwrap().ends_with('Z'));
    }
    for attempt in 0..3 {
        let start = events
            .iter()
            .position(|e| e["type"] == "attempt_start" && e["attempt"] == attempt)
            .unwrap();
        let end = events
            .iter()
            .position(|e| e["type"] == "attempt_end" && e["attempt"] == attempt)
            .unwrap();
        assert!(start < end);
    }
    let list = command(&dir).args(["sessions", "--json"]).output().unwrap();
    assert!(list.status.success());
    let summary: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(
        summary["id"],
        path.parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["task_preview"], "task");
    let list = command(&dir).arg("sessions").output().unwrap();
    assert!(
        String::from_utf8(list.stdout)
            .unwrap()
            .contains("completed\ttask")
    );
}

#[test]
fn tui_help_and_nonterminal_error() {
    let help = Command::new(env!("CARGO_BIN_EXE_kyora"))
        .args(["tui", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8(help.stdout).unwrap().contains("--demo"));
    for args in [vec![], vec!["tui"], vec!["tui", "--demo"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_kyora"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("requires an interactive terminal")
        );
    }
}
#[test]
fn invalid_tool_input_returns_error_and_does_not_execute() {
    let dir = tempfile::tempdir().unwrap();
    run(&dir, "invalid")
        .assert()
        .success()
        .stdout("recovered\n");
    let (_, events) = records(&dir);
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["is_error"], true);
    assert!(result["content"].as_str().unwrap().contains("INVALID_JSON"));
    assert!(!dir.path().join("result.txt").exists());
}
#[test]
fn limits_exit_three_and_print_partial_answer() {
    let dir = tempfile::tempdir().unwrap();
    run(&dir, "pause")
        .args(["--max-turns", "1"])
        .assert()
        .code(3)
        .stdout("partial\n");
    let dir = tempfile::tempdir().unwrap();
    run(&dir, "final")
        .args(["--budget", "1"])
        .assert()
        .code(3)
        .stdout("\n");
    let (_, events) = records(&dir);
    assert!(!events.iter().any(|e| e["type"] == "attempt_start"));
    assert_eq!(events.last().unwrap()["status"], "budget_exhausted");
}
#[test]
fn refusal_and_context_exhaustion_do_not_execute_tools() {
    for (name, code) in [("refusal", 4), ("context", 3)] {
        let dir = tempfile::tempdir().unwrap();
        run(&dir, name).assert().code(code);
        assert!(!dir.path().join("forbidden").exists());
        let (_, events) = records(&dir);
        assert_eq!(
            events.iter().find(|e| e["type"] == "tool_result").unwrap()["is_error"],
            true
        );
    }
}
#[test]
fn no_session_and_quiet_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    run(&dir, "final")
        .args(["--no-session", "--quiet"])
        .assert()
        .success()
        .stdout("done\n")
        .stderr("");
    assert!(!dir.path().join("home").exists());
    command(&dir)
        .args(["sessions", "--json"])
        .assert()
        .success()
        .stdout("");
    assert!(!dir.path().join("home").exists());
}
#[test]
fn flags_override_environment_and_fake_script_environment_works() {
    let dir = tempfile::tempdir().unwrap();
    command(&dir)
        .env("KYORA_FAKE_SCRIPT", fixture("final"))
        .env("KYORA_MODEL", "fake/env-root")
        .env("KYORA_LLM_MODEL", "fake/env-leaf")
        .args([
            "run",
            "task",
            "-m",
            "fake/flag-root",
            "--effort",
            "max",
            "--tools",
            "read_file",
        ])
        .assert()
        .success();
    let (_, events) = records(&dir);
    let start = events.iter().find(|e| e["type"] == "node_start").unwrap();
    assert_eq!(start["model"], "fake/flag-root");
    assert_eq!(start["tools"].as_array().unwrap().len(), 1);
}
#[test]
fn bad_flags_and_zero_limits_are_usage_errors() {
    for args in [
        vec!["--max-turns", "0"],
        vec!["--budget", "0"],
        vec!["--timeout", "0s"],
        vec!["--max-depth", "0"],
        vec!["--max-agents", "-1"],
        vec!["--tools", "missing"],
        vec!["--effort", "invalid"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        run(&dir, "final").args(args).assert().code(2);
    }
}
#[test]
fn unknown_tools_are_usage_errors_before_provider_setup() {
    let dir = tempfile::tempdir().unwrap();
    let output = command(&dir)
        .args(["run", "task", "--tools", "missing"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: unknown tool: missing\n"
    );
    assert!(!dir.path().join("home").exists());
}
#[test]
fn json_ndjson_event_shape_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(&dir, "final").args(["--json"]).output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let events = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for event in &events {
        assert_eq!(event["v"], 1);
        assert!(event["ts"].as_str().unwrap().ends_with('Z'));
        let _: kyora_core::trace::TraceRecord = serde_json::from_value(event.clone()).unwrap();
    }
    // Redact identifiers, paths, timestamps, durations and request-size arithmetic.
    let shape = events
        .into_iter()
        .map(|mut e| {
            e["ts"] = json!("<timestamp>");
            for key in [
                "session",
                "cwd",
                "attempt",
                "request_id",
                "ms",
                "reserved",
                "charged",
                "system",
            ] {
                if e.get(key).is_some() {
                    e[key] = json!(format!("<{key}>"));
                }
            }
            e.as_object_mut().unwrap().remove("tools");
            e.as_object_mut().unwrap().remove("limits");
            e
        })
        .collect::<Vec<_>>();
    insta::assert_json_snapshot!("json_events", shape);
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_gracefully_and_kills_the_shell_group() {
    use nix::{
        sys::signal::{Signal, kill, killpg},
        unistd::Pid,
    };
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("cancel.json");
    std::fs::write(&script,json!({"rules":[{"responses":[{"content":[{"type":"tool_use","id":"wait","name":"shell","input":{"command":"echo $$ > shell.pid; sleep 60 & echo $! > child.pid; wait"}}],"stop_reason":"tool_use"}]}]}).to_string()).unwrap();
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("kyora"))
        .args(["run", "task", "--fake-script"])
        .arg(script)
        .arg("-C")
        .arg(dir.path())
        .env("HOME", dir.path())
        .env("KYORA_HOME", dir.path().join("home"))
        .env_remove("KYORA_FAKE_SCRIPT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    struct Cleanup {
        child: std::process::Child,
        path: std::path::PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(pid) = std::fs::read_to_string(self.path.join("shell.pid"))
                .and_then(|s| s.trim().parse::<i32>().map_err(std::io::Error::other))
            {
                let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    let mut cleanup = Cleanup {
        child,
        path: dir.path().into(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.path().join("child.pid").exists() {
        assert!(Instant::now() < deadline, "shell did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    kill(Pid::from_raw(cleanup.child.id() as i32), Signal::SIGINT).unwrap();
    loop {
        if let Some(status) = cleanup.child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(130));
            break;
        }
        assert!(Instant::now() < deadline, "cancel did not terminate");
        std::thread::sleep(Duration::from_millis(10));
    }
    let (_, events) = records(&dir);
    assert_eq!(events.last().unwrap()["status"], "cancelled");
    assert_eq!(
        events.iter().find(|e| e["type"] == "tool_result").unwrap()["is_error"],
        true
    );
    let pid = std::fs::read_to_string(dir.path().join("child.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    for _ in 0..100 {
        if kill(Pid::from_raw(pid), None).is_err() {
            return;
        }
        #[cfg(target_os = "linux")]
        if std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            s.split_once(") ")
                .is_some_and(|(_, tail)| tail.starts_with('Z'))
        }) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("child survived cancellation");
}

#[cfg(unix)]
#[test]
fn fifo_read_does_not_block_run_timeout_or_first_ctrl_c() {
    use nix::{
        sys::{
            signal::{Signal, kill},
            stat::Mode,
        },
        unistd::{Pid, mkfifo},
    };
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    struct Cleanup(std::process::Child);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for interrupt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        mkfifo(&dir.path().join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let script = dir.path().join("fifo.json");
        // Both calls belong to one turn. The shell marks that the FIFO read has
        // returned, then keeps the run active for timeout or the first SIGINT.
        std::fs::write(&script, json!({"rules":[{"responses":[{
            "content":[
                {"type":"tool_use","id":"fifo","name":"read_file","input":{"path":"pipe"}},
                {"type":"tool_use","id":"wait","name":"shell","input":{"command":"echo ready > ready; sleep 60"}}
            ],"stop_reason":"tool_use"
        }]}]}).to_string()).unwrap();
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("kyora"));
        command
            .args(["run", "task", "--fake-script"])
            .arg(script)
            .arg("-C")
            .arg(dir.path())
            .env("HOME", dir.path())
            .env("KYORA_HOME", dir.path().join("home"))
            .env_remove("KYORA_FAKE_SCRIPT")
            .env_remove("KYORA_MODEL")
            .env_remove("KYORA_LLM_MODEL")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.args(["--timeout", if interrupt { "60s" } else { "300ms" }]);
        let mut cleanup = Cleanup(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        if interrupt {
            while !dir.path().join("ready").exists() {
                assert!(Instant::now() < deadline, "FIFO read blocked the executor");
                assert!(
                    cleanup.0.try_wait().unwrap().is_none(),
                    "run exited before SIGINT"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            kill(Pid::from_raw(cleanup.0.id() as i32), Signal::SIGINT).unwrap();
        }
        loop {
            if let Some(status) = cleanup.0.try_wait().unwrap() {
                assert_eq!(status.code(), Some(if interrupt { 130 } else { 3 }));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "FIFO read prevented run termination"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let (_, events) = records(&dir);
        let result = events
            .iter()
            .find(|e| e["type"] == "tool_result" && e["call"] == "fifo")
            .unwrap();
        assert_eq!(result["is_error"], true);
        assert!(result["content"].as_str().unwrap().contains("non-regular"));
        assert_eq!(
            events.last().unwrap()["status"],
            if interrupt { "cancelled" } else { "timeout" }
        );
    }
}
