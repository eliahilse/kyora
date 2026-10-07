use assert_cmd::Command;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const KEY: &str = "test-only-cli-credential";
const FILE_KEY: &str = "test-only-file-credential";
const TEXT: &str = include_str!("fixtures/anthropic-text.sse");

fn command(dir: &tempfile::TempDir) -> Command {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kyora");
    cmd.env("HOME", dir.path())
        .env("KYORA_HOME", dir.path().join("home"))
        .env_remove("KYORA_MODEL")
        .env_remove("KYORA_LLM_MODEL")
        .env_remove("KYORA_EFFORT")
        .env_remove("KYORA_FAKE_SCRIPT")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("KYORA_TEST_API_KEY")
        .timeout(Duration::from_secs(10));
    cmd
}

fn config(dir: &tempfile::TempDir, text: &str) -> std::path::PathBuf {
    let path = dir.path().join("home/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn assert_private(output: &std::process::Output, dir: &tempfile::TempDir) {
    for key in [KEY, FILE_KEY] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(key));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(key));
        scan(&dir.path().join("home/sessions"), key);
    }
}

fn scan(path: &Path, key: &str) {
    if !path.exists() {
        return;
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            scan(&entry.unwrap().path(), key);
        }
    } else {
        assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains(key));
    }
}

async fn server(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"max_input_tokens": 100_000, "max_tokens": 128})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    server
}

fn sse() -> ResponseTemplate {
    // The fixture stores event lines; terminate its last SSE frame here.
    ResponseTemplate::new(200).set_body_raw(format!("{TEXT}\n"), "text/event-stream")
}

#[test]
fn unknown_providers_are_usage_errors_for_either_model() {
    for flag in ["--model", "--llm-model"] {
        let dir = tempfile::tempdir().unwrap();
        let output = command(&dir)
            .args(["run", "task", flag, "unknown/model"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("unknown provider unknown"));
        assert!(error.contains("supported providers: anthropic"));
        assert!(!dir.path().join("home/sessions").exists());
    }
}

#[test]
fn fake_script_accepts_all_provider_names() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.json");
    std::fs::write(&script, json!({"rules":[{"responses":[{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}]}]}).to_string()).unwrap();
    command(&dir)
        .args([
            "run",
            "task",
            "-m",
            "one/model",
            "--llm-model",
            "two/model",
            "--fake-script",
        ])
        .arg(script)
        .assert()
        .success()
        .stdout("ok\n");
}

#[test]
fn invalid_configs_report_path_without_source_or_credentials() {
    for text in [
        format!("unknown = '{KEY}'"),
        format!("[providers]\nunknown = '{KEY}'"),
        format!("[providers.anthropic]\nunknown = '{KEY}'"),
        format!("[providers.anthropic]\napi_key = '{KEY}'\nbase_url = ["),
        format!("effort = '{KEY}'"),
        format!("[providers.anthropic]\napi_key = ['{KEY}']"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = config(&dir, &text);
        let output = command(&dir).args(["run", "task"]).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("invalid config"));
        assert_private(&output, &dir);
    }
}

