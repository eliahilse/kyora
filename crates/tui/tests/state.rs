use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kyora_tui::{
    app::{Action, App, Entry, Focus},
    demo,
    event::{NodeKind, NodeSpec, Status, UiEvent},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn node(id: u64, parent: Option<u64>) -> NodeSpec {
    NodeSpec {
        id,
        parent,
        name: format!("node {id}"),
        model: "fake/test".into(),
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn tree_tracks_parentage_kind_status_usage_and_budgets() {
    let mut app = App::new(true);
    app.apply(UiEvent::AgentSpawned(node(0, None)));
    assert_eq!(app.nodes[&0].spec.model, "fake/test");
    app.apply(UiEvent::AgentSpawned(node(2, Some(1))));
    assert_eq!(app.tree_rows(), vec![(0, 0), (2, 0)]);
    app.apply(UiEvent::ReplCellStarted {
        node: node(1, Some(0)),
        code: "print(42)".into(),
    });
    app.apply(UiEvent::LlmCall {
        node: node(3, Some(2)),
    });
    assert_eq!(app.tree_rows(), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    assert_eq!(app.nodes[&1].kind, NodeKind::Cell);
    assert_eq!(app.nodes[&3].kind, NodeKind::Llm);
    app.apply(UiEvent::Usage {
        node: 2,
        tokens: 900,
        cost_microusd: 1800,
    });
    app.apply(UiEvent::Usage {
        node: 2,
        tokens: 900,
        cost_microusd: 1800,
    });
    app.apply(UiEvent::Usage {
        node: 3,
        tokens: 100,
        cost_microusd: 200,
    });
    assert_eq!(app.totals(), (1000, 2000));
    app.apply(UiEvent::Budget {
        node: None,
        remaining: 19000,
    });
    app.apply(UiEvent::Budget {
        node: Some(2),
        remaining: 2100,
    });
    assert_eq!(app.remaining, 19000);
    assert_eq!(app.nodes[&2].remaining, Some(2100));
    app.apply(UiEvent::AgentFinished {
        node: 2,
        status: Status::Failed,
    });
    app.apply(UiEvent::LlmCallFinished {
        node: 3,
        status: Status::Done,
    });
    app.apply(UiEvent::ReplCellFinished {
        node: 1,
        output: "42".into(),
        status: Status::Done,
    });
    app.apply(UiEvent::AgentFinished {
        node: 0,
        status: Status::Done,
    });
    assert_eq!(app.nodes[&2].status, Status::Failed);
    assert_eq!(app.nodes[&3].status, Status::Done);
    assert!(!app.active());
}

#[test]
fn malformed_cycles_are_bounded_and_duplicate_admission_keeps_topology() {
    let mut app = App::new(true);
    app.apply(UiEvent::AgentSpawned(node(1, Some(2))));
    app.apply(UiEvent::AgentSpawned(node(2, Some(1))));
    app.apply(UiEvent::AgentSpawned(node(1, Some(0))));
    assert_eq!(app.nodes[&1].spec.parent, Some(2));
    assert_eq!(app.tree_rows().len(), 3);
}

#[test]
fn interleaved_streams_and_tool_results_preserve_their_owner() {
    let mut app = App::new(true);
    for (id, text) in [(0, "hello"), (1, "child"), (0, " world")] {
        app.apply(UiEvent::TextDelta {
            node: id,
            text: text.into(),
        });
    }
    assert!(matches!(&app.entries[0], Entry::Assistant { text, .. } if text == "hello world"));
    for id in [0, 1] {
        app.apply(UiEvent::ToolCallStarted {
            node: id,
            id: "same-id".into(),
            name: "python".into(),
            args: "print(42)".into(),
        });
    }
    app.apply(UiEvent::ToolCallFinished {
        node: 1,
        id: "same-id".into(),
        result: "42".into(),
        status: Status::Done,
    });
    assert!(matches!(&app.entries[2], Entry::Tool { result: None, .. }));
    assert!(matches!(&app.entries[3], Entry::Tool { result: Some(result), .. } if result == "42"));
    app.focus = Focus::Conversation;
    app.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &app.entries[2],
        Entry::Tool { expanded: true, .. }
    ));
}

#[test]
fn composer_focus_help_and_quit_confirmation() {
    let mut app = App::new(true);
    app.input.insert_str("query 🦀");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    app.input.insert_str("line two");
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Action::Submit("query 🦀\nline two".into())
    );
    assert_eq!(app.input.lines(), &[""]);
    app.input.insert_str("a");
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert_eq!(app.input.lines(), &["a", ""]);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Conversation);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Tree);
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.focus, Focus::Conversation);
    app.begin_turn("test".into());
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
    assert!(app.confirm_quit);
    app.handle_key(key(KeyCode::Char('n')));
    assert!(!app.confirm_quit);
    app.handle_key(key(KeyCode::Char('?')));
    assert!(app.help);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
    assert!(!app.help);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::Cancel);
    app.cancel_running();
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Action::Quit
    );
}

