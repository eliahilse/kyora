use kyora_tui::{
    app::{App, Focus},
    demo,
    event::{NodeSpec, UiEvent},
    view,
};
use ratatui::{Terminal, backend::TestBackend, style::Color};

fn running() -> App {
    let mut app = App::new(true);
    app.begin_turn(demo::PROMPT.into());
    app.apply(UiEvent::TextDelta {
        node: 0,
        text: "I will review the reports with three agents and check dates with llm() calls."
            .into(),
    });
    app.apply(UiEvent::ToolCallStarted {
        node: 0,
        id: "python-1".into(),
        name: "python".into(),
        args: demo::CODE.into(),
    });
    app.apply(UiEvent::ReplCellStarted {
        node: NodeSpec {
            id: 1,
            parent: Some(0),
            name: "cell 1".into(),
            model: "fake/root".into(),
        },
        code: demo::CODE.into(),
    });
    for id in 2..8 {
        let node = NodeSpec {
            id,
            parent: Some(1),
            name: format!(
                "{} {}",
                if id < 5 { "report" } else { "date" },
                if id < 5 { id - 2 } else { id - 5 }
            ),
            model: if id < 5 { "fake/agent" } else { "fake/leaf" }.into(),
        };
        app.apply(if id < 5 {
            UiEvent::AgentSpawned(node)
        } else {
            UiEvent::LlmCall { node }
        });
    }
    app.apply(UiEvent::Usage {
        node: 0,
        tokens: 1200,
        cost_microusd: 2400,
    });
    app.apply(UiEvent::Budget {
        node: None,
        remaining: 18800,
    });
    app
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| view::draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            let line = (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            format!("{}\n", line.trim_end())
        })
        .collect()
}

fn snapshot(name: &str, actual: String, expected: &str) {
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/snapshots")
            .join(name);
        std::fs::write(path, &actual).unwrap();
    } else {
        assert_eq!(actual, expected, "layout snapshot {name}");
    }
}

#[test]
fn empty_layout() {
    snapshot(
        "empty.txt",
        render(&mut App::new(true), 120, 32),
        include_str!("snapshots/empty.txt"),
    );
}

#[test]
fn mid_run_with_recursion_tree() {
    snapshot(
        "mid-run.txt",
        render(&mut running(), 120, 32),
        include_str!("snapshots/mid-run.txt"),
    );
}

#[test]
fn narrow_80_by_24() {
    snapshot(
        "narrow.txt",
        render(&mut running(), 80, 24),
        include_str!("snapshots/narrow.txt"),
    );
}

#[test]
fn resize_and_no_color_preserve_all_panes() {
    let mut app = running();
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    for (width, height) in [(120, 32), (80, 24), (40, 20), (1, 1)] {
        terminal.backend_mut().resize(width, height);
        terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
        );
    }
}

#[test]
fn selection_expansion_and_modals_render() {
    let mut app = running();
    app.focus = Focus::Tree;
    app.selected_node = 1;
    assert!(render(&mut app, 80, 24).contains("ThreadPoolExecutor"));
    app.selected_node = 0;
    app.focus = Focus::Conversation;
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(render(&mut app, 120, 32).contains("[-] python"));
    app.help = true;
    assert!(render(&mut app, 80, 24).contains("quit outside input"));
    app.help = false;
    app.confirm_quit = true;
    assert!(render(&mut app, 80, 24).contains("A run is active"));
}
