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
- First-class recursion: plain sub-calls and full sub-agents from Python code, with depth, token, agent-count and concurrency limits that hold for the whole tree, deterministic cancellation, and a trace of the recursion tree.
- Anthropic Messages API first (Claude models), then OpenAI Responses and Chat-Completions-compatible endpoints including local servers.
- Deterministic tests: every behavior above is testable with fake providers and no network.
- Execution backends are pluggable so the REPL and sub-agents can later run in kyora vms (section 22). Local process execution is the only backend built now.

Non-goals for now: Windows support (the REPL process model is Unix-only until a later milestone), a plugin system, a web UI, training integration (trajectory export for RL is noted in section 14 but not built), subscription-login auth.

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

Every node (agent or leaf LLM call) is created by the Rust `Runtime`, charged to a shared `Ledger`, gets a child cancellation token, and is recorded in the session trace. A Python REPL never talks to a model provider directly: it asks its host over a private pipe, and the host applies limits, routes the call, and returns the result. This mirrors the host-owned brokers in every surveyed RLM runtime (R§1, R§2) and keeps credentials out of the sandbox.

## 3. Crate layout

Directory names are short; package names carry the `kyora-` prefix.

| Dir | Package | Purpose | Depends on |
|---|---|---|---|
| `crates/protocol` | `kyora-protocol` | IO-free serde types: messages, content blocks, tool specs, usage, stream events, trace events, node ids. | serde |
| `crates/providers` | `kyora-providers` | `ModelProvider` trait, stream `Accumulator`, SSE parser, retry policy, Anthropic provider; later OpenAI Responses and Chat Completions; `fake` providers. | protocol |
| `crates/core` | `kyora-core` | `Runtime`, agent loop, `Tool` trait and registry, ledger and limits, cancellation tree, trace sink, session store, compaction, prompts. | protocol, providers |
| `crates/tools` | `kyora-tools` | Built-in tools: `shell`, `read_file`, `write_file`, `edit_file`, `glob`, `grep`. | core |
| `crates/repl` | `kyora-repl` | The `python` tool: REPL process host, IPC codec, embedded Python package (`python/kyora/`), result formatting. | core |
| `crates/sandbox` | `kyora-sandbox` | M3. Seatbelt profiles (macOS), bubblewrap and seccomp or Landlock (Linux), applied to shell and REPL processes. | (none) |
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

The canonical model is close to Anthropic's content blocks because that is the strictest wire format (thinking signatures, single tool-result message per turn). `Opaque` carries anything a provider needs back byte-for-byte that kyora does not interpret: Anthropic `redacted_thinking`, `compaction` and fallback blocks, OpenAI `reasoning` items with `encrypted_content`. A provider replays `Opaque` blocks whose `provider` matches its own and drops the rest; histories therefore stay portable across providers at the cost of losing foreign reasoning.

Identifiers:

- `SessionId`: UUIDv7 (time-ordered, so directory listings sort by creation time).
- `NodeId`: `u32`, allocated sequentially per session; node `0` is the root. Short ids keep traces readable (`#7`).
- `CellId`: `u32` per REPL.

`Usage { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens }`. The ledger's "budget tokens" for a call are `input + cache_creation + output`; cache reads are tracked and reported but not charged to the token budget, because every agent turn re-reads its whole history and charging cached re-reads at full weight would make budgets meaningless. A dollar budget (M3) prices all four fields.

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
    ledger: Ledger,                         // tree-wide accounting
    model_slots: Semaphore,                 // in-flight model requests
    trace: TraceSink,                       // JSONL writer + broadcast channel
    ids: AtomicU32,
    root_cancel: CancellationToken,
}

pub trait ToolsetFactory: Send + Sync {
    fn toolset(&self, node: &NodeInfo, selection: &ToolSelection) -> Result<Toolset>;
}

pub struct NodeCtx {          // handed to tools; the only door to recursion
    pub id: NodeId, pub parent: Option<NodeId>, pub depth: u32,
    pub cancel: CancellationToken, runtime: Runtime, budget: BudgetScope,
}
impl NodeCtx {
    pub async fn llm(&self, call: LlmCall) -> Result<LlmOutcome, RecursionError>;
    pub fn spawn_agent(&self, spec: AgentSpec) -> Result<AgentHandle, RecursionError>; // fail-fast on limits
    pub fn budget(&self) -> BudgetSnapshot;
}
```

An `AgentSpec` holds the task, optional name, model reference, tool selection, preloaded REPL variables, and per-node limits (max turns, token budget, timeout). `AgentHandle` exposes `result().await -> AgentOutcome`, `status()`, `cancel()`. `AgentOutcome { node, status, answer, usage_self, usage_subtree, turns }` where `answer` is `Answer::Text(String)` or `Answer::Value(serde_json::Value)` (from `kyora.final`), and `status` is one of `completed`, `max_turns`, `budget_exhausted`, `timeout`, `cancelled`, `refused`, `failed`.

The root of a session is an agent node like any other; `kyora run` creates it with depth 0 and submits one user turn. An interactive session (TUI, `kyora resume`) submits further user turns to the same root node.

### 5.2 One turn

```
on user input:
  history.push(user message)                         # append-only, recorded
  loop:
    if cancelled            -> stop(cancelled)
    if turns >= max_turns   -> stop(max_turns)
    maybe_compact(history)                           # only between rounds, section 12
    req = ModelRequest { system: frozen, tools: frozen, messages: history, ... }
    reservation = ledger.reserve(node, estimate(req))?    -> stop(budget_exhausted) on error
    permit = model_slots.acquire()                   # released when the stream ends
    resp = provider.stream(req) | accumulate         # deltas -> live events
    ledger.commit(reservation, resp.usage)
    history.push(assistant message = resp.content)   # recorded verbatim, incl. thinking/opaque
    match resp.stop_reason:
      tool_use     -> results = dispatch(tool_uses)  # 5.3
                      history.push(user message = results [+ appended notices])
                      if a tool reported a final answer -> stop(completed, that answer)
      end_turn     -> stop(completed, Answer::Text(last text))
      max_tokens   -> if the message has tool_use blocks: return is_error results
                      ("input truncated, retry with smaller input"); else append a
                      user text "continue" and loop
      pause_turn   -> loop (re-send as is)
      refusal      -> stop(refused, stop_details)
      other        -> stop(failed)