#[test]
fn composer_keeps_single_character_shortcuts_as_text() {
    let mut app = App::new(true);
    for character in "why? q ".chars() {
        assert_eq!(app.handle_key(key(KeyCode::Char(character))), Action::None);
    }
    assert!(!app.help);
    assert!(!app.confirm_quit);
    assert_eq!(app.input.lines(), &["why? q "]);
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Action::Submit("why? q ".into())
    );
    app.handle_key(key(KeyCode::Char('?')));
    assert_eq!(app.input.lines(), &["?"]);
    assert!(!app.help);
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::Char('?')));
    assert!(app.help);
    app.handle_key(key(KeyCode::Char('?')));
    assert!(!app.help);
}

#[tokio::test]
async fn demo_streams_a_complete_tree_and_replays_with_exact_accounting() {
    let mut app = App::new(true);
    for (first_id, expected) in [(1, 4200), (8, 8400)] {
        let (sender, mut receiver) = mpsc::channel(4);
        let tokens = app.totals().0;
        let root_tokens = app.nodes[&0].tokens;
        app.begin_turn(demo::PROMPT.into());
        let task = tokio::spawn(demo::play(
            sender,
            CancellationToken::new(),
            Duration::ZERO,
            first_id,
            tokens,
            root_tokens,
        ));
        while let Some(event) = receiver.recv().await {
            app.apply(event);
        }
        task.await.unwrap().unwrap();
        assert!(!app.active());
        assert_eq!(app.totals(), (expected, expected * 2));
        assert_eq!(app.remaining, demo::TOKEN_BUDGET - expected);
    }
    assert_eq!(app.nodes.len(), 15);
    assert_eq!(
        app.nodes
            .values()
            .filter(|node| node.kind == NodeKind::Llm)
            .count(),
        6
    );
    assert!(
        app.entries
            .iter()
            .filter(|entry| matches!(
                entry,
                Entry::Tool {
                    status: Status::Done,
                    result: Some(_),
                    ..
                }
            ))
            .count()
            == 2
    );
}

#[tokio::test]
async fn cancellation_unblocks_driver_with_a_full_event_queue() {
    let (sender, mut receiver) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(demo::play(
        sender,
        cancel.clone(),
        Duration::from_secs(60),
        1,
        0,
        0,
    ));
    receiver.recv().await.unwrap();
    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

#[test]
fn cancellation_finishes_every_live_node_and_tool() {
    let mut app = App::new(true);
    app.begin_turn("test".into());
    app.apply(UiEvent::ReplCellStarted {
        node: node(1, Some(0)),
        code: "wait()".into(),
    });
    app.apply(UiEvent::AgentSpawned(node(2, Some(1))));
    app.apply(UiEvent::LlmCall {
        node: node(3, Some(1)),
    });
    app.apply(UiEvent::ToolCallStarted {
        node: 0,
        id: "t".into(),
        name: "python".into(),
        args: "wait()".into(),
    });
    app.cancel_running();
    assert!(!app.active());
    assert!(
        app.nodes
            .values()
            .all(|node| node.status == Status::Cancelled)
    );
    assert!(matches!(
        &app.entries[2],
        Entry::Tool {
            status: Status::Cancelled,
            result: Some(_),
            ..
        }
    ));
}
