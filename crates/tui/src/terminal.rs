use std::{
    io::{self, IsTerminal},
    panic,
    time::Duration,
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{Action, App, Focus},
    demo,
    event::{Status, UiEvent},
    view,
};

type UiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn restore() {
    let _ = execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

struct Turn {
    receiver: mpsc::Receiver<UiEvent>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl Turn {
    fn start(app: &mut App, prompt: String) -> Self {
        app.begin_turn(prompt);
        let first_id = app.nodes.keys().next_back().copied().unwrap_or(0) + 1;
        let tokens = app.totals().0;
        let root_tokens = app.nodes[&0].tokens;
        let (sender, receiver) = mpsc::channel(128);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = demo::play(
                sender.clone(),
                task_cancel.clone(),
                Duration::from_millis(85),
                first_id,
                tokens,
                root_tokens,
            )
            .await
                && !task_cancel.is_cancelled()
            {
                let _ = sender
                    .send(UiEvent::TextDelta {
                        node: 0,
                        text: format!("\nDemo error: {error}"),
                    })
                    .await;
                let _ = sender
                    .send(UiEvent::AgentFinished {
                        node: 0,
                        status: Status::Failed,
                    })
                    .await;
            }
        });
        Self {
            receiver,
            cancel,
            task,
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

/// Launches on an interactive terminal. Enter submits offline turns until the
/// real runtime supplies an adapter for UiEvent and app::Action.
pub async fn run(demo_on_start: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "the TUI requires an interactive terminal; run `kyora tui --demo` in a terminal"
    );
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore();
        previous_hook(info);
    }));
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    // Supported terminals report Shift+Enter separately; Ctrl-J also inserts
    // a newline on terminals with the legacy keyboard protocol.
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let mut terminal = UiTerminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let mut app = App::new(std::env::var_os("NO_COLOR").is_some());
    let mut turn = demo_on_start.then(|| Turn::start(&mut app, demo::PROMPT.into()));
    let mut dirty = true;
    loop {
        if let Some(current) = &mut turn {
            // Bound each batch so input stays responsive under an event flood.
            for _ in 0..128 {
                match current.receiver.try_recv() {
                    Ok(event) => {
                        app.apply(event);
                        dirty = true;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        if app.active() {
                            app.cancel_running();
                            app.notice = "Demo ended before all nodes finished.".into();
                        } else if app.nodes[&0].status == Status::Done {
                            app.notice =
                                "Demo complete. Inspect the tree or send another prompt.".into();
                        }
                        turn = None;
                        dirty = true;
                        break;
                    }
                }
            }
        }
        if dirty {
            terminal.draw(|frame| view::draw(frame, &mut app))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(16))? {
            let action = match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Paste(text)
                    if app.focus == Focus::Input && !app.help && !app.confirm_quit =>
                {
                    app.input.insert_str(text);
                    Action::None
                }
                Event::Resize(_, _) => Action::None,
                _ => continue,
            };
            dirty = true;
            match action {
                Action::None => {}
                Action::Submit(prompt) => {
                    turn = Some(Turn::start(&mut app, prompt));
                }
                Action::Cancel => {
                    turn = None;
                    if app.active() {
                        app.cancel_running();
                    }
                }
                Action::Quit => break,
            }
        }
    }
    Ok(())
}
