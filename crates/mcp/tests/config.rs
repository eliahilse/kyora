#![cfg(unix)]
use kyora_mcp::{McpConfig, defaults};
use std::time::Duration;

fn parse(text: &str) -> McpConfig {
    toml::from_str(text).unwrap()
}

fn error(text: &str) -> String {
    parse(text).validate().unwrap_err().to_string()
}

#[test]
fn stdio_and_http_servers_parse_with_defaults() {
    let config = parse(
        r#"
        [servers.files]
        command = "mcp-files"
        args = ["--root", "."]
        env = { MODE = "fast" }
        env_vars = ["GITHUB_TOKEN"]
        cwd = "sub"
        tool_timeout_s = 2.5
        deny_tools = ["delete"]

        [servers.remote]
        url = "https://mcp.example.com/mcp"
        bearer_token_env = "REMOTE_TOKEN"
        headers = { X-Team = "core" }
        env_headers = { X-Api-Key = "REMOTE_KEY" }
        startup_timeout_s = 5
        allow_tools = ["search", "delete"]
        enabled = false
        "#,
    );
    config.validate().unwrap();
    let files = &config.servers["files"];
    assert_eq!(files.command.as_deref(), Some("mcp-files"));
    assert_eq!(files.args, ["--root", "."]);
    assert_eq!(files.env["MODE"], "fast");
    assert_eq!(files.tool_timeout(), Duration::from_millis(2500));
    assert_eq!(files.startup_timeout(), defaults::STARTUP_TIMEOUT);
    assert!(files.enabled);
    assert!(files.exposes("read") && !files.exposes("delete"));
    let remote = &config.servers["remote"];
    assert_eq!(remote.startup_timeout(), Duration::from_secs(5));
    assert_eq!(remote.tool_timeout(), defaults::TOOL_TIMEOUT);
    assert!(!remote.enabled);
    assert!(remote.exposes("search") && remote.exposes("delete") && !remote.exposes("other"));
}

#[test]
fn unknown_fields_are_rejected() {
    assert!(toml::from_str::<McpConfig>("[servers.a]\ncommand = 'x'\ntimeout = 3").is_err());
    assert!(toml::from_str::<McpConfig>("other = 1").is_err());
}

#[test]
fn transports_must_be_unambiguous() {
    assert!(error("[servers.a]\ncommand = 'x'\nurl = 'http://h/'").contains("not both"));
    assert!(error("[servers.a]\nargs = ['x']").contains("set command"));
    assert!(error("[servers.a]\ncommand = ''").contains("empty"));
    assert!(error("[servers.a]\ncommand = 'x'\nheaders = { A = 'b' }").contains("url servers"));
    assert!(error("[servers.a]\nurl = 'http://h/'\nenv_vars = ['A']").contains("command servers"));
    assert!(error("[servers.a]\nurl = 'ftp://h/'").contains("http or https"));
    assert!(error("[servers.a]\nurl = 'not a url'").contains("not valid"));
    assert!(error("[servers.a]\ncommand = 'x'\ntool_timeout_s = 0").contains("positive"));
    assert!(error("[servers.a]\ncommand = 'x'\nstartup_timeout_s = -1").contains("positive"));
}

#[test]
fn server_names_keep_tool_names_unambiguous_and_are_not_echoed() {
    for name in [
        "a__b",
        "trailing_",
        "has space",
        "dot.ted",
        "",
        &"x".repeat(33),
    ] {
        let message = error(&format!("[servers.\"{name}\"]\ncommand = 'x'"));
        assert!(message.contains("invalid mcp server name"), "{message}");
        if name.len() > 2 {
            assert!(!message.contains(name), "{message}");
        }
    }
    for name in ["a", "my-server", "under_score", &"x".repeat(32)] {
        parse(&format!("[servers.{name}]\ncommand = 'x'"))
            .validate()
            .unwrap();
    }
}

#[test]
fn credentials_must_come_from_the_environment() {
    const SECRET: &str = "test-only-literal-credential";
    for text in [
        format!("[servers.a]\ncommand = 'x'\nenv = {{ GITHUB_TOKEN = '{SECRET}' }}"),
        format!(
            "[servers.a]\nurl = 'https://h/'\nheaders = {{ Authorization = 'Bearer {SECRET}' }}"
        ),
        format!("[servers.a]\nurl = 'https://h/'\nheaders = {{ X-Api-Key = '{SECRET}' }}"),
        format!("[servers.a]\nurl = 'https://user:{SECRET}@h/'"),
    ] {
        let message = error(&text);
        assert!(message.starts_with("mcp server a: "), "{message}");
        assert!(!message.contains(SECRET), "{message}");
    }
    assert!(
        error("[servers.a]\nurl = 'https://h/'\nheaders = { Accept = 'x' }").contains("reserved")
    );
    assert!(
        error("[servers.a]\nurl = 'https://h/'\nenv_headers = { Mcp-Session-Id = 'X' }")
            .contains("reserved")
    );
}
