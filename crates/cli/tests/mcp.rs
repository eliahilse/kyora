#![cfg(unix)]
use assert_cmd::Command;
use serde_json::{Value, json};
use std::time::Duration;

fn command(dir: &tempfile::TempDir) -> Command {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kyora");
    cmd.env("KYORA_HOME", dir.path().join("home"))
        .env("HOME", dir.path())
        .env_remove("KYORA_FAKE_SCRIPT")
        .env_remove("KYORA_MODEL")
        .env_remove("KYORA_LLM_MODEL")
        .env_remove("KYORA_EFFORT")
        .env_remove("ANTHROPIC_API_KEY")
        .timeout(Duration::from_secs(30));
    cmd
}

/// A Python stdio server with one `echo` tool, and a server that cannot start.
fn configure(dir: &tempfile::TempDir) {
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[mcp.servers.py]\ncommand = 'python3'\nargs = ['{server}']\n\n[mcp.servers.broken]\ncommand = '/nonexistent/kyora-mcp-server'\n"
        ),
    )
    .unwrap();
}

fn script(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("script.json");
    std::fs::write(
        &path,
        json!({"rules": [{"responses": [
            {"content": [{"type": "tool_use", "id": "echo", "name": "mcp__py__echo",
                "input": {"text": "from mcp"}}], "stop_reason": "tool_use"},
            {"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"},
        ]}]})
        .to_string(),
    )
    .unwrap();
    path
}

fn events(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn offered(events: &[Value]) -> Vec<String> {
    let start = events.iter().find(|e| e["type"] == "node_start").unwrap();
    start["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn mcp_tools_join_the_builtins_and_failed_servers_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    configure(&dir);
    let output = command(&dir)
        .args(["run", "task", "--json", "-C"])
        .arg(dir.path())
        .arg("--fake-script")
        .arg(script(&dir))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("warning: mcp server broken: "), "{stderr}");
    assert!(stderr.contains("continuing without its tools"), "{stderr}");
    let events = events(&output.stdout);
    let tools = offered(&events);
    assert!(tools.contains(&"mcp__py__echo".to_owned()), "{tools:?}");
    assert!(tools.contains(&"shell".to_owned()), "{tools:?}");
    let result = events
        .iter()
        .find(|e| e["type"] == "tool_result" && e["call"] == "echo")
        .unwrap();
    assert_eq!(result["content"], "from mcp");
    assert_eq!(result["is_error"], false);
}

#[test]
fn tools_flag_selects_mcp_tools_and_rejects_unavailable_ones() {
    let dir = tempfile::tempdir().unwrap();
    configure(&dir);
    let output = command(&dir)
        .args(["run", "task", "--json", "--tools", "mcp__py__echo", "-C"])
        .arg(dir.path())
        .arg("--fake-script")
        .arg(script(&dir))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(offered(&events(&output.stdout)), ["mcp__py__echo"]);
    // No selected tool can come from the broken server, so it is not started.
    assert!(!stderr.contains("mcp server broken"), "{stderr}");

    let dir = tempfile::tempdir().unwrap();
    configure(&dir);
    let output = command(&dir)
        .args(["run", "task", "--tools", "mcp__broken__search", "-C"])
        .arg(dir.path())
        .arg("--fake-script")
        .arg(script(&dir))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning: mcp server broken: "), "{stderr}");
    assert!(
        stderr.contains("unknown tool: mcp__broken__search"),
        "{stderr}"
    );
    assert!(!dir.path().join("home/sessions").exists());
}