#[test]
fn config_diagnostics_never_output_delimiters_or_credential_values() {
    for (setting, category) in [
        (format!("effort = '\u{0060}{KEY}'"), "wrong type for effort"),
        (format!("effort = '\"{KEY}'"), "wrong type for effort"),
        (
            format!("effort = \"\\\"{KEY}\\\"\""),
            "wrong type for effort",
        ),
        (format!("effort = \"`\\\"'{KEY}\""), "wrong type for effort"),
        (format!("'`{KEY}' = 'value'"), "unknown field"),
        (format!("\"\\\"{KEY}\\\"\" = 'value'"), "unknown field"),
        (format!("'{KEY}' = 'value'"), "unknown field"),
        (
            format!("effort = \"\\\"{KEY}\"unterminated"),
            "syntax error",
        ),
        (format!("model = ['`{KEY}']"), "wrong type for model"),
        (
            format!("llm_model = ['\\\"{KEY}']"),
            "wrong type for llm_model",
        ),
        ("providers = 12".into(), "wrong type for providers"),
        (
            "[providers]\nanthropic = 12".into(),
            "wrong type for providers.anthropic",
        ),
        (
            format!("[providers.anthropic]\nbase_url = ['{KEY}']"),
            "wrong type for providers.anthropic.base_url",
        ),
        (
            format!("[providers.anthropic]\napi_key_env = ['{KEY}']"),
            "wrong type for providers.anthropic.api_key_env",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let text = if setting.starts_with("[providers.anthropic]\n") {
            format!("{setting}\napi_key = '{KEY}'")
        } else if setting.starts_with("providers =") || setting.starts_with("[providers]\n") {
            // A wrong provider table type cannot also hold a valid credential.
            setting
        } else {
            format!("providers.anthropic.api_key = '{KEY}'\n{setting}")
        };
        let path = config(&dir, &text);
        let output = command(&dir)
            .args(["run", "task", "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("at line ") && error.contains(", column "));
        assert!(error.contains(category), "expected {category}, got {error}");
        assert_private(&output, &dir);
        assert!(output.stdout.is_empty());
        assert!(!dir.path().join("home/sessions").exists());
    }
}

#[test]
fn invalid_models_report_the_setting_and_winning_source_without_values() {
    for (setting, flag, variable) in [
        ("model", "--model", "KYORA_MODEL"),
        ("llm_model", "--llm-model", "KYORA_LLM_MODEL"),
    ] {
        for source in ["config", "env", "flag"] {
            let dir = tempfile::tempdir().unwrap();
            let path = config(&dir, &format!("{setting} = '/'"));
            let mut cmd = command(&dir);
            cmd.args(["run", "task"]);
            let expected = match source {
                "flag" => {
                    cmd.env(variable, "anthropic/env-model").args([flag, "/"]);
                    format!("flag {flag}")
                }
                "env" => {
                    cmd.env(variable, "/");
                    format!("environment variable {variable}")
                }
                _ => format!("config {} key {setting}", path.display()),
            };
            let output = cmd.output().unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert_eq!(
                String::from_utf8_lossy(&output.stderr),
                format!("error: invalid {setting} from {expected}: expected provider/model\n")
            );
            assert!(output.stdout.is_empty());
            assert!(!dir.path().join("home/sessions").exists());
        }
    }
}

#[test]
fn credential_model_references_are_rejected_before_diagnostics_or_trace() {
    for (setting, flag, variable) in [
        ("model", "--model", "KYORA_MODEL"),
        ("llm_model", "--llm-model", "KYORA_LLM_MODEL"),
    ] {
        for source in ["config", "env", "flag"] {
            for key in [FILE_KEY, KEY] {
                for reference in [
                    format!("prefix-{key}/model"),
                    format!("anthropic/prefix-{key}"),
                    format!("/{key}"),
                ] {
                    for scripted in [false, true] {
                        let dir = tempfile::tempdir().unwrap();
                        let model = if source == "config" {
                            reference.as_str()
                        } else {
                            "anthropic/config-model"
                        };
                        config(
                            &dir,
                            &format!(
                                "{setting} = '{model}'\n[providers.anthropic]\napi_key = '{FILE_KEY}'\napi_key_env = 'KYORA_TEST_API_KEY'"
                            ),
                        );
                        let mut cmd = command(&dir);
                        cmd.args(["run", "task", "--json"])
                            .env("KYORA_TEST_API_KEY", KEY)
                            // The credential check must precede this diagnostic too.
                            .env("KYORA_EFFORT", "invalid");
                        match source {
                            "env" => {
                                cmd.env(variable, &reference);
                            }
                            "flag" => {
                                cmd.args([flag, &reference]);
                            }
                            _ => {}
                        }
                        if scripted {
                            cmd.arg("--fake-script").arg(
                                Path::new(env!("CARGO_MANIFEST_DIR"))
                                    .join("tests/fixtures/final.json"),
                            );
                        }
                        let output = cmd.output().unwrap();
                        assert_eq!(output.status.code(), Some(2));
                        assert_eq!(
                            output.stderr,
                            b"error: model settings must not contain API credentials\n"
                        );
                        assert!(output.stdout.is_empty());
                        assert_private(&output, &dir);
                        // No events.jsonl may be created before rejection.
                        assert!(!dir.path().join("home/sessions").exists());
                    }
                }
            }
        }
    }
}

#[test]
fn selected_environment_key_is_checked_before_other_model_errors() {
    let dir = tempfile::tempdir().unwrap();
    let output = command(&dir)
        .args(["run", "task", "--json", "--model", "/", "--llm-model"])
        .arg(format!("anthropic/{KEY}"))
        .arg("--fake-script")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/final.json"))
        .env("ANTHROPIC_API_KEY", KEY)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        output.stderr,
        b"error: model settings must not contain API credentials\n"
    );
    assert!(output.stdout.is_empty());
    assert_private(&output, &dir);
    assert!(!dir.path().join("home").exists());
}

#[tokio::test]
async fn base_url_flag_overrides_environment_and_config() {
    let server = server(sse()).await;
    let dir = tempfile::tempdir().unwrap();
    config(
        &dir,
        "[providers.anthropic]\nbase_url = 'http://127.0.0.1:1'",
    );
    let mut cmd = command(&dir);
    cmd.args(["run", "task", "--base-url", &server.uri()])
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:2")
        .env("ANTHROPIC_API_KEY", KEY);
    let output = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, "Hello 世界\n".as_bytes());
    assert_private(&output, &dir);
}

#[cfg(unix)]
#[test]
fn insecure_config_keys_are_refused_even_with_environment_override() {
    use std::os::unix::fs::PermissionsExt;
    for mode in [0o644, 0o640, 0o601] {
        let dir = tempfile::tempdir().unwrap();
        let path = config(
            &dir,
            &format!("[providers.anthropic]\napi_key = '{FILE_KEY}'"),
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let output = command(&dir)
            .args(["run", "task"])
            .env("ANTHROPIC_API_KEY", KEY)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("chmod 600"));
        assert_private(&output, &dir);
    }
}

#[test]
fn missing_indirect_key_names_the_configured_variable() {
    let dir = tempfile::tempdir().unwrap();
    let path = config(
        &dir,
        "[providers.anthropic]\napi_key_env = 'KYORA_TEST_API_KEY'",
    );
    command(&dir).args(["run", "task"]).assert().code(1).stderr(format!("error: no API key for provider anthropic: set KYORA_TEST_API_KEY or providers.anthropic.api_key in {}\n", path.display()));
}

#[tokio::test]
async fn anthropic_run_uses_config_credentials_and_keeps_outputs_private() {
    for json_output in [false, true] {
        let server = server(sse()).await;
        let dir = tempfile::tempdir().unwrap();
        config(
            &dir,
            &format!(
                "model = 'anthropic/claude-haiku-4-5'\neffort = 'high'\n[providers.anthropic]\napi_key = '{FILE_KEY}'\nbase_url = '{}'",
                server.uri()
            ),
        );
        let mut cmd = command(&dir);
        cmd.args(["run", "task"]);
        if json_output {
            cmd.arg("--json");
        }
        let output = tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if json_output {
            assert!(String::from_utf8_lossy(&output.stdout).contains("Hello "));
        } else {
            assert_eq!(String::from_utf8_lossy(&output.stdout), "Hello 世界\n");
        }
        assert_private(&output, &dir);
        let requests = server.received_requests().await.unwrap();
        let request = requests
            .iter()
            .find(|request| request.method == "POST")
            .unwrap();
        assert_eq!(request.headers["x-api-key"], FILE_KEY);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["max_tokens"], 128);
    }
}