```

Invariants:

- **Append-only history.** Nothing already sent is edited, reordered or removed. System prompt and tool list are frozen when the node starts. Dynamic information (budget status, "REPL was restarted", limit warnings) is appended: as a text block after the tool results in the same user message, or as a mid-conversation `role: "system"` message where the provider supports it. This is what Anthropic's preserved-thinking check requires (R§4) and also what keeps prompt caching effective.
- **Every tool_use gets exactly one tool_result**, in the order of the tool_use blocks, in a single user message. On cancellation, unfinished tools get `is_error` results ("cancelled") so the history stays valid for resume (Codex normalizes the same way, R§3).
- **Tools never run on a truncated or invalid input.** The accumulator parses tool-input JSON strictly; invalid input, `max_tokens` or `refusal` endings produce error results instead of execution (required with eager input streaming, R§4).

### 5.3 Tool dispatch

Each tool declares an `Effect`: `ReadOnly` or `Mutating`. Within one assistant message, consecutive read-only calls run concurrently; a mutating call waits for everything before it and blocks everything after it (Codex uses an RW lock for the same purpose, R§3). Results are collected in call order. The `python` tool is `Mutating` (cells of one REPL are serial), but recursion inside a cell is concurrent (section 8).

Unknown tool names and schema-invalid inputs return `is_error` results; inputs are validated against the tool's JSON schema before `call`.

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
    async fn model_info(&self, model: &str) -> Result<ModelInfo, ProviderError>;  // context window, max output
}
```

`ModelRequest` carries model, frozen system prompt, tools, messages, `max_tokens`, and provider-neutral options (`effort`, `thinking_display`, `cache`, `task_budget`, `metadata { node, depth }`); each provider maps what it supports and ignores the rest. `collect(stream)` turns a stream into a `ModelResponse` via the shared `Accumulator`, so non-streaming callers (leaf `llm` calls) use the same path.

Model references are `provider/model` (`anthropic/claude-opus-5-5`); a bare model name uses the default provider.

### 6.2 Anthropic (M1)

Raw HTTP (no official Rust SDK) with `reqwest` (rustls) and a small SSE parser of our own (event/data framing, `ping`, mid-stream `error` events).

- Endpoint `POST {base_url}/v1/messages`, headers `x-api-key` from `ANTHROPIC_API_KEY`, `anthropic-version: 2023-06-01`, `anthropic-beta` as needed. `ANTHROPIC_BASE_URL` overrides the base URL (used by tests against a local mock server).
- Always streaming. Client tools carry `eager_input_streaming: true` when they take large inputs (`python.code`, `write_file.content`).
- Caching: system prompt sent as text blocks with `cache_control: {type: "ephemeral"}` on the last block, plus top-level automatic `cache_control` for the message tail. Per-node system prompts are frozen and identical across siblings of the same role (no node ids or timestamps in them), so sibling sub-agents share a cached prefix.
- Thinking: on Claude Opus 5.5 thinking is always adaptive; kyora sends no `thinking` config unless `--show-thinking` (then `display: "summarized"`). Thinking and opaque blocks are stored and replayed verbatim. Requests set `thinking.block_binding.prefix_mismatch_behavior: "error"` (beta `thinking-binding-controls-2026-08-01`) where supported, so any accidental history edit fails loudly in tests; at runtime a mismatch is retried once with `drop_block` and logged as a bug.
- Effort: `output_config.effort` from config or `--effort`; unset means the API default.
- Task budgets: when a node has a token budget, its remaining budget is passed as `output_config.task_budget` (beta `task-budgets-2026-03-13`, minimum 20,000) so the model can pace itself. M2; advisory only, the ledger stays authoritative.
- Refusals: `stop_reason: "refusal"` ends the node with status `refused` and the `stop_details` category. Server-side refusal fallback (`fallbacks: "default"`) is a provider option, on by default for models that support it; served-by information and `fallback` blocks are recorded.
- Retries: up to 4 retries on transport errors, 408, 409, 429, 5xx and 529, honoring `retry-after`, otherwise exponential backoff with full jitter (1 s base, 60 s cap). A 429 without `retry-after` is retried at most once (it may be a spend cap that keeps failing, R§4). A stream that fails mid-way (`error` event, idle timeout of 300 s without any event including `ping`) is retried as a whole; a `stream_reset` event tells frontends to discard partial output. Retries never happen after tool execution started, because tools only run after a complete message.
- Forced `tool_choice` is never used (rejected on current models). Structured output for `kyora.llm(schema=...)` uses `output_config.format` (M3).
- Context windows come from `GET /v1/models/{id}` (`max_input_tokens`, `max_tokens`), cached per process, with a built-in fallback table.

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
    fn effect(&self) -> Effect;               // ReadOnly | Mutating
    fn large_input(&self) -> bool { false }   // enables eager input streaming
    async fn call(&self, input: serde_json::Value, cx: ToolCx<'_>) -> ToolOutput;
}
pub struct ToolCx<'a> { pub node: &'a NodeCtx, pub call_id: &'a str, pub cwd: &'a Path,
                        pub policy: &'a ExecPolicy, pub events: &'a EventTx }
pub struct ToolOutput { pub content: Vec<ToolResultPart>, pub is_error: bool,
                        pub final_answer: Option<Answer> }
