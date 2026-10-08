#![cfg(unix)]
use kyora_core::ToolOutput;
use kyora_mcp::{defaults, render_result, tool_name};
use serde_json::json;

fn valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= defaults::MAX_TOOL_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[test]
fn names_are_namespaced_sanitized_and_bounded() {
    assert_eq!(
        tool_name("github", "search_issues"),
        "mcp__github__search_issues"
    );
    assert_eq!(tool_name("a-b", "x-y_z9"), "mcp__a-b__x-y_z9");
    let dotted = tool_name("fs", "read.file");
    assert!(dotted.starts_with("mcp__fs__read_file_"), "{dotted}");
    // The hash keeps names apart that sanitize to the same text.
    assert_ne!(dotted, tool_name("fs", "read/file"));
    assert_ne!(dotted, tool_name("fs", "read_file"));
    assert_eq!(dotted, tool_name("fs", "read.file"));
    let server = "s".repeat(defaults::MAX_SERVER_NAME);
    for tool in [
        "x".repeat(200),
        "ünïcödé tööl".into(),
        String::new(),
        "a".repeat(64),
    ] {
        let name = tool_name(&server, &tool);
        assert!(valid(&name), "{name}");
        assert!(name.starts_with(&format!("mcp__{server}__")));
    }
    assert_ne!(
        tool_name("s", &format!("{}a", "x".repeat(100))),
        tool_name("s", &format!("{}b", "x".repeat(100)))
    );
}

fn text(output: &ToolOutput) -> String {
    output.text_content()
}

#[test]
fn results_render_every_content_type_as_text() {
    let output = render_result(&json!({"content": [
        {"type": "text", "text": "header"},
        {"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"},
        {"type": "audio", "data": "AAAA", "mimeType": "audio/wav"},
        {"type": "resource", "resource": {"uri": "file:///notes.txt", "text": "alpha"}},
        {"type": "resource", "resource": {"uri": "file:///blob.bin", "mimeType": "application/zip", "blob": "AAAAAA=="}},
        {"type": "resource_link", "uri": "file:///linked.md", "name": "linked", "description": "A linked file."},
        {"type": "hologram"},
    ]}));
    assert!(!output.is_error);
    assert_eq!(
        text(&output),
        "header\n[image: image/png, 5 bytes]\n[audio: audio/wav, 3 bytes]\n[resource: file:///notes.txt]\nalpha\n[resource: file:///blob.bin, application/zip, 4 bytes]\n[resource link: linked file:///linked.md]\nA linked file.\n[unsupported content type: hologram]"
    );
}

#[test]
fn errors_structured_and_empty_results() {
    let failed =
        render_result(&json!({"content": [{"type": "text", "text": "boom"}], "isError": true}));
    assert!(failed.is_error);
    assert_eq!(text(&failed), "boom");
    let bare = render_result(&json!({"isError": true}));
    assert!(bare.is_error);
    assert!(!text(&bare).is_empty());
    let structured = render_result(&json!({"content": [], "structuredContent": {"answer": 42}}));
    assert_eq!(text(&structured), r#"{"answer":42}"#);
    let empty = render_result(&json!({}));
    assert!(!empty.is_error);
    assert_eq!(text(&empty), "(no content)");
}
