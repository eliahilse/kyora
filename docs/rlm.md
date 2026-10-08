# The Python RLM layer

Status: specification, not implemented. It builds on what exists on main: the runtime loop, ledger and traces ([m1-runtime.md](m1-runtime.md)), child agents with ownership (`crates/core/src/recursion.rs`, `crates/core/src/runtime.rs`), asynchronous messages ([agent-messages.md](agent-messages.md)) and the MCP client ([mcp.md](mcp.md)).

The Python layer is the part of kyora that makes it a recursive language model runtime: the model drives a persistent Python kernel through a `python` tool, and code in that kernel calls models and spawns sub-agents through a pre-imported `kyora` module. Every such call is a request to the Rust runtime, which admits, charges, traces and cancels it like any other node.

References: `R§n` is a section of [research.md](research.md); `Dn` (for example D8.2) is a section of the design draft (`docs/design.md` on the `docs/design` branch), cited the same way in m1-runtime.md. Appendix A lists what this spec adopts from prior work and where it departs from the draft.

Contents:

1. [Goals and non-goals](#1-goals-and-non-goals)
2. [Architecture](#2-architecture)
3. [The Python API](#3-the-python-api)
4. [Wire protocol](#4-wire-protocol)
5. [Mapping onto the Rust runtime](#5-mapping-onto-the-rust-runtime)
6. [Executors: kernels on other machines](#6-executors-kernels-on-other-machines)
7. [Testing](#7-testing)
8. [Milestones](#8-milestones)
9. [Open questions](#9-open-questions)
10. [Appendix A: prior art and the design draft](#appendix-a-prior-art-and-the-design-draft)

## 1. Goals and non-goals

Goals:

- **Code that orchestrates models.** A model writes Python that calls leaf completions (`kyora.llm`) and full sub-agents (`kyora.spawn`), loops over their results, and decides what to do next in code instead of in its context window.
- **Map-reduce over data.** Large inputs live as kernel variables; code splits them, fans the pieces out to leaf calls or agents with a concurrency limit, and combines the results. Only what the code prints reaches the model (R§1, R§5.6).
- **Recursion with budgets.** A child spawned from code has its own kernel and can spawn its own children, under the same tree-wide depth, agent, call and token limits the runtime already enforces before dispatch (`Ledger::admit`, `Ledger::reserve` in `crates/core/src/ledger.rs`).
- **The lifecycle rules hold from code.** A cancelled agent is cancelled with its subtree; an open-ended agent runs and its parent is pinged when it is done; an agent with an output schema finishes when it submits a valid result. These are implemented in Rust (agent-messages.md, "How an agent ends"); the Python layer maps onto them and adds none of its own.
- **Exact semantics at the boundary.** Every Python call has one wire operation, one Rust entry point, typed errors and a trace record.
- **Placement-independent kernels.** A kernel runs as a local process or on another machine through a pluggable executor, with the same protocol, so one root on a laptop can drive children that each have their own machine.
- **Deterministic tests.** The whole layer is testable with the scripted fake provider and a real Python interpreter, without network or keys; user programs are testable with a Python-side fake host.

Non-goals:

- A Jupyter replacement: no notebook format, no magics, no widgets, no rich output beyond text and described images.
- A sandbox. Locally the kernel has the user's authority, like the `shell` tool (section 2.4). Isolation comes from the executor (a machine) or from the planned OS sandbox (D13.2), not from this layer.
- Calls from processes other than the kernel. Subprocesses and `multiprocessing` workers cannot use `kyora`; they get `StaleCell`.
- Snapshotting or migrating kernel state. A crashed or lost kernel loses its variables, and the model is told (section 2.5).
- Provider access from Python. The kernel never holds credentials and never talks to a model provider directly (R§5.2).
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
                   own process group, scrubbed env        (machine executor, another host)
```

Three parts:

- **Kernel.** A `python3` process running kyora's embedded, stdlib-only boot script and `kyora` package. It executes cells in a persistent namespace and turns `kyora.*` calls into requests on its control channel.
- **Supervisor.** Rust, one per agent node that holds the `python` tool, in a new crate `crates/repl` (package `kyora-repl`, as in D3). It starts and stops the node's kernel through an executor, runs cells, captures output, and serves kernel requests by calling the node's `NodeCtx`.
- **Executor.** Starts a kernel somewhere and hands the supervisor three channels: control, stdout and stderr (section 6). `LocalExecutor` is the default.

`kyora-core` stays free of Python, as today ("Core has no dependency on built-in tools or a REPL implementation", m1-runtime.md). The `python` tool reaches the runtime only through `ToolCx::node` (`crates/core/src/tool.rs`), and the CLI adds it through the node toolset factory, next to the MCP factory it uses today (`McpToolsets` in `crates/mcp/src/server.rs`, wired in `crates/cli/src/main.rs`).

### 2.2 One kernel per agent

Each agent node gets its own kernel, started lazily on its first `python` call and kept until the node ends. Leaf `llm` nodes have none.

Why per agent:

- **State is the point.** RLM keeps the large input and intermediate results in variables across turns (R§1). A kernel per cell would lose them; serializing the namespace between cells (`dill` in rlm's Docker, Modal and Prime environments, R§1) fails on unpicklable objects and copies large contexts on every cell.
- **Ownership matches.** A kernel's lifetime equals its node's, so node shutdown (cancel and join descendants, then `node_end`, `Runtime::run_node`) is also where the kernel stops.
- **Isolation between agents.** A kernel per session (one interpreter for the whole tree) would let siblings read each other's variables, share one GIL across parallel children, and make placing a child on another machine impossible. nano-rlm also gives every agent its own kernel (R§2).

One cell runs at a time per kernel. The loop already guarantees it: tool calls of one assistant message run sequentially (`Runtime::agent` in `crates/core/src/runtime.rs`), and the `python` tool declares `Effect::Mutating`, which stays a barrier under the parallel read-only dispatch planned in D5.3.

### 2.3 The supervisor

Per node, the supervisor owns:

- the executor handle and the current kernel **generation** (1, 2, ... per node; a restart starts a new one);
- the control channel pumps: a reader task that decodes frames and dispatches each kernel request to its own task (it never awaits a handler), a writer task draining a bounded queue, and a watchdog that can kill the kernel without going through the writer (D9.3);
- the **current cell**: its id, a cancellation token, deadline, output capture buffers, staged final answer, outstanding requests, and the cell-owned children it spawned;
- the node-level table of persistent children spawned from code, and the names of the kernel's variables (for the restart notice);
- event subscriptions (section 4.6).

The supervisor holds the node's `NodeCtx` only through each `ToolCx` it receives, plus a clone kept for requests made by background threads during a cell. Every authority check stays in `NodeCtx` (section 4.5).

### 2.4 Isolation assumptions

| | Local executor (laptop) | Machine executor (for example kyora vms) |
|---|---|---|
| Process | Separate `python3` in its own process group, like the `shell` tool (`process_group(0)`, `crates/tools/src/shell.rs`). | Separate machine; the kernel runs under a relay process there. |
| Environment | Cleared, then the shell allowlist (`ENV_ALLOWLIST` in `crates/tools/src/defaults.rs`) plus `KYORA_CONTROL_FD`. Provider keys are never passed. | Only what the executor's image provides plus the same allowlist. No kyora credentials of any kind. |
| Filesystem | The user's full authority, like `shell`. The file tools' workspace confinement (m1-runtime.md, "File access") does not apply to kernel code. | The machine's own disk with a workspace snapshot (section 6.3). |
| Network | Unrestricted locally until the OS sandbox lands (D13.2, which denies the REPL network in `workspace-write`). | Whatever the executor allows. |
| Resources | The boot script lowers rlimits before user code runs: `RLIMIT_CORE` 0, `RLIMIT_NOFILE` 1024, `RLIMIT_CPU` as a backstop, `RLIMIT_AS` on Linux (default 4 GiB), soft and hard together (D13.1). | Machine size, plus the same rlimits. |
| Model access | None. Every model call is a request to the supervisor. | Same. |

The local executor is not a security boundary, and this spec does not claim one: run untrusted inputs on a machine executor or, once it exists, under the OS sandbox. What the layer does guarantee locally: no credentials in the kernel, no listening socket, a private control channel, bounded output, and termination of the kernel's process group.

### 2.5 Kernel lifecycle

```
none --first python call--> starting --hello/welcome--> ready <--> running(cell)
  ^                            |                          |             |
  |                            +------ start failure -----+-- crash ----+--> dead
  +------------ next python call starts generation + 1 <-----------------------+
ready/running --node ends--> stopping --exit or 2 s--> stopped
```

**Start.** On the node's first `python` call (or eagerly at admission on machine executors, section 6.2):

1. The executor starts `python3 -I -X utf8 <runtime>/kyora_boot.py` in the node's cwd. `<runtime>` is a private 0700 directory per kyora process into which the embedded Python files are written (D9.1). `-I` ignores `PYTHONPATH` and user site packages, so the boot script inserts its own directory first in `sys.path`, imports `kyora`, then appends the workspace so user modules stay importable.
2. The host writes a fresh 256-bit token as one line to the kernel's stdin and closes it. The boot script reads that line, then points fd 0 at `/dev/null`, so neither user code nor its subprocesses can read the token from stdin, the environment or `argv`.
3. The boot script moves the control socket from fd 3 to a high descriptor, marks it close-on-exec, registers an `os.register_at_fork` hook that closes it in forked children, applies the rlimits, and sends `hello` (section 4.2). Startup is bounded (10 s to `hello`); otherwise the kernel is killed and the cell returns a tool error.
4. The supervisor answers `welcome`, then sets the node's preloaded variables with `vars.set` (section 3.9).

**Reuse.** The namespace persists across cells. Between cells nothing of kyora's runs in the kernel, and any request then is rejected as stale (section 4.5). User threads that outlive a cell keep running but cannot act.

**Crash.** The generation ends when the process exits, the control channel closes, the kernel violates the protocol (section 4.7), or the watchdog kills it. A running cell then ends with status `crashed` and the last 4 KiB of stderr; the cell exit sequence of section 5.4 runs; a `kernel_end` record notes the reason. The next `python` call starts generation + 1, reloads the preloaded variables, and prefixes its result with one line, appended content like every tool result, so the history stays append-only (D5.2):

```
[kyora] new kernel (generation 2): the previous one crashed. Lost variables: chunks, notes, h. Reloaded: context.
```

Persistent children survive a crash (they belong to the node); code in the new generation gets their handles back with `kyora.agents()` or `kyora.agent(id)`. After 5 restarts in one node (configurable) the tool refuses with an error instead of starting another kernel.

**Shutdown.** The kernel belongs to its agent node. When the node's loop ends, whatever the status, the supervisor closes its request gate (new requests get `cancelled`), then shuts the kernel down concurrently with the runtime cancelling and joining the node's descendants: a `shutdown` request, 2 s of grace, then SIGKILL to the process group (local) or the executor's destroy (machine). `node_end` is written only after both finish. This needs a tool shutdown hook in core (change C1, section 5.6). A second Ctrl-C kills every kernel group immediately, like shell and MCP groups today: `watch_interrupts` in `crates/cli/src/main.rs` calls `kyora_tools::cancel_processes` and `kyora_mcp::kill_servers`, and gains `kyora_repl::kill_kernels`.

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

The tool sets `ToolSpec::large_input`, so providers stream the code eagerly (`crates/protocol/src/lib.rs`), declares `Effect::Mutating` (code can change the workspace, so a cancelled cell is awaited until it has really stopped, `Runtime::agent`), and keeps the default `Tool::truncated() == true`.

**Execution.** The kernel parses the code with `ast` and compiles it with `PyCF_ALLOW_TOP_LEVEL_AWAIT`, so cells may `await`. If the last statement is an expression, it is evaluated separately and its value is the cell's result, also bound to `_`. A coroutine code object runs on the kernel's event loop, which persists across cells.

**Deadline.** `min(now + timeout, NodeCtx::deadline)`, where `timeout` defaults to `Limits::cell_timeout` (1,800 s) and is capped at `Limits::max_cell_timeout` (7,200 s) (`crates/core/src/defaults.rs`). On the deadline, or when the node is cancelled, the supervisor cancels the cell token, then sends `interrupt`; the kernel raises `kyora.Cancelled` in the main thread. If the kernel has not returned the cell 2 s later, the watchdog kills it and the generation ends.

**Output capture.** The host captures the kernel's stdout and stderr pipes itself, so output from prints, C extensions and subprocesses is bounded outside user code. Per cell and stream the host keeps the first 64 KiB and the last 64 KiB and counts what it drops. At the end of a cell the kernel flushes `sys.stdout` and `sys.stderr` and writes a per-cell random marker (sent in `exec`) to fds 1 and 2; the host strips it and closes the cell's capture when it has seen it on both streams, or 1 s after the cell result if user code closed or redirected a descriptor. Output arriving after the marker belongs to the next cell as `[background output]`, bounded the same way. Display data and logs travel on the control channel (section 4.4).

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
[kyora] cell 3 error after 41.2 s: 412 llm calls, 4 agents (2 completed, 1 failed, 1 cancelled at cell end: #11), 1 tool call; 9.8M tokens; 8.3M of 20M left
```

- `[result]` is a bounded repr (at most 8 KiB, computed in the kernel with `reprlib`-style limits).
- `[error]` is the traceback trimmed to frames from `<cell n>` files, with the exception chain.
- `[vars]` lists names bound for the first time or rebound to a new object, with type and size; in-place mutation is not detected.
- `[kyora]` is always present: status, wall time, what the cell started, which cell-owned children were cancelled when it ended, and the node's budget.

**Truncation.** The formatter fits the whole text into `Limits::tool_output_chars` (default 20,000) before returning it: the `[kyora]` line first, `[error]` up to a quarter of the cap, `[result]` up to an eighth, and the remainder shared by stdout, stderr and display, each cut with `kyora_core::tool::truncate` (head and tail around `[... N characters omitted ...]`). The runtime's own cut in `Runtime::agent` is then a no-op backstop.

**Status.** `ok`, `error` (an exception escaped), `timeout`, `interrupted` (the node was cancelled), `crashed`, `lost` (the executor lost the machine, section 6.4). Every status except `ok` sets `is_error`.

**Final answer.** If the cell staged `kyora.final(value)` and ended `ok`, the tool returns `ToolOutput::final_answer` (`Answer::Text` for a string when the node has no output schema, `Answer::Value` with the raw JSON otherwise) and the text `[kyora] final answer recorded`. The runtime then applies its existing rules: the first committed answer of a turn wins, and a node with an output schema accepts only a matching JSON object (`Runtime::accept`). A cell that fails discards its staged answer (D8.2).

## 3. The Python API

### 3.1 Conventions

- The module is `kyora`, pre-imported in every cell. Its async mirror is `kyora.aio`. The name avoids `rlm`, which the paper authors' `rlms` package uses as its import name (R§1), so that package stays usable inside the kernel.
- Stdlib only; Python 3.9 or newer (README: "Python 3.9+ for the REPL tests"). `pydantic` is used when installed, never required. Signatures below use modern annotations for readability.
- Values crossing the boundary are JSON: encoded with `json.dumps(..., allow_nan=False)`, tuples become lists, other objects raise `TypeError` in the caller. The host passes user values as raw JSON without converting numbers (D8.2).
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
kyora.workspace: pathlib.Path   # the node's cwd
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
    usage: Usage

class Usage:
    input_tokens: int; output_tokens: int
    cache_creation_input_tokens: int; cache_read_input_tokens: int
    total: int                  # all four, the ledger's budget unit (Usage::total)
```

`llm` is one completion without tools: a leaf node at the caller's depth, allowed at every depth including `max_depth`, consuming the `llm_calls` counter but no agent slot (`NodeCtx::llm`). The default model is the runtime's leaf model (`RuntimeConfig::llm_model`, default `anthropic/claude-sonnet-5-5`). `max_tokens` is capped by `Limits::llm_max_output_tokens`.

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
    messages: list[Message]  # the child's unread messages taken together with its result
    already_finished: bool   # set by cancel(): the child had ended before the cancellation

class AgentStatus:         # ChildStatus in recursion.rs
    status: str | None     # None while running or shutting down
    turns: int; usage_self: Usage; usage_subtree: Usage

class AgentInfo:
    id: int; name: str; persistent: bool; cell: int | None; status: str | None
```

**Spawn** admits the child at once or fails at once (`NodeCtx::spawn_agent` never queues). It returns before the child's first model request.

**Ownership.** By default a child is **cell-owned** (`Owner::Cell`): it belongs to the code that started it, posts no notice to the parent's mailbox, and is cancelled with its subtree when the cell ends (section 5.4). With `persistent=True` it is **node-owned** (`Owner::Node`): it outlives the cell, and when it ends its result, error or cancellation arrives in the agent's mailbox, so the model is pinged at a turn boundary; if the model ends its turn while it runs, the agent waits for it (agent-messages.md, "Idle agents"). Persistent children are still cancelled when their agent ends. Cell-owned is the default because the usual fan-out consumes its results in the same cell, and a forgotten handle then cannot leak work past the cell.

**Arguments.** `task` is the child's first user message. `tools` selects a subset of this agent's tools; omitted, the child gets `defaults::SUBAGENT_TOOLS` intersected with this agent's, which includes `python` (`crates/core/src/defaults.rs`). `model` defaults to this agent's model. `budget` is a subtree token budget, bounded by every ancestor. `timeout` (seconds) is the child's own deadline, capped by this agent's. `max_turns` defaults to `Limits::subagent_max_turns` (50). `context` preloads the variable `context` in the child's kernel; `vars` preloads several (section 3.9). `executor` names where the child's kernel runs (section 6.2). `output` makes it a structured task (section 3.5).

**`result(yield_after=None)`** waits for the child and returns its `AgentResult` if it completed. It takes the child's unread messages and its terminal notice from the mailbox in arrival order, so neither reaches the model again; they are on `.messages`. Non-completed endings raise `AgentFailed` (`AgentCancelled` for `cancelled`), with the result on `.result`. With `yield_after`, it waits at most that many seconds; if the child is still running it raises `StillRunning` and the child keeps going. Calling it again after the child ended returns the same outcome; messages are taken only once.

**`status()`** never blocks and takes nothing. **`done()`** is `status().status is not None`.

**`cancel()`** cancels the child together with its subtree and returns once it has stopped, with its outcome; cancelling a child that already ended only reports it (`already_finished`). It does not raise for the `cancelled` status, since the caller asked for it. It also works on deeper descendants obtained with `kyora.agent(id)`; `result` and `status` work on direct children only.

**Handles across cells.** A handle is a node id plus local state; it stays usable in later cells. A cell-owned child has ended by then (its cell cancelled and joined it), so `result()` returns its final outcome. `kyora.agent(ref)` builds a handle for any child, including ones the model started with the `spawn_agent` tool.

### 3.5 Structured output

```python
def schema(fields: Mapping[str, Any]) -> dict          # JSON Schema helper

spawn(task, output={"type": "object", ...})            # JSON Schema of type object
spawn(task, output=Dates)                              # a pydantic v2 BaseModel subclass
spawn(task, output=kyora.schema({"dates": [str], "count": int, "note": kyora.optional(str)}))
```

`kyora.schema` maps `str`, `int`, `float`, `bool`, `None`, `[T]`, nested mappings, `kyora.optional(T)` and `kyora.enum(*values)` to the subset the runtime enforces: single `type` names, `enum`, `required`, `properties`, `additionalProperties: false` and `items` (`tool::validate`, `tool::check_schema` in `crates/core/src/tool.rs`). Optional fields are left out of `required`. For a pydantic model the schema is `model_json_schema()`; keywords outside the subset (`$ref`, `anyOf`, `format`) are passed to the model but not enforced by the runtime.

The child gets a `submit_result` tool with that schema and finishes when it submits a valid object; its running children are cancelled; a child that ends without submitting is reminded once and then fails (agent-messages.md, "Structured results"). A child whose own code calls `kyora.final(obj)` commits the same way, through `Runtime::accept`.

On the parent side, `result().value` is the parsed object, or the pydantic instance after `model_validate`. A value that passed the runtime's subset but fails pydantic validation raises `SchemaError` with `.value` (the raw object) and `.errors`. A schema the runtime refuses raises `SchemaError` from `spawn`, before anything is admitted.

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
         timeout: float | None = None) -> Waited
def pending() -> int

class Message:              # Envelope in crates/core/src/messages.rs
    id: int; sender: int; to: int
    kind: str               # message, result, error, cancelled
    body: str; sent_at: datetime.datetime
    spawn: int | None; status: str | None

class Waited:
    finished: list[AgentResult]   # in node id order
    running: list[int]
    messages: list[Message]       # what the finished children had queued, in arrival order
```

These map onto `NodeCtx::send`, `receive`, `wait` and `pending_messages`, with the same addressing (parent, children, siblings), the same mailbox capacity and body cap, and the same errors (`MailboxFull`, `AgentFinished`, `InvalidRequest`).

What differs from the model-facing tools is the **delivery budget**. The per-turn `Limits::delivery_chars` exists to bound what enters the model's context. Messages taken by code go into Python variables, not into the conversation; what code prints is bounded by the cell output cap. So code deliveries are not charged against the turn's budget: `receive` takes whole messages in arrival order up to `max_bytes` of encoded messages (default and maximum: what fits one response frame, section 4.7), and `wait` and `result()` take a finished child's queued messages the same way. They are recorded as delivered `via: code` (change C3). Messages code does not take stay queued and reach the model at the next turn boundary under the turn budget, exactly as today. `pending()` reports how many wait.

### 3.8 Budgets and limits

```python
def budget() -> Budget
class Budget:               # BudgetSnapshot in crates/core/src/ledger.rs
    limit: int; used: int; reserved: int; remaining: int; closed: bool

def limits() -> Limits
class Limits:
    depth: int; max_depth: int
    agents_live: int; max_agents_live: int
    agents_total: int; max_agents_total: int
    llm_calls: int; max_llm_calls: int
    deadline: float          # node deadline, seconds since the epoch
    cell_deadline: float
```

`budget()` is this node's scope: its own limit, and what it and its descendants have used and reserved. Per-call limits are arguments: `budget`, `timeout` and `max_turns` on `spawn`, `max_tokens` on `llm`. Python cannot raise any limit; a child's budget and deadline are bounded by its ancestors (`Ledger::admit`, `NodeCtx::spawn_agent`).

### 3.9 Files, workspace and preloaded variables

Kernel code reads and writes files with plain Python; `kyora.workspace` is the node's cwd, which children share on the local executor (siblings writing the same file race, as D5.3 notes for the file tools). On a machine executor the workspace is that machine's copy (section 6.3).

Preloading passes data into a child's kernel without it entering any prompt:

```python
kyora.spawn("Find every date.", context=chunk)                    # inline JSON value
kyora.spawn("Summarize.", vars={"doc": kyora.file("data/a.txt"),   # workspace file
                                "meta": {"source": "a"}})
def file(path: str, *, format: str = "text") -> FileRef            # "text", "bytes" or "json"
```

- Inline values must fit the request frame (1 MiB encoded, section 4.7); larger ones raise `ValueTooLarge` locally, with a hint to write a file and pass `kyora.file(path)`.
- For a `kyora.file` reference the supervisor opens the path relative to this node's workspace with the file tools' confinement rules, hashes it, and records a `var_loaded` record (path, sha256, bytes). The child kernel loads the file itself and verifies the hash; a file changed in between fails the load, which the child's first cell reports.
- The child's first user message gets a manifest through `ChildSpec::preamble`: name, type, size and a 200-character preview per variable, never the values (D8.2). The values travel in `ChildSpec::init` (`{"kyora": {"vars": ...}}`), which the runtime hands to the child's toolset factory untouched (`NodeInfo::init`; tested by `factory_receives_opaque_init_selection_model_and_preamble` in `crates/core/tests/recursion.rs`).
- A child without the `python` tool cannot hold variables: `context` or `vars` then raise `InvalidRequest` before admission.
- The root's variables come from `kyora run --context-file PATH` and `--var NAME=@PATH|NAME=VALUE` (D16), which the CLI hands to the python toolset for node 0.

### 3.10 Tools

```python
def tools() -> list[str]                                   # tools this code may call
def call_tool(name: str, /, **input: Any) -> str           # raises ToolError on an error result
```

`call_tool` runs one of this agent's frozen tools, including MCP tools (`mcp__<server>__<tool>`, mcp.md), with the same input validation and cancellation as a model call, through `NodeCtx::call_tool` (change C4). The result text is not cut to `tool_output_chars`, since it goes to a variable; it is bounded by the response frame. Excluded: `python` (a cell cannot run a cell), `submit_result` (use `kyora.final`) and the agent tools (`spawn_agent`, `send_message`, `receive`, `wait`, `cancel_agent`), which the native API covers. At most 8 tool calls per kernel run at once. A mutating tool that has started runs to completion even if the cell is cancelled (`Effect::Mutating`).

### 3.11 Final answers, logs and display

```python
def final(value: Any) -> None       # stage this agent's answer; committed if the cell ends ok
def log(message: str, *, level: str = "info") -> None    # trace and UI only, never the model
def display(*objects: Any) -> None  # also injected as the builtin display()
```

`final` replaces an answer staged earlier in the same cell. When this node has an output schema, the value is checked at once against it (the `submit_result` spec in the node's toolset), and a mismatch raises `SchemaError` in the calling code instead of failing later. For the root, the committed value is what `kyora run` prints (strings as-is, other values as JSON).

`display` renders `text/markdown` (from `_repr_markdown_`) or `text/plain` (a bounded repr) into the cell's `[display]` section. Image bundles (`_repr_png_`, `_repr_jpeg_`) are described by type and size until tool results can carry images (`ToolResultPart` has only `Text` today, `crates/protocol/src/lib.rs`).

### 3.12 Errors

```
KyoraError(Exception)
    LimitExceeded          .limit
    BudgetExceeded
    InvalidRequest
        SchemaError        .value, .errors
    ValueTooLarge
    ModelError             .status
    AgentFailed            .result, .status
        AgentCancelled
    StillRunning(KyoraError, TimeoutError)   .agents
    MailboxFull            .agent
    AgentFinished          .agent
    ToolError              .text
    StaleCell
Cancelled(asyncio.CancelledError)            # a BaseException
```

| Exception | Wire code | Raised when | Rust origin |
|---|---|---|---|
| `LimitExceeded` | `limit_exceeded`, `data.limit` | Admission refused, nothing started. `depth`, `agents_live`, `agents_total`, `llm_calls` come from the ledger; `outstanding` and `tool_calls` from the supervisor (section 4.7). | `RecursionError::LimitExceeded`, `Ledger::admit` |
| `BudgetExceeded` | `budget_exceeded` | No headroom on this node's scope or an ancestor at admission, or a leaf call's reservation failed. | `RecursionError::BudgetExceeded` |
| `InvalidRequest` | `invalid_request` | Bad arguments, an unknown or unrelated address, a tool not held, an unknown model. | `RecursionError::InvalidRequest` |
| `SchemaError` | `schema_error` | An output schema refused at spawn, a result failing pydantic validation, or a `final` value failing this node's schema. | `tool::check_schema`, `tool::validate` |
| `ValueTooLarge` | `value_too_large` | An encoded request over 1 MiB (raised locally, nothing sent) or a response over 16 MiB. | supervisor |
| `ModelError` | `model_error` | A leaf call failed after retries, was refused, or ended without completing. | `RecursionError::ModelError` |
| `AgentFailed` | none (from the outcome) | `result()` on a child that ended `max_turns`, `budget_exhausted`, `timeout`, `context_exhausted`, `refused`, `failed` or `interrupted`. | `Status` |
| `AgentCancelled` | none | `result()` on a child that ended `cancelled` while this cell lives. | `Status::Cancelled` |
| `StillRunning` | `still_running` | `yield_after` passed; the children keep running. | `Waited::running` |
| `MailboxFull` | `mailbox_full` | The recipient's mailbox is at capacity. | `RecursionError::MailboxFull` |
| `AgentFinished` | `agent_finished` | The recipient has ended. | `RecursionError::AgentFinished` |
| `ToolError` | `tool_error` | The tool returned an error result. | `ToolOutput::is_error` |
| `StaleCell` | `stale_cell` | A call from a thread or task that outlived its cell, from outside any cell, or from another process. | supervisor, kernel |
| `Cancelled` | `cancelled` | This cell was interrupted, timed out, or its node was cancelled. | `RecursionError::Cancelled` |

Three distinctions matter for code that recovers:

- A child's own deadline ends the child with status `timeout`, which is `AgentFailed`; the caller's `yield_after` passing is `StillRunning`, and the child goes on; the cell's deadline is `Cancelled`.
- From Python, running out of budget before a call is the recoverable `BudgetExceeded`, so code can wrap up; a child that runs out mid-task ends `budget_exhausted` (D8.2).
- `Cancelled` derives from `asyncio.CancelledError`, so it is not caught by `except Exception`, and one `except` clause covers the sync and async paths. Code should let it propagate.

Unexpected supervisor failures arrive as `internal` and raise `KyoraError`.

### 3.13 Threads, tasks and cell scope

Every request carries the cell that issued it. The kernel keeps the scope in a `contextvars.ContextVar` set before each cell. Tasks created with `asyncio` copy the context natively; the boot script makes `threading.Thread` and `ThreadPoolExecutor.submit` (and so `kyora.parallel`) copy it too, since threads do not inherit context variables before Python 3.14 (D9.3). A thread started in cell 3 therefore keeps cell 3's scope while cell 4 runs, and its calls are rejected with `StaleCell`. Calls without a scope, and calls from a process whose pid is not the kernel's (a forked child), raise `StaleCell` locally. Async tasks still pending when a cell's code finishes are cancelled and awaited (within the 2 s grace) before the cell ends.

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

## 4. Wire protocol

### 4.1 Transport and framing

The kernel and its supervisor talk over one connected, private stream: the **control channel**. stdout and stderr are separate byte streams captured by the host (section 2.6).

- **Local.** `socketpair(AF_UNIX, SOCK_STREAM)`. The kernel's end is mapped to fd 3 in the child by a `dup2` in `pre_exec` (the one `unsafe` block in `kyora-repl`, a local exception to `unsafe_code = "deny"` in the workspace `Cargo.toml`, as D3 grants the sandbox crate). The host's end, and every other descriptor kyora opens, stay close-on-exec, so no other process kyora spawns (shell commands, MCP servers, other kernels) inherits either end.
- **Remote.** The same frames inside a multiplexed stream to a relay on the other machine (section 6.3).

There is no named socket and no listener anywhere. A future transport that needs a filesystem socket must place it in a 0700 directory with mode 0600, accept exactly one peer whose credentials (`SO_PEERCRED`, `getpeereid`) match the expected uid and pid, and unlink it before `hello`.

**Frames.** A 4-byte big-endian unsigned length, then that many bytes of UTF-8 JSON holding one object (the framing of the rlm broker and nano-rlm, R§1, R§2). Length 0, a length above the direction's cap (section 4.7), invalid UTF-8, or a body that is not a JSON object is a protocol violation. Ids are positive integers below 2^53.

### 4.2 Handshake and versioning

The first frame from the kernel must be `hello`, within 10 s of start:

```json
{"kind": "hello", "protocol": [1], "token": "q3Zk...", "pid": 4242,
 "python": "3.12.4", "platform": "linux", "features": ["aio", "display"]}
```

The supervisor compares the token in constant time with the one it generated for this generation, and picks the highest protocol version both list. Then:

```json
{"kind": "welcome", "protocol": 1, "node": 4, "parent": 0, "depth": 1, "max_depth": 2,
 "generation": 1, "cwd": "/work",
 "limits": {"request_max": 1048576, "response_max": 16777216, "outstanding": 256, "tool_calls": 8},
 "features": ["messages", "tools", "events", "output", "files"]}
```

or `{"kind": "reject", "code": "bad_token" | "unsupported_version", "message": "..."}` followed by a kill. Nothing else is accepted before `welcome`. The token is never logged, traced or shown to user code; it is dropped from kernel memory after `hello`.

Versioning: the version is an integer, bumped only for incompatible changes. Additions (new optional fields, new operations announced in `features`) keep it. Both sides ignore unknown fields. An unknown operation gets an `unsupported` error response and is not a violation. The kernel's Python files are embedded in the kyora binary, so a local kernel always matches; the handshake matters for relays and machine images that cache older files.

### 4.3 Envelope

```json
{"kind": "request",  "id": 17, "op": "agent.spawn", "cell": 4, "params": {...}}
{"kind": "response", "id": 17, "result": {...}}
{"kind": "response", "id": 17, "error": {"code": "limit_exceeded",
                                         "message": "limit exceeded: agents_live",
                                         "data": {"limit": "agents_live"}}}
{"kind": "event", "op": "agent.event", "params": {...}}
```

- Both sides send requests. Each allocates its own ids, increasing, never reused within a connection; a response refers to a request the receiver sent. Since each generation is a new connection with a new token, ids never cross generations.
- Every kernel request carries `cell`. Host requests carry `cell` where it applies (`exec`, `interrupt`).
- A response has exactly one of `result` and `error`. `error.code` is one of the codes in section 3.12 or `unsupported`, `internal`; `data` is optional and code-specific.
- Events have no id and get no response.

### 4.4 Operations

Host to kernel:

| op | kind | params | result |
|---|---|---|---|
| `exec` | request | `cell`, `code`, `deadline_ms`, `marker` | `ExecResult` |
| `interrupt` | event | `cell`, `reason` (`timeout`, `cancelled`) | |
| `vars.set` | request | `name`, and `value` (JSON) or `file {path, sha256, bytes, format}` | `{type, size}` |
| `vars.list` | request | | `[{name, type, size}]` |
| `agent.event` | event | `sub`, `node`, `type`, fields (section 4.6) | |
| `ping` | request | | `{}` |
| `shutdown` | request | | `{}`, then the process exits |

```json
{"status": "ok", "result": "{'found': 37}", "error": null,
 "vars": {"new": [{"name": "notes", "type": "list", "size": "412 items"}], "rebound": []},
 "wall_ms": 41210}
```

`status` is `ok`, `error` (with `error: {type, message, traceback}`) or `interrupted`; `timeout`, `crashed` and `lost` are decided by the host.

Kernel to host (all carry `cell`):

| op | kind | params | result | Rust (section 5.1) |
|---|---|---|---|---|
| `llm` | request | `prompt`, `system?`, `model?`, `max_tokens?` | `{text, node, model, stop_reason, usage}` | `NodeCtx::llm` |
| `agent.spawn` | request | `task`, `owner` (`cell`, `node`), `name?`, `output?`, `vars?`, `tools?`, `model?`, `budget?`, `timeout_ms?`, `max_turns?`, `executor?` | `{node, name}` | `NodeCtx::spawn_agent` |
| `agent.result` | request | `node`, `yield_after_ms?` | `{outcome, messages}` | `NodeCtx::wait_with` |
| `agent.status` | request | `node` | `{status, turns, usage_self, usage_subtree}` | `AgentHandle::status`, `NodeCtx::children` |
| `agent.cancel` | request | `node` | `{outcome, already_finished, messages}` | `NodeCtx::cancel_agent_with` |
| `agent.list` | request | | `[{node, name, persistent, cell, status}]` | `NodeCtx::children` |
| `agent.resolve` | request | `ref` | `{node}` | `NodeCtx::resolve` |
| `agent.watch` | request | `node`, `kinds?` | `{sub}` | `TraceSink::subscribe` |
| `agent.unwatch` | request | `sub` | `{}` | |
| `msg.send` | request | `to`, `body` | `{id, to}` | `NodeCtx::resolve`, `NodeCtx::send` |
| `msg.receive` | request | `yield_after_ms`, `max_bytes` | `{messages, pending}` | `NodeCtx::receive_with` |
| `msg.wait` | request | `agents?`, `timeout_ms?` | `{finished, running, messages}` | `NodeCtx::wait_with` |
| `budget` | request | | `{budget, counters, deadline_ms, cell_deadline_ms}` | `NodeCtx::budget`, `Ledger::counters` |
| `tool.list` | request | | `[{name, description, input_schema}]` | `NodeCtx::tools` |
| `tool.call` | request | `name`, `input` | `{text, is_error}` | `NodeCtx::call_tool` |
| `final` | request | `value` | `{}` | staged on the cell |
| `log` | event | `level`, `message` | | `TraceEvent::Log` |
| `display` | event | `bundle {mime: text}`, `described {mime: bytes}` | | cell buffer |

`outcome` is `AgentOutcome` as serialized in `node_end` records (`node`, `status`, `answer` as `{"text": ...}` or `{"value": ...}`, `usage_self`, `usage_subtree`, `turns`) plus `name`. `messages` are `Envelope`s. A still-running child answers `agent.result` with error `still_running` and `data.running`.

### 4.5 Cell scope and capabilities

- **The connection is the capability.** It is bound at `welcome` to one node, chosen by the host. No operation names a node to act as; every request is served through that node's `NodeCtx`.
- **Node ids are arguments, not authority.** Every id a request carries is checked by the `NodeCtx` method it reaches: `send` accepts the parent, children and siblings (`NodeCtx::kin`), `wait` only direct children, `cancel_agent` only descendants, `spawn_agent` only tools the node holds (all in `crates/core/src/runtime.rs`). The supervisor never resolves or touches a node outside those methods. A kernel can therefore act only for its own agent and its descendants, and message only the agents its node may message.
- **Cell scope.** A request is served only if its `cell` is the cell running on this connection; otherwise it gets `stale_cell`, and between cells every request does. The scope concerns the request, not the handle: a later cell may read the outcome of a child spawned by an earlier one.
- **Cell token.** Each cell has a `CancellationToken`, a child of the tool call's `ToolCx::cancel`. Leaf calls and cell-owned children of the cell are owned by it (`NodeCtx::llm`'s `owner`, `Owner::Cell`), so node cancellation reaches them through the token tree, and cell exit through the token itself.
- **Compared with nano-rlm.** nano-rlm uses capability and cell-scope identifiers on a session-wide socket (R§2). kyora keeps cell scope and replaces per-handle capability ids with a per-agent connection, because `NodeCtx` already enforces the relationship of every node an operation touches.

### 4.6 Child events

`agent.watch` subscribes to a child's subtree. The supervisor filters the runtime's live trace (`TraceSink::subscribe`) and pushes `agent.event` frames:

| `type` | From | Fields |
|---|---|---|
| `started` | `node_start` | `node`, `parent`, `depth`, `name` |
| `turn` | `message` (assistant) | `node`, `text` (first 500 characters of visible text) |
| `tool` | `tool_call` | `node`, `name` |
| `usage` | `attempt_end` | `node`, `charged` |
| `message` | `message_sent` | `from`, `to`, `kind`, `chars` |
| `ended` | `node_end` | `node`, `status`, `usage_subtree` |
| `lagged` | supervisor | `dropped` |

Events are observability, not delivery: results and messages reach code exactly once through their own operations. Streaming deltas are never forwarded. When the broadcast lags (`TRACE_CAPACITY`, 4096, `crates/core/src/defaults.rs`) or the subscription's queue (256 events) is full, events are dropped and a `lagged` event says how many. A subscription ends with the watched node's `ended`, with `agent.unwatch`, or with its cell. In Python, `Agent.events()` is a generator over them that ends after `ended`.

### 4.7 Bounds and backpressure

| Bound | Default | On violation |
|---|---|---|
| `hello` after start | 10 s | kill; tool error |
| frame body after its length prefix | 30 s | kill |
| kernel to host frame | 1 MiB | kill (the Python side measures first and raises `ValueTooLarge` locally, so only a broken kernel gets here) |
| host to kernel frame | 16 MiB | that request fails with `value_too_large`; the frame is never sent |
| outstanding kernel requests | 256 | `limit_exceeded` (`outstanding`) |
| bytes of outstanding kernel requests | 64 MiB | `limit_exceeded` (`outstanding`) |
| concurrent `tool.call` | 8 | `limit_exceeded` (`tool_calls`) |
| host writer queue per kernel | 32 MiB | responses wait; events are dropped (`lagged`); kill if the kernel has read nothing for 30 s |
| events queued per subscription | 256 | dropped, `lagged` |
| code delivery (`msg.receive`, `msg.wait`, `agent.result`, `agent.cancel`) | what fits 16 MiB | whole messages only; the rest stays queued |
| captured stdout, stderr, background output | 64 KiB head and 64 KiB tail each, per cell | dropped bytes counted |
| display items per cell | 32, at most 64 KiB of text each | omitted with a count |
| `log` events | 4 KiB each, 100 per cell | cut or dropped with a count |
| graceful shutdown | 2 s | kill the process group |
| kernel restarts per node | 5 | the tool refuses |

Responses are size-checked before they are queued, never after: an `llm` text, an agent answer or a tool result that would exceed the response cap fails that one request with `value_too_large`; a code delivery stops at the last whole message that fits.

The kernel's reader thread only decodes frames and routes them, so it always drains the channel; host writes stall only when the kernel process is stopped or wedged, and the 30 s rule ends that. On the host, the reader task never awaits a handler (D9.3).

**Violations.** A malformed envelope (bad frame, not JSON, unknown `kind`, missing `id`, a response to an id never sent, a second response to one id, a request before `welcome`) kills the kernel: the cell ends `crashed` with `protocol violation: <reason>`, the run goes on. A well-formed request with bad arguments gets `invalid_request` and the kernel keeps running (D9.5).

### 4.8 Security

- No network listener and no named socket; the control channel is an anonymous socketpair (section 4.1).
- A per-generation token authenticates the kernel on every transport, delivered over the stdin pipe and gone from the process environment, arguments and descriptors before user code runs.
- The kernel holds no credentials: a cleared environment with an allowlist, no provider keys (section 2.4). All model traffic goes through the host's providers.
- Authority is the bound `NodeCtx` (section 4.5); request parameters cannot raise a limit, reach a node outside the node's relationships, or grant a tool the node does not hold (capability attenuation, D18).
- Every frame, queue and request count is bounded (section 4.7); malformed traffic kills only that kernel.
- Child answers and messages arrive in Python variables as data. They reach the parent model only through printed, bounded output (D18, "Injection via sub-agent results").
- Python path and executor settings are security settings: read only from the user's config, environment and flags, never from a project file (D17, trust classes).
- Forked children of the kernel lose the control socket (`os.register_at_fork`), and calls from any pid other than the kernel's raise `StaleCell`.

## 5. Mapping onto the Rust runtime

### 5.1 Call mapping

| Python | Rust | Notes |
|---|---|---|
| a cell | `Tool::call` of the `python` tool, with `ToolCx` | One cell per call; result through `ToolOutput`; final answer through `ToolOutput::final_answer` and `Runtime::accept`. |
| `llm(prompt, ...)` | `NodeCtx::llm(LlmCall { prompt, system, model, max_tokens, origin_cell, .. }, &cell_token)` | Leaf node at the caller's depth; consumes `llm_calls`; reservations per attempt. `RecursionError` maps per section 3.12; a non-completed leaf is `ModelError`. |
| `llm_batch` | one `NodeCtx::llm` per item | Window of `concurrency` in the kernel. |
| `spawn(...)` | `NodeCtx::spawn_agent(ChildSpec { task, name, model, tools, max_turns, budget, timeout, init, preamble, origin_cell, output }, owner)` | `owner` is `Owner::Cell(cell_token)` or, with `persistent=True`, `Owner::Node`. `init` carries variables and the executor; `preamble` the manifest. |
| `Agent.result(yield_after)` | `NodeCtx::wait_with(Some(&[id]), yield_after, Take::Code { .. })` | Takes the child's queued messages and notice; for a cell-owned child the runtime stages its notice first (`NodeCtx::stage_notice`). |
| `Agent.status()` | `AgentHandle::status` or `NodeCtx::children` | Reads only. |
| `Agent.cancel()` | `NodeCtx::cancel_agent_with(id, Take::Code { .. })` | Cancels the subtree and returns once it stopped; for a direct child, takes its notice. |
| `gather`, `as_completed` | one `agent.result` per handle | Concurrent requests; no new Rust API. |
| `map` | `NodeCtx::spawn_agent` per item, `agent.result` per child | Retries `LimitExceeded("agents_live")` only while its own children run. |
| `agent(ref)`, `agents()` | `NodeCtx::resolve`, `NodeCtx::children` | |
| `send(to, body)` | `NodeCtx::resolve`, then `NodeCtx::send` | Same capacity, cap and errors as the tool. |
| `receive(...)` | `NodeCtx::receive_with(yield_after, Take::Code { .. })` | |
| `wait(...)` | `NodeCtx::wait_with(agents, timeout, Take::Code { .. })` | |
| `pending()` | `NodeCtx::pending_messages` | |
| `budget()` | `NodeCtx::budget` | `BudgetSnapshot` |
| `limits()` | `Ledger::counters`, `Limits`, `NodeCtx::deadline` | |
| `tools()`, `call_tool` | `NodeCtx::tools`, `NodeCtx::call_tool` | |
| `final(value)` | staged on the cell; `ToolOutput::final_answer` | Checked early against the node's `submit_result` schema. |
| `log`, `display` | `TraceEvent::Log`; the cell's display buffer | |

Every `NodeCtx` method above already exists in `crates/core/src/runtime.rs` except the `_with` variants, `children`, `tools` and `call_tool`, which section 5.6 adds.

### 5.2 Ownership

- **Cell-owned** (`spawn` default, `run`, `map`): `Owner::Cell(cell_token)`. `NodeCtx::spawn_agent` gives the child a node token that descends from this node's token and adds the cell token as an extra owner; `Runtime::run_node`'s watcher cancels the child when the cell token is cancelled. No notice is posted to the mailbox (`notify` is false for cell owners). Rule 1 (cancelled with its subtree) holds through the token tree and the child's ordered shutdown.
- **Node-owned** (`persistent=True`): `Owner::Node`. The child's notice is posted to this agent's mailbox (`Runtime::notify`), the agent idles at `end_turn` until it arrives (`Runtime::idle`), and the model receives it at a turn boundary unless code took it first (rule 2). The child is cancelled when this agent ends (`Runtime::join_descendants`).
- **Structured** (`output=`): either ownership; the child ends on a valid submission and its running children are cancelled (rule 3, `Runtime::agent` with `settings.output`).

### 5.3 Messages and the delivery budget

The runtime resets each agent's delivery budget before every model request (`mailbox.new_turn()` in `Runtime::agent`) and charges `receive`, `wait`, `cancel_agent` and the turn boundary against it (`Mailbox::take`, `Mailbox::charge` in `crates/core/src/messages.rs`). For code:

- Code deliveries use `Take::Code` and are recorded with `via: code`: they take whole messages in arrival order up to their own byte cap, do not charge the turn's budget, and do not add a taken notice's sender to the set of outcomes the model has seen (the `seen` set kept by `Mailbox::take` and `Mailbox::saw`), so a later model-facing `wait` still reports such an outcome in full. They keep every other mailbox rule: exactly once, sender order, a child's messages before its notice, take and hand-over in one step.
- A code delivery is refused atomically, without taking anything, once its cell token is cancelled. The `_with` variants check the token in the same critical section as the take (change C3). So at cell exit, every take either happened while the cell was live, and its response is queued to the kernel ahead of any `interrupt`, or did not happen.
- Delivery to code is final once the response is queued on the connection. A kernel that dies after that loses those messages like any state in its memory; their bodies remain in the trace (`message_sent`).
- What code leaves queued reaches the model at the next turn boundary, which for a running cell is the user message carrying the cell's tool result (agent-messages.md, "Turn boundaries"), under the turn's budget as today.

### 5.4 Cell exit and node shutdown

Cell exit runs on every ending (ok, error, timeout, interrupted, crashed, lost), before the tool result is returned:

1. The cell ends: the kernel returns `ExecResult`, or the deadline or node cancellation fires (then `interrupt`, the 2 s grace, and a kill if needed), or the kernel dies.
2. The supervisor cancels the cell token. Leaf calls of the cell stop and settle (m1-runtime.md, "Accounting and ownership"); cell-owned children are cancelled with their subtrees; code deliveries stop taking (section 5.3).
3. It closes the cell gate: new requests carrying this cell get `stale_cell`.
4. It awaits every request task of the cell, each of which queues its response (or `cancelled`). Leaf calls, waits and receives end promptly once the token is cancelled; a mutating tool call that has started runs to completion, bounded by its own timeout and the node deadline.
5. It awaits `AgentHandle::result()` for every cell-owned child of the cell. Each resolves only after that child's ordered shutdown: its mailbox closed, its descendants cancelled and joined, its `node_end` written (agent-messages.md, "Termination and cancellation").
6. It ends the cell's event subscriptions, writes `cell_end`, and returns the tool result, listing the children cancelled at cell end.

Persistent children are not touched. Every wait above is bounded: cancellation reaches a provider within `PROVIDER_CANCEL_GRACE` (250 ms) and children's deadlines bound the rest.

Node shutdown, with the Python layer, runs in this order (the existing order of `Runtime::run_node` plus C1):

1. The agent loop ends; no cell is running, since cells run inside the loop.
2. The mailbox closes; queued messages are recorded as undelivered.
3. Admission closes, descendants (persistent children, and cell-owned ones of a cell that was cut short) are cancelled and joined; concurrently, the supervisor closes its gate and shuts the kernel down (section 2.5).
4. `node_end` is written and the live-agent slot is released.
5. The node's handle resolves and its notice goes to its parent.

### 5.5 Ledger and trace

The Python layer adds no accounting of its own. Leaf calls and child agents go through `Ledger::admit` and per-attempt `Ledger::reserve` and `Ledger::settle` exactly as the agent tools do, so the scope tree, the pre-dispatch guarantee and the overshoot bound of D10.2 are unchanged. Kernel CPU, memory and machine time are not budgeted; `cell_timeout`, rlimits and the executor bound them.

What each call records:

| Call | Records |
|---|---|
| kernel start and end | `kernel_start {node, generation, executor, pid, python}`, `kernel_end {node, generation, reason, exit}` (new) |
| a cell | `tool_call` (holds the code) and `tool_result` as for any tool; `cell_start {node, generation, cell, call}` and `cell_end {node, cell, status, wall_ms, started: {llm, agents, tools}, cancelled_at_end, usage}` (new) |
| live cell output | `cell_output {node, cell, stream, text}`, ephemeral like `delta`, never persisted (new) |
| `llm` | `node_start` (`kind: llm`, `origin_cell`), `attempt_start`, `attempt_end`, `node_end` |
| `spawn` | `node_start` (`kind: agent`, `origin_cell`), then the child's own records |
| preloaded variables | `var_loaded {node, name, source: {inline, bytes} or {file, sha256, bytes}}` (new) |
| `send` | `message_sent` |
| `receive`, `wait`, `result`, `cancel` | `message_delivered` with `via: code`; `message_sent` for a staged notice of a cell-owned child |
| `call_tool` | `tool_call` and `tool_result` with `origin_cell` and call id `py:<generation>.<cell>.<n>` |
| `final` | `cell_end` notes it; `node_end` carries the answer |
| `log` | `log {node, cell, level, message}` (new) |
| `budget`, `limits`, `status`, `tools` | nothing |

`cell_end.usage` sums the outcomes of the cell's leaves and cell-owned children, which are final at that point, plus the usage so far of persistent children it spawned. Tree reconstruction (`reconstruct_tree` in `crates/core/src/trace.rs`) ignores the new records, as it ignores message records; the TUI already has cell nodes (`NodeKind::Cell`, `ReplCellStarted`, `ReplCellFinished` in `crates/tui/src/event.rs`) for `cell_start` and `cell_end` to feed.

### 5.6 Required core changes

Small, additive changes to `kyora-core`, each with its own tests:

- **C1. Tool shutdown hook.** `Tool::shutdown(&self)` (async, default no-op), called once per tool of a node's toolset after the agent loop ends, concurrently with `join_descendants`, and awaited before `node_end` in `Runtime::run_node`. Tools shared across nodes (MCP) keep the no-op.
- **C2. Leaf origin.** `LlmCall::origin_cell: Option<u32>`, recorded on the leaf's `node_start`; `NodeCtx::llm_owned` writes `origin_cell: None` today.
- **C3. Code delivery.** A `Delivery::Code` variant for the trace, and `NodeCtx::receive_with`, `wait_with` and `cancel_agent_with`, which take a `Take` argument: `Take::Turn` (today's behaviour, charged to the turn's delivery budget) or `Take::Code { max_bytes, owner }` (an independent byte cap, and an owner token checked in the same critical section as the take). The existing methods call them with `Take::Turn`.
- **C4. Tools from code.** Keep the node's frozen `Toolset` in `NodeState` (today only its names, `NodeState::tools`), and add `NodeCtx::tools()` and `NodeCtx::call_tool(name, input, cancel, origin_cell)`, which validate and execute like `Runtime::agent` and record `tool_call` and `tool_result` (both gain an optional `origin_cell`).
- **C5. Children.** `NodeCtx::children()` returning id, name, owner kind, origin cell and `ChildStatus` for each child, from the runtime's agent directory.
- **C6. Counters.** `Ledger::counters()` with live, total and leaf counts for `kyora.limits()`.
- **C7. Trace events.** `KernelStart`, `KernelEnd`, `CellStart`, `CellEnd`, `VarLoaded`, `Log` (persisted) and `CellOutput` (ephemeral) in `TraceEvent`.

No change to admission, reservation or ownership semantics is needed, and C1 adds a step to node shutdown without reordering the existing ones.

## 6. Executors: kernels on other machines

### 6.1 The executor interface

```rust
// kyora-repl
#[async_trait]
pub trait Executor: Send + Sync {
    fn name(&self) -> &str;
    fn traits(&self) -> ExecutorTraits;   // workspace: Shared | Snapshot; isolation: Process | Machine
    async fn start(&self, spec: KernelSpec, cancel: CancellationToken) -> Result<KernelLink, ExecError>;
}
pub struct KernelSpec {
    pub node: NodeId, pub generation: u32,
    pub python: String, pub cwd: PathBuf, pub env: Vec<(String, String)>,
    pub rlimits: Rlimits, pub token: Secret, pub files: Vec<FileRef>, pub lease: Duration,
}
pub struct KernelLink {
    pub control: Box<dyn Duplex>,                 // carries protocol frames
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    pub stderr: Box<dyn AsyncRead + Send + Unpin>,
    pub exited: BoxFuture<'static, ExitInfo>,     // exit status, or Lost
    pub kill: Box<dyn Fn() + Send + Sync>,        // process group kill or machine destroy
}
```

- **`LocalExecutor`**: socketpair, pipes and a process group, as in sections 2.5 and 4.1. Workspace `Shared`, isolation `Process`.
- **Machine executors**: any provider of four primitives can back one: create a machine from an image that has `python3`, open an authenticated byte stream to a process on it, upload files, and destroy it with a lease that expires on its own. kyora vms is the first adapter; containers, other microVM services or SSH hosts fit the same interface. Workspace `Snapshot`, isolation `Machine`.

This replaces D13.3's `ExecBackend::spawn_repl`: instead of one byte stream, an executor returns separate control and output channels, which lets a slow or flooded stdout never delay control frames, and gives loss detection a place to live.

### 6.2 Placement

Each node's kernel is placed by name: `kyora.spawn(..., executor="vm")`, carried in `ChildSpec::init` to the child's toolset factory; the root's comes from `kyora run --executor NAME`. A child inherits its parent's executor name; on a machine executor every node gets its own machine. Executors are defined in the user's config (`[executors.<name>]` with a `kind` naming a compiled-in adapter), never in a project file.

Only the kernel moves. A child placed on a machine still has its agent loop, its `NodeCtx`, its ledger scope, its mailbox and all its model traffic in the host process. Tools that act on the host's machine (`shell`, `read_file`, `write_file`, `edit_file`) would act on the wrong machine, so a node on a `Snapshot` executor does not get them; its code runs commands with `subprocess` and reads and writes files directly, on its own machine. MCP tools run where their servers run, on the host, and stay available.

On machine executors the kernel starts eagerly when the child is admitted, so the machine boots while the child's first model request is in flight. A start failure then surfaces as an error on the child's first `python` call, which the child's model sees; the node itself goes on.

### 6.3 What crosses the wire

Between the host and a machine there is one stream per kernel, multiplexed:

```
mux frame = u32 big-endian length of the rest | u8 channel | payload
channel 0  control: one protocol frame body (the JSON of section 4)
channel 1  stdout bytes
channel 2  stderr bytes
channel 3  relay: credit grants, ping and pong, exit report, kill request
```

The relay is a stdlib script shipped with the kernel files (`kyora_relay.py`). On the machine it starts the kernel exactly as the local executor does (socketpair on fd 3, pipes, process group, the token on stdin), forwards channels, and reports the exit status. Channels 1 and 2 use credit flow control (1 MiB windows): when credit runs out the relay stops reading the kernel's pipes, so the kernel blocks on writes as it would on a full local pipe, and control frames are never stuck behind output. Control traffic is bounded by the protocol's own limits.

| Crosses | Direction | Notes |
|---|---|---|
| Control frames, stdout, stderr | both | The same protocol as locally; the host still captures and bounds output. |
| Token | host to machine, then kernel | Inside the executor's authenticated stream; checked by the host in `hello`. |
| Kernel files | host to machine | The embedded boot, package and relay files, by content hash. |
| Workspace snapshot | host to machine | At kernel start, content-addressed so siblings reuse it. Changes made on the machine stay there; code returns what matters as results or messages. |
| `kyora.file` variables | host to machine | Included by hash in the snapshot or uploaded separately. |
| Never | | Model credentials, provider traffic, the ledger, other nodes' frames. Machines never talk to each other; the host is the hub. |

The host runs every agent loop (cheap async tasks) and every model call; machines hold Python state and compute. `max_agents_live` and `max_inflight_requests` bound the fan-out the host drives.

### 6.4 Failure modes

| Failure | Detection | Effect |
|---|---|---|
| Machine start fails or no capacity | `start` error, or no `hello` within the executor's start bound (default 120 s) | The `python` call returns a tool error; the next call tries again and counts toward the restart limit. |
| Kernel crash on the machine | relay exit report | Same as a local crash: `crashed`, a new generation on the next call. |
| Machine lost | the stream ends without an exit report, or the executor reports loss | The cell ends `lost`; cell exit runs (cell-owned children are host nodes and are cancelled); the generation ends; the next call starts a new machine with the restart notice. |
| Network partition | no frame, including pong, for 30 s (ping every 10 s) | Treated as lost. The executor is asked to destroy the machine (fencing), so a partitioned kernel stops. A new generation has a new stream and token, so nothing from the old one is accepted. |
| Slow link | credits exhausted | Output backpressure only; control frames go through. |
| Host crash | the lease is not renewed | The executor destroys the machine when the lease expires (default 2 min). On resume, nodes that were running are recorded as interrupted (D11.3). |

A partitioned kernel cannot spawn, call a model or message anyone, since all of that goes through the host. It can still have side effects of its own (writing to external systems), so the host never replays a cell: a cell that ends `lost` is reported to the model, which decides what to do.

## 7. Testing

All tests run without network or keys, as today (`cargo test --workspace --locked`, CLAUDE.md). The REPL tests need a real `python3`; CI runs them on the oldest supported (3.9) and the newest Python, on Linux and macOS (D19).

- **Codec and handshake (Rust units).** Frames split at every byte, zero and oversized lengths, invalid UTF-8, non-object bodies, a stalled body (30 s rule under paused time), bad and missing tokens, version negotiation, unknown operations, duplicate and unsolicited responses. The mux: random channel interleavings, credit exhaustion, property tests that control frames are never delayed by output.
- **Kernel against a scripted supervisor.** A Rust harness starts a real kernel through `LocalExecutor` and drives it with scripted host requests: namespace persistence, last-value repr, trimmed tracebacks, the vars report, top-level `await`, output markers with prints, C-level writes and subprocess output, a 100 MB output flood (host memory bounded by the capture caps), user code closing fd 1, interrupt during a blocking wait and during a busy loop, kill after the grace, crash by `os.kill(os.getpid(), 9)`, forged frames from user code (wrong cell, oversized, unknown kind), a forked child calling `kyora` (`StaleCell`), a thread from an earlier cell (`StaleCell`), the environment holding no provider key (the test sets one and asserts its absence in `os.environ`).
- **End to end with the fake provider.** `ScriptedProvider` (`crates/providers/src/fake.rs`) answers by conversation turn and is stateless, so scripts whose responses are `python` tool calls run identically however children are scheduled. Tests assert on reconstructed trees and sets of records, never on interleaving (D19), and gate children on a blocking provider, as `crates/core/tests/recursion.rs` does, instead of sleeping. Cases: the three-level recursion of section 8; cell exit with live cell-owned children (each `node_end` precedes `cell_end`, `reserved` is 0 after the cell); a persistent child's notice at the next turn; `map` under a small `max_agents_live`; budget exhaustion in one subtree; structured results and `SchemaError`; code delivery taking 200 messages in one cell, each delivered once; a staged `final` discarded on error; `kyora run` printing a structured root answer.
- **MCP from code.** `call_tool` against the stdio test server used by the CLI tests (`crates/cli/tests/fixtures/mcp_server.py`).
- **Executors.** A `LoopbackExecutor` runs the real relay locally over an in-memory duplex with injectable latency, bandwidth, stalls, drops and partitions, so every failure in section 6.4 is a deterministic test. The same kernel suite runs on it. Tests against real machines are `#[ignore]`d and need explicit opt-in.
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
- **Golden output.** Cell result formatting, including section budgets at small `tool_output_chars`, is checked against golden files.

## 8. Milestones

Each milestone is a small set of PRs that leaves `main` working.

**R0. Kernel and cells** (the smallest shippable slice).
Scope: `crates/repl`; `LocalExecutor`; socketpair transport, framing, handshake and token; the `python` tool with persistent namespace, last value, capture with markers, tracebacks, the vars report and section budgets; cell deadline, interrupt and kill; crash detection and restart notice; kernel shutdown at node end (C1) and `kill_kernels` on a second Ctrl-C; `kyora.llm`, `final`, `budget`, `log` and the identity constants; `kernel_*` and `cell_*` records and leaf `origin_cell` (C2, C7); `kyora.testing.FakeHost`; the CLI adds `python` to the root's tools, with `--no-repl` to leave it out.
Acceptance:
- `kyora run --fake-script` with a root whose first cell runs `x = [kyora.llm(f"q{i}") for i in range(3)]` and whose second runs `kyora.final({"n": len(x)})` prints `{"n":3}` and exits 0; the trace holds three leaves with `origin_cell` 1, and per-node charges sum to the session total.
- A cell printing 100 MB returns a result within `tool_output_chars` and the host's capture stays within its bounds.
- `while True: pass` with `timeout` 1 ends `timeout` within 2 s, and the next cell runs in the same generation with its variables intact.
- A cell that catches and ignores `kyora.Cancelled` in a loop is killed after the 2 s grace (`timeout` within 4 s); the next cell runs in generation 2 with the restart notice.
- `os.kill(os.getpid(), 9)` ends the cell `crashed`; the next cell runs in a new generation.
- A forged frame or a wrong token kills only the kernel; the run continues.
- No provider key is visible in the kernel's environment.

**R1. Agents from code.**
Scope: `spawn` (cell-owned by default, `persistent=True`), `Agent` (`result` with `yield_after`, `status`, `done`, `cancel`), `run`, `gather`, `as_completed`, `map`, `llm_batch`, `parallel`; `kyora.aio` and top-level `await`; the cell exit sequence of section 5.4; `context` and `vars` (inline) with the manifest preamble; `agent`, `agents` and `limits` (C5, C6); code delivery for `result` and `cancel` (C3).
Acceptance:
- A scripted three-level recursion through Python runs end to end: the root's cell maps over 4 items, each child's cell calls `llm_batch` with 5 prompts and `run`s one grandchild; the reconstructed tree has the expected shape and per-node charges sum to the session total.
- A cell that ends with 3 running cell-owned children returns only after their `node_end` records; its result lists them; the ledger's `reserved` is 0 at that point.
- A persistent child spawned in cell 1 finishes after the cell; its notice reaches the model at the next turn boundary; the agent idles until it arrives.
- `map` with `concurrency=8` under `max_agents_live = 4` completes without error.
- Interrupting a cell blocked in `result()` ends its cell-owned children `cancelled`.
- A thread started in cell 1 that calls `kyora.llm` during cell 2 gets `StaleCell`.

**R2. Messages, structured results, tools and events.**
Scope: `send`, `receive`, `wait`, `pending` with code delivery (C3); `output` with JSON Schema, pydantic and `kyora.schema`; `SchemaError`; `final` checks under an output schema; `tools` and `call_tool`, MCP included (C4); `agent.watch` and `Agent.events()`; `kyora.file` variables with `var_loaded`; `display`.
Acceptance:
- A child sends 3 progress messages; the parent's code receives each exactly once (`via: code`), the model sees none of them, and a message code leaves queued arrives at the next turn boundary.
- 200 messages of 10,000 characters are drained by code within one cell, beyond `delivery_chars`, each delivered once.
- `spawn(output=Model)` returns the pydantic instance in `.value`; a schema with a type list raises `SchemaError` with no `node_start` written.
- `kyora.call_tool` on the MCP test server returns its text and records `tool_call` with `origin_cell`.
- `Agent.events()` yields `started`, `turn` and `ended` for a scripted child, and `lagged` under forced overflow.

**R3. Executors.**
Scope: the `Executor` trait, `LocalExecutor` behind it, the relay and mux, `LoopbackExecutor` with fault injection, leases and heartbeats, placement (`executor=`, `--executor`, inheritance), workspace snapshots, and the first machine adapter (kyora vms).
Acceptance:
- The R0 to R2 kernel suites pass on `LoopbackExecutor`.
- A partition injected mid-cell ends the cell `lost` within 35 s, calls the executor's destroy once, and the next cell runs in a new generation that accepts nothing from the old stream.
- A stdout flood over a link throttled to 1 MB/s does not delay control frames by more than one output window.
- Manually: a root on a laptop fans out 32 children with `executor="vm"`, each on its own machine, and completes; an environment dump on each machine shows no provider credential.

**R4. Hardening.**
Scope: run local kernels under the OS sandbox once it lands (D13.2); image parts in tool results once the protocol has them; ablation switches (`--no-llm`, `--no-spawn` from code, R§5.10) that make the calls raise `LimitExceeded("disabled")`; a small evaluation comparing no REPL, REPL without sub-calls, and depths 1 to 3 (D21, M5).
Acceptance: each switch has an end-to-end test; the sandboxed kernel suite passes on Linux and macOS.

## 9. Open questions

1. **Module name.** `kyora` (proposed) or `rlm`, which shadows the `rlms` package's import name inside the kernel?
2. **Default ownership.** Spawns from code are cell-owned unless `persistent=True`. Confirm.
3. **Delivery budget.** Code deliveries bypass the per-turn `delivery_chars`, relying on the cell output cap to protect the context. Confirm.
4. **Remote placement.** Only kernels move to machines; agent loops, host tools and all model traffic stay on the host. Enough for fleet scale, or should whole subtrees later run in a remote kyora runtime?
5. **Snapshot workspaces.** Should files changed on a child's machine flow back to the parent: never (proposed for now), on explicit request, or merged automatically?
6. **Python floor.** System `python3` 3.9 or newer (proposed), or require 3.11?
7. **Results of persistent children.** `Agent.result()` takes the child's notice so the model is not pinged again (proposed). Or should the model always get the ping?

## Appendix A: prior art and the design draft

What this spec takes from the systems surveyed in research.md:

| Source | Their design | kyora |
|---|---|---|
| rlm (R§1) | `llm_query` for plain calls, `rlm_query` for child RLMs | Adopted: `kyora.llm` (leaf) and `kyora.spawn` / `run` (agent with its own kernel). |
| rlm (R§1) | 4-byte big-endian length plus JSON over a localhost TCP broker; frames read without a cap | Framing adopted; the listener is replaced by an anonymous socketpair, and frames are capped. |
| rlm (R§1) | LocalREPL runs cells in the host process | Changed: a separate process per agent. |
| rlm (R§1) | `FINAL(...)` tags, then an answer dictionary | Changed: `kyora.final`, a tool-free final reply, or `submit_result`. |
| rlm (R§1) | 20,000-character truncation of observations; worker output memory not bounded | Adopted as `tool_output_chars` (20,000), plus bounds at capture time. |
| Prime legacy `RLMEnv` (R§2) | FIFO worker, `llm_batch` over a host HTTP endpoint | Changed: one control channel; all calls owned by the host. |
| Prime legacy `RLMEnv` (R§2) | Timeout recovery recreates the sandbox and resets REPL state | Adopted the recovery, made explicit: generations and a restart notice. |
| nano-rlm (R§2) | A persistent kernel per agent | Adopted. |
| nano-rlm (R§2) | `rlm.agent.spawn(task=..., name=..., persistent=False)` returning a handle | Adopted as `kyora.spawn(task, name=..., persistent=False)`. |
| nano-rlm (R§2) | `child.result(yield_after=...)` | Name adopted; semantics defined here (`StillRunning`, the child continues). |
| nano-rlm (R§2) | Unix socket to a session supervisor; 1 MiB requests, 16 MiB responses | Caps adopted; a supervisor per agent on a private socketpair instead of one session socket. |
| nano-rlm (R§2) | Capability and cell-scope identifiers | Cell scope adopted; the capability is the connection bound to one `NodeCtx` (section 4.5). |
| nano-rlm (R§2) | IPython kernel through `jupyter_client` | Changed: a stdlib kernel with no IPython or ZeroMQ dependency; top-level `await` supported through the compiler flag. |
| nano-rlm (R§2) | Concurrency at least depth; budgets checked between calls | Changed: fail-fast admission needs no such rule (D10.3); the ledger reserves before dispatch. |

Where this spec departs from the design draft:

| Draft | This spec | Why |
|---|---|---|
| D8.2: `kyora.agent` blocks and is cell-owned, `kyora.spawn` is node-owned | `spawn` is cell-owned unless `persistent=True`; `run` blocks | Ownership is one explicit flag on one call, and the default cannot leak work past a cell. |
| D8.2: synchronous API only | Sync API plus `kyora.aio` and top-level `await` | Fan-out reads naturally with asyncio, without a thread per call. |
| D8.2: no tools from code | `kyora.call_tool`, MCP included | Code can combine shell, files and MCP servers with model calls. |
| D8.3: `agent.result` awaits the handle | Handles read results through `NodeCtx::wait` with code delivery | The child's messages and notice are consumed exactly once, by code or by the model. |
| D9.1: protocol on stdin and stdout, fds remapped inside the kernel | A socketpair control channel; the host captures stdout and stderr | Output bounds are enforced outside user code, and the channels map one to one onto the remote mux. |
| D9.1, D9.5: 64 MiB frames in both directions | 1 MiB requests, 16 MiB responses; large values as workspace files | Bounded memory per kernel, following nano-rlm. |
| D9.2: JSON-RPC shapes without `jsonrpc` | An explicit `kind` (`request`, `response`, `event`) | Validation by kind, and a place for streamed events. |
| D9.2: host-side `llm_batch` | A client-side window of `llm` requests | One operation fewer, same limits. |
| D13.3, D22: `ExecBackend::spawn_repl` over one byte stream | `Executor::start` returning control and output channels; a relay with a mux | Control is never stuck behind output, and loss and partitions have a defined detection. |