```

Built-in tools (M1):

| Tool | Effect | Input | Notes |
|---|---|---|---|
| `shell` | Mutating | `command`, `timeout_s?` | `bash -c` (falls back to `sh`) in the node's cwd, new process group, scrubbed environment, combined stdout/stderr, exit code. Default timeout 120 s, kill the group on timeout or cancel. |
| `read_file` | ReadOnly | `path`, `offset?`, `limit?` | Line-numbered output, default 2,000 lines, long lines clipped; refuses binary files. |
| `write_file` | Mutating | `path`, `content` | Creates parent directories; atomic write via temp file and rename. |
| `edit_file` | Mutating | `path`, `old`, `new`, `replace_all?` | Exact string replacement; `old` must match exactly once unless `replace_all`. |
| `glob` | ReadOnly | `pattern`, `path?` | `ignore`-crate walker honoring `.gitignore`, sorted by mtime, capped. |
| `grep` | ReadOnly | `pattern`, `path?`, `glob?`, `case_insensitive?`, `context?` | ripgrep libraries (`grep-searcher`, `grep-regex`, `ignore`), capped matches. |
| `python` | Mutating | `code`, `timeout_s?` | The REPL (section 8). |

All tool outputs pass through one truncation helper: if the text exceeds the per-tool cap (default 20,000 characters, configurable), keep the head and tail and insert `[... N characters omitted ...]`. Paths are resolved against the node's cwd; with the M3 sandbox, writes outside writable roots are refused before the OS sandbox is even reached.

GPT models are trained on Codex's `apply_patch` grammar (R§3); an `apply_patch` tool is added with the OpenAI providers in M2 and offered only to those models.

MCP (M3): each configured server's tools are registered as `mcp__<server>__<tool>`, `ReadOnly` only when the server marks them `readOnlyHint`. The tool list is resolved when the node starts and stays frozen for that node (append-only rule); servers that appear later are only visible to new nodes.

Default tool sets: the root gets all built-ins plus `python`. Sub-agents get `python`, `read_file`, `glob`, `grep` unless the spawning code asks for more, and can only receive tools their parent has (capability attenuation, section 18).

## 8. The recursive runtime

### 8.1 Model-facing behavior

The model sees one tool, `python`, whose description documents the injected `kyora` module. Each agent node owns at most one REPL process, started lazily on the first `python` call (or eagerly when variables must be preloaded) and kept for the node's lifetime, so variables persist across cells and across compaction. A cell's result is formatted for the model as:

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
[kyora] this cell: 412 llm calls, 0 agents, 1.21M budget tokens; remaining: 3.62M tokens, depth 0 of 2
```

Sections are omitted when empty. Output is bounded twice: inside Python at capture time (head and tail buffers, so a runaway print cannot exhaust memory, R§1) and again by the tool truncation cap. The bounded output is deliberate: it pushes the model to keep data in variables and print summaries, which is the core RLM idea (R§1).

### 8.2 The `kyora` Python API

The module is pre-imported in the REPL namespace. It is plain synchronous Python (works from threads too); no third-party dependencies, Python 3.9 or newer.

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

kyora.final(value) -> None      # set this agent's answer; the agent stops after the cell
kyora.budget() -> dict          # remaining tokens, agents, depth, deadline
kyora.log(message) -> None      # goes to the trace and UI, not to the model
kyora.node_id, kyora.depth, kyora.max_depth   # constants

class KyoraError(Exception)                 # base for recoverable errors
class LimitExceeded(KyoraError)             # depth, agent count; .limit names it
class BudgetExceeded(KyoraError)
class ModelError(KyoraError)                # provider failure after retries, refusal
class AgentFailed(KyoraError)               # .status, .partial (last text), .node_id
class Timeout(KyoraError)
class Cancelled(BaseException)              # not caught by `except Exception`
```

Semantics:

- **`llm`** is a single completion with no tools: a leaf node (`kind: llm`) in the tree. `prompt` is a string or a list of `{"role", "content"}` dicts. Default model is the configured `llm_model` (section 17). Allowed at every depth, including `max_depth`; disabled entirely with `--no-llm` (for ablations, R§1).
- **`llm_batch`** sends one request to the host, which runs the calls concurrently (default `max_concurrency` 8, never above the global in-flight cap) and returns results in input order. With `return_exceptions=True`, failed items are exception instances instead of raising.
- **`agent`** runs a full child agent (`kind: agent`, depth + 1) with its own history, tools, REPL and budget, and blocks until it ends. `context` (any JSON-serializable value) is preloaded as the variable `context` in the child's REPL; `vars` preloads several named variables. The child's first user message contains the task and a manifest of the preloaded variables (name, type, size, a short preview), never the values themselves. Returns the child's answer: the object passed to `kyora.final` in the child, otherwise the child's final assistant text. Non-completed endings raise `AgentFailed`.
- **`spawn`** registers the child and returns immediately; the child runs concurrently. Handles may outlive the cell that created them; all children of an agent are cancelled when that agent ends. `agent(...)` is `spawn(...).result()` with the child cancelled if the wait is interrupted.
- **`final`** records the answer for the current agent. The cell finishes normally, the tool result says the answer was recorded, and the loop ends without another model call. For the root, the value is what `kyora run` prints (strings as-is, other values as JSON).
- **Values crossing the boundary are JSON.** Python converts with `json.dumps` (tuples become lists; anything else raises `TypeError` in the calling code); the cap per value is the IPC frame cap (section 9). Larger data should be written to a file and passed by path (a content-addressed blob store is M3).

A typical long-context cell:

```python
chunks = [context[i:i + 200_000] for i in range(0, len(context), 200_000)]
notes = kyora.llm_batch([f"List every date mentioned:\n\n{c}" for c in chunks])
hard = [i for i, n in enumerate(notes) if "unclear" in n]
fixes = kyora.gather([kyora.spawn("Resolve the dates in this text precisely.",
                                  context=chunks[i], budget=200_000) for i in hard])