#[tokio::test]
async fn key_indirection_and_environment_override_config_values() {
    for indirect in [false, true] {
        let server = server(sse()).await;
        let dir = tempfile::tempdir().unwrap();
        config(
            &dir,
            &format!(
                "[providers.anthropic]\napi_key_env = '{}'\napi_key = '{FILE_KEY}'\nbase_url = 'http://127.0.0.1:1'",
                if indirect {
                    "KYORA_TEST_API_KEY"
                } else {
                    "ANTHROPIC_API_KEY"
                }
            ),
        );
        let mut cmd = command(&dir);
        cmd.args(["run", "task", "--no-session"])
            .env("ANTHROPIC_BASE_URL", server.uri())
            .env(
                if indirect {
                    "KYORA_TEST_API_KEY"
                } else {
                    "ANTHROPIC_API_KEY"
                },
                KEY,
            );
        if indirect {
            cmd.env("ANTHROPIC_API_KEY", "unused-test-only-key");
        }
        let output = tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap();
        assert!(output.status.success());
        assert_private(&output, &dir);
        assert!(!dir.path().join("home/sessions").exists());
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.headers["x-api-key"] == KEY)
        );
    }
}

#[tokio::test]
async fn provider_errors_do_not_leak_credentials_to_json_or_sessions() {
    let server = server(ResponseTemplate::new(401).set_body_json(
        json!({"error":{"type":"authentication_error","message":format!("invalid key {KEY}")}}),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = command(&dir);
    cmd.args(["run", "task", "--json"])
        .env("ANTHROPIC_API_KEY", KEY)
        .env("ANTHROPIC_BASE_URL", server.uri());
    let output = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_private(&output, &dir);
    assert!(String::from_utf8_lossy(&output.stdout).contains("[redacted]"));
}

#[test]
fn live_anthropic_run_smoke() {
    if std::env::var("KYORA_LIVE_TESTS").as_deref() != Ok("1") {
        return;
    }
    let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
        return;
    };
    if key.is_empty() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let output = command(&dir)
        .args([
            "run",
            "--no-session",
            "-m",
            "anthropic/claude-haiku-4-5",
            "Reply with the single word ok.",
        ])
        .env("ANTHROPIC_API_KEY", key)
        .timeout(Duration::from_secs(120))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).trim().is_empty());
}
