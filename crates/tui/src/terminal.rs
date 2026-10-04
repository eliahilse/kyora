use std::{
    io::{self, IsTerminal},
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, Once,
        atomic::{AtomicBool, Ordering},
    },
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
use futures::FutureExt;
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

struct RestoreState {
    once: Once,
    raw_enabled: AtomicBool,
    keyboard_pushed: AtomicBool,
}

impl Default for RestoreState {
    fn default() -> Self {
        Self {
            once: Once::new(),
            raw_enabled: AtomicBool::new(false),
            keyboard_pushed: AtomicBool::new(false),
        }
    }
}

impl RestoreState {
    fn restore_with(&self, cleanup: impl FnOnce(bool, bool)) {
        self.once.call_once(|| {
            cleanup(
                self.raw_enabled.load(Ordering::SeqCst),
                self.keyboard_pushed.load(Ordering::SeqCst),
            );
        });
    }

    fn restore(&self) {
        self.restore_with(|raw_enabled, keyboard_pushed| {
            if !raw_enabled {
                return;
            }
            if keyboard_pushed {
                let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
            }
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                LeaveAlternateScreen,
                crossterm::cursor::Show
            );
            let _ = disable_raw_mode();
        });
    }
}

#[derive(Default)]
struct ExitSignals {
    requested: Arc<AtomicBool>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl ExitSignals {
    #[cfg(unix)]
    fn new() -> io::Result<Self> {
        let mut signals = Self::default();
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            // The handler only sets a flag. Cleanup runs outside signal context.
            signals.registrations.push(signal_hook::flag::register(
                signal,
                Arc::clone(&signals.requested),
            )?);
        }
        Ok(signals)
    }

    #[cfg(not(unix))]
    fn new() -> io::Result<Self> {
        Ok(Self::default())
    }
}

impl Drop for ExitSignals {
    fn drop(&mut self) {
        #[cfg(unix)]
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

struct TerminalGuard {
    restore: Arc<RestoreState>,
    previous_hook: Option<Arc<PanicHook>>,
    signals: ExitSignals,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        let signals = ExitSignals::new()?;
        let restore = Arc::new(RestoreState::default());
        let previous_hook = Arc::new(panic::take_hook());
        let hook_restore = Arc::clone(&restore);
        let hook_previous = Arc::clone(&previous_hook);
        panic::set_hook(Box::new(move |info| {
            hook_restore.restore();
            hook_previous(info);
        }));
        Ok(Self {
            restore,
            previous_hook: Some(previous_hook),
            signals,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore.restore();
        drop(panic::take_hook());
        if let Some(previous) = self.previous_hook.take() {
            panic::set_hook(match Arc::try_unwrap(previous) {
                Ok(hook) => hook,
                Err(hook) => Box::new(move |info| hook(info)),
            });
        }
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
    let guard = TerminalGuard::new()?;
    // Rust forbids changing panic hooks during unwinding. Keep the guard outside
    // the caught future, restore its hook, then continue the original panic.
    let result = AssertUnwindSafe(run_session(demo_on_start, &guard))
        .catch_unwind()
        .await;
    drop(guard);
    match result {
        Ok(result) => result,
        Err(payload) => panic::resume_unwind(payload),
    }
}

async fn run_session(demo_on_start: bool, guard: &TerminalGuard) -> anyhow::Result<()> {
    enable_raw_mode()?;
    guard.restore.raw_enabled.store(true, Ordering::SeqCst);
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    // Supported terminals report Shift+Enter separately; Ctrl-J also inserts
    // a newline on terminals with the legacy keyboard protocol.
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    guard.restore.keyboard_pushed.store(true, Ordering::SeqCst);
    let mut terminal = UiTerminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let mut app = App::new(std::env::var_os("NO_COLOR").is_some());
    let mut turn = demo_on_start.then(|| Turn::start(&mut app, demo::PROMPT.into()));
    let mut dirty = true;
    while !guard.signals.requested.load(Ordering::SeqCst) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_runs_once_for_either_cleanup_order() {
        for paths in [["panic hook", "drop"], ["drop", "panic hook"]] {
            let state = RestoreState::default();
            state.raw_enabled.store(true, Ordering::SeqCst);
            state.keyboard_pushed.store(true, Ordering::SeqCst);
            let mut calls = Vec::new();
            for path in paths {
                state.restore_with(|raw, keyboard| calls.push((path, raw, keyboard)));
            }
            assert_eq!(calls, vec![(paths[0], true, true)]);
        }
    }

    #[test]
    fn partial_setup_does_not_pop_keyboard_flags() {
        let state = RestoreState::default();
        state.raw_enabled.store(true, Ordering::SeqCst);
        state.restore_with(|raw, keyboard| {
            assert!(raw);
            assert!(!keyboard);
        });
    }

    #[test]
    fn guard_restores_previous_hook_after_a_panic() {
        // Hooks are process-global. Isolate this regression from other tests.
        const CHILD: &str = "KYORA_TEST_PANIC_HOOK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "terminal::tests::guard_restores_previous_hook_after_a_panic",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = Arc::clone(&calls);
        panic::set_hook(Box::new(move |_| {
            hook_calls.fetch_add(1, Ordering::SeqCst);
        }));
        let guard = TerminalGuard::new().unwrap();
        let state = Arc::clone(&guard.restore);
        assert!(panic::catch_unwind(|| panic!("test session panic")).is_err());
        assert!(state.once.is_completed());
        drop(guard);
        assert!(panic::catch_unwind(|| panic!("test previous hook")).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Only the test owns this state now; the installed hook has released it.
        assert_eq!(Arc::strong_count(&state), 1);
    }
}