```

### 8.3 Host side

`kyora-repl` implements the `python` tool. Per agent node it owns a `ReplHost`: the child process handle, the IPC connection, the current cell, and a table of in-flight requests from Python. When a request arrives:

- `llm` / `llm_batch` -> `NodeCtx::llm` per item (ledger reservation, model slot, trace node, provider call). Batch items run on a `JoinSet` limited by the batch's own semaphore.
- `agent.spawn` -> `NodeCtx::spawn_agent` with an `AgentSpec` built from the request; limits are checked synchronously (depth, live and total agent counts, budget) and violations come back as typed errors. The handle is stored in the host's handle table under the child's node id.
- `agent.result` -> await the handle (optionally with a timeout); `agent.cancel` -> cancel the child's token.
- `final` -> store the answer; the tool output for this cell carries `final_answer`.

The child agent runs the same loop (section 5) as a Tokio task with a toolset built by the `ToolsetFactory`, which gives it its own `python` tool and therefore its own REPL process. Recursion depth is therefore bounded by `max_depth` and process count by the live-agent limit.

### 8.4 Variables and large inputs

- `kyora run --context-file big.txt` (or `--var name=@path`, `--var name=value`) preloads variables into the root REPL. The host computes metadata (bytes, characters, lines, first 500 characters) from the file and puts only the manifest in the first user message; the REPL loads the file itself (`set_var` with a file source) so the content never passes through the prompt.
- Values passed to `kyora.agent(context=...)` travel inline over IPC and are injected into the child REPL with `set_var`.
- REPL state is in-memory only. After `kyora resume` or a REPL crash, the REPL restarts empty, preloaded file variables are reloaded (their path and sha256 were recorded), and a notice listing the lost variables is appended to the conversation. Snapshotting REPL state (for example with `dill`) is deliberately not attempted; Prime and nano-rlm do not reconstruct kernels either (R§2).

## 9. REPL IPC protocol

### 9.1 Transport and framing

The REPL process is `python3 -I -X utf8 <runtime>/kyora_boot.py`, where `<runtime>` is a private temp directory (mode 0700) into which the embedded Python files are written at first use. The protocol runs over the child's stdin (host to REPL) and stdout (REPL to host), so the same protocol works unchanged over any byte stream, including a remote exec session later (section 22).

At startup the boot script duplicates fds 0 and 1 to private, non-inheritable descriptors for the protocol, points fd 0 at `/dev/null`, and points fds 1 and 2 at a spill file. User `print` output is captured per cell at the Python level; output written directly to fds 1 and 2 by subprocesses lands in the spill file and is attached to the current cell. Nothing user code does can write into the protocol stream.

Frames: 4-byte big-endian unsigned length, then UTF-8 JSON (the same framing as the rlm broker and nano-rlm, R§1, R§2). Maximum frame 64 MiB in both directions, configurable; an oversized or malformed frame is a protocol error that kills the REPL (the host never trusts the REPL to recover from its own corruption).

### 9.2 Messages

JSON-RPC 2.0 shapes without the `jsonrpc` member. Requests carry `id` (unique per sender), `method`, `params`; responses carry the same `id` plus `result` or `error {code, message, data?}`; notifications have no `id`. Both sides send requests, so each side tracks its own outstanding ids.

REPL to host, once at startup (notification):

```json
{"method": "ready", "params": {"protocol": 1, "python": "3.12.4", "pid": 4242}}
```

Host to REPL:

| Method | Params | Result |
|---|---|---|
| `init` | `node_id`, `depth`, `max_depth`, `limits` (output caps), `cwd` | `{}` |
| `exec` | `cell`, `code`, `compute_timeout_ms` | `ExecResult` (below) |
| `set_var` | `name`, and either `value` (JSON) or `file {path, format: "text" or "bytes" or "json"}` | `{type, size}` |
| `get_var` | `name`, `max_bytes` | `{json?, repr}` |
| `vars` | | `[{name, type, size}]` |
| `shutdown` | | `{}` then the process exits |
| `interrupt` (notification) | `cell` | |

REPL to host (only while a cell runs):

| Method | Params | Result |
|---|---|---|
| `llm` | `prompt` or `messages`, `system?`, `model?`, `max_tokens?` | `{text, model, usage, stop_reason, node_id}` |
| `llm_batch` | `items: [llm params]`, `max_concurrency?` | `{results: [{ok: llm result} or {error}]}` |
| `agent.spawn` | `task`, `context?`, `vars?`, `tools?`, `model?`, `max_turns?`, `budget?`, `timeout_ms?`, `name?` | `{node_id}` |
| `agent.result` | `node_id`, `timeout_ms?` | `{status, answer, text, turns, usage}` or error |
| `agent.status` | `node_id` | `{status, turns, usage}` |
| `agent.cancel` | `node_id` | `{}` |
| `final` | `value` | `{}` |
| `budget` | | `{tokens_remaining, agents_remaining, depth, max_depth, deadline_ms?}` |
| `log` (notification) | `level`, `message` | |

`ExecResult`:

```json
{"status": "ok", "stdout": "...", "stderr": "...", "omitted": {"stdout": 0, "stderr": 0},
 "result": "repr or null", "error": null,
 "vars_changed": [{"name": "notes", "type": "list[str]", "size": "412 items"}],
 "final": false, "wall_ms": 8120, "compute_ms": 410}
