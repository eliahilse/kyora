# The Python RLM layer

Status: specification, not implemented. It builds on what exists on main: the runtime loop, ledger and traces ([m1-runtime.md](m1-runtime.md)), child agents with ownership (`crates/core/src/recursion.rs`, `crates/core/src/runtime.rs`), asynchronous messages ([agent-messages.md](agent-messages.md)) and the MCP client ([mcp.md](mcp.md)).

The Python layer is the part of kyora that makes it a recursive language model runtime: the model drives a persistent Python kernel through a `python` tool, and code in that kernel calls models and spawns sub-agents through a pre-imported `kyora` module. Every such call is a request to the Rust runtime, which admits, charges, traces and cancels it like any other node.

Several defaults in this spec answer questions the owner has not confirmed yet. Each is marked **(pending owner confirmation)** where it applies and listed in section 9.

References: `R§n` is a section of [research.md](research.md); `Dn` (for example D8.2) is a section of the design draft (`docs/design.md` on the `docs/design` branch), cited the same way in m1-runtime.md. Appendix A lists what this spec adopts from prior work and where it departs from the draft; Appendix B gives the exact message schemas.

Contents:

1. [Goals and non-goals](#1-goals-and-non-goals)
2. [Architecture](#2-architecture)
3. [The Python API](#3-the-python-api)
4. [Wire protocol](#4-wire-protocol)
5. [Mapping onto the Rust runtime](#5-mapping-onto-the-rust-runtime)
6. [Executors: kernels on other machines](#6-executors-kernels-on-other-machines)
7. [Testing](#7-testing)
8. [Milestones](#8-milestones)
9. [Defaults pending owner confirmation](#9-defaults-pending-owner-confirmation)
10. [Appendix A: prior art and the design draft](#appendix-a-prior-art-and-the-design-draft)
11. [Appendix B: message schemas](#appendix-b-message-schemas)

## 1. Goals and non-goals

Goals:

- **Code that orchestrates models.** A model writes Python that calls leaf completions (`kyora.llm`) and full sub-agents (`kyora.spawn`), loops over their results, and decides what to do next in code instead of in its context window.
- **Map-reduce over data.** Large inputs live as kernel variables; code splits them, fans the pieces out to leaf calls or agents with a concurrency limit, and combines the results. Only what the code prints reaches the model, under a cap per model request (R§1, R§5.6).
- **Recursion with budgets.** A child spawned from code has its own kernel and can spawn its own children, under the same tree-wide depth, agent, call and token limits the runtime already enforces before dispatch (`Ledger::admit`, `Ledger::reserve` in `crates/core/src/ledger.rs`).
- **The lifecycle rules hold from code.** A cancelled agent is cancelled with its subtree; an open-ended agent runs and its parent is pinged when it is done; an agent with an output schema finishes when it submits a valid result. These are implemented in Rust (agent-messages.md, "How an agent ends"); the Python layer maps onto them and adds none of its own.
- **Exact semantics at the boundary.** Every Python call has one wire operation, one Rust entry point, typed errors and a trace record; every message a call takes is delivered exactly once.
- **Placement-independent kernels.** A kernel runs as a local process or on another machine through a pluggable executor, with the same protocol, so one root on a laptop can drive children that each have their own machine.
- **Deterministic tests.** The whole layer is testable with the scripted fake provider and a real Python interpreter, without network or keys; user programs are testable with a Python-side fake host.

Non-goals:

- A Jupyter replacement: no notebook format, no magics, no widgets, no rich output beyond text and described images.
- A sandbox. Locally the kernel has the user's authority, like the `shell` tool (section 2.4). Isolation comes from the executor (a machine) or from the planned OS sandbox (D13.2), not from this layer.
- Calls from processes other than the kernel. The client refuses them as a convenience; it is not a boundary. Whatever process holds the control descriptor speaks with the node's authority, and no more (section 4.5).
- Snapshotting or migrating kernel state. A crashed or lost kernel loses its variables, and the model is told (section 2.5).
- Provider access from Python. kyora gives the kernel no credentials and brokers every model call (R§5.2).
- Automatic merging of workspaces from other machines. Files cross machines only as explicit artifacts (section 6.4).
- Exactly-once side effects across machine loss. A cell interrupted by a lost machine is reported, never replayed.

## 2. Architecture

### 2.1 Overview

```
+------------------------------ kyora host process ------------------------------+
| Runtime: agent loops, Ledger, TraceSink, providers (credentials live only here)  |
|                                                                                  |
|  agent #0 loop --python tool--> Supervisor #0 --NodeCtx #0--> llm, spawn, send   |
|                                   |  ^                                           |
|  agent #4 loop --python tool--> Supervisor #4 ---------------------------+       |
+-----------------------------------|--|-----------------------------------|-------+
                    control frames  |  |  stdout, stderr       mux stream  |
                    (socketpair)    v  |  (pipes)                          v
                   python3 kernel #0 (LocalExecutor)      relay --> python3 kernel #4
                   own process group, cleared env         (machine executor, another host)
```

Three parts:

- **Kernel.** A `python3` process running kyora's embedded, stdlib-only boot script and `kyora` package. It executes cells in a persistent namespace and turns `kyora.*` calls into requests on its control channel.
- **Supervisor.** Rust, one per agent node that holds the `python` tool, in a new crate `crates/repl` (package `kyora-repl`, as in D3). It starts and destroys the node's kernel through an executor, runs cells, captures output, and serves kernel requests by calling the node's `NodeCtx`.
- **Executor.** Starts a kernel somewhere and hands the supervisor its control, stdout and stderr channels plus an instance handle it can renew and destroy (section 6). `LocalExecutor` is the default.

`kyora-core` stays free of Python, as today ("Core has no dependency on built-in tools or a REPL implementation", m1-runtime.md). The `python` tool reaches the runtime only through `ToolCx::node` (`crates/core/src/tool.rs`), and the CLI adds it through the node toolset factory, next to the MCP factory it uses today (`McpToolsets` in `crates/mcp/src/server.rs`, wired in `crates/cli/src/main.rs`).

### 2.2 One kernel per agent

Each agent node gets its own kernel, started lazily on its first `python` call and kept until the node ends. Leaf `llm` nodes have none.

Why per agent:

- **State is the point.** RLM keeps the large input and intermediate results in variables across turns (R§1). A kernel per cell would lose them; serializing the namespace between cells (`dill` in rlm's Docker, Modal and Prime environments, R§1) fails on unpicklable objects and copies large contexts on every cell.
- **Ownership matches.** A kernel's lifetime equals its node's, so node shutdown (cancel and join descendants, then `node_end`, `Runtime::run_node`) is also where the kernel stops.
- **Isolation between agents.** A kernel per session (one interpreter for the whole tree) would let siblings read each other's variables, share one GIL across parallel children, and make placing a child on another machine impossible. nano-rlm also gives every agent its own kernel (R§2).

One cell runs at a time per kernel. The loop already guarantees it: tool calls of one assistant message run sequentially (`Runtime::agent` in `crates/core/src/runtime.rs`), and the `python` tool declares `Effect::Mutating`, which stays a barrier under the parallel read-only dispatch planned in D5.3. The same barrier means no model-facing tool runs while a cell runs.

### 2.3 The supervisor

Per node, the supervisor owns:

- the executor instance and the current kernel **generation** (1, 2, ... per node; a restart starts a new one);
- the control channel pumps: a reader task that decodes frames, checks them and dispatches each kernel request to its own task without awaiting it, a writer task that writes queued frames and reports each frame's write result, and a watchdog that can destroy the kernel without going through the writer (D9.3);
- the connection **gate**, a mutex that orders reply admission against cell exit (section 4.7);
- the **current cell**: its id, scope nonce, cancellation token, deadline, output capture buffers, staged final answer, outstanding requests, and the cell-owned children it spawned;
- the per-model-request Python output budget (section 2.6), keyed by the node's admitted turn;
- the node's table of child handles, the names of the kernel's variables (for the restart notice), and event subscriptions (section 4.6);
- the kernel's response memory permits (section 4.8).

The supervisor gets the node's `NodeCtx` from each cell's `ToolCx`. Every authority check stays in `NodeCtx` (section 4.5).

### 2.4 Isolation and trust

| | Local executor (laptop) | Machine executor (for example kyora vms) |
|---|---|---|
| Process | Separate `python3` in its own process group, like the `shell` tool (`process_group(0)`, `crates/tools/src/shell.rs`). | Separate machine; the kernel runs under a relay process there. |
| Environment | Cleared, then the shell allowlist (`ENV_ALLOWLIST` in `crates/tools/src/defaults.rs`) plus `KYORA_CONTROL_FD`. kyora passes no provider keys or other credentials. | Only what the executor's image provides plus the same allowlist. kyora passes no credentials. |
| Filesystem | The user's full authority, like `shell`. The file tools' workspace confinement (m1-runtime.md, "File access") does not apply to kernel code. | The machine's own disk, holding a workspace snapshot (section 6.4). |
| Network | Unrestricted locally until the OS sandbox lands (D13.2, which denies the REPL network in `workspace-write`). | Whatever the executor allows. |
| Resources | The boot script lowers rlimits before user code runs: `RLIMIT_CORE` 0, `RLIMIT_NOFILE` 1024, `RLIMIT_CPU` as a backstop, `RLIMIT_AS` on Linux (default 4 GiB), soft and hard together (D13.1). | Machine size, plus the same rlimits. |
| Model access | None of its own. Every model call is a request to the supervisor. | Same. |

Clearing the environment keeps kyora's own provider keys out of the kernel. It does not stop code on the local executor from finding credentials elsewhere: files the user can read, keychains, other processes where the OS allows it. The local executor is not a security boundary and this spec does not claim one; run untrusted inputs on a machine executor or, once it exists, under the OS sandbox.

What the host enforces regardless of executor: the connection acts only as its node, through `NodeCtx` and the checks of section 4.5; every frame, queue and memory use on the host is bounded (section 4.8); malformed traffic ends only that kernel. Restrictions the Python client applies to itself (cell scope from threads, fork and pid checks, its own request limits) are cooperative conveniences, not boundaries (section 4.5).

### 2.5 Kernel lifecycle

```
none --first python call--> starting --hello/welcome--> ready <--> running(cell)
  ^                            |                          |             |
  |                            +------ start failure -----+-- ends -----+--> stopping
  +--- next python call, after the old instance is fenced -------------+     (destroy)
ready/running --node ends--> stopping --teardown report--> stopped
```

**Start.** On the node's first `python` call, or, on machine executors, right after admission through the tool start hook of change C1 (section 6.2):

1. The executor starts `python3 -I -X utf8 <runtime>/kyora_boot.py` in the node's cwd. `<runtime>` is a private 0700 directory per kyora process into which the embedded Python files are written (D9.1). `-I` ignores `PYTHONPATH` and user site packages, so the boot script inserts its own directory first in `sys.path`, imports `kyora`, then appends the workspace so user modules stay importable.
2. The host writes a fresh 256-bit token as one line to the kernel's stdin and closes it. The boot script reads that line, then points fd 0 at `/dev/null`, so the token is in no environment variable, argument or stdin that user code or subprocesses see.
3. The boot script moves the control socket from fd 3 to a high descriptor, marks it close-on-exec, registers an `os.register_at_fork` hook that closes it in children forked through `os.fork`, applies the rlimits, and sends `hello` (section 4.2). These steps keep the descriptor away from ordinary subprocesses; code that passes it on deliberately (`subprocess.Popen(pass_fds=...)`) or forks from C bypasses them, which section 4.5 accounts for.
4. The supervisor answers `welcome`, then sets the node's preloaded variables with `vars.set` (section 3.9).

Startup is bounded: 10 s to `hello` locally, the executor's start bound remotely (section 6.6). A start runs as a task owned by the `python` tool: the first `python` call awaits a start already in flight, and the tool's shutdown hook cancels and joins it before destroying anything. On timeout, on a start error, or when the cell or node is cancelled during startup, the supervisor destroys whatever the executor started (section 6.5) before it reports; the executor must make `start` itself cancel-safe, so a machine created before the cancellation is destroyed too.

**Reuse.** The namespace persists across cells. Between cells nothing of kyora's runs in the kernel, and the host rejects every request then as stale (section 4.5). User threads that outlive a cell keep running but cannot act through kyora.

**End of a generation.** A generation ends when the process exits, the control channel reaches end of file, the kernel violates the protocol (section 4.8), the host kills it after an interrupt grace, or the executor reports the machine lost. In every case the supervisor first makes one destroy attempt, bounded by the destroy call timeout of section 6.5 (locally: SIGKILL to the process group, then reaping the process, so no zombie and no straggler in the group remains; this always confirms), then drains and closes the stdout and stderr pumps (bounded by 1 s), then runs the cell exit of section 5.4 if a cell was running, and writes `kernel_end` with the reason and the destroy result, `confirmed` or `pending`. A pending destroy is retried in the background (section 6.5); nothing on the cell's or node's path waits for those retries.

The running cell's status follows the first recorded cause, so a kill that a timeout caused never reads as a crash:

1. a host-initiated interrupt recorded first: `timeout` (the cell deadline) or `interrupted` (the node was cancelled), even if the kernel was then killed;
2. otherwise an executor loss report or a liveness failure: `lost`;
3. otherwise a process exit, end of file or protocol violation: `crashed`, with the last 4 KiB of stderr;
4. otherwise the kernel's own result: `ok`, `error` or `interrupted`.

**Restart.** The next `python` call starts generation + 1, reloads the preloaded variables, and prefixes its result with one line, appended content like every tool result, so the history stays append-only (D5.2):

```
[kyora] new kernel (generation 2): the previous one crashed. Lost variables: chunks, notes, h. Reloaded: context.
```

A new generation starts only once the previous instance is fenced, that is known to be unable to run: always true locally (killed and reaped), and on machine executors decided by section 6.5, which bounds how long a `python` call waits for it. Persistent children survive a restart (they belong to the node); code in the new generation gets their handles back with `kyora.agents()` or `kyora.agent(id)`. After 5 restarts in one node (configurable) the tool refuses with an error instead of starting another kernel.

**Shutdown.** The kernel belongs to its agent node. When the node's loop ends, whatever the status, the supervisor closes its gate (new requests get `cancelled`), cancels and joins a start in flight, and tears the kernel down: a `shutdown` request, 2 s of grace, then destroy attempts under the call timeout and retry policy of section 6.5. This runs concurrently with the runtime cancelling and joining the node's descendants, through the tool shutdown hook of change C1 (section 5.6). The hook returns within 12 s whatever destroy reports, with a **teardown report**: complete, or incomplete with the time by which the instance is fenced anyway. Core records it on `node_end` and in the node's outcome (section 5.4), so whoever waits for the node can tell whether its code is known to have stopped. A second Ctrl-C kills every local kernel group immediately, like shell and MCP groups today: `watch_interrupts` in `crates/cli/src/main.rs` calls `kyora_tools::cancel_processes` and `kyora_mcp::kill_servers`, and gains `kyora_repl::kill_kernels`.

Resume (D11.3) never restores a kernel: the first cell after a resume starts a new generation with the notice above.

### 2.6 Cells and the `python` tool

A cell is one call of the `python` tool. Its spec is static and identical in every node (no node ids in it), so sibling agents keep sharing a cached prompt prefix, as `prompts::SUBAGENT` intends (`crates/core/src/prompts.rs`):

```json
{"name": "python",
 "description": "Run Python in your persistent kernel. Variables persist across calls. The kyora module is imported. <a compact reference of the kyora API follows>",
 "input_schema": {"type": "object",
   "properties": {"code": {"type": "string"},
                  "timeout": {"type": "number", "description": "Seconds before the cell is interrupted. Default 1800, at most 7200."}},
   "required": ["code"], "additionalProperties": false}}
```

The tool sets `ToolSpec::large_input`, so providers stream the code eagerly (`crates/protocol/src/lib.rs`), declares `Effect::Mutating` (code can change the workspace, so the runtime awaits a cancelled cell until the tool returns, `Runtime::agent`; the tool bounds that wait by the 2 s grace and one destroy attempt under its call timeout, section 5.4), and keeps the default `Tool::truncated() == true`.

**Execution.** The kernel parses the code with `ast` and compiles it with `PyCF_ALLOW_TOP_LEVEL_AWAIT`, so cells may `await`. If the last statement is an expression, it is evaluated separately and its value is the cell's result, also bound to `_`. A coroutine code object runs on the kernel's event loop, which persists across cells.

**Deadline.** `min(now + timeout, NodeCtx::deadline)`, where `timeout` defaults to `Limits::cell_timeout` (1,800 s) and is capped at `Limits::max_cell_timeout` (7,200 s) (`crates/core/src/defaults.rs`). On the deadline, or when the node is cancelled, the supervisor runs the cell exit (section 5.4), which records the cause, cancels the cell token and then sends `interrupt`; the kernel raises `kyora.Cancelled` in the main thread. If the kernel has not returned the cell 2 s later, the watchdog makes one bounded destroy attempt and the generation ends; the cell keeps the status of its first cause, and the tool result does not wait for destroy retries.

**Output capture.** The host captures the kernel's stdout and stderr pipes itself, so output from prints, C extensions and subprocesses is bounded outside user code. Per cell and stream the host keeps the first 64 KiB and the last 64 KiB and counts what it drops. At the end of a cell the kernel flushes `sys.stdout` and `sys.stderr` and writes the cell's random marker (sent in `exec`) to fds 1 and 2; the host strips it and closes the cell's capture when it has seen it on both streams, or 1 s after the cell result if user code closed or redirected a descriptor. Output arriving after the marker belongs to the next cell as `[background output]`, bounded the same way. Display data and logs travel on the control channel (section 4.4).

**Result text.** Sections are omitted when empty:

```
[stdout]
412 chunks, 37 dates
[stderr]
UserWarning: ...
[display]
| file    | dates |
| a.txt   | 12    |
[result]
{'found': 37, 'unclear': 4}
[error]
Traceback (most recent call last):
  File "<cell 3>", line 7, in <module>
kyora.AgentFailed: agent 9 (chunk-17) ended with status max_turns
[vars] new: notes: list (412 items), hard: list (4 items); rebound: context
[kyora] cell 3 error after 41.2 s: 412 llm calls, 4 agents (2 completed, 1 failed, 1 cancelled at cell end: #11), 1 tool call; consumed by code: 3 results, 5 messages; 9.8M tokens; 8.3M of 20M left
```

- `[result]` is a bounded repr (at most 8 KiB, computed in the kernel with `reprlib`-style limits).
- `[error]` is the traceback trimmed to frames from `<cell n>` files, with the exception chain.
- `[vars]` lists names bound for the first time or rebound to a new object, with type and size; in-place mutation is not detected.
- `[kyora]` is always present: status, wall time, what the cell started, which cell-owned children were cancelled when it ended, how many results and messages code consumed (so the model knows they will not arrive as messages), and the node's budget.

**Output budgets.** Two caps bound what cells add to the model's context:

- per cell, `Limits::tool_output_chars` (default 20,000), as for any tool;
- per model request, `python_turn_chars` (default 40,000; a new repl setting): every `python` result of one assistant message shares it, since one message can hold several `python` calls whose results travel together in one user message (`Runtime::agent` collects them into one tool-results message). **(pending owner confirmation)**

A cell's text is fitted into `min(tool_output_chars, remaining python_turn_chars)`. Two limits are distinct here: `python_turn_chars` is a hard cap on everything except footers, and the `[kyora]` footer (itself cut to at most 500 characters) is always kept. A cell that starts after the budget is spent therefore returns only its footer, and one model request carries at most `python_turn_chars` characters of cell output plus 500 characters for each such further cell. So that a footer always fits, the repl configuration is refused unless `tool_output_chars` is at least 2,000 for nodes that hold `python` (core accepts any positive value, `Limits::validate`) and `python_turn_chars` is at least `tool_output_chars`. The formatter fills the cap in priority order: the `[kyora]` line, `[error]` up to a quarter of the cap, `[result]` up to an eighth, and the remainder shared by stdout, stderr and display, each cut with `kyora_core::tool::truncate` (head and tail around `[... N characters omitted ...]`). The runtime's own cut in `Runtime::agent` is then a no-op backstop. The supervisor keys the shared budget by the node's admitted turn (`ToolCx::turn`, change C8).

**Status.** `ok`, `error` (an exception escaped), `timeout`, `interrupted` (the node was cancelled), `crashed`, `lost` (the executor lost the machine, section 6.6), decided as in section 2.5. Every status except `ok` sets `is_error`.

**Final answer.** If the cell staged a final answer (section 3.11) and ended `ok`, the tool returns `ToolOutput::final_answer` and the text `[kyora] final answer recorded`. The runtime then applies its existing rules: the first committed answer of a turn wins, a node with an output schema accepts only a matching JSON object, and an answer committed after the node was cancelled or timed out is refused (`Runtime::accept`, `Runtime::agent`). A cell that fails discards its staged answer (D8.2).

## 3. The Python API

### 3.1 Conventions

- The module is `kyora`, pre-imported in every cell; its async mirror is `kyora.aio`. **(pending owner confirmation)** The name avoids `rlm`, which the paper authors' `rlms` package uses as its import name (R§1), so that package stays usable inside the kernel.
- Stdlib only, Python 3.11 or newer **(pending owner confirmation)**: 3.11 brings `asyncio.TaskGroup` and `asyncio.timeout`, which the async API uses, and the integer digit limit that bounds number parsing. The README's "Python 3.9+ for the REPL tests" changes when R0 lands. `pydantic` is used when installed, never required.
- **Values crossing the boundary are JSON, with fidelity.** The client encodes with `json.dumps(..., allow_nan=False, ensure_ascii=False)`: tuples become lists, other objects raise `TypeError` in the caller, integers longer than the interpreter's digit limit (4,300 digits by default) raise `ValueError`. The host checks syntax with `serde_json::value::RawValue` and passes the text through unchanged; it never converts numbers, since the workspace's `serde_json` has `raw_value` but not `arbitrary_precision` (`Cargo.toml`). Where the host must store a value inside a `serde_json::Value` (`ChildSpec::init`), it stores the JSON text as a string. The raw path skips values iteratively and does not apply the parser's recursion limit, so the host runs its own depth check: a single pass over the text that tracks string state and bracket depth, converts nothing, and rejects nesting deeper than 128 levels with `InvalidRequest`. The client applies the same limit before sending. Where the host validates a value against a schema it parses a copy for that check only; integers outside the 64-bit range then fail an `integer` type.
- Every call is bounded: by its own timeout argument where it has one, and always by the cell deadline.
- Sync calls block the calling thread. Inside a coroutine use `kyora.aio`; a sync call there works but stalls the event loop.

### 3.2 Identity

```python
kyora.node: int                 # this agent's node id
kyora.parent: int | None
kyora.depth: int                # 0 for the root
kyora.max_depth: int
kyora.generation: int           # kernel generation of this node
kyora.cell: int                 # current cell, unique per node across generations
kyora.workspace: pathlib.Path   # the node's cwd (on a snapshot executor, this machine's copy)
kyora.scratch: pathlib.Path     # private per-kernel directory, deleted with the kernel
kyora.executor: str             # executor name this kernel runs on
```

### 3.3 Leaf calls

```python
def llm(prompt: str, *, system: str | None = None, model: str | None = None,
        max_tokens: int | None = None) -> Completion

def llm_batch(prompts: Iterable[str], *, system: str | None = None, model: str | None = None,
              max_tokens: int | None = None, concurrency: int = 8,
              return_exceptions: bool = False) -> list[Completion | KyoraError]

class Completion(str):          # the text; also:
    node: int                   # leaf node id in the trace
    model: str
    stop_reason: str
    usage: Usage                # everything charged to this leaf, retries included (the ledger's figure)
    response_usage: Usage       # what the final response reported

class Usage:
    input_tokens: int; output_tokens: int
    cache_creation_input_tokens: int; cache_read_input_tokens: int
    total: int                  # all four, the ledger's budget unit (Usage::total)
```

`llm` is one completion without tools: a leaf node at the caller's depth, allowed at every depth including `max_depth`, consuming the `llm_calls` counter but no agent slot (`NodeCtx::llm`). The default model is the runtime's leaf model (`RuntimeConfig::llm_model`, default `anthropic/claude-sonnet-5-5`). `max_tokens` is capped by `Limits::llm_max_output_tokens`. `usage` comes from the ledger, which charges failed attempts too (`Ledger::settle` charges the reservation when usage is unknown); a failed call's charge is still counted in the cell's footer (change C2).

`llm_batch` sends one `llm` request per prompt with at most `concurrency` in flight, and returns results in input order. With `return_exceptions=False` the first failure is raised after the requests already in flight finish; with `True`, failed items hold their exception.

### 3.4 Agents

```python
def spawn(task: str, *, name: str | None = None, persistent: bool = False,
          output: dict | type | None = None, context: Any = <unset>,
          vars: Mapping[str, Any] | None = None, tools: Sequence[str] | None = None,
          model: str | None = None, budget: int | None = None,
          timeout: float | None = None, max_turns: int | None = None,
          executor: str | None = None) -> Agent

def run(task: str, **spawn_kwargs) -> AgentResult        # spawn(...).result(); never persistent
def agent(ref: int | str) -> Agent                       # a child by id or name, or a deeper descendant by id
def agents() -> list[AgentInfo]                          # this agent's children

class Agent:
    id: int; name: str; persistent: bool; cell: int | None   # cell that spawned it
    relation: str                                         # "child" or "descendant"
    def result(self, *, yield_after: float | None = None) -> AgentResult
    def status(self) -> AgentStatus
    def done(self) -> bool
    def cancel(self) -> AgentResult
    def send(self, body: str) -> int
    def events(self, *, kinds: Sequence[str] | None = None) -> Iterator[AgentEvent]
    def __await__(self)                                   # await agent == await kyora.aio.result(agent)

class AgentResult:
    node: int; name: str
    status: str            # completed, max_turns, budget_exhausted, timeout, context_exhausted,
                           # cancelled, refused, failed, interrupted (Status in runtime.rs)
    ok: bool               # status == "completed"
    text: str              # the answer as text (JSON text for a structured answer)
    value: Any             # structured answer: parsed JSON, or the pydantic instance; else None
    turns: int
    usage_self: Usage; usage_subtree: Usage
    messages: list[Message]  # the child's unread messages consumed together with its result
    already_finished: bool   # set by cancel(): the child had ended before the cancellation
    teardown_complete: bool  # its kernel and tools are known stopped (False: see fenced_at)
    fenced_at: datetime.datetime | None   # when an incomplete teardown is fenced anyway

class AgentStatus:         # ChildStatus in recursion.rs
    status: str | None     # None while running or shutting down
    turns: int; usage_self: Usage; usage_subtree: Usage

class AgentInfo:
    id: int; name: str; persistent: bool; cell: int | None; status: str | None
```

**Spawn** admits the child at once or fails at once (`NodeCtx::spawn_agent` never queues). It returns before the child's first model request.

**Ownership.** By default a child is **cell-owned** (`Owner::Cell`) **(pending owner confirmation)**: it belongs to the code that started it, posts no notice to the parent's mailbox, and is cancelled with its subtree when the cell ends (section 5.4). With `persistent=True` it is **node-owned** (`Owner::Node`): it outlives the cell, and when it ends its result, error or cancellation arrives in the agent's mailbox, so the model is pinged at a turn boundary unless code consumed it first; if the model ends its turn while it runs, the agent waits for it (agent-messages.md, "Idle agents"). Persistent children are still cancelled when their agent ends. Cell-owned is the default because the usual fan-out consumes its results in the same cell, and a forgotten handle then cannot leak work past the cell.

**Arguments.** `task` is the child's first user message. `tools` selects a subset of this agent's tools; omitted, the child gets `defaults::SUBAGENT_TOOLS` intersected with this agent's (`crates/core/src/defaults.rs`), minus the tools its executor cannot host (section 6.2). `model` defaults to this agent's model. `budget` is a subtree token budget, bounded by every ancestor. `timeout` (seconds) is the child's own deadline, capped by this agent's. `max_turns` defaults to `Limits::subagent_max_turns` (50). `context` preloads the variable `context` in the child's kernel; `vars` preloads several (section 3.9). `executor` names where the child's kernel runs, defaulting to this kernel's executor (section 6.2). `output` makes it a structured task (section 3.5).

**`result(yield_after=None)`** waits for the child and returns its `AgentResult` if it completed; non-completed endings raise `AgentFailed` (`AgentCancelled` for `cancelled`), with the result on `.result`. It **consumes** the child's result: its terminal notice and every unread message it sent before it, which arrive on `.messages`, so the model is not pinged again **(pending owner confirmation)**. `result()` returns only after the terminal notice itself has been committed to the kernel (section 4.7): when the child's queued messages do not fit one reply, the client keeps requesting further replies, each taking the next messages in order, until the reply that carries the notice; only that reply carries the outcome. If a reply cannot be delivered, nothing in it is consumed and the call fails or retries; whatever remains queued, including the notice, still reaches the model. `yield_after` bounds how long the child may keep running: if it has not ended by then, `StillRunning` is raised and the child keeps going. Once the child has ended, `result()` waits for its result to become deliverable however long that takes (it is one trace record for a cell-owned child, section 5.3), bounded only by the cell deadline. `yield_after=0` polls. Calling `result()` again later returns the same outcome without messages. Works on direct children only.

**Large answers.** An answer of more than 1 MiB never travels cut: the host puts it in the session artifact store and the reply carries a reference (Appendix B, `Answer`), which the client fetches whole before `result()` or `outcome` returns.

**`status()`** never blocks and consumes nothing. **`done()`** is `status().status is not None`. Direct children only.

**`cancel()`** cancels the child together with its subtree and returns once the child's node has shut down (its loop ended, its descendants joined, its `node_end` written), with its outcome; cancelling a child that already ended only reports it (`already_finished`). Shut down does not always mean stopped: if a kernel on a machine could not be confirmed destroyed, `teardown_complete` is false and `fenced_at` says when it is fenced by its lease (section 6.5). For a direct child it consumes the result like `result()`, draining through the notice. It does not raise for the `cancelled` status, since the caller asked for it. It also works on deeper descendants obtained with `kyora.agent(id)`.

**Handles across cells.** A handle is a node id plus local state; it stays usable in later cells. A cell-owned child has ended by then (its cell cancelled and joined it), so `result()` returns its final outcome. `kyora.agent(ref)` builds a handle only for a node the host has confirmed is a child (by id or name) or a deeper descendant (by id), including children the model started with the `spawn_agent` tool; anything else raises `InvalidRequest` (section 4.5).

### 3.5 Structured output

```python
def schema(fields: Mapping[str, Any]) -> dict          # JSON Schema helper

spawn(task, output={"type": "object", ...})            # JSON Schema of type object
spawn(task, output=Dates)                              # a pydantic v2 BaseModel subclass
spawn(task, output=kyora.schema({"dates": [str], "count": int, "note": kyora.optional(str)}))
```

`kyora.schema` maps `str`, `int`, `float`, `bool`, `None`, `[T]`, nested mappings, `kyora.optional(T)` and `kyora.enum(*values)` to the subset the runtime enforces: single `type` names, `enum`, `required`, `properties`, `additionalProperties: false` and `items` (`tool::validate`, `tool::check_schema` in `crates/core/src/tool.rs`). Optional fields are left out of `required`. For a pydantic model the schema is `model_json_schema()`; keywords outside the subset (`$ref`, `anyOf`, `format`) are passed to the model but not enforced by the runtime.

The child gets a `submit_result` tool with that schema and finishes when it submits a valid object; its running children are cancelled; a child that ends without submitting is reminded once and then fails (agent-messages.md, "Structured results"). A child whose own code commits a final answer does so through the same `Runtime::accept` contract (section 3.11).

On the parent side, `result().value` is the parsed object, or the pydantic instance after `model_validate`. A value that passed the runtime's subset but fails pydantic validation raises `SchemaError` with `.value` (the raw object) and `.errors`; the child has completed and its result is consumed. A schema the runtime refuses raises `SchemaError` from `spawn`, before anything is admitted.

### 3.6 Fan-out helpers

```python
def map(task: str, items: Iterable[Any], *, concurrency: int = 4, var: str = "context",
        name: str | None = None, return_exceptions: bool = False,
        **spawn_kwargs) -> list[AgentResult | KyoraError]

def gather(agents: Iterable[Agent], *, return_exceptions: bool = False) -> list[AgentResult]
def as_completed(agents: Iterable[Agent], *, yield_after: float | None = None) -> Iterator[Agent]
def parallel(fn: Callable[[T], R], items: Iterable[T], *, concurrency: int = 8,
             return_exceptions: bool = False) -> list[R | Exception]
```

- **`map`** spawns one cell-owned child per item, the item preloaded as variable `var`, all with the same `task` text (so siblings share a cached prefix), named `f"{name}[{i}]"` when `name` is given. At most `concurrency` of its children are alive at once. When admission refuses with `LimitExceeded("agents_live")` while at least one of its own children runs, `map` waits for one to finish and retries; with none running it raises. Results come back in input order. On the first failure with `return_exceptions=False`, it cancels its children still running and raises that failure. `BudgetExceeded` stops further spawns in either mode; with `return_exceptions=True` the items not spawned hold it. `persistent=True` is refused.
- **`gather`** waits for all and returns results in order. It does not cancel the others when one fails; an uncaught exception ends the cell, which does.
- **`as_completed`** yields handles in completion order; their `result()` then returns at once. With `yield_after`, raises `StillRunning` (listing the rest) when the time passes.
- **`parallel`** runs arbitrary functions in a thread pool whose threads inherit the cell (section 3.13), so `fn` may call any `kyora` function.

### 3.7 Messages

```python
def send(to: Agent | int | str, body: str) -> int        # "parent", an id, a name, or a handle
def receive(*, yield_after: float = 0, max_bytes: int | None = None) -> list[Message]
def wait(agents: Iterable[Agent | int | str] | None = None, *,
         timeout: float | None = None, max_bytes: int | None = None) -> Waited
def pending() -> int

class Message:              # Envelope in crates/core/src/messages.rs
    id: int; sender: int; to: int
    kind: str               # message, result, error, cancelled
    body: str; sent_at: datetime.datetime
    spawn: int | None; status: str | None

class Waited:
    finished: list[AgentResult]   # in node id order; each child's notice consumed, answers read after
    running: list[int]
    messages: list[Message]       # what the finished children had queued, in arrival order
```

These map onto `NodeCtx::send`, `receive`, `wait` and `pending_messages`, with the same addressing (parent, children, siblings), the same mailbox capacity and body cap, and the same errors (`MailboxFull`, `AgentFinished`, `InvalidRequest`).

`wait()` with no agents waits for every child whose result nobody has consumed yet: the node-owned children the runtime counts as outstanding (`Mailbox::outstanding`, which lists children still awaited and notices still queued) together with this cell's cell-owned children, which the runtime does not count there because they post no notice until a wait stages one (`NodeCtx::wait_via`). The supervisor passes that set explicitly. `timeout` covers running children as `yield_after` does for `result()`. Like `result()`, `wait()` drains: a finished child whose queued messages did not all fit one reply (`Waited::deferred` in `crates/core/src/messages.rs`) is followed up with further replies until its notice is committed, so `finished` lists only children whose results were consumed.

**Delivery budget.** The per-turn `Limits::delivery_chars` bounds what the runtime puts into the model's context as messages. Messages taken by code go into Python variables instead; what code prints is bounded by the output budgets of section 2.6. So code deliveries are not charged against the turn's delivery budget **(pending owner confirmation)**: `receive`, `wait`, `result()` and `cancel()` take whole messages in arrival order up to `max_bytes` of encoded messages per reply (default 4 MiB, at least 1 MiB, at most what fits one reply, section 4.8). They are recorded as delivered `via: code`. Messages code does not take stay queued and reach the model at the next turn boundary under the turn budget, exactly as today. `pending()` reports how many wait.

### 3.8 Budgets and limits

```python
def budget() -> Budget
class Budget:               # BudgetSnapshot in crates/core/src/ledger.rs
    limit: int; used: int; reserved: int; remaining: int; closed: bool

def limits() -> Limits
class Limits:
    depth: int; max_depth: int
    agents_live: int; max_agents_live: int        # session-wide, what admission checks
    agents_total: int; max_agents_total: int      # session-wide
    llm_calls: int; max_llm_calls: int            # session-wide
    subtree_agents_live: int; subtree_agents_total: int; subtree_llm_calls: int
    deadline: float          # node deadline, seconds since the epoch
    cell_deadline: float
```

`budget()` is this node's scope: its own limit, and what it and its descendants have used and reserved. The admission counters are session-wide, because that is what `Ledger::admit` checks; the subtree counters show this node's share (change C6). The maxima come from `welcome` (section 4.2). Per-call limits are arguments: `budget`, `timeout` and `max_turns` on `spawn`, `max_tokens` on `llm`. Python cannot raise any limit; a child's budget and deadline are bounded by its ancestors (`Ledger::admit`, `NodeCtx::spawn_agent`).

### 3.9 Files, workspace, variables and artifacts

Kernel code reads and writes files with plain Python. On the local executor `kyora.workspace` is the shared workspace, and siblings writing the same file race, as D5.3 notes for the file tools. On a snapshot executor it is that machine's copy, and nothing written there flows back by itself **(pending owner confirmation)**: files cross machines only as artifacts.

Preloading passes data into a child's kernel without it entering any prompt:

```python
kyora.spawn("Find every date.", context=chunk)                     # inline JSON value
kyora.spawn("Summarize.", vars={"doc": kyora.file("data/a.txt"),    # a file in this agent's workspace view
                                "table": report_artifact,           # an artifact
                                "meta": {"source": "a"}})
def file(path: str, *, format: str = "text") -> FileRef             # "text", "bytes" or "json"
```

- **Inline values** must fit the request frame (1 MiB encoded, section 4.8); larger ones raise `ValueTooLarge` locally, with a hint to use a file or an artifact.
- **File references** carry provenance stamped by the host, not by the kernel: the client sends only the path and format, and the supervisor, which knows this kernel's executor, instance and generation, resolves the path, hashes the file and records the stamped reference (Appendix B, `StampedFile`). A reference is valid only where the host can check it. On a shared-workspace executor (local) the supervisor opens the path relative to this node's workspace with the file tools' confinement rules and hashes it. On a snapshot executor a reference always selects the path's **original snapshot content**: the file as it was in the snapshot this kernel started from, by the hash in the snapshot manifest, whatever the machine's copy holds now. The host cannot see the machine's disk, so it does not try to detect changes; a path that is not in the snapshot raises `InvalidRequest` naming `kyora.artifact`, which is how code passes on files created or changed on the machine. The child's kernel receives the file by hash (from the shared workspace, or in its own snapshot) and verifies it; a mismatch fails the load, which the child's first cell reports.
- **Artifacts** move bytes explicitly:

  ```python
  def artifact(path: str, *, name: str | None = None) -> Artifact   # upload a file from this kernel's machine
  def fetch(artifact: Artifact, path: str | None = None) -> pathlib.Path   # download; default kyora.scratch / name
  ```

  `artifact` streams the file to the host's session artifact store (content-addressed under the session directory, which is 0700), at most 256 MiB per artifact and 4 GiB per session, and records `artifact_put`. An `Artifact` is a small JSON value (`{"$artifact": "sha256:...", "name": ..., "bytes": ...}`), so it travels in results, messages and `vars`. Artifacts are scoped to the session: any agent of the session that holds a reference can fetch it, the same trust domain as a shared workspace.
- **Manifest.** The child's first user message gets a manifest through `ChildSpec::preamble`: per variable its name, type, size and a 200-character preview, never the value (D8.2). The manifest is capped at 4,000 characters: at most 12 variables are listed, then `and N more: <names>`.
- The values travel in `ChildSpec::init` (`{"kyora": {"vars": {name: {"json": "<JSON text>"} | {"file": ...} | {"artifact": ...}}, "executor": ...}}`), which the runtime hands to the child's toolset factory untouched (`NodeInfo::init`; tested by `factory_receives_opaque_init_selection_model_and_preamble` in `crates/core/tests/recursion.rs`). Inline values are stored as JSON text, as section 3.1 requires.
- A child without the `python` tool cannot hold variables: `context` or `vars` then raise `InvalidRequest` before admission.
- The root's variables come from `kyora run --context-file PATH` and `--var NAME=@PATH|NAME=VALUE` (D16), which the CLI hands to the python toolset for node 0.

### 3.10 Tools

```python
def tools() -> list[str]                                   # tools this code may call
def call_tool(name: str, /, **input: Any) -> ToolResult    # raises ToolError on an error result

class ToolResult(str):      # the result text; also:
    final_staged: bool      # the tool committed a final answer, which is now staged on this cell
```

`call_tool` runs one of this agent's frozen tools, including MCP tools (`mcp__<server>__<tool>`, mcp.md), with the same input validation and cancellation as a model call, through `NodeCtx::call_tool` (change C4). The result text is not cut to `tool_output_chars`, since it goes to a variable; the tool renders it under an output limit of at most 15 MiB, so it fits one reply, and a tool that cannot render under a limit is not callable from code (section 4.8). Excluded: `python` (a cell cannot run a cell), `submit_result` (use `kyora.final`) and the agent tools (`spawn_agent`, `send_message`, `receive`, `wait`, `cancel_agent`), which the native API covers, and tools that cannot render under an output limit (section 4.8). `tools()` and `tool.list` return only the callable subset. At most 8 tool calls per kernel run at once.

A tool that returns `ToolOutput::final_answer` (`crates/core/src/tool.rs`) does not finish the agent from code: its answer is staged on the cell exactly as if passed to `kyora.final`, with the same checks (section 3.11), and `final_staged` is set.

**Cancellation is cooperative, and differs by tool.** A started mutating call runs until the tool returns, even if the cell is cancelled (`Effect::Mutating`); what "returns" means depends on the tool:

- `shell` returns once its process group is killed (`crates/tools/src/shell.rs`);
- `write_file` and `edit_file` that have started run to completion with no deadline, and the runtime waits for their real outcome (m1-runtime.md, "File access"; `run_blocking` in `crates/tools/src/files.rs`);
- an MCP call returns after kyora sends `notifications/cancelled`, which does not confirm that the server stopped or undid anything; the server may have applied part of a mutating call (mcp.md, "Timeouts and cancellation"; `crates/mcp/src/server.rs`).

### 3.11 Final answers, logs and display

```python
def final(value: Any) -> None       # stage this agent's answer; committed if the cell ends ok
def log(message: str, *, level: str = "info") -> None    # trace and UI only, never the model
def display(*objects: Any) -> None  # also injected as the builtin display()
```

`final` replaces an answer staged earlier in the same cell, whether by `final` or by a tool through `call_tool`. The host checks it when it is staged, so code learns at once instead of after the cell:

- Without an output schema: a string becomes `Answer::Text`, any other JSON value `Answer::Value`; the value must fit the request frame.
- With an output schema (the `submit_result` spec in the node's toolset): the value must be a JSON object that passes the runtime's subset (else `SchemaError`) and whose JSON text is at most `Limits::message_chars` characters (else `ValueTooLarge` with `limit = "message_chars"`), the same checks `Runtime::accept` applies at commit (`crates/core/src/runtime.rs`).

A staged answer can still be refused at commit, for example because the node was cancelled while the cell ran; the tool result then carries the runtime's `final answer not accepted:` line. For the root, the committed value is what `kyora run` prints (strings as-is, other values as JSON).

`display` renders `text/markdown` (from `_repr_markdown_`) or `text/plain` (a bounded repr) into the cell's `[display]` section. Image bundles (`_repr_png_`, `_repr_jpeg_`) are described by type and size until tool results can carry images (`ToolResultPart` has only `Text` today, `crates/protocol/src/lib.rs`).

### 3.12 Errors

```
KyoraError(Exception)
    LimitExceeded          .limit
    BudgetExceeded
    InvalidRequest
        SchemaError        .value, .errors
    ValueTooLarge          .limit
    ModelError             .status
    AgentFailed            .result, .status
        AgentCancelled
    StillRunning(KyoraError, TimeoutError)   .agents
    MailboxFull            .agent
    AgentFinished          .agent
    ToolError              .text
    StaleCell
    Overloaded
Cancelled(asyncio.CancelledError)            # a BaseException
```

| Exception | Wire code | Raised when | Rust origin |
|---|---|---|---|
| `LimitExceeded` | `limit_exceeded`, `data.limit` | Admission refused, nothing started: `depth`, `agents_live`, `agents_total`, `llm_calls` from the ledger; `tool_calls`, `subscriptions` from the supervisor; `outstanding` raised locally by the client (section 4.8). | `RecursionError::LimitExceeded`, `Ledger::admit` |
| `BudgetExceeded` | `budget_exceeded` | No headroom on this node's scope or an ancestor at admission, or a leaf call's reservation failed. | `RecursionError::BudgetExceeded` |
| `InvalidRequest` | `invalid_request` | Bad arguments, an unknown or unrelated node, a tool not held or not placeable, an unknown model, a stale file reference. | `RecursionError::InvalidRequest`, section 4.5 checks |
| `SchemaError` | `schema_error` | An output schema refused at spawn, a result failing pydantic validation, or a staged final value failing this node's schema. | `tool::check_schema`, `tool::validate` |
| `ValueTooLarge` | `value_too_large`, `data.limit` | An encoded request over 1 MiB (raised locally, nothing sent), a reply over 16 MiB (nothing consumed), a tool result over 15 MiB, or a final value over `message_chars`. | supervisor |
| `ModelError` | `model_error` | A leaf call failed after retries, was refused, or ended without completing. | `RecursionError::ModelError` |
| `AgentFailed` | none (from the outcome) | `result()` on a child that ended `max_turns`, `budget_exhausted`, `timeout`, `context_exhausted`, `refused`, `failed` or `interrupted`. | `Status` |
| `AgentCancelled` | none | `result()` on a child that ended `cancelled` while this cell lives. | `Status::Cancelled` |
| `StillRunning` | `still_running`, `data.running` | `yield_after` passed while the children were still running; they keep running. | section 5.3 |
| `MailboxFull` | `mailbox_full` | The recipient's mailbox is at capacity. | `RecursionError::MailboxFull` |
| `AgentFinished` | `agent_finished` | The recipient has ended. | `RecursionError::AgentFinished` |
| `ToolError` | `tool_error` | The tool returned an error result. | `ToolOutput::is_error` |
| `StaleCell` | `stale_cell` | A call from a thread or task that outlived its cell, from outside any cell, or from another process. | supervisor, client |
| `Overloaded` | `overloaded` | The host could not admit the reply within 10 s because the kernel's response memory was full (section 4.8). Nothing was consumed; a leaf call's tokens were still charged. | supervisor |
| `Cancelled` | `cancelled` | This cell was interrupted, timed out, or its node was cancelled. | `RecursionError::Cancelled` |

Three distinctions matter for code that recovers:

- A child's own deadline ends the child with status `timeout`, which is `AgentFailed`; the caller's `yield_after` passing is `StillRunning`, and the child goes on; the cell's deadline is `Cancelled`.
- From Python, running out of budget before a call is the recoverable `BudgetExceeded`, so code can wrap up; a child that runs out mid-task ends `budget_exhausted` (D8.2).
- `Cancelled` derives from `asyncio.CancelledError`, so it is not caught by `except Exception`, and one `except` clause covers the sync and async paths. Code should let it propagate.

Unexpected supervisor failures arrive as `internal` and raise `KyoraError`.

### 3.13 Threads, tasks and cell scope

Every request carries the cell that issued it and that cell's **scope nonce**, a 128-bit random value the host sends in `exec`. The kernel keeps both in a `contextvars.ContextVar` set before each cell. Tasks created with `asyncio` copy the context natively; the boot script makes `threading.Thread` and `ThreadPoolExecutor.submit` (and so `kyora.parallel`) copy it too, since threads do not inherit context variables before Python 3.14 (D9.3). A thread started in cell 3 therefore keeps cell 3's nonce while cell 4 runs, and the host rejects its calls with `StaleCell`; because the nonce is unpredictable, a thread cannot reach the new cell by guessing its id. Calls without a scope, and calls from a process whose pid is not the kernel's, raise `StaleCell` in the client. Async tasks still pending when a cell's code finishes are cancelled and awaited (within the 2 s grace) before the cell ends.

This is a correctness aid against stale and accidental calls, not a security boundary: code in the kernel can read the current nonce from memory, and a process that obtains the control descriptor can send frames. Neither gains anything beyond the node's own authority (section 4.5).

### 3.14 Async API

```python
from kyora import aio
await aio.llm(prompt, ...);            await aio.llm_batch(prompts, ...)
await aio.spawn(task, ...);            await aio.run(task, ...)
await aio.result(agent, yield_after=None)          # also: await agent
await aio.gather(agents, return_exceptions=False)
async for agent in aio.as_completed(agents, yield_after=None): ...
await aio.map(task, items, ...)
await aio.send(to, body);  await aio.receive(...);  await aio.wait(...)
await aio.cancel(agent);   await aio.call_tool(name, **input)
await aio.artifact(path);  await aio.fetch(artifact, path=None)
```

Same arguments, results and errors as the sync functions. Responses resolve futures from the kernel's reader thread (`asyncio.wrap_future`), so no thread per call is needed.

### 3.15 Examples

Map-reduce over a long input with leaf calls:

```python
chunks = [context[i:i + 200_000] for i in range(0, len(context), 200_000)]
notes = kyora.llm_batch([f"List every date in this text, one per line:\n\n{c}" for c in chunks],
                        concurrency=16)
dates = sorted({d.strip() for n in notes for d in n.splitlines() if d.strip()})
print(len(chunks), "chunks,", len(dates), "dates")
```

Agents with structured output and a budget each:

```python
Dates = kyora.schema({"dates": [str], "unclear": [str]})
results = kyora.map("Resolve every date in `context` to ISO 8601.", hard_chunks,
                    concurrency=8, output=Dates, budget=400_000, timeout=600,
                    return_exceptions=True)
ok = [r.value for r in results if isinstance(r, kyora.AgentResult)]
failed = [r for r in results if isinstance(r, kyora.KyoraError)]
print(len(ok), "resolved;", [str(e) for e in failed][:3])
```

A long-running persistent child, polled across cells, with progress messages:

```python
# cell 5
auditor = kyora.spawn("Audit every file under src/ and report risky patterns. "
                      "Send me a message after each directory.",
                      name="auditor", persistent=True, budget=2_000_000)
# cell 6, later
for m in kyora.receive():
    print(m.body[:200])
try:
    report = auditor.result(yield_after=5)
except kyora.StillRunning:
    print("auditor still running:", auditor.status().turns, "turns")
```

Async fan-out where the first good answer wins:

```python
tries = [await kyora.aio.spawn(f"Prove lemma 3 using approach {k}.", output=Proof) for k in "ABC"]
async for a in kyora.aio.as_completed(tries, yield_after=900):
    try:
        r = await kyora.aio.result(a)
    except kyora.AgentFailed as e:
        print(a.name, e.status)
        continue
    if r.value.valid:
        kyora.final(r.value.proof)
        break
# the other children are cell-owned: they are cancelled when this cell ends
```

A child on its own machine returns a file as an artifact:

```python
# in the child, running on a machine executor
subprocess.run(["make", "report.pdf"], check=True)
kyora.final({"report": kyora.artifact("report.pdf")})
# in the parent
r = kyora.run("Build the report.", executor="vm", output=kyora.schema({"report": dict}))
path = kyora.fetch(r.value["report"])
```

## 4. Wire protocol

### 4.1 Transport and framing

The kernel and its supervisor talk over one connected, private stream: the **control channel**. stdout and stderr are separate byte streams captured by the host (section 2.6).

- **Local.** `socketpair(AF_UNIX, SOCK_STREAM)`. The kernel's end is mapped to fd 3 in the child by a `dup2` in `pre_exec` (the one `unsafe` block in `kyora-repl`, a local exception to `unsafe_code = "deny"` in the workspace `Cargo.toml`, as D3 grants the sandbox crate). The host's end, and every other descriptor kyora opens, stay close-on-exec, so no other process kyora spawns (shell commands, MCP servers, other kernels) inherits either end.
- **Remote.** The same frames inside a multiplexed stream to a relay on the other machine (section 6.3).

There is no named socket and no listener anywhere. A future transport that needs a filesystem socket must place it in a 0700 directory with mode 0600, accept exactly one peer whose credentials (`SO_PEERCRED`, `getpeereid`) match the expected uid and pid, and unlink it before `hello`.

**Frames.** A 4-byte big-endian unsigned length, then that many bytes of UTF-8 JSON holding one object (the framing of the rlm broker and nano-rlm, R§1, R§2). Length 0, a length above the direction's cap (section 4.8), invalid UTF-8, or a body that is not a JSON object is a protocol violation. Ids are positive integers below 2^53.

### 4.2 Handshake and versioning

The first frame from the kernel must be `hello`, within the startup bound:

```json
{"kind": "hello", "protocol": [1], "token": "q3Zk...", "pid": 4242,
 "python": "3.12.4", "platform": "linux", "features": ["aio", "display"]}
```

The supervisor compares the token in constant time with the one it generated for this generation, and picks the highest protocol version both list. Then:

```json
{"kind": "welcome", "protocol": 1, "node": 4, "parent": 0, "depth": 1,
 "generation": 1, "cwd": "/work", "executor": "local", "instance": "local-4242", "scratch": "/tmp/kyora-4-1",
 "limits": {"max_depth": 2, "max_agents_live": 16, "max_agents_total": 100, "max_llm_calls": 2000,
            "budget_limit": 20000000, "message_chars": 20000, "mailbox_capacity": 64,
            "tool_output_chars": 20000, "python_turn_chars": 40000,
            "cell_timeout_ms": 1800000, "max_cell_timeout_ms": 7200000,
            "request_max": 1048576, "response_max": 16777216, "delivery_max_bytes": 14680064,
            "outstanding": 256, "outstanding_bytes": 67108864, "tool_calls": 8, "subscriptions": 16},
 "features": ["messages", "tools", "events", "output", "files", "artifacts"]}
```

or `{"kind": "reject", "code": "bad_token" | "unsupported_version", "message": "..."}` followed by destruction. Nothing else is accepted before `welcome`. The token authenticates the connection once, at `hello`; after that the connection itself is the credential, so the token is never logged or traced and the client discards it, but reading it from memory later gains nothing.

Versioning: the version is an integer, bumped only for incompatible changes. Additions (new optional fields, new operations announced in `features`) keep it. Both sides ignore unknown fields. An unknown operation gets an `unsupported` error response and is not a violation. The kernel's Python files are embedded in the kyora binary, so a local kernel always matches; the handshake matters for relays and machine images that cache older files.

### 4.3 Envelope

```json
{"kind": "request",  "id": 17, "op": "agent.spawn", "cell": 4, "scope": "kH2v...", "params": {...}}
{"kind": "response", "id": 17, "result": {...}}
{"kind": "response", "id": 17, "error": {"code": "limit_exceeded",
                                         "message": "limit exceeded: agents_live",
                                         "data": {"limit": "agents_live"}}}
{"kind": "event", "op": "agent.event", "params": {...}}
```

- Both sides send requests. Each sender's request ids strictly increase within a connection. The reader checks this before dispatch, so a duplicate or reused id is a protocol violation caught before any side effect. A response refers to a request the receiver sent, at most once. Each generation is a new connection with a new token, so ids never cross generations.
- Every kernel request carries `cell` and `scope`. Host requests carry `cell` where it applies (`exec`, `interrupt`).
- A response has exactly one of `result` and `error`. `error.code` is one of the codes in section 3.12 or `unsupported`, `internal`; `data` is optional and code-specific.
- Events have no id and get no response.

### 4.4 Operations

Exact parameter and result schemas are in Appendix B.

Host to kernel:

| op | kind | params | result |
|---|---|---|---|
| `exec` | request | `cell`, `scope`, `code`, `deadline_ms`, `marker` | `ExecResult` |
| `interrupt` | event | `cell`, `reason` (`timeout`, `cancelled`) | |
| `vars.set` | request | `name`, `source` (JSON, file or artifact) | `{type, size}` |
| `vars.list` | request | | `[{name, type, size}]` |
| `agent.event` | event | `sub`, `type`, fields (section 4.6) | |
| `ping` | request | | `{}` |
| `shutdown` | request | | `{}`, then the process exits |

Kernel to host (all carry `cell` and `scope`):

| op | result | Rust (section 5.1) |
|---|---|---|
| `llm` | the completion and its ledger usage | `NodeCtx::llm` |
| `agent.spawn` | `{node, name, executor, tools}` | `NodeCtx::spawn_agent` |
| `agent.result` | `{outcome or null, more, messages}`, consuming | `NodeCtx::wait_with` |
| `agent.outcome` | `outcome`, read-only, for an ended child | `NodeCtx::child_outcome` |
| `agent.status` | `ChildStatus` | `NodeCtx::child_status` |
| `agent.cancel` | `{outcome or null, more, already_finished, messages}` | `NodeCtx::cancel_agent_with` |
| `agent.list` | this node's children | `NodeCtx::children` |
| `agent.resolve` | `{node, relation}` | `NodeCtx::resolve`, `NodeCtx::relation` |
| `agent.watch` | `{sub, snapshot}` or `{ended}` | `NodeCtx::relation`, `NodeCtx::subtree`, `TraceSink::subscribe` |
| `agent.unwatch` | `{}` | |
| `msg.send` | `{id, to}` | `NodeCtx::resolve`, `NodeCtx::send` |
| `msg.receive` | `{messages, pending}`, consuming | `NodeCtx::receive_with` |
| `msg.wait` | `{finished, undrained, running, messages}`, consuming | `NodeCtx::wait_with` |
| `msg.pending` | `{pending}` | `NodeCtx::pending_messages` |
| `budget` | `{budget, counters, deadline_ms, cell_deadline_ms}` | `NodeCtx::budget`, `Ledger::counters` |
| `tool.list` | the callable subset: `[{name, description, input_schema}]` | `NodeCtx::tools`, minus the exclusions of section 3.10 |
| `tool.call` | `{text, is_error, final_staged}` | `NodeCtx::call_tool` |
| `final` | `{}` | staged on the cell (section 3.11) |
| `artifact.put` | `{upload}`, then the artifact on the last chunk | session artifact store |
| `artifact.get` | one chunk | session artifact store |
| `log` (event) | | `TraceEvent::Log` |
| `display` (event) | | the cell's display buffer |

`msg.wait` returns summaries of the finished children whose notices it consumed (status, turns, usage, answer size) and the ids of those it could not finish draining; the client drains those with `agent.result` and then fetches each answer with `agent.outcome`, so no single reply has to hold every answer. `agent.result` and `agent.cancel` return `outcome: null, more: true` until the reply that carries the child's notice (section 4.7). A still-running child answers `agent.result` with error `still_running`.

### 4.5 Authority and cell scope

What the host enforces:

- **The connection is the node.** It is bound at `welcome` to one node, chosen by the host. No operation names a node to act as; every request is served through that node's `NodeCtx`, so a kernel can do exactly what its node may do, and anything that obtains its control descriptor can do the same and no more.
- **Every node id is checked before use.** `NodeCtx::resolve` parses a numeric address without checking that the node exists or is related (`crates/core/src/runtime.rs`), so the supervisor never treats a resolved id as authorization. Two checks cover every node argument, and each operation uses exactly one of them:

  | Operation | Allowed targets | Check |
  |---|---|---|
  | `agent.resolve` | a child (by id or name) or a deeper descendant (by id) | `NodeCtx::relation` (change C5) |
  | `agent.result`, `agent.outcome`, `agent.status`, `msg.wait` | direct children | `NodeCtx::relation` |
  | `agent.cancel` | any descendant | `NodeCtx::relation`, and `NodeCtx::cancel_agent` checks again |
  | `agent.watch` | any descendant | `NodeCtx::relation` |
  | `msg.send` | the parent, children and siblings | `NodeCtx::kin`, the existing check inside `NodeCtx::send`; `relation` is never used for messages |
  | `agent.spawn` tools | tools the node holds, placeable on the child's executor (section 6.2) | the explicit selection, then `NodeCtx::spawn_agent` |

  An unknown or unrelated id gets `invalid_request` without revealing whether the node exists.
- **Event watching is scoped.** `TraceSink::subscribe` delivers the whole session's trace (`crates/core/src/trace.rs`). The supervisor filters it by the watched subtree (section 4.6) before anything else touches a record, and nothing from outside that subtree is ever encoded for the kernel.
- **Cell scope.** A request is served only if its `cell` and `scope` match the cell running on this connection; otherwise it gets `stale_cell`, and between cells every request does. The scope concerns the request, not the handle: a later cell may read the outcome of a child spawned by an earlier one.
- **Cell token.** Each cell has a `CancellationToken`, a child of the tool call's `ToolCx::cancel`. Leaf calls and cell-owned children of the cell are owned by it (`NodeCtx::llm`'s `owner`, `Owner::Cell`), so node cancellation reaches them through the token tree, and cell exit through the token itself.

What the client does for itself, without claiming a boundary: it refuses calls from threads without a scope and from other processes, closes the control descriptor in children forked through `os.fork`, and keeps its own request counts under the limits. Code can bypass all of that; the host then still applies everything above, and kills the kernel on any protocol violation.

Compared with nano-rlm, which uses capability and cell-scope identifiers on a session-wide socket (R§2): kyora keeps cell scope and replaces per-handle capability ids with a per-agent connection whose every node argument is checked against the runtime's tree.

### 4.6 Child events

`agent.watch` subscribes to a descendant's subtree:

1. The supervisor checks the relation (section 4.5) and the subscription limit (16 per kernel).
2. If the watched node has already ended, the reply carries `ended` with its outcome and no subscription is created.
3. Otherwise it subscribes to `TraceSink::subscribe` first, then builds a **snapshot** of the subtree from the runtime's agent directory (`NodeCtx::subtree`, change C5): each node's parent, depth, name, status, turns and subtree usage. The reply carries the subscription id and the snapshot. The watched set starts as the snapshot's nodes and grows with each `node_start` whose parent is in the set; records about other nodes are dropped unread.

Events then follow as `agent.event` frames:

| `type` | From | Fields |
|---|---|---|
| `started` | `node_start` | `node`, `parent`, `depth`, `name` |
| `turn` | `message` (assistant) | `node`, `text` (first 500 characters of visible text) |
| `tool` | `tool_call` | `node`, `name` |
| `usage` | `attempt_end` | `node`, `charged` |
| `message` | `message_sent` | `from`, `to`, `kind`, `chars` |
| `ended` | `node_end` | `node`, `status`, `usage_subtree` |
| `lagged` | supervisor | `dropped` |

- Events between subscribing and the snapshot may repeat what the snapshot shows; `started` and `ended` are delivered at most once per node.
- Events are observability, not delivery: results and messages reach code exactly once through their own operations. Streaming deltas are never forwarded.
- When the broadcast lags (`TRACE_CAPACITY`, 4096, `crates/core/src/defaults.rs`), the subscription's queue (256 events) is full, or no response memory is free, events are dropped and a `lagged` event says how many.
- **Terminal signal.** The watched node's `ended` comes from its outcome (the handle's watch channel), not from the lossy broadcast. It is delivered even with a `kinds` filter that excludes `ended` and even after drops, always as the subscription's last event, preceded by `lagged` if anything was dropped. A subscription also ends with `agent.unwatch` or with its cell.

In Python, `Agent.events()` is a generator over these that ends after `ended`.

### 4.7 Delivery to code

A request that consumes messages (`agent.result`, `agent.cancel`, `msg.receive`, `msg.wait`) must not lose them between the mailbox and the kernel, must not deliver them twice, and must keep each sender's order. The runtime takes them as a **lease** (change C3), and the supervisor commits the lease only once the reply has left the process. The steps, in order:

1. **Barrier.** The supervisor acquires the mailbox's code-delivery barrier: at most one code lease per mailbox is in flight, from lease to commit or abort. Without it, request A could lease a child's progress messages and wait for reply memory while request B leased the same child's later notice and committed first; aborting A could not undo B, and the result would reach code before the progress, breaking the FIFO order the mailbox keeps today (`take` in `crates/core/src/messages.rs`). Readiness waits happen before this step and outside the barrier: waiting for a child to end and its entry to be queued (`result`, `wait`), or for a message to arrive (`receive`'s `yield_after`), uses take-nothing readiness calls (change C3); only then is the barrier acquired, and if another request took what was ready meanwhile, the request goes back to waiting. The barrier is held for an absolute bound: at most 10 s waiting for reply memory (step 2), then a **lease deadline** of 15 s from lease to commit or abort (steps 3 to 6). A lease not committed by its deadline aborts; if part of its frame was already written, the frame cannot be retracted, so the kernel is destroyed as for a protocol failure. A slow reader therefore cannot hold the barrier longer than 25 s. Waiting for the barrier ends when the cell token is cancelled. Turn deliveries and model-facing tools never contend for it, since none of them runs during a cell (section 2.2).
2. **Reserve.** Before creating any reply material, it computes the reply's maximum encoded size: `max_bytes` for messages, plus the encoded size of everything else (an outcome is measured by borrowing it, without a copy, change C5), plus a fixed envelope allowance. It acquires response memory for that size (section 4.8). If none is free within 10 s it answers `overloaded`, having leased nothing.
3. **Lease.** The `_with` method takes the messages with `Take::Code { max_bytes, owner }`, where `owner` is the cell token. Leased envelopes leave the queue but are not delivered: no other taker sees them, they still count against the mailbox capacity, and their notices' senders are not yet consumed. The take is refused, leasing nothing, if `owner` is already cancelled; the check is in the same critical section as the take. A code take always includes at least one whole envelope: `max_bytes` is at least 1 MiB, and the repl configuration is refused unless an envelope of `message_chars` characters fits in it.
4. **Validate.** It encodes the reply. It fits the reservation by construction; if it does not anyway, the lease is aborted and the request fails with `value_too_large`, consuming nothing.
5. **Admit.** Under the connection gate it checks that the cell is still open and the connection alive, and queues the frame for the writer. Otherwise it aborts the lease and answers `cancelled`.
6. **Commit or abort.** When the writer has written the whole frame to the socket, the lease commits: `message_delivered` is recorded with `via: code`, and each notice's sender is marked consumed by this cell. If the write fails because the connection ended, the lease aborts. Either way the barrier and the unused part of the reservation are released.

Aborting returns every leased envelope to its place in the queue (each queued envelope keeps an arrival sequence), wakes waiters, and records nothing; those messages reach the model at the next turn boundary as if code had never asked.

**Draining a child's result.** `agent.result` and `agent.cancel` take only that child's envelopes, in arrival order. When the child's terminal notice is among them, the reply carries the outcome. When it is not, because the messages queued ahead of it did not fit (the runtime reports this as `Waited::deferred`), the reply carries those messages with `outcome: null, more: true`, and the client immediately asks again for the same child, each time under the barrier, until the reply that carries the notice. Each round takes at least one envelope, so the drain ends. `result()` returns only then, with every message on `.messages`, so its contract holds: once it has returned, the notice is consumed and the model is not pinged. A failed round (overload, cancellation) leaves the rest queued; what earlier rounds committed stays delivered, in order.

**The ordering point** is the gate. Cell exit (section 5.4) takes the gate to close the cell before it sends `interrupt`, so every reply was either queued ahead of everything cell exit sends, and is delivered before the kernel sees the interrupt, or is aborted. A lease never outlives its cell: cell exit awaits every request of the cell, and each finishes with a commit or an abort.

**After commit**, delivery is final. A kernel that dies after its socket accepted the frame loses those messages like any other state in its memory; their bodies remain in the trace (`message_sent`). An interrupt can still land in the main thread just after a reply was handed over; the messages then count as delivered to code.

### 4.8 Bounds, memory and overload

| Bound | Default | On violation |
|---|---|---|
| `hello` after start | 10 s (local) | destroy; tool error |
| frame body after its length prefix | 30 s | destroy |
| kernel to host frame | 1 MiB | destroy (the client measures first and raises `ValueTooLarge` locally) |
| host to kernel frame | 16 MiB | that reply is not sent; `value_too_large`, nothing consumed |
| outstanding kernel requests | 256 | the client raises `LimitExceeded("outstanding")` locally; an excess request is a violation and destroys the kernel |
| bytes of outstanding kernel requests | 64 MiB | as above |
| concurrent `tool.call` | 8 | `limit_exceeded` (`tool_calls`) |
| tool result returned to code | 15 MiB, and the tool's declared maximum (below) | `value_too_large` |
| answer inside a reply | 1 MiB | larger answers travel as an artifact reference (`{artifact}` in `Outcome.answer`), never cut |
| code delivery per reply | `max_bytes`, default 4 MiB, at least 1 MiB, at most 14 MiB | whole messages only; the rest stays queued or follows in the next drain round |
| leaf slots, from dispatch until the response is written or dropped | 32 per kernel, 256 process-wide | further requests wait for a slot, holding only their request |
| subscriptions per kernel | 16 | `limit_exceeded` (`subscriptions`) |
| events queued per subscription | 256, at most 2 KiB each | dropped, `lagged` |
| response memory per kernel | 64 MiB | reply waits up to 10 s, then `overloaded`; events dropped |
| response memory, all kernels | 512 MiB | same |
| writer progress | 30 s without a byte written while frames are queued | destroy |
| captured stdout, stderr, background output | 64 KiB head and 64 KiB tail each, per cell | dropped bytes counted |
| display items per cell | 32, at most 64 KiB of text each | omitted with a count |
| `log` events | 4 KiB each, 100 per cell | cut or dropped with a count |
| graceful shutdown | 2 s | destroy |
| kernel restarts per node | 5 | the tool refuses |

**Reply material.** Any bytes the supervisor creates or copies for a reply count: encoded frames, copies of outcomes, rendered tool results, artifact chunks and their encodings. The rule is that a handler reserves response memory for the most material it can create before it creates or copies any of it; where the material is produced by the runtime rather than the supervisor, a separate aggregate limit covers it. The per-kernel budget (64 MiB) and the process budget (512 MiB) apply to every reservation, and the writer queue is inside them, not extra.

| Operation | Material | Control |
|---|---|---|
| any reply or event | the encoded frame | reserved for its exact size, measured with a counting serializer that allocates nothing, before encoding; events that cannot reserve are dropped (`lagged`) |
| `agent.result`, `agent.cancel`, `agent.outcome`, `msg.wait`, `msg.receive` | outcome copies, leased envelopes | reserved before the lease and before any copy (section 4.7); outcomes are measured by borrowing (change C5); leased envelopes are moved out of mailbox memory, which the capacity and `message_chars` already bound |
| `tool.call` | the tool's rendered result and its buffers | reserved before the call for `output_limit` (the reply's room, at most 15 MiB) plus the tool's declared buffer bytes. The limit is passed in `ToolCx::output_limit` (change C4), and the tool enforces it while rendering, results and errors alike: it renders into a bounded buffer and stops with an error result at the limit, never allocating past it. Declared buffers cover what the tool holds besides its rendering: MCP tools the 16 MiB raw server message of mcp.md (redaction and rendering then count against the limit), `shell` its head and tail capture buffers (escaping and markers count against the limit), `read_file` its 4 MiB read cap (line-number prefixes count against the limit, so a newline-only file stops at the limit instead of rendering about 39 MiB). A tool that does not declare bounded rendering (`Tool::bounded_output`) cannot be called from code. |
| `artifact.get` | a 2 MiB chunk and its base64 | reserved before the file is read |
| `artifact.put` | incoming chunks | request-side limits (outstanding request bytes); each chunk is written to disk before the next is accepted |
| `llm` | the leaf's response, produced by the runtime | a leaf slot (32 per kernel, 256 process-wide) acquired before `NodeCtx::llm` is called and held until the response has been encoded into a written reply or dropped; `max_inflight_requests` alone does not bound finished responses, because the runtime releases its model slot before the leaf returns. Each response is bounded by `llm_max_output_tokens`; the reply is then reserved like any other |
| cell exit joining children | outcomes | not copied: cell exit awaits `AgentHandle::finished` (change C5) instead of `AgentHandle::result`, which clones (`crates/core/src/recursion.rs`) |

Without this rule, 256 concurrent handlers could each build a near-cap reply before reserving, about 4 GiB per kernel.

**Overload path.** A handler that cannot get its reservation within 10 s gives up before creating material, or, for runtime-produced material such as a finished leaf, drops it, and answers `overloaded`; a lease is never taken without a reservation, so nothing is consumed. That error, like every error reply, is a small fixed-size frame (at most 1 KiB) drawn from one slot reserved for each outstanding request when the request was accepted, so an error can always be queued. The reader never awaits a reservation or a handler; it only decodes, checks and dispatches, so it keeps draining the channel however full the reply side is. A kernel that stops reading is destroyed by the writer progress rule.

**Violations.** A malformed envelope kills the kernel: a bad frame, a body that is not JSON, an unknown `kind`, a missing or non-increasing `id`, a response to an id never sent or already answered, a request before `welcome`, or a request beyond the outstanding limits. The cell ends `crashed` with `protocol violation: <reason>`, and the run goes on. A well-formed request with bad arguments gets `invalid_request` and the kernel keeps running (D9.5).

### 4.9 Security

Enforced by the host:

- No network listener and no named socket; the control channel is an anonymous socketpair (section 4.1).
- A per-generation token authenticates the connection at `hello` on every transport. It is delivered over the stdin pipe and appears in no environment variable or argument.
- The connection acts only as its node, and every node argument is checked against the tree (section 4.5). Request parameters cannot raise a limit, reach a node outside the node's relationships, read another subtree's events, or grant a tool the node does not hold (capability attenuation, D18).
- Every frame, queue, subscription and byte of reply memory is bounded (section 4.8); malformed traffic ends only that kernel.
- Child answers and messages arrive in Python variables as data. They reach the parent model only through printed output under the budgets of section 2.6 (D18, "Injection via sub-agent results").
- Python path and executor settings are security settings, read only from the user's config, environment and flags, never from a project file (D17, trust classes).

Not enforced, and not claimed:

- Confinement of kernel code on the local executor, including access to credentials stored outside kyora (section 2.4).
- The client's cooperative checks: cell scope from threads, fork and pid checks, and its own request limits (section 4.5).

## 5. Mapping onto the Rust runtime

### 5.1 Call mapping

| Python | Rust | Notes |
|---|---|---|
| a cell | `Tool::call` of the `python` tool, with `ToolCx` | One cell per call; result through `ToolOutput`; final answer through `ToolOutput::final_answer` and `Runtime::accept`. |
| `llm(prompt, ...)` | `NodeCtx::llm(LlmCall { prompt, system, model, max_tokens, origin_cell, .. }, &cell_token)` | Leaf node at the caller's depth; consumes `llm_calls`; reservations per attempt. The result carries the leaf id and ledger usage on success and failure (C2). |
| `llm_batch` | one `NodeCtx::llm` per item | Window of `concurrency` in the client. |
| `spawn(...)` | `NodeCtx::spawn_agent(ChildSpec { task, name, model, tools, max_turns, budget, timeout, init, preamble, origin_cell, output }, owner)` | `owner` is `Owner::Cell(cell_token)` or, with `persistent=True`, `Owner::Node`. `tools` is always an explicit, placement-aware selection (section 6.2). `init` carries variables and the executor; `preamble` the manifest. |
| `Agent.result(yield_after)` | `NodeCtx::wait_with(Some(&[id]), .., Take::Code { .. })`, committed per section 4.7 | `yield_after` covers running; an ended child's delivery is awaited to the cell deadline (section 5.3). |
| `Agent.status()` | `NodeCtx::child_status` | Direct children; reads only. |
| `Agent.cancel()` | `NodeCtx::cancel_agent_with(id, Take::Code { .. })` | Cancels the subtree; consumes a direct child's result. |
| `gather`, `as_completed` | one `agent.result` per handle | Concurrent requests; no new Rust API. |
| `map` | `NodeCtx::spawn_agent` per item, `agent.result` per child | Retries `LimitExceeded("agents_live")` only while its own children run. |
| `agent(ref)`, `agents()` | `NodeCtx::resolve` then `NodeCtx::relation`; `NodeCtx::children` | Never trusts a parsed id (section 4.5). |
| `send(to, body)` | `NodeCtx::resolve`, then `NodeCtx::send` | `send` checks kinship itself. |
| `receive(...)` | `NodeCtx::receive_with(yield_after, Take::Code { .. })` | |
| `wait(...)` | `NodeCtx::wait_with(targets, timeout, Take::Code { .. })` | Explicit targets (section 3.7). |
| `pending()` | `NodeCtx::pending_messages` | |
| `budget()` | `NodeCtx::budget` | `BudgetSnapshot` |
| `limits()` | `Ledger::counters(node)`, `NodeCtx::deadline`, `welcome` maxima | |
| `tools()`, `call_tool` | `NodeCtx::tools`, `NodeCtx::call_tool` | |
| `final(value)` | staged on the cell; `ToolOutput::final_answer` | Checked when staged (section 3.11). |
| `Agent.events()` | `NodeCtx::relation`, `NodeCtx::subtree`, `TraceSink::subscribe`, the handle's outcome | Section 4.6. |
| `artifact`, `fetch` | the session artifact store in `kyora-repl` | Not a core concept. |
| `log`, `display` | `TraceEvent::Log`; the cell's display buffer | |

`NodeCtx::spawn_agent`, `llm`, `resolve`, `send`, `receive`, `wait`, `cancel_agent`, `pending_messages` and `budget` exist today in `crates/core/src/runtime.rs`; the rest are added by section 5.6.

### 5.2 Ownership

- **Cell-owned** (`spawn` default, `run`, `map`): `Owner::Cell(cell_token)`. `NodeCtx::spawn_agent` gives the child a node token that descends from this node's token and adds the cell token as an extra owner; `Runtime::run_node`'s watcher cancels the child when the cell token is cancelled. No notice is posted to the mailbox (`notify` is false for cell owners). Rule 1 (cancelled with its subtree) holds through the token tree and the child's ordered shutdown.
- **Node-owned** (`persistent=True`): `Owner::Node`. The child's notice is posted to this agent's mailbox (`Runtime::notify`), the agent idles at `end_turn` until it arrives (`Runtime::idle`), and the model receives it at a turn boundary unless code consumed it first (rule 2). The child is cancelled when this agent ends (`Runtime::join_descendants`).
- **Structured** (`output=`): either ownership; the child ends on a valid submission and its running children are cancelled (rule 3, `Runtime::agent` with `settings.output`).

### 5.3 Messages, notices and the model's view

Today the mailbox keeps two sets per agent (`crates/core/src/messages.rs`): `concluded`, the children whose terminal entry was queued or staged, and `seen`, the children whose outcome this agent's model has been handed. A notice taken at a turn boundary or by a model-facing tool enters `seen` (`take`), and `NodeCtx::report` decides how a model-facing `wait` or `cancel_agent` reports an outcome: a repeat shrinks to its status line; a first report of a child that is `concluded` but not `seen` says it "follows at your next turn", because its notice must still be queued; any other first report is charged or staged (`NodeCtx::report` in `crates/core/src/runtime.rs`).

Code consumption breaks that inference: after code takes a notice, the child stays `concluded` (set when the notice was pushed), never enters `seen`, and cannot be staged again (`Mailbox::stage` refuses a concluded child), so a later model-facing `wait` would promise a notice that will never come. Change C3 therefore separates consumption from model observation:

- The mailbox gains `consumed: map from child to cell`, set when a code lease commits (section 4.7) for each notice in it. Code takes never add to `seen`.
- `NodeCtx::report` checks, in order:
  1. `seen`: a repeat, as today;
  2. the notice is still queued or staged: "follows at your next turn", as today, now tested directly instead of inferred from `concluded`;
  3. `consumed`: a first report from the handle's outcome, charged against the turn's delivery budget; if it fits, the child enters `seen` and the full report is returned; if not, the status line with `(answer consumed by code in cell N; left out here: no room left in this turn's messages)`, and nothing is staged, since the result was delivered;
  4. otherwise: charged or staged, as today.
- No model-facing tool can observe a lease in flight, because leases live only within a cell and no other tool runs during a cell (section 2.2).
- `Mailbox::outstanding` already excludes consumed children (their notice is neither awaited nor queued), so a later model-facing `wait` without arguments does not wait for them.

**Yield semantics.** The runtime's `wait` counts a child as finished only when its outcome is published and its terminal entry is queued (`finished` in `NodeCtx::wait_via`), and for a cell-owned child the first wait stages that entry as owned work that records it before queueing it. `Agent.result(yield_after)` therefore runs in two phases: it waits up to `yield_after` for the child to end (its outcome published, `AgentHandle::is_finished`), raising `StillRunning` if it has not; once the child has ended it waits, without `yield_after`, until the result is deliverable, bounded by the cell deadline. Staging is one trace record, so the second phase is short.

**Turn budget.** Code takes use `Take::Code` and charge nothing to `Mailbox::charge`; turn-boundary deliveries and model-facing tools keep `Take::Turn` and today's budget. What code leaves queued reaches the model at the next boundary, which for a running cell is the user message carrying the cell's tool result (agent-messages.md, "Turn boundaries").

### 5.4 Cell exit and node shutdown

Cell exit runs on every ending (ok, error, timeout, interrupted, crashed, lost), before the tool result is returned:

1. **Close.** Under the gate, the supervisor records the cause (which fixes the cell's status, section 2.5), marks the cell closed, and cancels the cell token. From here no reply for this cell is admitted (section 4.7); leaf calls of the cell stop and settle (m1-runtime.md, "Accounting and ownership"); cell-owned children are cancelled with their subtrees; code takes are refused.
2. **Stop the code.** If the kernel is still running the cell, it sends `interrupt`, waits up to 2 s for the cell's result, and otherwise makes one destroy attempt under its call timeout (section 6.5) and ends the generation. It does not wait for destroy retries: host-side work for the cell was cancelled in step 1, and the kernel cannot act through kyora once the gate is closed.
3. **Drain requests.** It awaits every request task of the cell; each ends with a reply written, a lease aborted, or `cancelled`. Leaf calls, waits and receives end promptly once the token is cancelled. Tool calls end as section 3.10 describes: a shell call when its group is killed, a started file write or edit when it completes (no deadline), an MCP call once kyora has sent its cancellation, which does not mean the server stopped.
4. **Join children.** It awaits `AgentHandle::finished()` (change C5; it waits without copying the outcome) for every cell-owned child of the cell. Each resolves only after that child's ordered shutdown: its mailbox closed, its descendants cancelled and joined, its `node_end` written (agent-messages.md, "Termination and cancellation").
5. **Report.** It ends the cell's event subscriptions, writes `cell_end`, and returns the tool result, listing the children cancelled at cell end.

Persistent children are not touched. Every step is bounded by the cell's deadline machinery, the 2 s grace and the destroy call timeout, except started file mutations and MCP calls, which end on their own terms as described; cancellation reaches a provider within `PROVIDER_CANCEL_GRACE` (250 ms). A child joined in step 4 may report an incomplete teardown (below); its node has ended, but code on its machine is only known stopped by its `fenced_at`.

Node shutdown, with the Python layer, runs in this order (the existing order of `Runtime::run_node` plus C1):

1. The agent loop ends; no cell is running, since cells run inside the loop.
2. The mailbox closes; queued messages are recorded as undelivered.
3. Admission closes, descendants (persistent children, and cell-owned ones of a cell that was cut short) are cancelled and joined; concurrently, each tool's start task is cancelled and joined, then its shutdown hook runs, for `python` closing the gate and tearing the kernel down within 12 s (section 2.5). Each hook returns a teardown report.
4. `node_end` is written with the combined teardown report (complete only if every tool's is; `fenced_at` the latest), the same report goes into the node's `AgentOutcome`, and the live-agent slot is released.
5. The node's handle resolves and its notice goes to its parent.

### 5.5 Ledger and trace

The Python layer adds no accounting of its own. Leaf calls and child agents go through `Ledger::admit` and per-attempt `Ledger::reserve` and `Ledger::settle` exactly as the agent tools do, so the scope tree, the pre-dispatch guarantee and the overshoot bound of D10.2 are unchanged. Kernel CPU, memory and machine time are not budgeted; `cell_timeout`, rlimits and the executor bound them.

What each call records:

| Call | Records |
|---|---|
| kernel start and end | `kernel_start {node, generation, executor, instance, pid, python}`, `kernel_end {node, generation, reason, exit, destroy}` (new) |
| a cell | `tool_call` (holds the code) and `tool_result` as for any tool; `cell_start {node, generation, cell, call}` and `cell_end {node, cell, status, cause, wall_ms, started: {llm, agents, tools}, cancelled_at_end, consumed: {results, messages}, usage}` (new) |
| live cell output | `cell_output {node, cell, stream, text}`, ephemeral like `delta`, never persisted (new) |
| `llm` | `node_start` (`kind: llm`, `origin_cell`), `attempt_start`, `attempt_end`, `node_end` |
| `spawn` | `node_start` (`kind: agent`, `origin_cell`), then the child's own records |
| preloaded variables | `var_loaded {node, name, source: {inline, bytes} or {file, sha256, bytes, origin} or {artifact, sha256, bytes}}` (new) |
| `artifact`, `fetch` | `artifact_put {node, generation, name, sha256, bytes}`, `artifact_get {node, sha256}` (new) |
| `send` | `message_sent` |
| `receive`, `wait`, `result`, `cancel` | `message_delivered` with `via: code`, written at lease commit; `message_sent` for a staged notice of a cell-owned child |
| `call_tool` | `tool_call` and `tool_result` with `origin_cell` and call id `py:<generation>.<cell>.<n>` |
| `final` | `cell_end` notes it; `node_end` carries the answer |
| node shutdown | `node_end` gains `teardown {complete, fenced_at?, note?}` (C1) |
| `log` | `log {node, cell, level, message}` (new) |
| `budget`, `limits`, `status`, `tools`, `pending` | nothing |

`cell_end.usage` is the sum of the ledger usage of the cell's leaves (from C2, failed ones included) and cell-owned children, which are final at that point, plus the usage so far of persistent children it spawned. Tree reconstruction (`reconstruct_tree` in `crates/core/src/trace.rs`) ignores the new records, as it ignores message records; the TUI already has cell nodes (`NodeKind::Cell`, `ReplCellStarted`, `ReplCellFinished` in `crates/tui/src/event.rs`) for `cell_start` and `cell_end` to feed.

### 5.6 Required core changes

Additive changes to `kyora-core`, each with its own tests. C1 (with its read accessors), C2, C6, C8 and the kernel, cell and log events of C7 block R0; C3 and C5 block R1; C4 lands with R2; `ArtifactGet` lands with R1 and `ArtifactPut` with R3.

- **C1. Toolset retention, lifecycle hooks and accessors.** Today the frozen toolset is a local of `Runtime::agent` (`let tools = tools?;`) and only its names reach `NodeState::tools`; `submit_result` is added after the factory returns (`with_own_validation` in `Runtime::agent`), and neither `NodeInfo` nor `ToolCx` exposes the output contract. C1:
  - keeps the final `Toolset` (after selection and `submit_result`) in `NodeState` from the moment it is built, and adds read accessors `NodeCtx::tools()` (frozen specs) and `NodeCtx::output_schema()` (the contract, if any), which R0 needs for early `final` checks;
  - adds `Tool::start(&self, node: &NodeCtx)` (async, default no-op). Once the toolset is retained and `node_start` is written, `Runtime::run_node` runs each tool's `start` as a task owned by the node, concurrently with the first model request; the node token cancels it. This is the entry point for eager remote kernels (section 6.2), since factories are synchronous (`ToolsetFactory::toolset`, `crates/core/src/tool.rs`) and must not start processes;
  - adds `Tool::shutdown(&self) -> Teardown` (async, default `Teardown { complete: true, fenced_at: None, note: None }`). After the agent loop ends, and also when the loop never started because `node_start` or a later step failed after the toolset was built, `Runtime::run_node` cancels and joins the start tasks, then calls `shutdown` once on each tool, concurrently with `join_descendants`, each under `catch_unwind` (a panic marks the run failed, like other panics there, and counts as an incomplete teardown), and awaits them before `node_end`. Hooks bound their own duration; the runtime adds no timeout;
  - adds `AgentOutcome::teardown` (serde default complete) and the same field on `node_end`, combining the tools' reports.

  A factory that fails or panics returns no toolset, so there is nothing to start or shut down; factories must therefore not start processes or hold external resources, which the `python` tool satisfies by starting its kernel lazily or in `start`. Tools shared across nodes (MCP) keep the no-op hooks.
- **C2. Leaf origin and usage.** `LlmCall::origin_cell: Option<u32>`, recorded on the leaf's `node_start` (`NodeCtx::llm_owned` writes `origin_cell: None` today). `LlmOutcome` gains the leaf's ledger usage (`Ledger::usage`, which includes reservations charged for failed attempts), and a failed call reports the leaf id and that usage alongside its `RecursionError` when a leaf was admitted, so the cell's accounting is exact.
- **C3. Code delivery.** `Take::Turn` (today's behaviour) and `Take::Code { max_bytes, owner }` arguments for `NodeCtx::receive_with`, `wait_with` and `cancel_agent_with`; the existing methods call them with `Take::Turn`. `Take::Code` returns a lease with `commit(cell)` and `abort()` (also on drop); queued envelopes keep an arrival sequence so an abort restores their order; leased plain messages keep counting against the capacity. `Delivery::Code` for the trace, the mailbox's `consumed` map, and the revised `NodeCtx::report` of section 5.3.
- **C4. Tools from code.** `NodeCtx::call_tool(name, input, cancel, origin_cell, output_limit)`, over the toolset C1 retains, validating and executing like `Runtime::agent` and recording `tool_call` and `tool_result` (both gain an optional `origin_cell`). `ToolOutput::final_answer` is returned to the caller, never committed by `call_tool`. `ToolCx::output_limit: Option<usize>` (unset for model-facing calls) and `Tool::bounded_output() -> Option<usize>` (default `None`): a tool returning `Some(buffer_bytes)` promises to keep every rendered result, error results included, within `output_limit` while it renders, and to hold at most `buffer_bytes` of other material; `shell`, the file tools and MCP tools implement it.
- **C5. Agent directory.** `AgentEntry` (`crates/core/src/runtime.rs`) gains the owner kind and the origin cell. New: `NodeCtx::relation(id) -> Result<Relation, RecursionError>` (child or deeper descendant, checking that the node exists), `NodeCtx::children()`, `NodeCtx::subtree(id)` (the snapshot of section 4.6), `NodeCtx::child_status(id)`, `NodeCtx::child_outcome(id)` (a read of an ended child's outcome that consumes nothing) and `NodeCtx::with_child_outcome(id, f)` (borrows the outcome, so its encoded size can be measured before anything is copied), all from the runtime's agent directory, so they also cover children the model started with `spawn_agent`. `AgentHandle::finished()` waits for the outcome without cloning it, unlike `AgentHandle::result()` (`crates/core/src/recursion.rs`).
- **C6. Counters.** `Ledger::counters(node)` returning the session-wide live, total and leaf counts that admission checks, and the node's subtree counts, kept on each scope under the ledger's mutex. The R0 `budget` reply carries them.
- **C7. Trace events.** `KernelStart`, `KernelEnd`, `CellStart`, `CellEnd`, `VarLoaded`, `ArtifactPut`, `ArtifactGet` and `Log` (persisted) and `CellOutput` (ephemeral) in `TraceEvent`.
- **C8. Turn identity.** `ToolCx::turn`, the node's admitted turn number, so tools can share a budget across the calls of one assistant message (section 2.6).

No change to admission, reservation or ownership semantics is needed. C1 adds a step to node shutdown without reordering the existing ones.

## 6. Executors: kernels on other machines

### 6.1 The executor interface

```rust
// kyora-repl
#[async_trait]
pub trait Executor: Send + Sync {
    fn name(&self) -> &str;
    fn traits(&self) -> ExecutorTraits;
    /// Cancel-safe: if `cancel` fires or the future is dropped after resources were
    /// created, the executor destroys them itself.
    async fn start(&self, spec: KernelSpec, cancel: CancellationToken) -> Result<Started, ExecError>;
}
pub struct ExecutorTraits {
    pub workspace: Workspace,   // Shared | Snapshot
    pub isolation: Isolation,   // Process | Machine
    pub fencing: Fencing,       // Confirmed | Lease { max_skew: Duration } | None (section 6.5)
}
pub struct KernelSpec {
    pub node: NodeId, pub generation: u32,
    pub python: String, pub cwd: PathBuf, pub env: Vec<(String, String)>,
    pub rlimits: Rlimits, pub token: Secret,
    pub files: Vec<Transfer>,   // workspace snapshot, file and artifact variables, kernel files
    pub lease_expires_at: DateTime<Utc>,   // absolute; recorded as sent before start is called
}
pub struct Started {
    pub link: KernelLink,
    pub instance: Arc<dyn Instance>,
}
pub struct KernelLink {
    pub control: Box<dyn Duplex>,                     // protocol frames
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    pub stderr: Box<dyn AsyncRead + Send + Unpin>,
    pub exited: BoxFuture<'static, Exit>,             // exit status, or Lost
}
#[async_trait]
pub trait Instance: Send + Sync {
    fn id(&self) -> &str;
    /// Extends the lease to an absolute UTC time; never shortens it.
    async fn renew(&self, expires_at: DateTime<Utc>) -> Result<(), ExecError>;
    async fn destroy(&self) -> Destroyed;
}
pub enum Destroyed { Confirmed, Failed(String), Unknown }
```

The supervisor makes every `renew` and `destroy` call under a call timeout (10 s each); a call that times out counts as `Unknown` for `destroy` and as a failed renewal for `renew`, and its future is dropped, so no caller ever waits on an executor without bound.

- **`LocalExecutor`**: socketpair, pipes and a process group, as in sections 2.5 and 4.1. `destroy` sends SIGKILL to the group and reaps the process, then returns `Confirmed`. Traits: `Shared`, `Process`, `Confirmed`.
- **Machine executors**: any provider of these primitives can back one: create a machine from an image that has `python3`, open an authenticated byte stream to a process on it, transfer files, destroy it and report the result, and, for `Lease` fencing, stop it on its own at the absolute expiry it was last given, by a clock within `max_skew` of the host's. kyora vms is the first adapter; containers, other microVM services or SSH hosts fit the same interface. Traits: `Snapshot`, `Machine`, and the fencing the provider can guarantee.

This replaces D13.3's `ExecBackend::spawn_repl`: instead of one byte stream and a kill switch, an executor returns separate control and output channels and an instance with an asynchronous destroy result, which gives slow output, loss and fencing a defined place.

### 6.2 Placement and tools

Each node's kernel is placed by executor name: `kyora.spawn(..., executor="vm")`, carried in `ChildSpec::init` to the child's toolset factory; the root's comes from `kyora run --executor NAME`. A child inherits its parent's executor name; on a machine executor every node gets its own machine. Executors are defined in the user's config (`[executors.<name>]` with a `kind` naming a compiled-in adapter), never in a project file.

Only the kernel moves **(pending owner confirmation)**. A child placed on a machine keeps its agent loop, its `NodeCtx`, its ledger scope, its mailbox and all its model traffic in the host process. Tools that act on the host's machine (`shell`, `read_file`, `write_file`, `edit_file`) would act on the wrong machine, so they are not placeable on a `Snapshot` executor; the child's code runs commands with `subprocess` and reads and writes files on its own machine. MCP tools run on the host, where their servers are, and stay placeable.

The selection is fixed before admission, because core freezes it: `NodeCtx::spawn_agent` checks the requested names against the parent's, and `Runtime::agent` reapplies the selection to whatever the factory returns. The supervisor therefore always passes an explicit `ToolSelection`:

- without `tools`: `defaults::SUBAGENT_TOOLS` intersected with the parent's tools (the rule `spawn_agent` applies for `None`), minus the tools the child's executor cannot host. The default includes `read_file`, which this removes for a snapshot executor.
- with `tools`: the request as given; naming a tool the executor cannot host fails with `InvalidRequest` (`tool read_file cannot run on executor vm`) before admission, nothing started.

The same filter applies to the root when `--executor` names a snapshot executor. A child of a node that lacks these tools never regains them, by the existing attenuation rule.

On machine executors the kernel starts eagerly, from the `python` tool's start hook (change C1), which the runtime runs as node-owned work once the child's toolset is retained, so the machine boots while the child's first model request is in flight. The first `python` call awaits that start if it is still in flight. Cancelling the node cancels the start, which is cancel-safe (section 6.1), and the shutdown sequence joins it before the tool's shutdown hook runs (section 5.4), so a machine is never started after its node began shutting down and never left without an owner. A start failure surfaces as an error on the child's first `python` call, which the child's model sees; the node itself goes on.

### 6.3 The mux

Between the host and a machine there is one stream per kernel, multiplexed:

```
mux frame = u32 big-endian length of the rest | u8 channel | u8 flags | payload (at most 64 KiB)
channel 0  control: protocol frames, fragmented
channel 1  stdout bytes
channel 2  stderr bytes
channel 3  relay: credit grants, ping and pong, exit report, destroy request
flags      bit 0 MORE: this control frame continues in the next channel 0 mux frame
```

- **Fragmentation.** A protocol frame (up to 16 MiB toward the kernel, 1 MiB from it) is split into consecutive channel 0 chunks of at most 64 KiB; the last has `MORE` clear. Chunks of two control frames never interleave; bulk chunks may appear between them. A mux frame longer than 64 KiB plus its header, an unknown channel, or a fragment sequence that exceeds the protocol cap is a violation.
- **Scheduling.** Each direction's sender takes whole chunks in a weighted round: channel 3 first whenever it has a chunk, then up to four channel 0 chunks, then one bulk chunk (channels 1 and 2 alternating) if any bulk is waiting with credit, and again. Each control chunk therefore waits behind at most one bulk chunk; a whole control frame of `k` chunks may have up to `ceil(k / 4)` bulk chunks interleaved with it. Output keeps a guaranteed share: at least one chunk in every six, about a sixth of the link when both are backlogged. Strict priority would let a busy control channel starve output indefinitely, so it is not used.
- **Credits.** Channels 1 and 2 (machine to host) flow under credit windows of 1 MiB each: the host grants credit on channel 3 as it consumes, the relay never sends beyond its credit, and when credit runs out the relay stops reading the kernel's pipes, so the kernel blocks on writes as it would on a full local pipe. A grant that would raise a window above 1 MiB, or bulk bytes beyond the granted credit, is a violation. Channel 0 has no window; it is bounded by the protocol's own limits (outstanding requests, response memory), and both ends always drain it.
- **Watchdogs.** Three separate checks. Stream progress: a side with bytes waiting to be written that cannot write any for 30 s declares the stream stalled. Output progress: a bulk channel with data waiting and credit available that sends nothing for 30 s declares the scheduler stalled, which the weighted round rules out unless an end misbehaves. Liveness: the host pings on channel 3 every 10 s; no mux frame of any kind for 30 s declares the kernel lost. Any of them ends the generation as `lost` (section 6.6).

The relay is a stdlib script shipped with the kernel files (`kyora_relay.py`). On the machine it starts the kernel exactly as the local executor does (socketpair on fd 3, pipes, process group, the token on stdin), forwards channels, and reports the exit status.

### 6.4 What crosses the wire, and artifacts

| Crosses | Direction | Notes |
|---|---|---|
| Control frames, stdout, stderr | both | The same protocol as locally; the host still captures and bounds output. |
| Token | host to machine, then kernel | Inside the executor's authenticated stream; checked by the host in `hello`. |
| Kernel files | host to machine | The embedded boot, package and relay files, by content hash. |
| Workspace snapshot | host to machine | At kernel start, content-addressed so siblings reuse it; its manifest (path to hash) stays with the supervisor for checking file references (section 3.9). |
| File variables | host to machine | By hash, from the snapshot or uploaded separately. |
| Artifacts | both, explicitly | `kyora.artifact` uploads from the machine to the host's session store; `kyora.fetch` and artifact variables download to a machine. Chunks travel over channel 0 (`artifact.put` 512 KiB, `artifact.get` 2 MiB of data per frame); an executor may move the bytes through its own file transfer instead. |
| Never | | Model credentials, provider traffic, the ledger, other nodes' frames. Machines never talk to each other; the host is the hub. |

Nothing flows back implicitly. Changes made on a machine stay there until code publishes them as artifacts or returns them as values; there is no workspace merge **(pending owner confirmation)**.

The host runs every agent loop (cheap async tasks) and every model call; machines hold Python state and compute. `max_agents_live` and `max_inflight_requests` bound the fan-out the host drives.

### 6.5 Leases, destruction and fencing

A partitioned old kernel may still be running and causing side effects of its own, so the host must know when an instance can no longer run before it starts a replacement or reports a teardown as complete. That moment is its **fence time**.

- **Leases with absolute expiry.** The initial lease is part of `KernelSpec` as an absolute expiry (`now + 120 s`), recorded as sent before `start` is called. While a generation is live, the host renews its instance every 30 s, each time to an absolute expiry `expires_at = now + 120 s` on the host's clock, and records the largest `expires_at` it has ever **sent**, whether or not the call was acknowledged. An executor with `Lease { max_skew }` fencing must stop the machine at the latest expiry it has received, by a clock within `max_skew` of the host's, and never extends a lease on its own. A renewal whose acknowledgement is lost may still have taken effect, so the fence time is the largest expiry sent plus `max_skew` plus 30 s of margin, never the last acknowledged renewal plus the TTL. Renewals stop when teardown begins. Renewals failing for longer than 120 s end the generation as `lost`.
- **Destroy, bounded.** Every `destroy` call has the 10 s call timeout of section 6.1. `Confirmed` makes the fence time now. On `Failed` or `Unknown` the host retries in the background with backoff (1 s, doubling to at most 60 s) until a call confirms or the run ends, and records each result in `kernel_end`. Neither cell exit nor node shutdown waits for these retries: cell exit makes one attempt (section 5.4), and the shutdown hook makes attempts for at most 10 s after the 2 s grace, then returns its teardown report (section 5.6, C1): complete if destroy was confirmed; otherwise incomplete, with `fenced_at` set to the fence time under `Lease` fencing, or absent under `None`.
- **What "stopped" means to callers.** A child's node can end while its machine is unconfirmed. Its outcome then carries `teardown_complete: false` and `fenced_at` (Appendix B, `Outcome.teardown`), and `cancel()` and `result()` say so (section 3.4), instead of promising stopped execution.
- **Startup cancellation.** `start` is cancel-safe (section 6.1). When the supervisor abandons a kernel after `start` returned but before `welcome`, it destroys it like any other end.
- **Restart waits for the fence.** On a machine executor a new generation starts only after the previous instance's fence time: at once after a confirmed destroy, or once the lease bound has passed. A `python` call waits up to 30 s for that; if the fence is further away it returns a tool error naming the instance and the time a kernel can start again. With `None` fencing and no confirmation there is no fence time, so the tool refuses new kernels for that node and says why (`previous kernel on instance i-7 not confirmed stopped`); the node's model can go on without Python. Locally the old process is always killed and reaped first.
- **Host crash.** Renewals stop; `Lease` machines stop at their last expiry; `None` machines may run until someone removes them, which the executor's documentation must state. On resume, nodes that were running are recorded as interrupted (D11.3).

### 6.6 Failure modes

| Failure | Detection | Effect |
|---|---|---|
| Machine start fails or no capacity | `start` error, or no `hello` within the executor's start bound (default 120 s) | Whatever was created is destroyed; the `python` call returns a tool error; the next call tries again and counts toward the restart limit. |
| Kernel crash on the machine | relay exit report | As a local crash: `crashed`, destroy, a new generation on the next call. |
| Machine lost | the stream ends without an exit report, or the executor reports loss | The cell ends `lost`; cell exit runs (cell-owned children are host nodes and are cancelled); destroy is attempted; the next call starts a new machine once section 6.5 allows. |
| Network partition | liveness or progress watchdog (section 6.3) | Treated as lost, with destroy as the fencing step. A new generation has a new stream and token, so nothing from the old one is accepted. |
| Slow link | credits exhausted | Output backpressure only; control frames go through. |
| Destroy fails or times out | `Failed` or `Unknown`, or the 10 s call timeout | Background retries; teardown reported incomplete with its fence time; a restart waits for the fence as section 6.5 says. |
| Host crash | renewals stop | Machines stop at their last expiry under `Lease` fencing (section 6.5). |

A partitioned kernel cannot spawn, call a model or message anyone, since all of that goes through the host. It can still have side effects of its own (writing to external systems), so the host never replays a cell: a cell that ends `lost` is reported to the model, which decides what to do.

## 7. Testing

All tests run without network or keys, as today (`cargo test --workspace --locked`, CLAUDE.md). The REPL tests need a real `python3`; CI runs them on the oldest supported version (3.11, pending confirmation) and the newest, on Linux and macOS (D19).

- **Codec and handshake (Rust units).** Frames split at every byte, zero and oversized lengths, invalid UTF-8, non-object bodies, a stalled body (30 s rule under paused time), bad and missing tokens, version negotiation, unknown operations, non-increasing and duplicate request ids (rejected before dispatch, so no side effect happens), duplicate and unsolicited responses.
- **Mux.** Random interleavings, fragmentation at every boundary, oversized and unknown frames, credit overrun, property tests that each control chunk waits behind at most one bulk chunk and a control frame of `k` chunks behind at most `ceil(k / 4)` and that a backlogged bulk channel gets at least one chunk in every six, and the three watchdogs under paused time.
- **Leases (core).** Property tests over random interleavings of code takes, commits, aborts, turn deliveries, sends and cell cancellation: every message is delivered exactly once or recorded undelivered, order per sender is kept, capacity is never exceeded, and an aborted lease restores the queue exactly. Two concurrent code takes on one mailbox, the first stalled on reply memory and then aborted, never let the second commit a sender's later envelope before the first's earlier one (the barrier). A child with more queued progress than one reply holds is drained through its notice: `result()` returns only after the notice commits, and the model is never pinged for it.
- **Code to model regressions (core).** After code consumes a child's result, the model-facing `wait` reports it in full or as the consumed status line, never as "follows at your next turn"; `wait` without arguments does not wait for it; a turn boundary does not deliver it again.
- **Kernel against a scripted supervisor.** A Rust harness starts a real kernel through `LocalExecutor` and drives it with scripted host requests: namespace persistence, last-value repr, trimmed tracebacks, the vars report, top-level `await`, output markers with prints, C-level writes and subprocess output, a 100 MB output flood (host memory bounded by the capture caps), user code closing fd 1, interrupt during a blocking wait and during a busy loop, a cell that swallows `Cancelled` (destroyed after the grace, status `timeout`, never `crashed`), crash by `os.kill(os.getpid(), 9)`, end of file (process killed and reaped, pumps closed), forged frames from user code (wrong scope, oversized, unknown kind), a forked child calling `kyora` (`StaleCell`), a thread from an earlier cell (`StaleCell`), and an environment without provider keys (the test sets one in kyora's environment and asserts its absence in `os.environ`).
- **Authorization.** `agent.resolve`, `agent.status`, `agent.watch` and `agent.cancel` with unknown ids, sibling ids, ancestor ids and ids from another subtree are refused; a watch on one child never yields a record about any other node.
- **Bounds.** 256 slow handlers of every kind (`agent.result` with large outcomes, `artifact.get`, MCP `tool.call` with 16 MiB results, `llm`) stay within the reply budgets, with the supervisor's allocations measured, not estimated; a kernel that stops reading is destroyed by the progress rule while the reader keeps answering; `overloaded` consumes nothing; 17 subscriptions fail; a repl configuration with `tool_output_chars` below 2,000 is refused.
- **End to end with the fake provider.** `ScriptedProvider` (`crates/providers/src/fake.rs`) answers by conversation turn and is stateless, so scripts whose responses are `python` tool calls run identically however children are scheduled. Tests assert on reconstructed trees and sets of records, never on interleaving (D19), and gate children on a blocking provider, as `crates/core/tests/recursion.rs` does, instead of sleeping. Cases: the three-level recursion of section 8; cell exit with live cell-owned children; a persistent child's notice at the next turn; `map` under a small `max_agents_live`; budget exhaustion in one subtree; structured results and `SchemaError`; a staged `final` discarded on error and refused early when too large; `kyora run` printing a structured root answer; two `python` calls in one assistant message sharing `python_turn_chars`.
- **MCP from code.** `call_tool` against the stdio test server used by the CLI tests (`crates/cli/tests/fixtures/mcp_server.py`).
- **Executors.** A `LoopbackExecutor` runs the real relay locally over an in-memory duplex with injectable latency, bandwidth, stalls, drops, partitions, failing destroys and a lease clock, so every failure in section 6.6 is a deterministic test. The same kernel suite runs on it. Tests against real machines are `#[ignore]`d and need explicit opt-in. Lease tests drop renewal acknowledgements and check that the fence time follows the largest expiry sent, never the last one acknowledged; destroy tests hang the adapter and check that teardown reports incomplete within 12 s and that the outcome carries `teardown_complete: false`; start tests cancel a node while its eager start is in flight and check that the machine is destroyed and the start joined.
- **Python-side test kit.** `kyora.testing.FakeHost` speaks the real protocol over a socketpair from a thread in the same interpreter and installs itself as the module's connection, with an open cell, for the duration of the `with` block, so it exercises the real client library and codec. Outside a kernel the package is imported from `crates/repl/python`, where its sources live before they are embedded:

  ```python
  from kyora.testing import FakeHost
  with FakeHost(budget=1_000_000) as host:
      host.on_llm(lambda prompt, **kw: "2026-01-02")
      host.on_spawn(match="Resolve", result={"dates": ["2026-01-02"], "unclear": []})
      host.on_spawn(match="slow", status="timeout")
      out = my_program(chunks)                 # plain kyora.* calls go to the fake
      assert host.count("llm") == len(chunks)
  ```

  kyora's own Python unit tests use it with `python3 -m unittest`, run from a Rust test so `cargo test` covers them (D19); users use it to test their programs offline.
- **Golden output.** Cell result formatting, including section budgets at the minimum `tool_output_chars` and a spent `python_turn_chars`, is checked against golden files.
- **JSON.** Raw values with 128 and 129 levels of nesting (accepted, refused) through the host's own depth check, large integers through `init`, and an answer over 1 MiB, which arrives as an artifact reference and is fetched whole.

## 8. Milestones

Each milestone is a small set of PRs that leaves `main` working.

**R0. Kernel and cells** (the smallest shippable slice).
Scope: C1 (retention, `start` and `shutdown` hooks with teardown reports, `NodeCtx::tools` and `NodeCtx::output_schema`), C2 and C6; `crates/repl`; `LocalExecutor` with confirmed destroy (kill and reap); socketpair transport, framing, handshake, token, increasing ids; the `python` tool with persistent namespace, last value, capture with markers, tracebacks, the vars report, section budgets and `python_turn_chars` (C8); cell deadline, interrupt, destroy and the status precedence of section 2.5; end of file handling; crash detection and restart notice; kernel shutdown through `Tool::shutdown` and `kill_kernels` on a second Ctrl-C; `kyora.llm`, `final` (early checks through `NodeCtx::output_schema`), `budget` (with the C6 counters), `log` and the identity constants; `kernel_*` and `cell_*` records (C7, first part); response memory and the overload path; `kyora.testing.FakeHost`; validation of the repl configuration (minimum caps); the CLI adds `python` to the root's tools, with `--no-repl` to leave it out.
Acceptance:
- `kyora run --fake-script` with a root whose first cell runs `x = [kyora.llm(f"q{i}") for i in range(3)]` and whose second runs `kyora.final({"n": len(x)})` prints `{"n":3}` and exits 0; the trace holds three leaves with `origin_cell` 1, and per-node charges sum to the session total, including a scripted failed attempt.
- A cell printing 100 MB returns a result within `tool_output_chars` and the host's capture stays within its bounds; two cells in one assistant message together stay within `python_turn_chars` plus one `[kyora]` line.
- `while True: pass` with `timeout` 1 ends `timeout` within 2 s, and the next cell runs in the same generation with its variables intact.
- A cell that swallows `kyora.Cancelled` in a loop is destroyed after the 2 s grace and ends `timeout`, not `crashed`; the next cell runs in generation 2 with the restart notice.
- `os.kill(os.getpid(), 9)` ends the cell `crashed`; no process of the old group remains; the next cell runs in a new generation.
- A forged frame or a wrong token ends only the kernel; the run continues.
- A node that ends with a live kernel writes `node_end` only after `kernel_end`, with a complete teardown report.
- A child spawned by the model with an output schema and the `python` tool gets `SchemaError` from `kyora.final` with a mismatching value, inside the cell.
- `kyora.budget()` returns the session and subtree counters.

**R1. Agents from code.**
Scope: C3 (leases, the per-mailbox barrier, draining through the notice, the `consumed` map and the revised report) and C5; `spawn` (cell-owned by default, `persistent=True`), `Agent` (`result` with `yield_after`, `status`, `done`, `cancel`), `run`, `gather`, `as_completed`, `map`, `llm_batch`, `parallel`; `kyora.aio` and top-level `await`; the cell exit sequence of section 5.4; the lease protocol of section 4.7 for `result` and `cancel`; `context` and `vars` (inline, with JSON fidelity) and the capped manifest; `agent`, `agents` and `limits` with relation checks. The minimal session artifact store also lands here, with `artifact.get` and `kyora.fetch`, because answers over 1 MiB travel as artifacts (section 3.4).
Acceptance:
- A scripted three-level recursion through Python runs end to end: the root's cell maps over 4 items, each child's cell calls `llm_batch` with 5 prompts and `run`s one grandchild; the reconstructed tree has the expected shape and per-node charges sum to the session total.
- A cell that ends with 3 running cell-owned children returns only after their `node_end` records; its result lists them; the ledger's `reserved` is 0 at that point.
- A persistent child spawned in cell 1 finishes after the cell; its notice reaches the model at the next turn boundary; the agent idles until it arrives. When cell 2 calls `result()` on it first, the model gets no notice, and a later model-facing `wait` reports it from the outcome.
- `result(yield_after=0)` on a child that has ended returns its result, including a cell-owned child whose notice was not yet staged.
- A child that sent 5 MiB of progress before ending: `result()` with the default `max_bytes` returns after two replies with every message and the outcome; the model gets no notice; a later model-facing `wait` reports the outcome from the handle (code-to-model regression).
- A reply that cannot be written (the kernel killed between lease and write) consumes nothing: the messages reach the model at the next turn boundary.
- `map` with `concurrency=8` under `max_agents_live = 4` completes without error.
- Interrupting a cell blocked in `result()` ends its cell-owned children `cancelled`.
- A thread started in cell 1 that calls `kyora.llm` during cell 2 gets `StaleCell`.
- An integer of 30 digits passed as `context` arrives unchanged in the child's kernel.

**R2. Messages, structured results, tools and events.**
Scope: C4 and the rest of C7; `send`, `receive`, `wait` (explicit targets), `pending`; `output` with JSON Schema, pydantic and `kyora.schema`; `SchemaError`; final answers from `call_tool`; `tools` and `call_tool`, MCP included; `agent.watch` with relation checks, the 16-subscription limit, snapshots and the terminal signal; `kyora.file` variables with provenance; `display`.
Acceptance:
- A child sends 3 progress messages; the parent's code receives each exactly once (`via: code`), the model sees none of them, and a message code leaves queued arrives at the next turn boundary.
- 200 messages of 10,000 characters are drained by code within one cell, beyond `delivery_chars`, each delivered once.
- `spawn(output=Model)` returns the pydantic instance in `.value`; a schema with a type list raises `SchemaError` with no `node_start` written; `final` over `message_chars` in a structured child raises `ValueTooLarge` in the cell.
- `kyora.call_tool` on the MCP test server returns its text and records `tool_call` with `origin_cell`; a tool returning a final answer stages it and does not end the agent.
- `Agent.events()` on a child that is running yields the snapshot, then `started`, `turn` and `ended`; on a child that has ended yields `ended` at once; under forced overflow yields `lagged` and still ends with `ended`; a watch on a sibling or an unknown id is refused.

**R3. Executors.**
Scope: the `Executor` and `Instance` traits; `LocalExecutor` behind them; the relay and the mux of section 6.3; `LoopbackExecutor` with fault injection; leases, destroy results and the restart rule of section 6.5; placement (`executor=`, `--executor`, inheritance) with placement-aware tool selection; workspace snapshots and their manifests; `kyora.artifact` uploads and artifact transfer to and from machines; the first machine adapter (kyora vms).
Acceptance:
- The R0 to R2 kernel suites pass on `LoopbackExecutor`.
- A partition injected mid-cell ends the cell `lost` within 35 s and calls destroy under its call timeout; with destroy failing and `Lease` fencing, the next kernel starts only after the largest expiry sent plus skew and margin, even when renewal acknowledgements were lost; with `None` fencing it is refused with the instance named; the child's outcome reports `teardown_complete: false` with `fenced_at`.
- A stdout flood over a link throttled to 1 MB/s delays no control chunk by more than one 64 KiB bulk chunk, and a control flood leaves output at least a sixth of the link.
- Cancelling a node while its eager start is in flight destroys the machine and joins the start before `node_end`.
- Spawning onto a snapshot executor with `tools=["read_file"]` fails before admission; without `tools`, the child's frozen tools exclude `shell` and the file tools.
- `kyora.file` on a file changed on the machine passes the snapshot's original content, and on a file created there raises `InvalidRequest`; `kyora.artifact` then `kyora.fetch` in another node reproduces the changed file byte for byte.
- Manually: a root on a laptop fans out 32 children with `executor="vm"`, each on its own machine, and completes; an environment dump on each machine shows no provider credential.

**R4. Hardening.**
Scope: run local kernels under the OS sandbox once it lands (D13.2); image parts in tool results once the protocol has them.
Acceptance: the kernel suite passes under the sandbox on Linux and macOS, including network denial; a displayed PNG reaches the model as an image part.

**R5. Evaluation.**
Scope: ablation switches (`--no-llm`, `--no-spawn` from code, R§5.10) that make those calls raise `LimitExceeded("disabled")`; a small evaluation comparing no REPL, REPL without sub-calls, and depths 1 to 3 (D21, M5).
Acceptance: each switch has an end-to-end test; the evaluation reports accuracy, tokens and tail latency per configuration.

## 9. Defaults pending owner confirmation

The owner has not answered these yet. The spec adopts the following defaults throughout; each is marked where it applies.

| Question | Default in this spec | Where |
|---|---|---|
| Module name | `kyora` (not `rlm`) | 3.1 |
| Default ownership of spawns from code | cell-owned; `persistent=True` opts out | 3.4, 5.2 |
| Delivery budget for code | code deliveries bypass the per-turn `delivery_chars`; a separate shared `python_turn_chars` bounds what Python output adds to one model request | 2.6, 3.7, 5.3 |
| Remote placement | only kernels move; agent loops, host tools and model traffic stay on the host | 6.2 |
| Workspaces on other machines | no automatic merging; explicit artifact transfer | 3.9, 6.4 |
| Python floor | 3.11 | 3.1, 7 |
| Results of persistent children | `result()` consumes the notice when its reply commits; no mandatory duplicate ping to the model | 3.4, 4.7, 5.3 |

Remaining questions:

1. **Artifact scope.** Session-wide read access for anyone holding a reference (proposed), or only the uploading node's ancestors and descendants?
2. **Fencing `None`.** Refuse a new kernel until the old instance is confirmed stopped (proposed), or allow it with a warning for executors that cannot confirm?
3. **`python_turn_chars`.** Is 40,000 characters the right default next to `tool_output_chars` of 20,000?

## Appendix A: prior art and the design draft

What this spec takes from the systems surveyed in research.md:

| Source | Their design | kyora |
|---|---|---|
| rlm (R§1) | `llm_query` for plain calls, `rlm_query` for child RLMs | Adopted: `kyora.llm` (leaf) and `kyora.spawn` / `run` (agent with its own kernel). |
| rlm (R§1) | 4-byte big-endian length plus JSON over a localhost TCP broker; frames read without a cap | Framing adopted; the listener is replaced by an anonymous socketpair, and frames are capped. |
| rlm (R§1) | LocalREPL runs cells in the host process | Changed: a separate process per agent. |
| rlm (R§1) | `FINAL(...)` tags, then an answer dictionary | Changed: `kyora.final`, a tool-free final reply, or `submit_result`. |
| rlm (R§1) | 20,000-character truncation of observations; worker output memory not bounded | Adopted as `tool_output_chars` (20,000), plus a per-request Python budget and bounds at capture time. |
| Prime legacy `RLMEnv` (R§2) | FIFO worker, `llm_batch` over a host HTTP endpoint | Changed: one control channel; all calls owned by the host. |
| Prime legacy `RLMEnv` (R§2) | Timeout recovery recreates the sandbox and resets REPL state | Adopted the recovery, made explicit: generations, a restart notice, and a restart only after the old instance is fenced. |
| nano-rlm (R§2) | A persistent kernel per agent | Adopted. |
| nano-rlm (R§2) | `rlm.agent.spawn(task=..., name=..., persistent=False)` returning a handle | Adopted as `kyora.spawn(task, name=..., persistent=False)`. |
| nano-rlm (R§2) | `child.result(yield_after=...)` | Name adopted; semantics defined here (`yield_after` covers running; `StillRunning`; the child continues). |
| nano-rlm (R§2) | Unix socket to a session supervisor; 1 MiB requests, 16 MiB responses | Caps adopted; a supervisor per agent on a private socketpair instead of one session socket. |
| nano-rlm (R§2) | Capability and cell-scope identifiers | Cell scope adopted with an unpredictable nonce; authority is the connection bound to one `NodeCtx`, with every node argument checked (section 4.5). |
| nano-rlm (R§2) | IPython kernel through `jupyter_client` | Changed: a stdlib kernel with no IPython or ZeroMQ dependency; top-level `await` supported through the compiler flag. |
| nano-rlm (R§2) | Concurrency at least depth; budgets checked between calls | Changed: fail-fast admission needs no such rule (D10.3); the ledger reserves before dispatch. |

Where this spec departs from the design draft:

| Draft | This spec | Why |
|---|---|---|
| D8.2: `kyora.agent` blocks and is cell-owned, `kyora.spawn` is node-owned | `spawn` is cell-owned unless `persistent=True`; `run` blocks | Ownership is one explicit flag on one call, and the default cannot leak work past a cell. |
| D8.2: synchronous API only, Python 3.9 | Sync API plus `kyora.aio` and top-level `await`; Python 3.11 | Fan-out reads naturally with asyncio; 3.11 brings `TaskGroup` and `asyncio.timeout`. |
| D8.2: no tools from code | `kyora.call_tool`, MCP included | Code can combine shell, files and MCP servers with model calls. |
| D8.3: `agent.result` awaits the handle | Results are consumed through `NodeCtx::wait` with leases committed on write | Messages and notices are delivered exactly once, by code or by the model, even when a reply cannot be delivered. |
| D9.1: protocol on stdin and stdout, fds remapped inside the kernel | A socketpair control channel; the host captures stdout and stderr | Output bounds are enforced outside user code, and the channels map one to one onto the remote mux. |
| D9.1, D9.5: 64 MiB frames in both directions | 1 MiB requests, 16 MiB responses, response memory permits; files and artifacts for larger values | Bounded memory per kernel, following nano-rlm. |
| D9.2: JSON-RPC shapes without `jsonrpc` | An explicit `kind` (`request`, `response`, `event`) | Validation by kind, and a place for streamed events. |
| D9.2: host-side `llm_batch` | A client-side window of `llm` requests | One operation fewer, same limits. |
| D13.3, D22: `ExecBackend::spawn_repl` over one byte stream | `Executor::start` returning control and output channels and an instance with renew and destroy results; a relay with a prioritized, credited mux | Control is never stuck behind output, and loss, partitions and fencing have a defined detection and outcome. |

## Appendix B: message schemas

Notation: `u32`, `u64` are JSON integers in range; `ms` is a `u64` of milliseconds; `Raw` is any JSON value, passed through as text without number conversion; `?` marks an optional field; `|` separates alternatives. Unknown fields are ignored everywhere.

Shared types:

```
Usage       = {input_tokens: u64, output_tokens: u64, cache_creation_input_tokens: u64, cache_read_input_tokens: u64}
Status      = "completed" | "max_turns" | "budget_exhausted" | "timeout" | "context_exhausted"
            | "cancelled" | "refused" | "failed" | "interrupted"
Envelope    = {id: u64, from: u32, to: u32, kind: "message" | "result" | "error" | "cancelled",
               body: string, sent_at: string (RFC 3339), spawn?: u32, status?: Status}
Answer      = {text: string} | {value: Raw}
            | {artifact: ArtifactRef, form: "text" | "value"}   // answers over 1 MiB, never cut
Teardown    = {complete: bool, fenced_at?: string (RFC 3339), note?: string}
Outcome     = {node: u32, name: string, status: Status, answer: Answer, answer_bytes: u64,
               turns: u32, usage_self: Usage, usage_subtree: Usage, teardown: Teardown}
Summary     = {node: u32, name: string, status: Status, turns: u32, usage_subtree: Usage, answer_bytes: u64}
ChildStatus = {status: Status | null, turns: u32, usage_self: Usage, usage_subtree: Usage}
FileSpec    = {path: string, format: "text" | "bytes" | "json"}          // what the kernel sends
Origin      = {executor: string, instance: string, generation: u32}
StampedFile = {path: string, format: "text" | "bytes" | "json", sha256: string, bytes: u64,
               origin: Origin}                                              // stamped by the host
ArtifactRef = {"$artifact": string ("sha256:" and 64 hex digits), name: string, bytes: u64}
VarSource   = {json: Raw} | {file: FileSpec} | {artifact: ArtifactRef}     // kernel to host
VarLoad     = {json: Raw} | {file: StampedFile} | {artifact: ArtifactRef}  // host to kernel
Var         = {name: string, type: string, size: string}
Counters    = {session: {agents_live: u32, agents_total: u32, llm_calls: u32},
               subtree: {agents_live: u32, agents_total: u32, llm_calls: u32}}
Error       = {code: string, message: string, data?: object}
```

Handshake: `hello`, `welcome` and `reject` as in section 4.2. `welcome` carries `node`, `parent`, `depth`, `generation`, `cwd`, `executor`, `instance` (the executor's instance id, shown to code; provenance is stamped by the host, not built by the kernel), `scratch`, `features`, and `limits`, which holds every maximum the client needs: `max_depth`, `max_agents_live`, `max_agents_total`, `max_llm_calls`, `budget_limit`, `message_chars`, `mailbox_capacity`, `tool_output_chars`, `python_turn_chars`, `cell_timeout_ms`, `max_cell_timeout_ms`, `request_max`, `response_max`, `delivery_max_bytes`, `outstanding`, `outstanding_bytes`, `tool_calls`, `subscriptions` (all `u64`).

Host to kernel:

```
exec        {cell: u32, scope: string, code: string, deadline_ms: ms, marker: string}
            -> {status: "ok" | "error" | "interrupted", result: string | null,
                error: {type: string, message: string, traceback: string} | null,
                vars: {new: [Var], rebound: [Var]}, wall_ms: ms}
vars.set    {name: string, source: VarLoad}             -> {type: string, size: string}
vars.list   {}                                          -> [Var]
ping        {}                                          -> {}
shutdown    {}                                          -> {}
interrupt   event {cell: u32, reason: "timeout" | "cancelled"}
agent.event event {sub: u32, type: "started" | "turn" | "tool" | "usage" | "message" | "ended" | "lagged",
                   node?: u32, parent?: u32, depth?: u32, name?: string, text?: string,
                   charged?: u64, from?: u32, to?: u32, kind?: string, chars?: u64,
                   status?: Status, usage_subtree?: Usage, dropped?: u64}
```

Kernel to host (every request also carries `cell: u32` and `scope: string`):

```
llm            {prompt: string, system?: string, model?: string, max_tokens?: u32}
               -> {text: string, node: u32, model: string, stop_reason: string,
                   usage: Usage, response_usage: Usage}
agent.spawn    {task: string, owner: "cell" | "node", name?: string, output?: object,
                vars?: {[name: string]: VarSource}, tools?: [string], model?: string,
                budget?: u64, timeout_ms?: ms, max_turns?: u32, executor?: string}
               -> {node: u32, name: string, executor: string, tools: [string]}
agent.result   {node: u32, yield_after_ms?: ms, max_bytes?: u64}
               -> {outcome: Outcome | null, more: bool, messages: [Envelope]}   // more: notice not yet reached
agent.outcome  {node: u32}                                        -> Outcome
agent.status   {node: u32}                                        -> ChildStatus
agent.cancel   {node: u32, max_bytes?: u64}
               -> {outcome: Outcome | null, more: bool, already_finished: bool, messages: [Envelope]}
agent.list     {}  -> [{node: u32, name: string, persistent: bool, cell: u32 | null, status: Status | null}]
agent.resolve  {ref: string | u32}                                -> {node: u32, relation: "child" | "descendant"}
agent.watch    {node: u32, kinds?: [string]}
               -> {sub: u32, snapshot: [{node: u32, parent: u32, depth: u32, name: string,
                                         status: Status | null, turns: u32, usage_subtree: Usage}]}
                | {ended: Outcome}
agent.unwatch  {sub: u32}                                         -> {}
msg.send       {to: string | u32, body: string}                   -> {id: u64, to: u32}
msg.receive    {yield_after_ms: ms, max_bytes?: u64}              -> {messages: [Envelope], pending: u32}
msg.wait       {agents?: [u32], timeout_ms?: ms, max_bytes?: u64}
               -> {finished: [Summary], undrained: [u32], running: [u32], messages: [Envelope]}
msg.pending    {}                                                 -> {pending: u32}
budget         {}  -> {budget: {limit: u64, used: u64, reserved: u64, remaining: u64, closed: bool},
                       counters: Counters, deadline_ms: ms, cell_deadline_ms: ms}
tool.list      {}  -> [{name: string, description: string, input_schema: object}]   // callable tools only (3.10)
tool.call      {name: string, input: object}                     -> {text: string, is_error: bool, final_staged: bool}
final          {value: Raw}                                       -> {}
artifact.put   {upload?: u32, name?: string, data: string (base64, at most 512 KiB decoded), last: bool}
               -> {upload: u32} | ArtifactRef (on the last chunk)
artifact.get   {artifact: string, offset: u64}                   -> {data: string (base64, at most 2 MiB decoded), last: bool}
log            event {level: "debug" | "info" | "warning" | "error", message: string}
display        event {bundle: {[mime: string]: string}, described: {[mime: string]: u64}}
```

Error codes. Any request can return `cancelled`, `stale_cell`, `overloaded`, `internal`, `unsupported`, `invalid_request` (malformed or out-of-range parameters, unknown or unrelated node ids) and `value_too_large` (a reply that would exceed the response cap). Operations add: `llm`: `limit_exceeded`, `budget_exceeded`, `model_error`; `agent.spawn`: `limit_exceeded`, `budget_exceeded`, `schema_error`; `agent.result`: `still_running`; `msg.send`: `mailbox_full`, `agent_finished`; `tool.call`: `limit_exceeded`, `tool_error`, `schema_error` (a final answer the tool commits fails this node's schema); `final`: `schema_error`; `agent.watch`: `limit_exceeded`.
