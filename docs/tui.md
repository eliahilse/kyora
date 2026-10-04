# TUI prototype

Run from the workspace in an interactive terminal:

```sh
CARGO_BUILD_JOBS=4 cargo run -p kyora-cli --
CARGO_BUILD_JOBS=4 cargo run -p kyora-cli -- tui
CARGO_BUILD_JOBS=4 cargo run -p kyora-cli -- tui --demo
```

The first two commands open an empty session. `--demo` immediately streams a
scripted run. Sending any prompt plays the same fixture; further prompts replay
it with new child IDs and cumulative session accounting. No API key, network,
core runtime, or Python interpreter is needed. Python code is displayed but is
not executed. The fixture fans out three agents and three `kyora.llm()` calls
from a root REPL cell. Select a tree node to inspect its text or cell code/output.

The bottom bar shows the root model, session tokens, estimated cost, and shared
remaining budget. Tree rows show status, model, self tokens (`t`) and remaining
budget (`r`). A node-specific budget takes precedence over the shared snapshot.
Cell usage is not added to session totals, since its calls are charged individually.
Demo usage is synthetic. Cost uses an illustrative $2 per million processed tokens;
the fake provider is free. Budgets are display fixtures, not enforced runtime limits.

| Key | Action |
| --- | --- |
| Tab / Shift+Tab | Cycle input, conversation, and tree focus |
| Enter in input | Send a nonempty prompt while idle |
| Shift+Enter | Insert a newline |
| Ctrl-J | Insert a newline on terminals without enhanced keyboard support |
| Arrows in input | Edit multiline text; bracketed paste is supported |
| Up / Down in tree | Select a node and show its transcript |
| Up / Down in conversation | Select a tool block |
| Enter / Space in conversation | Expand or collapse selected tool arguments/result |
| PgUp / PgDn outside input | Scroll the transcript |
| End outside input | Follow the streaming transcript |
| ? outside input | Toggle help |
| Esc | Close a modal, otherwise cancel the active turn |
| q outside input / Ctrl-C anywhere | Quit, confirming with y/n when a run is active |

`?`, `q`, and Space are ordinary text in the composer. Shift+Enter needs a terminal
that supports the enhanced keyboard protocol; Ctrl-J works with the legacy protocol. Resize
keeps all panes visible; below 70 columns the tree sits below the conversation.
Ratatui diffs frames, and the loop redraws only for UI events, input or resize.
Set `NO_COLOR=1` to use terminal defaults with emphasis and no accent color.

## Event model

`kyora_tui::event::UiEvent` is the integration boundary. The frontend currently
consumes a bounded Tokio channel from `demo::play`. The demo uses the workspace's
`ScriptedProvider` and `Accumulator` for real protocol text deltas and tool inputs,
and supplies scripted lifecycle events for orchestration.

Events cover `TextDelta`, `ToolCallStarted`/`ToolCallFinished`,
`AgentSpawned`/`AgentFinished`, `ReplCellStarted`/`ReplCellFinished`,
`LlmCall`/`LlmCallFinished`, `Usage`, and `Budget`. Nodes carry an ID, parent, name
and model. All UI IDs are session-wide and unique, including cells and leaf calls;
root is 0. A future adapter should map runtime node/cell/generation IDs into this
space. Parentage is fixed after admission. Usage snapshots are cumulative **self**
usage per node, with cost in microdollars, not subtree usage or additive deltas.
Budget snapshots optionally target one node; `None` targets the shared session.

`App::apply` is IO-free. `view::draw` renders it with either a real terminal or
`TestBackend`. `App::handle_key` returns `Action::Submit`, `Cancel`, or `Quit` for
the driver. Replacing the demo driver with a core adapter should preserve this
split. Cancel drops the turn receiver, cancels its token and marks unfinished
nodes/tools cancelled; a replay uses a fresh channel so stale events cannot leak
into the next turn. The terminal restores raw mode, alternate screen, paste and
keyboard settings once on exit, errors and panics. On Unix, delivered SIGINT,
SIGTERM and SIGHUP exit through the same cleanup path. Keyboard Ctrl-C still
asks for confirmation when a turn is active.

Check the workspace:

```sh
CARGO_BUILD_JOBS=4 cargo fmt --check
CARGO_BUILD_JOBS=4 cargo clippy --workspace --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=4 cargo test --workspace --locked
```

The Unix PTY regression uses `python3` and its standard library to verify terminal
settings after SIGINT, SIGTERM, SIGHUP and keyboard Ctrl-C, both idle and during a
turn. Unit tests cover restore-once behavior, restoring the previous panic hook,
and composer punctuation.

Layout snapshots cover empty and mid-run sessions at 120x32 and a mid-run session
at 80x24. To deliberately regenerate the checked-in text fixtures:

```sh
CARGO_BUILD_JOBS=4 UPDATE_SNAPSHOTS=1 cargo test -p kyora-tui --test layouts
```