```

`status` is `ok`, `error` (exception in user code; `error` holds `type`, `message`, `traceback`), `interrupted`, or `timeout`.

Error codes host to REPL: `limit_exceeded` (with `data.limit`: `depth`, `agents_live`, `agents_total`, `llm_calls`), `budget_exceeded`, `cancelled`, `timeout`, `model_error`, `agent_failed`, `invalid_request`, `value_too_large`. The Python module maps each to the exception classes in 8.2.

### 9.3 Concurrency inside Python

The boot script runs cells on the main thread. A reader thread owns the protocol input: it routes responses to waiting futures by `id`, queues host requests for the main thread, and handles `interrupt` by failing all pending futures with `cancelled` and calling `_thread.interrupt_main()`. Writes are serialized by a lock. Waits use short timed waits in a loop, so a blocked `kyora.agent()` call notices an interrupt within about 100 ms even when the transport cannot deliver signals. User code may call `kyora.*` from its own threads; each call is an independent request.

### 9.4 Timeouts and interruption

A cell's timeout counts compute time only: the host pauses the cell clock while any request from that cell is outstanding, so a cell that waits ten minutes for sub-agents is not killed for it, while a tight loop is (default 300 s compute). Sub-agents have their own budgets and optional wall-clock timeouts. On timeout or cancellation the host sends `interrupt`; if no `exec` response arrives within 2 s it kills the REPL's process group, marks the cell `timeout` or `interrupted`, and the next `python` call starts a fresh REPL (with the restart notice of 8.4).

## 10. Limits, budgets and cancellation

### 10.1 Limits

| Limit | Default | Scope | Behavior when hit |
|---|---|---|---|
| `max_depth` | 2 | tree | `agent.spawn` fails with `limit_exceeded` |
| `max_agents_total` | 100 | tree | spawn fails |
| `max_agents_live` | 16 | tree | spawn fails (fail-fast, never queues) |
| `max_llm_calls` | 2,000 | tree | `llm` fails |
| `max_inflight_requests` | 16 | process | requests wait for a model slot |
| `budget_tokens` | 2,000,000 | tree, and optionally per subtree | reservation fails; the node stops with `budget_exhausted` |
| `max_turns` | root 200, sub-agent 50 | node | node stops with `max_turns` |
| `agent_timeout` | none | per spawn | child cancelled, `Timeout` raised |
| `cell_compute_timeout` | 300 s | cell | cell interrupted |
| `max_output_tokens` | agents 32,000, llm 16,000 | request | passed as `max_tokens` |
| `tool_output_chars` | 20,000 | tool result | head and tail kept |
| `ipc_frame_bytes` | 64 MiB | REPL | protocol error |

All are configurable (section 17) and overridable per run with flags.

### 10.2 Ledger

The ledger is a tree of `BudgetScope`s mirroring the node tree. Each scope has optional limits and atomic counters (`reserved`, `used`). A child spawned with `budget=N` gets a scope limited to `N`; without it, the child's scope has no own limit and is bounded by its ancestors.

- `reserve(scope, estimate)` succeeds only if `used + reserved + estimate <= limit` holds for the scope and every ancestor; it then adds `estimate` to `reserved` on the whole path. The estimate for a model call is `ceil(request_bytes / 4) + max_tokens`, or, for an agent turn, the previous turn's input plus new bytes / 4 plus `max_tokens`.
- `commit(reservation, usage)` subtracts the reservation and adds the actual budget tokens to `used` on the whole path, so every ancestor always sees the full cost of its subtree (the rlm reference under-aggregates child cost, R§1).
- Overshoot is bounded by estimation error of calls already in flight, never by unchecked dispatch (nano-rlm checks only between calls, R§2).

### 10.3 Why there are no blocking agent slots

A parent that waits for its children must not hold a resource its children need, otherwise a fan-out tree deadlocks (nano-rlm guards this with `max_concurrent_subagents >= max_depth`, R§2). kyora's only blocking semaphore is `max_inflight_requests`, held strictly for the duration of one HTTP request, never across a wait on another node. Agent counts are admission limits that fail fast. Therefore no wait-for cycle can form.

### 10.4 Cancellation

Every node owns a `CancellationToken` that is a child of its parent's (`tokio_util::sync::CancellationToken::child_token`). Cancelling a node cancels its subtree:

- in-flight HTTP streams are dropped;
- running tools get the token (shell kills its process group);
- the node's REPL gets `interrupt`, then a process-group kill after the grace period;
- pending IPC requests from that REPL are answered with `cancelled`;
- the node ends with status `cancelled`, unfinished tool calls get error results, and `node_end` is recorded.

`kyora run`: first Ctrl-C cancels the root gracefully (the session stays resumable); a second Ctrl-C within 2 s exits immediately with code 130.

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

Directories are created 0700 and files 0600: transcripts contain whatever the agent read.

### 11.2 Record format

One JSON object per line, written by a single writer task (records arrive over a channel, so concurrent nodes never interleave partial lines), flushed after every record. Common fields: `v` (format version, 1), `seq` (gapless per session), `ts` (RFC 3339 UTC), `type`, `node` (when applicable).

```json
{"v":1,"seq":0,"ts":"2026-10-04T20:00:00.000Z","type":"session_start","session":"0199...","cwd":"/work","kyora":"0.1.0","limits":{...}}
{"v":1,"seq":1,"type":"node_start","node":0,"parent":null,"depth":0,"kind":"agent","name":"root","model":"anthropic/claude-opus-5-5","system":"...","tools":[...],"limits":{...}}
{"v":1,"seq":2,"type":"var_loaded","node":0,"name":"context","source":{"file":"/work/big.txt","sha256":"...","bytes":48213557}}
{"v":1,"seq":3,"type":"message","node":0,"message":{"role":"user","content":[...]}}
{"v":1,"seq":4,"type":"model_call","node":0,"model":"claude-opus-5-5","request_id":"req_...","usage":{...},"stop_reason":"tool_use","ms":5120,"retries":0}
{"v":1,"seq":5,"type":"message","node":0,"message":{"role":"assistant","content":[...]}}
{"v":1,"seq":6,"type":"cell_start","node":0,"cell":1,"code":"..."}
{"v":1,"seq":7,"type":"node_start","node":1,"parent":0,"depth":1,"kind":"llm","cell":1,"model":"anthropic/claude-sonnet-5-5","prompt":{"blob":"sha256:..."}}
{"v":1,"seq":8,"type":"node_end","node":1,"status":"completed","text":"...","usage":{...},"ms":2210}
{"v":1,"seq":9,"type":"cell_end","node":0,"cell":1,"status":"ok","compute_ms":410,"wall_ms":8120}
{"v":1,"seq":10,"type":"message","node":0,"message":{"role":"user","content":[{"type":"tool_result",...}]}}
{"v":1,"seq":11,"type":"compaction","node":0,"through_seq":10,"message":{...}}
{"v":1,"seq":12,"type":"node_end","node":0,"status":"completed","answer":{"text":"..."},"usage_self":{...},"usage_subtree":{...},"turns":12}
{"v":1,"seq":13,"type":"session_end","status":"completed"}
```

Strings or values above 64 KiB (prompts of leaf calls, context values, large tool results) are written to `blobs/` and referenced as `{"blob": "sha256:<hex>", "bytes": N}`; message records keep content inline so resume never depends on blobs. `--trace-payloads=none` stores hashes and sizes only.

### 11.3 Resume

`kyora resume <session> ["message"]` replays the root node's records: `node_start` gives the frozen system prompt and tool specs (so the request prefix is byte-identical to before, which keeps preserved thinking valid and the prompt cache usable), `message` and `compaction` records rebuild the history. Unanswered tool calls at the end of the log get `is_error` results ("interrupted"). Children that were running when the process died are recorded as `interrupted` and not restarted; their partial results are visible in `kyora trace`. The REPL restarts as described in 8.4. Resuming appends to the same `events.jsonl`; `kyora fork <session>` (M2) copies the root history into a new session.

## 12. Context management and compaction

First line of defense is structural: large data stays in REPL variables, tool outputs are bounded, and sub-agents keep their own histories, so the root's context grows slowly. Compaction (M2) handles the rest.

- **Trigger:** after a completed round (never between a `tool_use` and its results), when the last request's `input + cache_creation + cache_read + output` exceeds a threshold (default 80% of the model's `max_input_tokens`, configurable), or reactively when the provider rejects a request as too long.
- **Anthropic:** server-side compaction on demand (beta `compact-2026-09-04`, `compaction: {type: "summarize", instructions}`) with kyora's own instructions. The returned signed `compaction` block is stored as `Opaque` and becomes the first item of the history; summarized turns are dropped from the request (not from the log). This is the documented way to compact without breaking preserved thinking (R§4).
- **Other providers:** client-side whole-history summary: one call with the compaction instructions, then the history is replaced by a single user message (summary plus "continue the task"). No turns are kept verbatim, because keep-tail compaction invalidates thinking on providers that bind it (R§4).
- **Instructions** ask the summary to keep the task, constraints, decisions, files touched, open sub-agent handles, and the names and meanings of important REPL variables. After compaction kyora appends a fresh variable manifest from the live REPL (`vars`), so the model can rely on its variables after the summary (nano-rlm makes the same point, R§2).
- **Record:** a `compaction` record with the block or summary and `through_seq`, so resume rebuilds the compacted history exactly.

## 13. Isolation

### 13.1 M1: process isolation

- Shell commands and the REPL run as separate processes in their own process group, with the node's cwd, a scrubbed environment (allowlist: `PATH`, `HOME`, `LANG`, `LC_*`, `TERM`, `TMPDIR`, `USER`, plus configured extras; provider keys are never passed), and stdin closed or `/dev/null`.
- The REPL boot script lowers its own resource limits before running user code, setting soft and hard together so user code cannot raise them: `RLIMIT_CORE` 0, `RLIMIT_NOFILE` 1024, `RLIMIT_CPU` (generous, as a backstop for the compute timeout), and `RLIMIT_AS` on Linux (default 4 GiB). No `unsafe` Rust is needed for this.
- The REPL needs no network: model access goes through IPC.

M1 has no filesystem or network sandbox. Until M3, run kyora in a disposable checkout, container or VM.

### 13.2 M3: OS sandbox

Policies: `read-only`, `workspace-write` (default: write to the workspace and temp dirs, network off), `full-access`. Applied per process by `kyora-sandbox`:

- macOS: `/usr/bin/sandbox-exec` with a generated Seatbelt profile (deny by default, allow reads, allow writes to writable roots, deny network), as Codex does (R§3).
- Linux: bubblewrap when available (`--ro-bind / /`, writable binds for roots, `--unshare-net`, fresh `/tmp`), else Landlock, plus a seccomp filter denying network sockets. Codex defaults to bubblewrap and keeps Landlock as an opt-in fallback (R§3).
- Sensitive paths are not readable even in `read-only` mode: `~/.ssh`, `~/.aws`, `~/.config/gh`, `~/.kyora` (except the session's own runtime dir), keychains, browser profiles. The list is configurable.
- If the sandbox cannot be applied, the process is not started (no silent fallback to unsandboxed execution).

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
  #0 agent root  claude-opus-5-5  completed  14 turns  412k tok (2.1M subtree)  3m12s
  |-- #1 llm   claude-sonnet-5-5  completed  41k tok  9s        (cell 2)
  |-- #2 agent "resolve dates"  depth 1  completed  6 turns  88k tok  41s  (cell 3)
  |   `-- #5 llm  completed  12k tok  3s
  `-- #3 agent "resolve dates"  depth 1  budget_exhausted  50k tok  (cell 3)
  ```

  `--json` emits the tree; `--node <id>` prints one node's transcript; `--cells` shows the code that created each child. The rlm visualizer and nano-rlm's semantic edges (`subagent_call`, `subagent_return`) informed this (R§1, R§2).
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
| [kyora] 412 llm calls, 1.2M tok                    | `- #3 agent dates  budget!  |
+----------------------------------------------------+-----------------------------+
| > composer                                         tokens 2.1M/5M  depth 1/2  $  |
+----------------------------------------------------------------------------------+
```

