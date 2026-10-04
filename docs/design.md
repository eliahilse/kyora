# kyora design

Status: draft for review, 2026-10-04. Background and sources: [research.md](research.md) (cited below as R§n).

kyora is an agent harness written in Rust whose distinguishing feature is a recursive runtime: the model drives a persistent Python REPL through a tool, and code in that REPL can call the model again (`kyora.llm`) or spawn full sub-agents (`kyora.agent`) that run through the same Rust core, recursively, under tree-wide limits. Large inputs live as REPL variables instead of in the prompt.

This document is technical. Product naming and user-facing copy are out of scope.

Contents:

1. [Goals and non-goals](#1-goals-and-non-goals)
2. [Architecture overview](#2-architecture-overview)
3. [Crate layout](#3-crate-layout)
4. [Core data model](#4-core-data-model)
5. [The agent loop](#5-the-agent-loop)
6. [Providers](#6-providers)
7. [Tools](#7-tools)
8. [The recursive runtime](#8-the-recursive-runtime)
9. [REPL IPC protocol](#9-repl-ipc-protocol)
10. [Limits, budgets and cancellation](#10-limits-budgets-and-cancellation)
11. [Sessions and transcripts](#11-sessions-and-transcripts)
12. [Context management and compaction](#12-context-management-and-compaction)
13. [Isolation](#13-isolation)
14. [Observability](#14-observability)
15. [TUI](#15-tui)
16. [CLI surface](#16-cli-surface)
17. [Configuration](#17-configuration)
18. [Security](#18-security)
19. [Testing](#19-testing)
20. [npm distribution](#20-npm-distribution)
21. [Milestones](#21-milestones)
22. [Later: kyora cloud and CLI unification](#22-later-kyora-cloud-and-cli-unification)
23. [Open questions](#23-open-questions)

## 1. Goals and non-goals

Goals:

- A standalone, local-first harness: `kyora run "<task>"` works on a laptop with an API key and a system `python3`, nothing else.
- The core loop, providers and tools are in Rust and usable as a library (the CLI and TUI are thin frontends).
- Recursive model calls and sub-agents from Python code, with depth, token, agent-count and concurrency limits that hold for the whole tree, deterministic cancellation, and a trace of the recursion tree.
- Anthropic Messages API first (Claude models), then OpenAI Responses and Chat-Completions-compatible endpoints including local servers.
- Deterministic tests: every behavior above is testable with fake providers and no network.
- Execution backends are pluggable so the REPL and sub-agents can later run in kyora vms (section 22). Local process execution is the only backend built now.

Non-goals for now: Windows support (the REPL process model is Unix-only until a later milestone), a plugin system, a web UI, training integration (trajectory export is noted in section 14 but not built), subscription-login auth.

## 2. Architecture overview

```
                 +--------------------------------------------------------------+
  kyora run ---> |                        kyora-core                            |
  kyora (TUI) -> |  Runtime ---- Ledger (tree budgets)    TraceSink --> JSONL    |
                 |    |                                       |      --> events  |
                 |    +-- AgentNode #0 (root)  loop: model -> tools -> model     |
                 |    |     tools: python, shell, read_file, ...                 |
                 |    |       python tool --> ReplHost #0 ===IPC===> python3 #0  |
                 |    |                          ^   llm / agent.spawn requests   |
                 |    |                          |                                |
                 |    +-- LlmNode #1 (leaf completion, no tools)                  |
                 |    +-- AgentNode #2 (depth 1)  same loop, own history          |
                 |          python tool --> ReplHost #2 ===IPC===> python3 #2     |
                 |          +-- AgentNode #5 (depth 2) ...                         |
                 +-------------------------|------------------------------------+
                                           v
                           kyora-providers: anthropic | openai | fake
```

Every node (agent or leaf LLM call) is created by the Rust `Runtime`, admitted and charged by a shared `Ledger`, gets a child cancellation token, and is recorded in the session trace. A Python REPL never talks to a model provider directly: it asks its host over a private pipe, and the host applies limits, routes the call, and returns the result. The isolating implementations surveyed in R§1 and R§2 broker model calls through the host the same way; kyora does it for every backend, including local execution.

## 3. Crate layout

Directory names are short; package names carry the `kyora-` prefix.

| Dir | Package | Purpose | Depends on |
|---|---|---|---|
| `crates/protocol` | `kyora-protocol` | IO-free serde types: messages, content blocks, tool specs, usage, stream events, trace events, node ids. | serde |
| `crates/providers` | `kyora-providers` | `ModelProvider` trait, stream `Accumulator`, SSE parser, retry policy, Anthropic provider; later OpenAI Responses and Chat Completions; `fake` providers. | protocol |
| `crates/core` | `kyora-core` | `Runtime`, agent loop, `Tool` trait and registry, ledger and admission, cancellation tree, trace sink, session store, compaction, prompts. | protocol, providers |
| `crates/tools` | `kyora-tools` | Built-in tools: `shell`, `read_file`, `write_file`, `edit_file`, later `glob`, `grep`. | core |
| `crates/repl` | `kyora-repl` | The `python` tool: REPL process host, IPC codec and pumps, embedded Python package (`python/kyora/`), result formatting. | core |
| `crates/sandbox` | `kyora-sandbox` | M3. Path policy for host file operations; Seatbelt profiles (macOS), bubblewrap and seccomp or Landlock (Linux) for processes. | (none) |
| `crates/mcp` | `kyora-mcp` | M3. MCP client (official `rmcp` crate), stdio and Streamable HTTP, exposing server tools through the registry. | core |
| `crates/tui` | `kyora-tui` | M4. ratatui + crossterm frontend, consumes the event stream. | core |
| `crates/cli` | `kyora-cli` | Binary `kyora`: clap commands, wiring of providers, tools and REPL into a `Runtime`, human and JSON output. | all |
| `npm/` | | M5. `kyora` launcher package and per-platform binary packages. | |

Rules: `protocol` has no IO. `core` knows nothing about Python or specific tools; tools and the REPL plug in through traits, which keeps the dependency graph acyclic (the REPL needs `core` to spawn sub-agents; `core` gets the REPL through a `ToolsetFactory` supplied by the CLI). `unsafe_code` is denied workspace-wide; the sandbox crate may opt out locally when it needs `pre_exec`.

## 4. Core data model

All types below live in `kyora-protocol` unless noted (the scaffold PR already contains the message types).

```rust
pub enum Role { User, Assistant }
pub struct Message { pub role: Role, pub content: Vec<ContentBlock> }

pub enum ContentBlock {
    Text { text: String },
    Thinking { thinking: String, signature: Option<String> },   // replayed verbatim
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: Vec<ToolResultPart>, is_error: bool },
    Opaque { provider: String, kind: String, raw: serde_json::Value },
}
```

The canonical model is close to Anthropic's content blocks because that is the strictest wire format (thinking signatures, single tool-result message per turn). `Opaque` carries anything a provider needs back byte-for-byte that kyora does not interpret: Anthropic `redacted_thinking`, `compaction` and `fallback` blocks, OpenAI `reasoning` items with `encrypted_content`. A provider replays `Opaque` blocks whose `provider` matches its own and drops the rest; histories therefore stay portable across providers at the cost of losing foreign reasoning.

Identifiers:

- `SessionId`: UUIDv7 (time-ordered, so directory listings sort by creation time).
- `NodeId`: `u32`, allocated sequentially per session and never reused, also across resumes; node `0` is the root. Short ids keep traces readable (`#7`).
- `CellId`: `u32` per REPL generation; `ReplGeneration`: `u32` per node, incremented whenever the node's REPL process is (re)started.

`Usage { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens }`. Providers that report several billed iterations for one request (Anthropic `usage.iterations`, used by compaction and refusal fallback) are summed into one `Usage` for charging; the raw usage object is kept in the trace.

**Budget unit.** A "budget token" is any token the provider processed: `input + cache_creation + cache_read + output` (`Usage::total`). Counting cache reads makes budgets larger in absolute numbers than a "new tokens" metric (nano-rlm subtracts cached tokens, R§2), but it does not depend on whether the cache hit, so a reservation can be estimated from the request alone (10.2). A dollar budget (M3) prices the same four fields and uses the same reservation logic.

Trace events (`TraceEvent` in protocol) are one enum used for both the live event stream and the persisted JSONL (section 11); streaming deltas are marked ephemeral and never persisted.

## 5. The agent loop

### 5.1 Runtime and nodes

```rust
// kyora-core
pub struct Runtime(Arc<RuntimeInner>);
struct RuntimeInner {
    providers: ProviderRegistry,            // "anthropic" -> Arc<dyn ModelProvider>
    toolsets: Arc<dyn ToolsetFactory>,      // builds the tool set for a node
    limits: Limits,                         // section 10
    ledger: Ledger,                         // admission and accounting, one lock (10.2)
    model_slots: Semaphore,                 // in-flight model requests
    trace: TraceSink,                       // JSONL writer + broadcast channel
    run_cancel: CancellationToken,          // fresh for every process invocation
}

pub trait ToolsetFactory: Send + Sync {
    fn toolset(&self, node: &NodeInfo, selection: &ToolSelection) -> Result<Toolset>;
}

pub struct NodeCtx {          // handed to tools; the only door to recursion
    pub id: NodeId, pub parent: Option<NodeId>, pub depth: u32,
    pub cancel: CancellationToken, pub deadline: Option<Instant>,
    runtime: Runtime, scope: ScopeId,
}
impl NodeCtx {
    pub async fn llm(&self, call: LlmCall, owner: &CancellationToken) -> Result<LlmOutcome, RecursionError>;
    pub fn spawn_agent(&self, spec: AgentSpec, owner: Owner) -> Result<AgentHandle, RecursionError>; // fail-fast admission
    pub fn budget(&self) -> BudgetSnapshot;
}
pub enum Owner { Cell(CancellationToken), Node }   // who cancels the child (8.3)
```

An `AgentSpec` holds the task, optional name, model reference, tool selection, preloaded REPL variables, and per-node limits (max turns, token budget, deadline). `AgentHandle` exposes `result().await -> AgentOutcome`, `status()`, `cancel()`. `AgentOutcome { node, status, answer, usage_self, usage_subtree, turns }` where `answer` is `Answer::Text(String)` or `Answer::Value(Box<RawValue>)` (from `kyora.final`), and `status` is one of `completed`, `max_turns`, `budget_exhausted`, `timeout`, `context_exhausted`, `cancelled`, `refused`, `failed`.

The root of a session is an agent node like any other; `kyora run` creates it with depth 0 and submits one user turn. Interactive sessions (TUI, `kyora resume` in M2) submit further user turns to the same root node.

### 5.2 One round

A round is one model request plus the tool calls it produced.

```
on user input:
  history.push(user message)                           # append-only, recorded
  loop:
    if cancelled or past deadline -> stop(cancelled | timeout)
    if turns >= max_turns          -> stop(max_turns)
    maybe_compact(history)                             # M2; a no-op in M1; never mid-round (12)
    resp = attempt_with_retries(request(history))      # 6.2, 10.2: reserve, call, settle
                                                       # a request rejected as too long -> stop(context_exhausted)
    turn = admit(resp)                                 # see "admission" below
    history.push(assistant message = turn.content)     # recorded verbatim
    if turn has tool_use blocks and stop_reason != tool_use:
      history.push(user message = ordered error results)   # "not executed: <stop reason>"
    match resp.stop_reason:
      tool_use     -> results = dispatch(turn)         # 5.3; invalid inputs never run
                      history.push(user message = results [+ appended notices])
                      if a cell committed a final answer -> stop(completed, that answer)
      end_turn     -> stop(completed, Answer::Text(last text))
      max_tokens   -> if tool_use blocks were present: loop (their error results are appended)
                      else: append user text "continue", loop
      pause_turn   -> loop (re-send as is)
      model_context_window_exceeded -> M2: compact and loop; M1: stop(context_exhausted)
      refusal      -> stop(refused)
      other        -> stop(failed)
```

Non-executable tool calls are handled in one place, before branching on the stop reason, so every admitted `tool_use` gets its result whatever the ending. If one assistant message contains several `python` calls and one of them commits a final answer, the remaining calls of that message are not executed (error result "skipped: final answer already committed"), so the first committed answer wins.

**Admission of a response.** The accumulator keeps, per tool_use block, the raw concatenated `partial_json` and a validity flag (strict JSON parse, then JSON-schema validation against the tool). A response whose tool input is invalid because generation was cut off (`max_tokens`) is not admitted: the request is retried once with a doubled `max_tokens` (bounded by the model's output cap and the budget). If the retry still produces invalid input, or the input is invalid for any other reason, the turn is admitted with `input: {}` in place of the unparseable input (the same placeholder the API streams at `content_block_start`) and the tool gets an `is_error` result `{"INVALID_JSON": "<raw input>"}`, the shape the Anthropic docs recommend (R§4). The raw input is kept in the trace. Invalid tool calls are never executed.

Invariants:

- **Append-only history.** Nothing already sent is edited, reordered or removed. System prompt and tool list are frozen when the node starts. Dynamic information (budget status, "REPL was restarted", limit warnings) is appended: as a text block after the tool results in the same user message, or as a mid-conversation `role: "system"` message where the provider supports it. This is what Anthropic's preserved-thinking check requires on the accounts and models that run it, and it keeps prompt caching effective (R§4).
- **Every admitted tool_use gets exactly one tool_result**, in the order of the tool_use blocks, in a single user message. On cancellation, unfinished tools get `is_error` results ("cancelled") so the history stays valid (Codex normalizes the same way, R§3).
- **Tools never run on truncated, invalid or refused input.**

### 5.3 Tool dispatch and concurrency guarantees

M1 runs the tool calls of one assistant message sequentially, in order. M2 adds concurrency: each tool declares an `Effect` (`ReadOnly` or `Mutating`), consecutive read-only calls run concurrently and a mutating call is a barrier (Codex uses an RW lock for the same purpose, R§3); results are always recorded in call order.

Guarantee scope: ordering holds within one node only. Different nodes (a parent and its spawned children, siblings) share the workspace and run concurrently, so kyora provides no tree-wide consistency for files. File tools write atomically (temp file and rename), and `edit_file` holds a per-path lock only for the duration of its read-modify-write. No lock is ever held across a recursive wait (that would reintroduce the deadlock section 10.3 rules out). Code that needs isolation between writers should give children disjoint files; separate workspaces per child (for example git worktrees) are a later option.

Unknown tool names and schema-invalid inputs return `is_error` results.

### 5.4 Output to the caller

The loop emits `TraceEvent`s on a broadcast channel: node lifecycle, text/thinking/tool-input deltas, tool begin/end, REPL cells, usage, errors. Frontends render these; `kyora run --json` prints them as NDJSON. The final answer of the root is printed to stdout by `kyora run`.

## 6. Providers

### 6.1 Trait

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn stream(&self, req: ModelRequest, cancel: CancellationToken)
        -> Result<EventStream, ProviderError>;
    async fn model_info(&self, model: &str) -> Result<ModelInfo, ProviderError>;  // context window, max output, capabilities
}
```

`ModelRequest` carries model, frozen system prompt, tools, messages, `max_tokens`, and provider-neutral options (`effort`, `thinking_display`, `cache`, `task_budget_total`, `metadata { node, depth }`); each provider maps what it supports and ignores the rest. `collect(stream)` turns a stream into a `ModelResponse` via the shared `Accumulator`, so leaf `llm` calls use the same path.

Model references are `provider/model` (`anthropic/claude-opus-5-5`); a bare model name uses the default provider.

### 6.2 Anthropic (M1)

Raw HTTP (no official Rust SDK) with `reqwest` (rustls) and a small SSE parser of our own (event/data framing, `ping`, mid-stream `error` events).

- **Transport.** `POST {base_url}/v1/messages`, headers `x-api-key` from `ANTHROPIC_API_KEY`, `anthropic-version: 2023-06-01`, `anthropic-beta` as needed. `ANTHROPIC_BASE_URL` overrides the base URL (tests use a local mock server). Always streaming. Client tools that take large inputs (`python.code`, `write_file.content`) carry `eager_input_streaming: true`; validation is then ours (5.2).
- **Caching.** System prompt sent as text blocks with `cache_control: {type: "ephemeral"}` on the last block, plus top-level automatic `cache_control` for the message tail. Per-node system prompts are frozen and identical across siblings of the same role (no node ids or timestamps in them), so sibling sub-agents can share a cached prefix.
- **Thinking.** Per-model capabilities come from `GET /v1/models/{id}` (with a built-in fallback table). For models with adaptive thinking (Claude Opus 5.5, Sonnet 5.5) every request sends one complete configuration: `thinking: {type: "adaptive", display: "omitted" | "summarized", block_binding: {prefix_mismatch_behavior: "error"}}` with beta `thinking-binding-controls-2026-08-01`. `display` is `summarized` only with `--show-thinking`. Models without adaptive thinking get no `thinking` field. Thinking and opaque blocks are stored and replayed verbatim. If a request is rejected for a prefix mismatch (which means a kyora bug), the session switches to `drop_block` permanently: the choice is recorded in the session log and sent on every later request, including after resume, and every `input_transformations` entry in responses is recorded. Tests run with `"error"` so a mismatch fails CI.
- **Effort.** `output_config.effort` from config or `--effort`; unset means the API default.
- **Usage.** Top-level usage, or the sum over `usage.iterations` when present (the top-level fields cover only the serving iteration in that case).
- **Refusals.** `stop_reason: "refusal"` ends the node with status `refused` and the `stop_details` category. Server-side refusal fallback is **off in M1**: after a mid-output fallback the next request must drop client `tool_use` and thinking blocks before the final `fallback` block (R§4, Anthropic "Continuing the conversation"), which needs a replay projection the M1 loop does not have. M2 adds it: the accumulator keeps the raw content for the trace, the loop executes only tool calls after the final `fallback` block, and replay applies the documented keep/drop table.
- **Retries.** Each attempt is a separate ledger reservation (10.2). Up to 4 retries on transport errors, 408, 409, 429, 5xx and 529, honoring `retry-after`, otherwise exponential backoff with full jitter (1 s base, 60 s cap); backoff sleeps and slot waits are cancellable. A 429 without `retry-after` is retried at most once (it may be a spend cap that keeps failing, R§4). A stream that fails mid-way (`error` event, or 300 s without any event including `ping`) is retried as a whole; this is kyora's policy (the docs also describe continuing from partial text, which cannot recover partial tool_use or thinking blocks, R§4). A `stream_reset` event tells frontends to discard partial output. Tools only run after a complete, admitted message, so a retry never repeats a side effect.
- **Not used.** Forced `tool_choice` (rejected on current models, R§4). Structured output for `kyora.llm(schema=...)` will use `output_config.format` (M3).
- **Task budgets (M2).** When a node has its own token budget of at least 20,000, kyora sends `output_config.task_budget = {type: "tokens", total: <node budget at node start>}` (beta `task-budgets-2026-03-13`) and keeps `total` stable for the node's whole loop; it never sends `remaining`, because the server counts a different quantity than kyora's ledger and mirroring it client-side under-reports the budget (R§4, Anthropic task-budget docs). Below the minimum or on unsupported models nothing is sent. The countdown is advisory; the ledger stays authoritative.

### 6.3 OpenAI and local models (M2)

- `openai-responses`: `POST /v1/responses`, items mapped to content blocks; `reasoning` items with `encrypted_content` stored as `Opaque` and replayed (with `store: false` and `include: ["reasoning.encrypted_content"]`); function calls and outputs mapped to `ToolUse`/`ToolResult`.
- `openai-chat`: Chat Completions wire format for OpenAI-compatible servers (vLLM, llama.cpp, Ollama, LM Studio). No reasoning replay; tool calls via `tools`/`tool_calls`; streaming via `choices[].delta`.
- Provider config (section 17) selects `kind`, `base_url`, `api_key_env`, extra headers. Codex now only supports Responses (R§3); kyora keeps Chat Completions because it is the lowest common denominator for local models.

### 6.4 Fake providers

`kyora-providers::fake` (exists in the scaffold) provides `ScriptedProvider` (rules with matchers on depth, system text and last user text, each with a list of canned responses) and `FnProvider` (closure). Change for M1: a rule's response is selected by the conversation's assistant-turn index (`responses[n]` where `n` = number of assistant messages in the request) instead of a mutable queue, so the provider is stateless and identical concurrent sub-agents get identical scripts regardless of scheduling. The CLI accepts `--fake-script <file>` (hidden flag, also `KYORA_FAKE_SCRIPT`) to run the whole binary against a script.

## 7. Tools

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;               // name, description, JSON schema
    fn effect(&self) -> Effect;               // ReadOnly | Mutating (used from M2)
    fn large_input(&self) -> bool { false }   // enables eager input streaming
    async fn call(&self, input: serde_json::Value, cx: ToolCx<'_>) -> ToolOutput;
}
pub struct ToolCx<'a> { pub node: &'a NodeCtx, pub call_id: &'a str, pub cwd: &'a Path,
                        pub policy: &'a ExecPolicy, pub events: &'a EventTx }
pub struct ToolOutput { pub content: Vec<ToolResultPart>, pub is_error: bool,
                        pub final_answer: Option<Answer> }
```

| Tool | Milestone | Input | Notes |
|---|---|---|---|
| `shell` | M1 | `command`, `timeout_s?` | `bash -c` (falls back to `sh`) in the node's cwd, new process group, scrubbed environment, stdin `/dev/null`. stdout and stderr are drained continuously into bounded head and tail buffers (never collected whole); exit code reported. Default timeout 120 s; the group is killed on timeout or cancel. |
| `read_file` | M1 | `path`, `offset?`, `limit?` | Line-numbered output, default 2,000 lines, long lines clipped, refuses binary files, reads at most a bounded number of bytes. |
| `write_file` | M1 | `path`, `content` | Creates parent directories; atomic write via temp file and rename. |
| `edit_file` | M1 | `path`, `old`, `new`, `replace_all?` | Exact string replacement; `old` must match exactly once unless `replace_all`; per-path lock during the edit. |
| `glob` | M2 | `pattern`, `path?` | `ignore`-crate walker honoring `.gitignore`, capped. |
| `grep` | M2 | `pattern`, `path?`, `glob?`, `case_insensitive?`, `context?` | ripgrep libraries (`grep-searcher`, `grep-regex`, `ignore`), capped. |
| `python` | M1 | `code`, `timeout_s?` | The REPL (section 8). |

All tool outputs pass through one truncation helper: above the cap (default 20,000 characters) the head and tail are kept around `[... N characters omitted ...]`.

File tools run in the kyora process, not in a sandboxed child. From M3 they apply the same policy as sandboxed processes (13.2): paths are canonicalized with symlinks resolved, reads of sensitive paths are refused, writes outside writable roots are refused. In M1 they act with the user's full authority, like the shell.

GPT models are trained on Codex's `apply_patch` grammar (R§3); an `apply_patch` tool is added with the OpenAI providers in M2 and offered only to those models.

MCP (M3): each configured server's tools are registered as `mcp__<server>__<tool>`, `ReadOnly` only when the server marks them `readOnlyHint`. The tool list is resolved when the node starts and stays frozen for that node (append-only rule).

Default tool sets: the root gets all built-ins plus `python`. Sub-agents get `python` and `read_file` (plus `glob` and `grep` from M2) unless the spawning code asks for more, and can only receive tools their parent has (capability attenuation, section 18).

## 8. The recursive runtime

### 8.1 Model-facing behavior

The recursive interface is exposed through the `python` tool, whose description documents the injected `kyora` module. Each agent node owns at most one REPL process, started lazily on the first `python` call (or eagerly when variables must be preloaded) and kept for the node's lifetime, so variables persist across cells and across compaction. A cell's result is formatted for the model as:

```
[stdout]
...
[stderr]
...
[result]
<repr of the last expression, if any>
[error]
ZeroDivisionError: division by zero
  (traceback, trimmed to the user code frames)
[vars] new or changed: chunks: list[str] (412 items); summaries: list[str] (412 items)
[kyora] this cell: 412 llm calls, 0 agents, 9.8M budget tokens; remaining: 8.3M, depth 0 of 2
```

Sections are omitted when empty. Output is bounded at capture time (head and tail buffers inside Python, and a drained pipe for subprocess output, 9.1) and again by the tool truncation cap. The bounded output is deliberate: it pushes the model to keep data in variables and print summaries, which is the core RLM idea (R§1).

### 8.2 The `kyora` Python API

The module is pre-imported in the REPL namespace. It is plain synchronous Python (callable from threads too), stdlib only, Python 3.9 or newer.

```python
kyora.llm(prompt, *, system=None, model=None, max_tokens=None) -> str
kyora.llm_batch(prompts, *, system=None, model=None, max_tokens=None,
                max_concurrency=None, return_exceptions=False) -> list

kyora.agent(task, *, context=None, vars=None, tools=None, model=None,
            max_turns=None, budget=None, timeout=None, name=None) -> object
kyora.spawn(task, **same_kwargs) -> kyora.AgentHandle
    handle.result(timeout=None) -> object   # raises AgentFailed / Cancelled / Timeout
    handle.done() -> bool
    handle.status() -> dict                 # status, turns, usage so far
    handle.cancel() -> None
kyora.gather(handles, *, return_exceptions=False) -> list

kyora.final(value) -> None      # stage this agent's answer; committed if the cell succeeds
kyora.budget() -> dict          # remaining tokens, agents, depth, deadline
kyora.log(message) -> None      # goes to the trace and UI, not to the model
kyora.node_id, kyora.depth, kyora.max_depth   # constants

kyora.map(fn, items, *, max_workers=8) -> list   # thread pool whose workers inherit the cell

class KyoraError(Exception)                 # base for recoverable errors
class LimitExceeded(KyoraError)             # admission refused: depth, agents, calls, batch size, outstanding; .limit
class BudgetExceeded(KyoraError)            # admission or reservation refused for lack of budget
class ValueTooLarge(KyoraError)             # request or result exceeds the frame cap
class ModelError(KyoraError)                # provider failure after retries, refusal
class AgentFailed(KyoraError)               # child ended non-completed; .status, .partial, .node_id
class Timeout(KyoraError)                   # a wait timed out
class InvalidRequest(KyoraError)            # malformed arguments rejected by the host
class StaleCall(KyoraError)                 # call made outside its cell (thread outlived its cell)
class Cancelled(BaseException)              # the calling cell was cancelled; not caught by `except Exception`
```

Error semantics, one code per situation:

| Situation | Python sees |
|---|---|
| Admission refused (depth, agent or call counts, batch size, outstanding requests) | `LimitExceeded`, nothing started |
| No budget left for the call or the child | `BudgetExceeded`, nothing started |
| Child started and ended in any status except `completed` (including its own timeout, `max_turns`, `budget_exhausted`, `refused`) | `AgentFailed` with `.status` |
| `handle.result(timeout=...)` expired | `Timeout`; a node-owned child keeps running |
| `kyora.agent(..., timeout=...)` expired | the cell-owned child is cancelled, then `Timeout` |
| The calling cell was interrupted, timed out or cancelled | `Cancelled` |
| Provider failure of an `llm` call after retries, or a refusal | `ModelError` |
| Oversized request (checked locally) or oversized result (checked by the host) | `ValueTooLarge` |

Inside an agent's own loop (not Python), a failed reservation ends that agent with status `budget_exhausted`; from Python, the same condition is a recoverable `BudgetExceeded` so the code can wrap up.

Semantics:

- **`llm`** is a single completion with no tools: a leaf node (`kind: llm`) in the tree. `prompt` is a string or a list of `{"role", "content"}` dicts. Default model is the configured `llm_model` (section 17). Allowed at every depth, including `max_depth`; disabled entirely with `--no-llm` (for ablations, R§1).
- **`llm_batch`** sends one request to the host, which runs the calls concurrently (default `max_concurrency` 8; values below 1 are rejected; never above the global in-flight cap; at most 1,000 items per batch) and returns results in input order. With `return_exceptions=True`, failed items are exception instances instead of raising.
- **`agent`** runs a full child agent (`kind: agent`, depth + 1) with its own history, tools, REPL and budget, and blocks until it ends. The child is owned by the calling cell: if the cell is interrupted or times out, the child is cancelled. `context` (any JSON value) is preloaded as the variable `context` in the child's REPL; `vars` preloads several named variables. The child's first user message contains the task and a manifest of the preloaded variables (name, type, size, a short preview), never the values themselves. Returns the child's answer: the value committed with `kyora.final` in the child, otherwise the child's final assistant text. Non-completed endings raise `AgentFailed`.
- **`spawn`** registers a child owned by the agent node (not the cell) and returns immediately; the child runs concurrently and its handle stays valid in later cells. All node-owned children are cancelled when the agent ends.
- **`final`** stages an answer for the current cell. It is committed only if the cell finishes with status `ok`; then the tool result says the answer was recorded and the loop ends without another model call. A cell that fails, times out or is interrupted discards its staged answer. For the root, the committed value is what `kyora run` prints (strings as-is, other values as JSON).
- **Values crossing the boundary are JSON**, encoded with `json.dumps(..., allow_nan=False)` (NaN and infinity are rejected; tuples become lists; other non-JSON objects raise `TypeError` in the calling code). Integers are limited to what the interpreter can convert to text (on Python 3.11 and later, 4,300 digits by default); a larger integer raises `ValueError` locally. Python measures the complete encoded request frame before sending; if it exceeds the frame cap (9.1) it raises `ValueTooLarge` locally, which is recoverable. The host passes context values, variables and final answers through as raw JSON (`serde_json::value::RawValue`) without converting numbers. Larger data should be written to a file and passed by path (a content-addressed blob store is M3).
- **Threads.** A call is attributed to the cell that was running when the calling thread was created or the work was submitted, never to whatever cell runs when the call happens (9.3). `kyora.map` and standard `threading.Thread` / `concurrent.futures.ThreadPoolExecutor` usage inside a cell work; a thread that outlives its cell gets `StaleCall` on its next `kyora` call.

A typical long-context cell:

```python
chunks = [context[i:i + 200_000] for i in range(0, len(context), 200_000)]
notes = kyora.llm_batch([f"List every date mentioned:\n\n{c}" for c in chunks])
hard = [i for i, n in enumerate(notes) if "unclear" in n]
fixes = kyora.gather([kyora.spawn("Resolve the dates in this text precisely.",
                                  context=chunks[i], budget=500_000) for i in hard])
```

### 8.3 Host side

`kyora-repl` implements the `python` tool. Per agent node it owns a `ReplHost`: the child process, the IPC pumps (9.3), the current cell (id, generation, cancellation token, deadline, staged final answer), a table of in-flight requests, and a table of node-owned child handles. Requests from Python are handled as follows:

- `llm` / `llm_batch` -> `NodeCtx::llm` per item, owned by the cell's token: ledger admission and reservation, a model slot, a trace node, the provider call. Batch items run on a `JoinSet` limited by the batch's own semaphore.
- `agent.spawn` -> `NodeCtx::spawn_agent` with `Owner::Cell` (for `kyora.agent`) or `Owner::Node` (for `kyora.spawn`). Admission (depth, live and total agent counts, budget) is checked in the same ledger transaction that registers the child; violations come back as typed errors.
- `agent.result` -> await the handle, bounded by the request's timeout and the cell's deadline; `agent.cancel` -> cancel the child's token. `agent.result`, `agent.status` and `agent.cancel` accept only node ids of children registered to the calling node (other ids are `invalid_request`), so code cannot wait on or cancel arbitrary nodes.
- `final` -> stage the answer on the current cell.

The child agent runs the same loop (section 5) as a Tokio task with a toolset built by the `ToolsetFactory`, which gives it its own `python` tool and therefore its own REPL process.

**Cell exit** runs on every cell ending (ok, error, timeout, interrupt): close the cell for new requests, cancel the cell token and join the cell's remaining direct work (`llm` calls, batches) and its cell-owned children, settle their reservations, and drop any late replies. Node-owned children (`kyora.spawn`) are not touched. Only then is the tool result returned to the loop.

**Node shutdown** always runs the same sequence, whatever the ending: close admission for the node (no new children or calls), cancel and join all descendants, settle all outstanding reservations (10.2), shut down the REPL (graceful `shutdown`, then kill the process group), write `node_end`, and only then release the node's live-agent slot.

### 8.4 Variables and large inputs

- `kyora run --context-file big.txt` (or `--var name=@path`, `--var name=value`) preloads variables into the root REPL. The host computes metadata (bytes, characters, lines, first 500 characters) from the file and puts only the manifest in the first user message; the REPL loads the file itself (`set_var` with a file source) so the content never passes through the prompt.
- Values passed to `kyora.agent(context=...)` travel inline over IPC and are injected into the child REPL with `set_var`.
- REPL state is in-memory only. After a REPL crash or kill (and after resume, M2), the next `python` call starts a new REPL generation, preloaded file variables are reloaded (their path and sha256 were recorded), and a notice listing the lost variables is appended to the conversation. Snapshotting REPL state is deliberately not attempted; Prime and nano-rlm do not reconstruct kernels either (R§2).

## 9. REPL IPC protocol

### 9.1 Process, transport and framing

The REPL process is `python3 -I -X utf8 <runtime>/kyora_boot.py`. `<runtime>` is a private directory (mode 0700) under the system temp dir, created per kyora process, into which the embedded Python files are written. `-I` ignores `PYTHONPATH` and the user site and removes the script's directory from `sys.path`, so the boot script imports the package explicitly: it inserts its own directory (`os.path.dirname(os.path.abspath(__file__))`) at the front of `sys.path`, imports `kyora`, and then appends the node's cwd to the end of `sys.path` so user modules in the workspace stay importable.

The protocol runs over the child's stdin (host to REPL) and stdout (REPL to host), so it works unchanged over any byte stream, including a remote exec session later (section 22). At startup the boot script:

1. duplicates fds 0 and 1 to private descriptors (non-inheritable by default since PEP 446) and builds the protocol reader and writer on them, so the protocol keeps using the original pipes;
2. points fd 0 at `/dev/null`, so user code and its subprocesses see an empty stdin;
3. creates a pipe and points fds 1 and 2 at its write end; a drain thread reads the other end continuously into the current cell's bounded head and tail buffer (64 KiB head, 64 KiB tail, dropped bytes counted), so subprocess output can neither fill a disk nor block the writer. Output that arrives between cells is attributed to the next cell as `[background output]`, with the same bounds.

This keeps ordinary output (prints, subprocesses, C extensions) out of the protocol stream. It is not a defense against code that deliberately writes to the duplicated descriptors; such code can only send protocol messages, which the host validates and bounds like any other (9.5, section 18).

Frames: 4-byte big-endian unsigned length, then UTF-8 JSON (the same framing as the rlm broker and nano-rlm, R§1, R§2). Maximum frame 64 MiB in both directions, configurable.

### 9.2 Messages

JSON-RPC 2.0 shapes without the `jsonrpc` member. Requests carry `id` (unique per sender), `method`, `params`; responses carry the same `id` plus `result` or `error {code, message, data?}`; notifications have no `id`. Both sides send requests, so each side tracks its own outstanding ids.

REPL to host, once at startup (notification):

```json
{"method": "ready", "params": {"protocol": 1, "python": "3.12.4", "pid": 4242}}
```

Host to REPL (at most one at a time, and only `interrupt` while a cell runs):

| Method | Params | Result |
|---|---|---|
| `init` | `node_id`, `depth`, `max_depth`, `generation`, `limits` (output caps), `cwd` | `{}` |
| `exec` | `cell`, `code` | `ExecResult` (below) |
| `set_var` | `name`, and either `value` (JSON) or `file {path, format: "text" or "bytes" or "json"}` | `{type, size}` |
| `get_var` | `name`, `max_bytes` | `{json?, repr}` |
| `vars` | | `[{name, type, size}]` |
| `shutdown` | | `{}` then the process exits |
| `interrupt` (notification) | `cell` | |

REPL to host (accepted only while a cell runs; every request carries `cell`):

| Method | Params | Result |
|---|---|---|
| `llm` | `cell`, `prompt` or `messages`, `system?`, `model?`, `max_tokens?` | `{text, model, usage, stop_reason, node_id}` |
| `llm_batch` | `cell`, `items: [llm params]`, `max_concurrency?` | `{results: [{ok: llm result} or {error}]}` |
| `agent.spawn` | `cell`, `owner: "cell" or "node"`, `task`, `context?`, `vars?`, `tools?`, `model?`, `max_turns?`, `budget?`, `timeout_ms?`, `name?` | `{node_id}` |
| `agent.result` | `cell`, `node_id`, `timeout_ms?` | `{status, answer, text, turns, usage}` or error |
| `agent.status` | `cell`, `node_id` | `{status, turns, usage}` |
| `agent.cancel` | `cell`, `node_id` | `{}` |
| `final` | `cell`, `value` | `{}` |
| `budget` | `cell` | `{tokens_remaining, agents_remaining, depth, max_depth, deadline_ms?}` |
| `log` (notification) | `cell`, `level`, `message` | |

`ExecResult`:

```json
{"status": "ok", "stdout": "...", "stderr": "...", "background": "",
 "omitted": {"stdout": 0, "stderr": 0, "background": 0},
 "result": "repr or null", "error": null,
 "vars_changed": [{"name": "notes", "type": "list[str]", "size": "412 items"}],
 "final_staged": false, "wall_ms": 8120}
```

`status` is `ok`, `error` (exception in user code; `error` holds `type`, `message`, `traceback`), `interrupted`, or `timeout`.

Error codes host to REPL and their Python exceptions: `limit_exceeded` (with `data.limit`: `depth`, `agents_live`, `agents_total`, `llm_calls`, `batch_size`, `outstanding`, `memory`) -> `LimitExceeded`; `budget_exceeded` -> `BudgetExceeded`; `cancelled` -> `Cancelled`; `timeout` -> `Timeout`; `stale_cell` -> `StaleCall`; `model_error` -> `ModelError`; `agent_failed` -> `AgentFailed`; `invalid_request` -> `InvalidRequest`; `value_too_large` -> `ValueTooLarge`.

### 9.3 Pumps and cell ownership

Host side, per REPL:

- a **reader task** decodes frames and never awaits anything but the socket: responses are matched to the host's outstanding requests; each incoming request is validated and dispatched to its own task in a `JoinSet`, so a long `agent.result` never blocks later requests or cancellations;
- a **writer task** drains a bounded queue of outbound frames;
- a **watchdog** owns the process handle and can kill the process group without going through the writer.

Pumps and tasks are bound to one REPL generation and end with it. Requests whose `cell` is not the current cell of the current generation are rejected with `stale_cell`; between cells every request is rejected the same way. Responses to stale requests are dropped.

Python side: the main thread runs cells. A reader thread owns the protocol input: it routes responses to waiting futures by `id`, queues host requests for the main thread, and handles `interrupt` only if its `cell` matches the running cell, by failing that cell's pending futures with `cancelled` and calling `_thread.interrupt_main()`. The cell dispatcher catches the resulting `KeyboardInterrupt` (and a late one arriving between cells is swallowed by the main loop), so an interrupt can never kill the dispatcher. Waits use short timed waits in a loop, so a blocked call notices an interrupt within about 100 ms even where signals cannot be delivered. Writes are serialized by a lock that is held only for one write (the host reader always drains, 9.3).

Cell ownership is captured, not read from shared state. The dispatcher sets a `contextvars.ContextVar` holding `(generation, cell)` before running a cell, and every `kyora.*` request carries the value from the caller's context. Threads do not inherit context variables before Python 3.14, so the boot script makes them: `threading.Thread` captures `contextvars.copy_context()` when it is created and runs its target inside it, and `ThreadPoolExecutor.submit` (and therefore `kyora.map`) captures the context at submission. A thread created in cell A therefore keeps A's id even when it wakes during cell B, and the host rejects its calls as stale. Calls from contexts without a cell (threads started by other means, code running between cells) are rejected locally with `StaleCall`.

### 9.4 Timeouts and interruption

M1 uses a **wall-clock** cell timeout: default 30 minutes, adjustable per call with the tool's `timeout_s` (capped at 2 hours), and never beyond the node's deadline. Every blocking wait in Python (`llm`, `agent`, `handle.result`) is bounded by the cell deadline. A process-wide `RLIMIT_CPU` is only a backstop. (Excluding time spent waiting on sub-calls from the timeout needs a per-thread wait policy; it is a possible later refinement, not part of M1.)

On timeout or cancellation the host starts the 2 s grace deadline first (on the watchdog, so a blocked writer cannot delay it), then cancels the cell's token (which cancels its `llm` calls and cell-owned children) and queues `interrupt`. Otherwise the watchdog kills the process group, the cell is reported as `timeout` or `interrupted`, the generation ends, and the next `python` call starts a fresh REPL with the restart notice of 8.4.

### 9.5 Bounds

| Bound | Default | On violation |
|---|---|---|
| startup (`ready` after spawn) | 10 s | kill, tool error |
| frame body after its length prefix | 30 s | kill |
| frame size | 64 MiB | kill |
| outstanding requests from one REPL | 256 | `limit_exceeded` (`outstanding`) |
| bytes of outstanding request payloads | 256 MiB | `limit_exceeded` (`outstanding`) |
| items per `llm_batch` | 1,000 | `limit_exceeded` (`batch_size`) |
| one response frame (encoded, checked before enqueueing) | 64 MiB | that request fails with `value_too_large` |
| bytes queued for one REPL's writer | 128 MiB | requests wait (backpressure); kill if the REPL has not read for 30 s |
| bytes of buffered IPC payloads, process-wide (requests, accumulated results, queued frames) | 1 GiB | new requests fail with `limit_exceeded` (`memory`) |
| graceful shutdown | 2 s | kill |

Results are size-checked as they are produced, never only after aggregation: an `llm` result or an agent answer that alone would exceed the response frame becomes a `value_too_large` error for that call, and a batch whose accumulated results would exceed the frame turns the remaining items into `value_too_large` item errors instead of building an oversized aggregate. Request concurrency parameters must be positive integers; other values are `invalid_request`.

Any malformed frame, unknown method or schema violation from the REPL is a protocol error: the REPL is killed and the cell reports an error. The host never trusts a REPL to recover from its own corruption.

## 10. Limits, budgets and cancellation

### 10.1 Limits

| Limit | Default | Scope | Behavior when hit |
|---|---|---|---|
| `max_depth` | 2 | tree | `agent.spawn` fails with `limit_exceeded` |
| `max_agents_total` | 100 | session | spawn fails |
| `max_agents_live` | 16 | tree | spawn fails (fail-fast, never queues) |
| `max_llm_calls` | 2,000 | session | `llm` fails |
| `max_inflight_requests` | 16 | process | requests wait for a model slot (cancellable) |
| `budget_tokens` | 20,000,000 | session, and optionally per subtree | reservation fails; the node stops with `budget_exhausted` |
| `max_turns` | root 200, sub-agent 50 | node | node stops with `max_turns` |
| `run_timeout` | 2 h | root deadline | everything is cancelled, status `timeout` |
| agent `timeout` | parent's deadline | per spawn | child cancelled, `Timeout` raised |
| cell timeout | 30 min (max 2 h) | cell | cell interrupted |
| request timeouts | 300 s idle, 30 min total | model request | attempt fails, retry policy applies |
| `max_output_tokens` | agents 32,000, llm 16,000 | request | passed as `max_tokens` |
| `tool_output_chars` | 20,000 | tool result | head and tail kept |

Every wait in the system is therefore finite. Budget counts all processed tokens including cache reads (section 4), so the default is large in absolute terms; with a warm cache most of it is cheap cache reads. The root counts as one agent toward the live and total limits. All limits are validated at startup: counts, caps and concurrency must be positive integers and durations positive.

### 10.2 Ledger

The ledger is a single structure behind one mutex: a tree of scopes mirroring the node tree, each with an optional limit and `reserved` and `used` counters, plus the admission counters (live agents, total agents, llm calls). Contention is negligible next to HTTP latency, and one lock makes every check-and-update atomic:

- **Admission** (spawn, llm call): one critical section checks depth, counters and budget headroom for the whole ancestor path and registers the node, or changes nothing.
- **Reservation** (each model attempt): one critical section checks `used + reserved + R <= limit` for the node's scope and every ancestor, then adds `R` to `reserved` on the whole path, or changes nothing.
- **Settlement**: subtracts `R` from `reserved` and adds the charge to `used` on the whole path.

**Reservation estimate.** `R` is a conservative estimate of an attempt's budget tokens, not a proven bound: no provider documents a tokenizer contract, and Anthropic describes even its token-counting endpoint as an estimate. kyora uses:

- first request of a node: `R = bytes(system + tools + messages) + 16 * messages + 512 + max_tokens` (text only; a token rarely covers less than one byte of UTF-8 text);
- later requests: `R = P + O + bytes(new user content) + 16 * new messages + max_tokens`, where `P` is the previous request's measured prompt tokens (`input + cache_creation + cache_read`) and `O` its `output_tokens`, standing in for the appended assistant message;
- after compaction the first-request formula applies to the new history;
- requests that can bill several iterations (M2: compaction, refusal fallback) reserve `R` per possible iteration.

If `R` does not fit, `max_tokens` is reduced to fit, down to a floor of `min(4096, requested max_tokens)`; below that the request is refused (`budget_exhausted` in the loop, `BudgetExceeded` in Python).

**What is guaranteed.** Dispatch is exact: no attempt is sent unless its reservation fits every scope on its path, and admission and reservation are atomic. Accounting is exact: every attempt is settled with what the provider reported, or conservatively (below). The limit itself is enforced at dispatch time with an estimate, so it can be exceeded when an attempt is charged more than its `R`. The overshoot is bounded: only attempts already in flight can cause it, there are at most `max_inflight_requests` of them, and each one is charged at most the model's context window plus `max_tokens` (the API rejects larger prompts), so the worst case is `max_inflight_requests * (context_window + max_output_tokens - R)`. In practice it is the estimator's error. When an attempt settles above its reservation, the excess is recorded on the `attempt` record, and every scope on its path with `used >= limit` is closed to further reservations. Live smoke tests compare `R` with actual usage to keep the estimator honest; property tests check the ledger arithmetic itself (dispatch never exceeds headroom, settlement is exact, closing on overshoot).

**Settlement and durability.** Each attempt is written ahead: an `attempt_start` record (attempt id, node, `R`) is persisted and flushed before the request is sent, and an `attempt_end` record (outcome, usage, charge) after it settles. The charge depends on what is known:

| Attempt outcome | Charge |
|---|---|
| completed | actual usage (summed over `usage.iterations` when present) |
| not sent: connection or TLS failure before the request body was written | 0 |
| rejected: HTTP 4xx or 529 response before any stream event | 0 (kyora policy: these responses carry no usage and indicate the request was not processed) |
| anything else without final usage: other 5xx, failure or timeout mid-stream, cancellation after sending | `R` (conservative) |

Retries reserve again before every attempt. Settlement also happens during cell exit and node shutdown, so no reservation outlives its owner. After a crash, any `attempt_start` without a matching `attempt_end` is charged `R` on recovery (11.3); settlement records carry the attempt id, so recovery never counts an attempt twice.

### 10.3 Deadlock freedom for harness resources

A parent that waits for its children must not hold a resource its children need, otherwise a fan-out tree deadlocks (nano-rlm guards this with `max_concurrent_subagents >= max_depth`, R§2). The harness's own blocking primitives are:

| Primitive | Held while | Ever held across a wait on another node? |
|---|---|---|
| `max_inflight_requests` semaphore | one HTTP attempt (the wait for it is cancellable) | no |
| per-batch semaphore of `llm_batch` | one leaf `llm` call of that batch | no |
| ledger mutex | a synchronous check-and-update | no (never held across an await) |
| per-path file lock | one `edit_file` operation | no |
| Python protocol writer lock | one frame write; the host reader always drains | no |
| writer backpressure (9.5) | until the REPL reads; bounded by a 30 s kill deadline | no |

Agent counts are admission limits that fail fast, the IPC reader never awaits handlers (9.3), and a node can only wait on its own children, so the wait-for graph between harness-owned resources follows the node tree and cannot contain a cycle. This argument does not cover user code: Python locks, thread joins, files or subprocesses used by model-written code can still deadlock among themselves. Those cases are ended by the cell, node and run deadlines, which are termination backstops, not a proof of progress.

### 10.4 Cancellation

Tokens form a tree: run, then node, then cell. An agent's own loop requests (its model attempts and tool calls) are owned by the node token. Inside the `python` tool, the cell token (a child of the node token) owns the cell's `llm` calls and cell-owned children; node-owned children (`kyora.spawn`) hang off the node token. Cancelling a token cancels its subtree:

- in-flight HTTP streams are dropped and their reservations settled;
- running tools get the token (shell kills its process group);
- a cell's REPL gets `interrupt`, then a kill after the grace period;
- pending IPC requests from that cell are answered with `cancelled`;
- cancelled nodes run the shutdown sequence of 8.3 with status `cancelled`.

`kyora run`: the first Ctrl-C cancels the root gracefully; a second Ctrl-C within 2 s exits immediately with code 130.

**Process scope (M1).** Cancellation reliably reaches kyora-managed processes and their process groups. A process that deliberately leaves its group (for example with `setsid`) is not tracked in M1; the M3 sandbox runs each REPL and shell in its own sandbox (and on Linux its own PID namespace via bubblewrap), which closes that gap.

## 11. Sessions and transcripts

### 11.1 Layout

`KYORA_HOME` defaults to `~/.kyora` (shared with kyora-switch, which uses `~/.kyora/switch`). Tests always set `KYORA_HOME` to a temp directory.

```
$KYORA_HOME/
  config.toml
  sessions/<session-id>/events.jsonl     # authoritative record, append-only
  sessions/<session-id>/blobs/<sha256>   # payloads above 64 KiB, content-addressed
  logs/                                   # tracing output when enabled
```

Directories are created 0700 and files 0600: transcripts contain whatever the agent read. A process holds an exclusive lock on `events.jsonl` (`File::try_lock`) for as long as it writes to the session; a second writer fails fast.

### 11.2 Record format

One JSON object per line, written by a single writer task (records arrive over a channel, so concurrent nodes never interleave partial lines), flushed after every record. Common fields: `v` (format version, 1), `seq` (gapless per session), `ts` (RFC 3339 UTC), `type`, `node` (when applicable).

```json
{"v":1,"seq":0,"ts":"2026-10-04T20:00:00.000Z","type":"session_start","session":"0199...","cwd":"/work","kyora":"0.1.0","limits":{...}}
{"v":1,"seq":1,"type":"node_start","node":0,"parent":null,"depth":0,"kind":"agent","name":"root","model":"anthropic/claude-opus-5-5","system":"...","tools":[...],"limits":{...}}
{"v":1,"seq":2,"type":"var_loaded","node":0,"name":"context","source":{"file":"/work/big.txt","sha256":"...","bytes":48213557}}
{"v":1,"seq":3,"type":"message","node":0,"message":{"role":"user","content":[...]}}
{"v":1,"seq":4,"type":"attempt_start","node":0,"attempt":"0.1","model":"claude-opus-5-5","reserved":61234}
{"v":1,"seq":5,"type":"attempt_end","node":0,"attempt":"0.1","request_id":"req_...","outcome":"completed","charged":48211,"excess":0,"usage":{...},"stop_reason":"tool_use","ms":5120}
{"v":1,"seq":6,"type":"message","node":0,"message":{"role":"assistant","content":[...]}}
{"v":1,"seq":7,"type":"cell_start","node":0,"generation":1,"cell":1,"code":"..."}
{"v":1,"seq":8,"type":"node_start","node":1,"parent":0,"depth":1,"kind":"llm","cell":1,"model":"anthropic/claude-sonnet-5-5","prompt":{"blob":"sha256:..."}}
{"v":1,"seq":9,"type":"node_end","node":1,"status":"completed","text":"...","usage":{...},"ms":2210}
{"v":1,"seq":10,"type":"cell_end","node":0,"generation":1,"cell":1,"status":"ok","wall_ms":8120}
{"v":1,"seq":11,"type":"message","node":0,"message":{"role":"user","content":[{"type":"tool_result",...}]}}
{"v":1,"seq":12,"type":"compaction","node":0,"through_seq":11,"message":{...}}
{"v":1,"seq":13,"type":"node_end","node":0,"status":"completed","answer":{"text":"..."},"usage_self":{...},"usage_subtree":{...},"turns":12}
{"v":1,"seq":14,"type":"session_end","status":"completed"}
```

Every model attempt, including failed and cancelled ones, has an `attempt_start` record written and flushed to the OS before the request is sent, and an `attempt_end` record with what was charged (10.2), so session usage can be recomputed from the log alone, including after a crash. Records are flushed to the OS after every line, not fsynced: a crash of kyora loses nothing, a crash of the machine can lose the tail of the log. Leaf `llm` prompts, context values and other payloads above 64 KiB are written to `blobs/` and referenced as `{"blob": "sha256:<hex>", "bytes": N}`. Message records keep their content inline, because resume needs them. `--trace-payloads=none` drops blobs (hashes and sizes only) and therefore covers leaf prompts and context values, but not message content, tool inputs, tool outputs or cell code; `--no-session` writes nothing at all (no trace, no resume).

### 11.3 Resume (M2)

M1 writes resume-ready logs; `kyora resume <session> ["message"]` arrives in M2:

- Acquire the session lock. If the last line is incomplete (no trailing newline or not parseable), truncate only that line; corruption anywhere else aborts.
- Scan all records: the next node id is one above the highest seen; session usage and admission counters are recomputed from `attempt_end` and `node_start` records of all nodes, and every `attempt_start` without an `attempt_end` is charged its reservation (a recovery `attempt_end` with outcome `unknown` is appended); limits are per session (the `session_start` limits, possibly raised by a `limits_changed` record when the user passes new limits to `resume`).
- Rebuild the root: `node_start` gives the frozen system prompt and tool specs (so the request prefix is byte-identical to before, which keeps preserved thinking valid and the cache usable), `message` and `compaction` records give the history, and a recorded `drop_block` choice is restored. Unanswered tool calls at the end get `is_error` results ("interrupted").
- Children that were running when the process died get a `node_end` with status `interrupted` and are not restarted.
- The invocation gets a fresh run cancellation token; the REPL restarts as in 8.4.

`kyora fork <session>` (M2) copies the root history into a new session.

## 12. Context management and compaction

First line of defense is structural: large data stays in REPL variables, tool outputs are bounded, and sub-agents keep their own histories, so the root's context grows slowly. Compaction (M2) handles the rest.

- **Trigger:** after a completed round (never between a `tool_use` and its results), when the last request's `input + cache_creation + cache_read + output` exceeds a threshold (default 80% of the model's `max_input_tokens`), or reactively on `model_context_window_exceeded` or a request rejected as too long.
- **Anthropic:** server-side compaction on demand (beta `compact-2026-09-04`, `compaction: {type: "summarize", instructions}`) with the node's frozen system prompt and tools and kyora's instructions. The call goes through the ledger and a model slot like any attempt, and is charged from `usage.iterations` (top-level input and output are zero on these calls). Success means `stop_reason: "compaction"` and exactly one `compaction` block; it is stored as `Opaque` inside an assistant message that becomes the first item of the history, and the beta header is recorded and sent on every later request carrying the block. Any other outcome (HTTP 200 with empty content and `max_tokens`, `tool_use`, `refusal`, `end_turn` or `model_context_window_exceeded`) leaves the history unchanged; kyora retries at most twice (doubling `max_tokens` after `max_tokens`) and then falls back.
- **Other providers, and the fallback:** client-side whole-history summary: one call with the compaction instructions, then the history is replaced by a single user message (summary plus "continue the task"). No turns are kept verbatim, because keep-tail compaction invalidates thinking on providers that bind it (R§4). If the history itself is too large to summarize in one call, kyora summarizes it in chunks with leaf `llm` calls and summarizes the summaries (the recursion machinery, used on its own history).
- **Instructions** ask the summary to keep the task, constraints, decisions, files touched, open sub-agent handles, and the names and meanings of important REPL variables, and forbid tool calls. After compaction kyora appends a fresh variable manifest from the live REPL (`vars`), so the model can rely on its variables after the summary (nano-rlm makes the same point, R§2).
- **Record:** a `compaction` record with the block or summary and `through_seq`, so resume rebuilds the compacted history exactly.

## 13. Isolation

### 13.1 M1: process isolation only

What M1 does:

- Shell commands and the REPL run as separate processes in their own process group, with the node's cwd, a scrubbed environment (allowlist: `PATH`, `HOME`, `LANG`, `LC_*`, `TERM`, `TMPDIR`, `USER`, plus configured extras; provider keys are never passed), and stdin `/dev/null`.
- The REPL boot script lowers its own resource limits before running user code, setting soft and hard together so user code cannot raise them: `RLIMIT_CORE` 0, `RLIMIT_NOFILE` 1024, `RLIMIT_CPU` (a generous backstop), and `RLIMIT_AS` on Linux (default 4 GiB).

What M1 does not do: there is no filesystem or network sandbox. The REPL, the shell and the file tools act with the user's full authority: they can read any file the user can (including credential files on disk), use the network, and start processes that escape their process group. Environment scrubbing only keeps keys out of child environments. Until M3, run kyora only on trusted inputs, or inside a disposable container or VM.

### 13.2 M3: OS sandbox and path policy

Policies: `read-only`, `workspace-write` (default: write to the workspace and temp dirs, network off), `full-access`. One policy object per node is inherited by its children (they can only get narrower policies) and enforced in two places:

- **Host file operations** (`read_file`, `write_file`, `edit_file`, `glob`, `grep`, file preloads requested by code): containment is enforced during the open itself, not by a separate check. On Linux, `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` relative to a directory handle of the allowed root; on macOS (and older Linux kernels), a component-by-component walk from that handle with `O_NOFOLLOW` on every component, resolving symlinks manually and re-checking each target against the policy. Creation uses `O_CREAT | O_EXCL | O_NOFOLLOW` relative to the checked parent handle, and atomic replacement renames within the same checked directory (`renameat`). Sensitive paths are refused for reading as well as writing.
- **Protected metadata:** inside writable roots, `.git/config`, `.git/hooks/` and `.kyora/` stay read-only for tools and sandboxed processes, because writing them is a code-execution path for later commands.
- **Processes** (shell, REPL), by `kyora-sandbox`:
  - macOS: `/usr/bin/sandbox-exec` with a generated Seatbelt profile: deny by default, allow reads except sensitive paths (Seatbelt can deny subpaths of an allowed tree), allow writes to writable roots, deny network (as Codex does, R§3).
  - Linux with bubblewrap: `--ro-bind / /`, every sensitive path masked with an empty `--tmpfs` or a `/dev/null` bind, writable binds for writable roots, fresh `/tmp`, `--unshare-net`, `--unshare-pid`, plus a seccomp filter denying network sockets. Codex defaults to bubblewrap (R§3).
  - Linux without bubblewrap, Landlock: Landlock can only allow paths, not deny subpaths, so read access is limited to an allowlist (system directories such as `/usr`, `/lib`, `/bin`, `/etc`, the interpreter's prefix, the workspace and temp dirs); the home directory outside the workspace is not readable at all. If an allowed root contains a sensitive path or protected metadata that must stay read-only, Landlock cannot express the policy and the process is refused (fail closed). Network denied by seccomp.
- **Sensitive paths** (default list, configurable): `~/.ssh`, `~/.aws`, `~/.config/gh`, `~/.gnupg`, `~/.kyora` (except the session's own runtime dir), `~/.claude`, `~/.codex`, keychains and browser profiles.
- **Fail closed.** If a policy cannot be enforced on the platform, the operation or process is refused, never run unsandboxed.

The REPL always runs with network denied in `workspace-write`, independent of the shell's network setting.

### 13.3 Execution backends

```rust
#[async_trait]
pub trait ExecBackend: Send + Sync {
    async fn spawn_repl(&self, spec: ReplSpawn) -> Result<ReplProcess>;   // byte streams + kill handle
    async fn run_command(&self, spec: CommandSpec) -> Result<CommandOutput>;
}
```

`LocalBackend` is the only implementation now. Because the REPL protocol is a byte stream on stdin and stdout, a remote backend only has to provide those streams (section 22).

## 14. Observability

- **Trace tree.** `kyora trace <session>` reconstructs the node tree from `events.jsonl`:

  ```
  #0 agent root  claude-opus-5-5  completed  14 turns  4.1M tok (11.2M subtree)  3m12s
  |-- #1 llm   claude-sonnet-5-5  completed  41k tok  9s        (cell 2)
  |-- #2 agent "resolve dates"  depth 1  completed  6 turns  880k tok  41s  (cell 3)
  |   `-- #5 llm  completed  12k tok  3s
  `-- #3 agent "resolve dates"  depth 1  budget_exhausted  500k tok  (cell 3)
  ```

  M1 ships this tree view with `--json`. Per-node transcripts (`--node <id>`) and cell code (`--cells`) follow in M2. The rlm visualizer and nano-rlm's semantic edges (`subagent_call`, `subagent_return`) informed this (R§1, R§2).
- **Live events.** The same tree is available live via `TraceEvent`s for the TUI and `kyora run --json`.
- **Logs.** `tracing` with `KYORA_LOG` (env-filter syntax) to stderr or `$KYORA_HOME/logs/`. API keys and auth headers are never logged.
- **Later.** OpenTelemetry spans per node; trajectory export (prompt and completion per node, plus causal edges) for training pipelines like prime-rl (R§2). Not built now; the JSONL keeps enough to derive both.

## 15. TUI

M4, optional. `kyora` with no arguments on a TTY opens it. Built with ratatui and crossterm as a pure consumer of `TraceEvent`s plus a sender of operations (`UserTurn`, `Interrupt`, `CancelNode(id)`, `Approve(id, decision)`), the same split Codex uses between its TUI and core (R§3).

Layout:

```
+-- transcript (focused node) ----------------------+-- recursion tree ------------+
| > summarize the incidents in context               | #0 root            running  |
| python  cell 3                                     | |- #1 llm          done     |
|   chunks = [...]                                   | |- #2 agent dates  running  |
|   notes = kyora.llm_batch(...)                     | |  `- #5 llm       running  |
| [kyora] 412 llm calls, 9.8M tok                    | `- #3 agent dates  budget!  |
+----------------------------------------------------+-----------------------------+
| > composer                                        tokens 11.2M/20M  depth 1/2   |
+----------------------------------------------------------------------------------+
```

- The tree pane updates live (status, turns, tokens per node); selecting a node switches the transcript pane to that node; `c` cancels the selected subtree; `Esc` interrupts the root turn.
- Cells render code and bounded output; sub-agent spawns link to their node.
- Approval prompts (once permission modes exist) are modal in the bottom pane.
- Streaming text is committed line by line, as Codex does, to avoid re-rendering the whole transcript.

## 16. CLI surface

M1:

```
kyora run [OPTIONS] <TASK>       non-interactive; final answer on stdout
  -m, --model <provider/model>     root model
      --llm-model <provider/model> default model for kyora.llm
      --agent-model <provider/model>  default model for sub-agents (default: parent's)
      --effort <low|medium|high|xhigh|max>
      --context-file <PATH>        preload REPL variable `context`
      --var <NAME=@PATH|NAME=VALUE>  preload other variables (repeatable)
      --max-depth <N>              --budget <TOKENS>         --max-turns <N>
      --max-agents <N>             (total per session)       --max-live-agents <N>
      --timeout <DURATION>         root deadline (default 2h)
      --no-repl  --no-llm          ablations
      --tools <list>               restrict the root's tools
  -C, --cd <DIR>                   working directory
      --json                       NDJSON events on stdout instead of the answer
      --show-thinking              request summarized thinking and print it to stderr
      --quiet                      no progress output on stderr
      --no-session                 do not write a session log
kyora sessions [--json]            list sessions
kyora trace <SESSION> [--json]     recursion tree of a session
```

M2 adds `kyora resume <SESSION> [MESSAGE]` (`--last`), `kyora fork`, `trace --node/--cells`, `--trace-payloads <full|none>`, and flags for the remaining limits (`--max-llm-calls`, `--max-inflight`, `--max-output-tokens`, `--cell-timeout`, `--tool-output-chars`). All numeric flags are validated as in 10.1. M4: `kyora` without arguments opens the TUI; until then it prints help.

Exit codes: 0 completed; 1 runtime failure (provider error after retries, internal error); 2 usage error; 3 stopped by a limit (`max_turns`, `budget_exhausted`, `timeout`, `context_exhausted`; the partial answer is printed); 4 refused; 130 interrupted.

Progress goes to stderr: one line per tool call and per node start or end, indented by depth. `--json` replaces both with the event stream.

## 17. Configuration

`$KYORA_HOME/config.toml` (M2), overridden by `KYORA_*` environment variables, overridden by flags. M1 needs only the environment (`ANTHROPIC_API_KEY`, optional `KYORA_HOME`, `KYORA_PYTHON`) and flags.

```toml
model = "anthropic/claude-opus-5-5"
llm_model = "anthropic/claude-sonnet-5-5"
# agent_model = "anthropic/claude-opus-5-5"     # default: inherit from parent
effort = "high"

[limits]
max_depth = 2
budget_tokens = 20_000_000
max_agents_total = 100
max_agents_live = 16
max_inflight_requests = 16

[repl]
python = "python3"            # any interpreter, e.g. a venv with numpy/pandas
cell_timeout_s = 1800
memory_limit_mb = 4096        # Linux RLIMIT_AS

[providers.anthropic]
kind = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com"

[providers.local]
kind = "openai-chat"
base_url = "http://localhost:11434/v1"
api_key_env = ""
```

**Trust classes.** Security settings can only come from the user's own config, environment or flags: provider definitions (`kind`, `base_url`, `api_key_env`, headers), the Python interpreter path, environment allowlist extras, sandbox policy, sensitive paths, writable roots, and limits. A project file (`.kyora/config.toml`, M3) may set only non-security settings (models, effort, prompts, tool selection within the user's policy); security settings found there are ignored with a warning. Otherwise a malicious repository could redirect API keys to its own endpoint or pick the interpreter that runs.

## 18. Security

Threat model: the model and everything it reads are untrusted. Long-context work processes large amounts of third-party text, so prompt injection is expected, and recursion multiplies both cost and the number of actors.

| Risk | M1 | M3 |
|---|---|---|
| Model-written code damages the machine or reads secrets on disk | Not prevented. Separate processes, scrubbed env, rlimits; documented requirement to run on trusted inputs or in a disposable environment. | OS sandbox for shell and REPL, the same policy for host file tools, sensitive paths unreadable, network off by default, fail closed. |
| API keys leak through kyora itself | Keys live only in the host process; never in child environments, prompts, logs or transcripts. | Same, plus the sandbox hides key files and the REPL has no network. |
| Runaway recursion or spend | Atomic admission and pre-dispatch reservations; exact, crash-safe accounting with a stated worst-case overshoot (10.2); depth, agent, call and turn caps; finite deadlines on every wait; cancellation through the whole token tree. | Same, plus dollar budgets. |
| Capability escalation through sub-agents | A child's tools are a subset of its parent's; its budget and deadline are bounded by its ancestors; its depth is parent + 1; request parameters cannot raise any of these. | Policies are inherited and can only narrow. |
| Malicious or buggy REPL traffic | Private pipes only (no listening sockets), framing and progress deadlines, frame, queue, request and batch quotas, schema validation, stale-cell rejection, kill on protocol violation (9.5). | Same. |
| Injection via sub-agent results | Child answers return into Python variables as data; they reach the parent's context only through bounded printed output. | Same. |
| Data at rest | Session files 0600 in 0700 directories, exclusive session lock, `--trace-payloads=none` and `--no-session`; no telemetry. | Same. |
| Untrusted configuration | Only user config, env and flags are read. | Project config limited to non-security settings (17). |
| Supply chain | Python runtime is stdlib-only and embedded; Rust dependencies pinned via `Cargo.lock`. | `cargo deny` (licenses, advisories) in CI from M2. |

## 19. Testing

All tests run without network or API keys. Real-model smoke tests exist but run only with `KYORA_LIVE_TESTS=1` and a key in the environment.

- **Unit (Rust):** SSE parser (chunk boundaries split anywhere), accumulator (including invalid and truncated tool input), retry policy and `retry-after` handling, Anthropic request building (golden JSON per feature: caching, eager streaming, thinking configuration, opaque replay, `usage.iterations` summation), ledger arithmetic (property tests with `proptest`: random concurrent admissions, reservations and settlements, including failures, cancellations and settlements above the reservation, check that dispatch never exceeds headroom, settlement is exact, overshoot closes the scope, and recovery from a log truncated at any line charges each attempt exactly once; simultaneous sibling reservations and spawns exactly at the limit), IPC codec (random frame splits, oversized and malformed frames, stalled frames), output truncation, tool implementations against temp dirs.
- **Provider integration:** a local mock HTTP server (`wiremock`) serving recorded SSE transcripts: tool-use round trips, thinking with signatures, mid-stream `overloaded_error`, 429 with and without `retry-after`, idle timeout, prefix-mismatch rejection and the switch to `drop_block`. Codex tests its core the same way (R§3).
- **Core loop:** `ScriptedProvider` and `FnProvider` drive the loop: invalid tool JSON, `max_tokens` with a tool call (retry, then error result), refusal with pending tool calls, `model_context_window_exceeded` with parseable and with truncated tool input (every admitted call gets a result), several `python` calls in one message where the first commits a final answer, cancellation mid-stream and mid-tool (history stays valid, reservations settled).
- **Live estimator check** (with `KYORA_LIVE_TESTS=1`): reservations versus actual usage on real requests, reported as a ratio.
- **REPL (real `python3`, required in CI, on Python 3.9 and 3.12, Linux and macOS):** a thread created in cell A calling `kyora.llm` during cell B gets `StaleCall`; a successful cell exit with unfinished `llm` work and cell-owned children cancels and settles them while node-owned children keep running; cell state persistence, output capture including subprocess and background output, bounded capture under a flood, exceptions, wall-clock timeout, interrupt during a blocked `kyora.agent` call (the child is cancelled), interrupt arriving between cells, REPL crash and restart notice, staged `final` discarded on a failed cell, every error code mapped to its Python exception, `ValueTooLarge` raised locally, stale-cell rejection for a thread outliving its cell, threads calling `kyora.llm` concurrently, user code writing to fds 1 and 2.
- **Recursion end to end:** scripted trees: root spawns N children that each call `llm_batch`; depth limit; live and total agent limits; budget exhaustion in one subtree while siblings finish; cancellation of a subtree; node-owned handles awaited in a later cell; deterministic trace shape regardless of scheduling (assert on the tree, not on event order).
- **CLI:** `assert_cmd` runs `kyora run --fake-script ...` with `KYORA_HOME` in a temp dir; snapshot tests (`insta`) of `kyora trace` output and of `--json` event streams with timestamps and durations redacted.
- **Python unit tests** for the `kyora` package run with `python3 -m unittest` against an in-process fake host, invoked from a Rust test so `cargo test` covers them.
- **CI:** fmt, clippy `-D warnings`, tests on ubuntu-latest and macos-latest, `cargo deny` from M2.

## 20. npm distribution

M5, following Codex's model (R§3) without publishing anything until the owner decides:

- `npm/kyora/`: the `kyora` package with `bin/kyora.js`, a small Node launcher that maps `process.platform` and `process.arch` to a target, resolves the matching optional dependency, and execs the binary with stdio inherited and signals forwarded.
- Per-platform packages, one per target: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin` (Windows when supported). Names are an open question (section 23).
- The optional dependencies are injected by a packaging script at release time, as Codex does, so the checked-in manifest stays clean.
- A release workflow builds static musl Linux and macOS binaries on tag, runs the test suite against each artifact, and produces npm tarballs as build artifacts. Publishing is a separate, manual, owner-run step.
- The old `kyora@0.0.3` on npm is superseded by the first real release; nothing is unpublished.

## 21. Milestones

**M1: core loop and recursion (local, Anthropic).** Delivered in three stages, each a small set of PRs with its own tests:

- **M1.1 Loop.** Stateless scripted fake provider; Anthropic provider (streaming, SSE, retries with per-attempt settlement, caching, thinking configuration and opaque replay, `usage.iterations`, model info); `kyora-core` with the runtime, the sequential agent loop and admission rules of 5.2, the ledger with atomic admission, reservation estimates and write-ahead attempt settlement, run and node deadlines, the cancellation tree, the trace sink and JSONL session writer with the session lock; tools `shell`, `read_file`, `write_file`, `edit_file`; `kyora run` and `kyora sessions`.
- **M1.2 REPL.** The `python` tool: process spawn and boot, fd remapping and drained output, IPC codec, pumps, watchdog and bounds of 9.5, cells with generations, context-captured cell ownership, cell exit cleanup and wall-clock timeouts, `kyora.llm`, `llm_batch`, `final`, `budget`, `log`; variable preloading (`--context-file`, `--var`).
- **M1.3 Recursion.** `kyora.agent`, `spawn`, handles and `gather`; cell-owned and node-owned children; node shutdown sequence; depth, live and total agent limits; subtree budgets; `kyora trace` tree view; the three-level scripted recursion test; a manual smoke run against the real API.

The four file and shell tools and `kyora sessions` stay in M1 because M1 is defined to include basic tools and JSONL sessions; they are small and independent of the recursion work.

Exit criterion for M1: a scripted three-level recursion (root, children with `llm_batch`, grandchildren) runs end to end in CI with exact accounting (the trace's per-node charges sum to the session total), and a manual smoke run against the real API succeeds.

**M2: providers, context and sessions.** OpenAI Responses, Chat Completions for compatible and local servers, `apply_patch` for GPT models; config file; compaction (Anthropic server-side, client-side elsewhere); `kyora resume` and `fork`; parallel read-only tool dispatch; `glob` and `grep`; refusal fallback with replay projection; task budget passthrough; `trace --node/--cells`; `cargo deny`.

**M3: isolation and integrations.** OS sandbox and path policy (13.2); permission modes; MCP client (stdio and Streamable HTTP via `rmcp`); structured outputs (`schema=` for `llm` and `agent`); dollar budgets with a price table; content-addressed blobs for large values over IPC; project config with trust classes.

**M4: TUI.** Interactive sessions, live recursion tree, node inspection and cancellation, interactive approvals, `kyora` without arguments opens it.

**M5: distribution and evaluation.** npm launcher and per-platform packages, release workflow (artifacts only), evaluation harness with small long-context tasks (needle-in-haystack and aggregation style, after S-NIAH and OOLONG, R§1) comparing no REPL, REPL without sub-calls, and depths 1 to 3, reporting accuracy, tokens and tail latency.

## 22. Later: kyora cloud and CLI unification

Not built now; recorded so current interfaces do not block it.

- **kyora vms as an execution backend.** A `VmsBackend` implementing `ExecBackend`: the REPL runs inside a kyora vm and the IPC protocol runs over the sandbox exec session's stdin and stdout (the vms client already streams stdin for `exec`); the workspace is synced with the existing `session up` / `pull` mechanics. Sub-agents can each get their own VM for wide fan-out. Model credentials stay on the host; VMs receive none.
- **One CLI.** The `kyora` npm package becomes the entry point for all kyora tools. Proposal: Git-style external subcommands, where `kyora vms ...` and `kyora switch ...` exec `kyora-vms` and `kyora-switch` found on `PATH` (today TypeScript CLIs on Bun), with `kyora help` listing what is installed. A later step can port them into the Rust binary. This avoids bundling a JavaScript runtime into the Rust binary.

## 23. Open questions

1. **Default models (provisional).** Proposal: root `claude-opus-5-5`, `kyora.llm` defaults to `claude-sonnet-5-5`, sub-agents inherit the parent's model. Acceptable, or should sub-calls default to the root model?
2. **Default limits (provisional).** Proposal: `max_depth` 2, `budget_tokens` 20M processed tokens per session (cache reads included, so the bound is hard), 100 agents total, 16 live, 2 h run timeout. The research suggests depth 1 is the safe baseline and deeper recursion is model dependent (R§1).
3. **Milestone order.** M2 (OpenAI and local providers, compaction, resume) before M3 (OS sandbox and MCP), or sandbox first? M1 runs model-written code without a sandbox (13.1).
4. **npm names.** Per-platform package names, for example `@kyora-sh/kyora-darwin-arm64`, and whether `kyora` should later also dispatch to the switch and vms CLIs as proposed in section 22.
5. **Data directory.** Share `~/.kyora` with kyora-switch (sessions under `~/.kyora/sessions`), or use a separate directory?
6. **Auth.** M1 uses API keys from the environment only. Should subscription logins (as managed by kyora-switch for other tools) ever be a provider auth source? This needs a terms-of-service check first.
7. **Python.** System `python3` (3.9+) by default with a configurable interpreter, or should kyora manage an interpreter (for example via `uv`)?
8. **REPL by default.** The `python` tool is enabled for every `kyora run` unless `--no-repl`. Confirm.
9. **TUI priority.** The TUI is planned as M4 and optional; confirm it stays after providers and isolation.