- The tree pane updates live (status, turns, tokens per node); selecting a node switches the transcript pane to that node; `c` cancels the selected subtree; `Esc` interrupts the root turn.
- Cells render code and bounded output; sub-agent spawns link to their node.
- Approval prompts (once permission modes exist) are modal in the bottom pane.
- Streaming text is committed line by line, as Codex does, to avoid re-rendering the whole transcript.

## 16. CLI surface

```
kyora run [OPTIONS] <TASK>       non-interactive; final answer on stdout
  -m, --model <provider/model>     root model (default from config)
      --llm-model <provider/model> default model for kyora.llm
      --agent-model <...>          default model for sub-agents (default: parent's)
      --effort <low|medium|high|xhigh|max>
      --context-file <PATH>        preload REPL variable `context`
      --var <NAME=@PATH|NAME=VALUE>  preload other variables (repeatable)
      --max-depth <N>  --max-agents <N>  --budget <TOKENS>  --max-turns <N>
      --no-repl  --no-llm          ablations
      --tools <list>               restrict the root's tools
  -C, --cd <DIR>                   working directory
      --json                       NDJSON events on stdout instead of the answer
      --show-thinking              request summarized thinking and print it to stderr
      --quiet                      no progress output on stderr
kyora resume <SESSION> [MESSAGE]   continue a session (latest with --last)
kyora sessions [--json]            list sessions
kyora trace <SESSION> [--json] [--node <ID>] [--cells]
kyora                              TUI (M4); prints help until then
```

Exit codes: 0 completed; 1 runtime failure (provider error after retries, internal error); 2 usage error; 3 stopped by a limit (`max_turns`, `budget_exhausted`; the partial answer is printed); 4 refused; 130 interrupted.

Progress goes to stderr: one line per tool call and per node start or end, indented by depth. `--json` replaces both with the event stream.

## 17. Configuration

`$KYORA_HOME/config.toml`, overridden by `KYORA_*` environment variables, overridden by flags. A project file (`.kyora/config.toml`) is read only for non-security settings until a trust mechanism exists (M3).

```toml
model = "anthropic/claude-opus-5-5"
llm_model = "anthropic/claude-sonnet-5-5"
# agent_model = "anthropic/claude-opus-5-5"     # default: inherit from parent
effort = "high"

[limits]
max_depth = 2
budget_tokens = 2_000_000
max_agents_total = 100
max_agents_live = 16
max_inflight_requests = 16

[repl]
python = "python3"            # any interpreter, e.g. a venv with numpy/pandas
cell_compute_timeout_s = 300
memory_limit_mb = 4096        # Linux RLIMIT_AS

[providers.anthropic]
kind = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com"

[providers.local]              # M2
kind = "openai-chat"
base_url = "http://localhost:11434/v1"
api_key_env = ""
```

M1 needs only the environment (`ANTHROPIC_API_KEY`, optional `KYORA_HOME`, `KYORA_PYTHON`) and flags; the config file arrives in M2.

## 18. Security

Threat model: the model and everything it reads are untrusted. Long-context work processes large amounts of third-party text, so prompt injection is expected, and recursion multiplies both cost and the number of actors.

| Risk | Control |
|---|---|
| Model-written code damages the machine | M1: separate processes, scrubbed env, rlimits, documented "run in a disposable environment". M3: OS sandbox, network off by default, sensitive paths unreadable, no silent fallback. |
| Credential exfiltration | Keys live only in the host process; never in tool or REPL environments, prompts, logs or transcripts. The REPL has no network and reaches models only through the host. |
| Runaway recursion or spend | Tree-wide ledger with pre-dispatch reservations, depth, agent and call caps, per-subtree budgets, compute timeouts, cancellation that always reaches the whole subtree. |
| Capability escalation through sub-agents | A child's tools are a subset of its parent's; a child's budget is bounded by its ancestors; a child's depth is parent + 1; none of these can be raised by request parameters. |
| Malicious or buggy REPL traffic | Private pipes only (no listening sockets), frame size caps, schema validation of every message, unknown methods rejected, REPL killed on protocol violation. |
| Injection via sub-agent results | Child answers return into Python variables as data; they reach the parent's context only through bounded printed output. |
| Sensitive data at rest | Session files 0600 in 0700 directories; `--trace-payloads=none` to avoid storing large inputs; no telemetry. |
| Supply chain | Python runtime is stdlib-only and embedded in the binary; Rust dependencies pinned via `Cargo.lock`; CI runs `cargo deny` (licenses, advisories) from M2. |

## 19. Testing

All tests run without network or API keys. Real-model smoke tests exist but run only with `KYORA_LIVE_TESTS=1` and a key in the environment.

- **Unit (Rust):** SSE parser (chunk boundaries split anywhere), accumulator, retry policy and `retry-after` handling, Anthropic request building (golden JSON per feature: caching, eager streaming, thinking replay, opaque blocks), ledger arithmetic with property tests (`proptest`: random reserve and commit sequences never let any scope exceed its limit by more than in-flight estimates), IPC codec (random frame splits, oversized and malformed frames), output truncation, tool implementations against temp dirs.
- **Provider integration:** a local mock HTTP server (`wiremock`) serving recorded SSE transcripts: tool-use round trips, thinking with signatures, mid-stream `overloaded_error`, 429 with and without `retry-after`, idle timeout. Codex tests its core the same way (R§3).
- **Core loop:** `ScriptedProvider` and `FnProvider` drive the loop: parallel read-only tools with ordered results, mutating tool serialization, invalid tool JSON, `max_tokens` with a tool call, refusal, cancellation mid-stream and mid-tool (history stays valid), resume after a crash at every record boundary (truncate the JSONL at each line and resume).
- **REPL (real `python3`, required in CI):** cell state persistence, output capture including subprocess output, bounded capture, exceptions, compute timeout versus waiting time, interrupt during a blocked `kyora.agent` call, REPL crash and restart notice, `final`, every error code mapped to its Python exception, protocol robustness (user code writing to fd 1 and 2, threads calling `kyora.llm` concurrently).
- **Recursion end to end:** scripted trees: root spawns N children that each call `llm_batch`; depth limit; live and total agent limits; budget exhaustion in one subtree while siblings finish; cancellation of a subtree; deterministic trace shape regardless of scheduling (assert on the tree, not on event order).
- **CLI:** `assert_cmd` runs `kyora run --fake-script ...` with `KYORA_HOME` in a temp dir; snapshot tests (`insta`) of `kyora trace` output and of `--json` event streams with timestamps and durations redacted.
- **Python unit tests** for the `kyora` package run with `python3 -m unittest` against an in-process fake host, invoked from a Rust test so `cargo test` covers them.
- **CI:** fmt, clippy `-D warnings`, tests on ubuntu-latest and macos-latest (Python 3.12 installed explicitly), `cargo deny` from M2.

## 20. npm distribution

M5, following Codex's model (R§3) without publishing anything until the owner decides:

- `npm/kyora/`: the `kyora` package with `bin/kyora.js`, a small Node launcher that maps `process.platform` and `process.arch` to a target, resolves the matching optional dependency, and execs the binary with stdio inherited and signals forwarded.
- Per-platform packages, one per target: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin` (Windows when supported). Names are an open question (section 23).
- The optional dependencies are injected by a packaging script at release time, as Codex does, so the checked-in manifest stays clean.
- A release workflow builds static musl Linux and macOS binaries on tag, runs the test suite against each artifact, and produces npm tarballs as build artifacts. Publishing is a separate, manual, owner-run step.
- The old `kyora@0.0.3` on npm is superseded by the first real release; nothing is unpublished.

## 21. Milestones

**M1: core loop and recursion (local, Anthropic).**
Workspace and CI (done in the scaffold PR); protocol types; Anthropic provider (streaming, retries, caching, thinking and opaque replay, model info); stateless scripted fake provider; agent loop with parallel read-only tools; built-in tools (`shell`, `read_file`, `write_file`, `edit_file`, `glob`, `grep`); JSONL sessions with resume; `kyora run`, `resume`, `sessions`, `trace`; the `python` tool with the IPC protocol and the `kyora` module (`llm`, `llm_batch`, `agent`, `spawn`, `gather`, `final`, `budget`, `log`); ledger and limits (depth, agents, calls, tokens, turns, compute timeout); cancellation tree; process isolation; the test suite of section 19 (minus later features); docs. Exit criterion: a scripted three-level recursion runs end to end in CI, and a manual smoke run against the real API succeeds.

**M2: providers and context.** OpenAI Responses, Chat Completions for compatible and local servers, `apply_patch` for GPT models; config file; compaction (Anthropic server-side, client-side elsewhere); task budget passthrough; `kyora fork`; `cargo deny`.

**M3: isolation and integrations.** OS sandbox (Seatbelt, bubblewrap with seccomp, Landlock fallback) for shell and REPL; permission modes; MCP client (stdio and Streamable HTTP via `rmcp`); structured outputs (`kyora.llm(schema=...)`, `kyora.agent(schema=...)`); dollar budgets with a price table; content-addressed blobs for large values over IPC; project config trust.

**M4: TUI.** Interactive sessions, live recursion tree, node inspection and cancellation, interactive approvals, `kyora` without arguments opens it.

**M5: distribution and evaluation.** npm launcher and per-platform packages, release workflow (artifacts only), evaluation harness with small long-context tasks (needle-in-haystack and aggregation style, after S-NIAH and OOLONG, R§1) comparing no REPL, REPL without sub-calls, and depths 1 to 3, reporting accuracy, tokens and tail latency.

## 22. Later: kyora cloud and CLI unification

Not built now; recorded so current interfaces do not block it.

- **kyora vms as an execution backend.** A `VmsBackend` implementing `ExecBackend`: the REPL runs inside a kyora vm and the IPC protocol runs over the sandbox exec session's stdin and stdout (the vms client already streams stdin for `exec`); the workspace is synced with the existing `session up` / `pull` mechanics. Sub-agents can each get their own VM for wide fan-out. Model credentials stay on the host; VMs receive none.
- **One CLI.** The `kyora` npm package becomes the entry point for all kyora tools. Proposal: Git-style external subcommands, where `kyora vms ...` and `kyora switch ...` exec `kyora-vms` and `kyora-switch` found on `PATH` (today TypeScript CLIs on Bun), with `kyora help` listing what is installed. A later step can port them into the Rust binary. This avoids bundling a JavaScript runtime into the Rust binary.

## 23. Open questions

1. **Default models.** Proposal: root `claude-opus-5-5`, `kyora.llm` defaults to `claude-sonnet-5-5`, sub-agents inherit the parent's model. Acceptable, or should sub-calls default to the root model?
2. **Default limits.** Proposal: `max_depth` 2, `budget_tokens` 2M per run (cache reads not counted), 100 agents total, 16 live. The research suggests depth 1 is the safe baseline and deeper recursion is model dependent (R§1).
3. **Milestone order.** M2 (OpenAI and local providers, compaction) before M3 (OS sandbox and MCP), or sandbox first?
4. **npm names.** Per-platform package names, for example `@kyora-sh/kyora-darwin-arm64`, and whether `kyora` should later also dispatch to the switch and vms CLIs as proposed in section 22.
5. **Data directory.** Share `~/.kyora` with kyora-switch (sessions under `~/.kyora/sessions`), or use a separate directory?
6. **Auth.** M1 uses API keys from the environment only. Should subscription logins (as managed by kyora-switch for other tools) ever be a provider auth source? This needs a terms-of-service check first.
7. **Python.** System `python3` (3.9+) by default with a configurable interpreter, or should kyora manage an interpreter (for example via `uv`)?
8. **REPL by default.** The `python` tool is enabled for every `kyora run` unless `--no-repl`. Confirm.
9. **TUI priority.** The TUI is planned as M4 and optional; confirm it stays after providers and isolation.
