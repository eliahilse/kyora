# Research notes

Background research for kyora's design ([design.md](design.md)). Collected on 2026-10-04.

Method: primary sources only (papers, official documentation, source code). Repositories were shallow-cloned and read at the commit SHAs listed in each section; code links pin those SHAs. Statements that could not be checked against a source are marked **unverified**. Benchmark numbers are copied from the cited papers and posts, not reproduced. Sections end with "Implications for kyora", which are inferences, not findings.

Contents:

1. [Recursive Language Models: paper and reference implementation](#1-recursive-language-models-paper-and-reference-implementation)
2. [Prime Intellect: RLM environments and harnesses](#2-prime-intellect-rlm-environments-and-harnesses)
3. [Rust harness patterns: Codex CLI, opencode, Claude Code, MCP](#3-rust-harness-patterns-codex-cli-opencode-claude-code-mcp)
4. [Anthropic Messages API: constraints that shape the provider and the loop](#4-anthropic-messages-api-constraints-that-shape-the-provider-and-the-loop)
5. [Cross-cutting conclusions for kyora](#5-cross-cutting-conclusions-for-kyora)

## 1. Recursive Language Models: paper and reference implementation

### Summary

RLM moves the large input into a persistent programming environment. The root model selects what to inspect, computes over variables, and calls models on selected inputs. External context storage and recursive model calls are independently useful: the paper's no-sub-call ablation sometimes wins. For kyora, the useful architecture is a stateful Python worker plus a host-owned model-call service, with explicit completion, concurrency, accounting, and isolation contracts. The reference code provides these components, but its current interface differs from the original experiments. [Paper v1, §§1-3][rlm-p1]; [current runtime][rlm-core].

### Sources, dates, and verification scope

The paper is **“Recursive Language Models”**, by **Alex L. Zhang, Tim Kraska, and Omar Khattab**, arXiv **2512.24601**. There are exactly three listed authors. Version 1 was submitted **31 December 2025, 03:43:41 UTC**; v2 followed on **28 January 2026**, and v3 on **11 May 2026**. The historical results below use v1 unless explicitly marked otherwise. [arXiv record][rlm-abs].

The original author post is also **“Recursive Language Models”**, by **Alex Zhang**, published **15 October 2025**, according to its HTML front matter. The live post now includes links and updates about the subsequent paper, so it is not an untouched October snapshot. [Author blog][rlm-blog].

Both repositories were shallow-cloned and inspected. All code links below pin the commits returned by `git rev-parse HEAD`:

- `alexzhang13/rlm`: `d04208afbad29ca675ab13478c40ee8bebc84bfe`. [Repository snapshot][rlm-repo].
- `alexzhang13/rlm-minimal`: `973f8d4acf3af2c86dc170af91607bf8b0c4d0ea`. [Repository snapshot][rlm-minimalrepo].

This is a static source audit. Benchmark reruns and live sandbox behavior are **unverified**. The current main repository is not asserted to be the exact code used for the 2025 experiments. [Current README][rlm-readme]; [minimal README][rlm-minimalreadme].

The official distribution is **`rlms`**, imported as **`rlm`**. The inspected project declares version **0.1.3**, Python **>=3.11**, and the author repository as its homepage; PyPI independently reports the same identity. Do not assume `pip install rlm` is the authors' installation command. [pyproject.toml][rlm-project]; [PyPI metadata][rlm-pypi].

### Core interface and recursion semantics

The large prompt is available as `context` inside a persistent Python REPL. Initially, the root receives environment instructions and context metadata rather than the full payload. It can print prefixes, search with regexes, partition documents, classify chunks through sub-calls, and combine results in variables. Subsequent observations can contain actual selected input text, so “only metadata” describes initial access, not every root-model turn. [Paper v1, §1 and Appendix D.1][rlm-p1]; [prompt construction][rlm-prompts].

The original blog and paper use fenced `repl` code blocks, `llm_query`, and textual `FINAL(...)` or `FINAL_VAR(variable_name)` termination. `FINAL` returns literal answer text; `FINAL_VAR` returns the string value of a REPL variable, allowing output assembled over many calls to exceed one model response's length. These are harness-parsed termination tags, not both ordinary Python functions. The minimal implementation additionally installs a Python `FINAL_VAR` helper, while its host parser resolves the variable directly. [Blog][rlm-blog]; [minimal repl.py][rlm-minimalrepl]; [minimal utils/utils.py][rlm-minimalutils].

The current main implementation exposes the following LocalREPL interface. These names should not be retroactively attributed to the original paper's experimental prompt. [local_repl.py][rlm-local]; [utils/prompts.py][rlm-prompts].

| Surface | Current behavior |
|---|---|
| `context` | Loaded payload, including string, list, or dictionary inputs |
| `llm_query(prompt, model=None)` | One plain LM completion, returning a string |
| `llm_query_batched(prompts, model=None)` | Concurrent plain completions, returning strings in input order |
| `rlm_query(prompt, model=None)` | Child RLM with its own REPL when configured; otherwise plain-call fallback |
| `rlm_query_batched(prompts, model=None)` | Concurrent child RLMs, with input-order results and plain-call fallback |
| `SHOW_VARS()` | Variable names and types |
| `answer` | Completion dictionary, surfaced through `REPLResult.final_answer` |

The current completion idiom, reproduced from the runtime contract, is:

```python
answer["content"] = result
answer["ready"] = True
```

LocalREPL captures completion through `_AnswerDict`; the host loop reads `final_answer` from executed code-block results. It does not use the original `FINAL` tag parser. `RLM.completion(prompt, root_prompt=None)` returns an `RLMChatCompletion` object containing `.response`, usage, execution time, and optional trajectory metadata. `root_prompt` lets callers expose a short question separately from the stored payload. [local_repl.py][rlm-local]; [core/rlm.py][rlm-core]; [core/types.py][rlm-types].

In **v1**, maximum experimental recursion depth was **one**: root depth zero could call LMs, whose calls had no child REPL. GPT-5, with medium reasoning, was the root and GPT-5-mini the sub-model. Qwen3-Coder-480B-A35B was the other evaluated backbone; §2.2 explicitly identifies a distinct sub-model only for GPT-5, so a separate Qwen sub-model configuration is **unverified** here. The blog explored GPT-5 and GPT-5-mini configurations. [Paper v1, §§2.2, 5][rlm-p1]; [Blog][rlm-blog].

In current code, `max_depth=1` preserves plain sub-calls; `max_depth>1` enables child RLMs for Local, IPython, and Docker. `_subcall` creates children until the next depth reaches the cap, then calls a plain model. The two query families distinguish inexpensive one-shot processing from iterative child reasoning. [core/rlm.py, `_spawn_completion_context`, `_subcall`][rlm-core]; [local_repl.py][rlm-local].

### Benchmarks, results, and observed strategies

V1 evaluates S-NIAH, BrowseComp-Plus, OOLONG, OOLONG-Pairs, and the **CodeQA subset of LongBench-v2**, rather than reporting a whole-LongBench aggregate. S-NIAH requires one needle; BrowseComp-Plus requires multi-document evidence; OOLONG requires semantic transformation and aggregation across entries; OOLONG-Pairs adds pairwise aggregation. The paper characterizes their processing requirements as roughly constant, linear, or quadratic with input size. It uses 50 S-NIAH tasks, 50 OOLONG tasks on `trec_coarse`, 20 constructed pairwise queries, and 150 BrowseComp-Plus tasks with 1,000 documents each, including guaranteed gold/evidence documents. [Paper v1, §2.1][rlm-p1].

The following values are copied from **v1 Table 1**. Each cell is **score / mean API dollars per query**. CodeQA and BrowseComp scores are percentage correct; OOLONG uses its benchmark scoring rule, including exponential penalties for numerical error; OOLONG-Pairs reports F1. Dollar values are historical experimental costs, not current provider prices. [Paper v1, §2.1 and Table 1][rlm-p1].

| Method | CodeQA, 23K-4.2M tokens | BrowseComp+, 6M-11M | OOLONG, 131K | OOLONG-Pairs, 32K |
|---|---:|---:|---:|---:|
| GPT-5 base | 24.00 / $0.13* | 0.00 / N/A* | 44.00 / $0.14 | 0.04 / $0.16 |
| GPT-5 summary agent | 58.00 / $1.31 | 70.47 / $0.57 | 46.00 / $0.13 | 0.01 / $0.13 |
| GPT-5 RLM | 62.00 / $0.11 | 91.33 / $0.99 | 56.50 / $0.43 | 58.00 / $0.33 |
| GPT-5 RLM, no sub-calls | 58.00 / $0.18 | 88.00 / $0.44 | 36.00 / $0.37 | 43.93 / $0.69 |
| Qwen3-Coder RLM | 56.00 / $0.92 | 44.66 / $0.84 | 48.00 / $0.61 | 23.11 / $1.02 |
| Qwen3-Coder RLM, no sub-calls | 66.00 / $0.18 | 46.00 / $0.82 | 43.50 / $0.32 | 17.34 / $1.77 |

`*` denotes context-limit failures in the reported method. The paper reports cost standard deviations as well: GPT-5 RLM on BrowseComp-Plus is **$0.99 ± $1.22**, illustrating substantial variability. Its CodeAct plus BM25 baseline scores **51.00** at **$0.71** there. RLM therefore improves accuracy but is not uniformly cheaper than either retrieval or summarization. [Paper v1, Table 1][rlm-p1].

V1 Figure 1 scales S-NIAH, OOLONG, and OOLONG-Pairs from **2^13 to 2^18 tokens**. The **10M+** evidence comes from BrowseComp-Plus, not those needle experiments. The authors observe cheaper median costs in some settings but long, expensive trajectory tails; all measured model calls were blocking/sequential. [Paper v1, Figure 1, §3, Appendix C][rlm-p1].

Observed strategies include prefix inspection followed by regex filtering, uniform or newline chunking, semantic processing through sub-calls, small-context verification, and answer assembly in variables. The authors did not observe sophisticated partitioning beyond chunking and keyword search. Verification sometimes repeats unnecessarily and can replace an already-correct answer with an incorrect one. [Paper v1, §3.1 and Appendix B][rlm-p1].

**Later revision:** v3 tests depths zero through three. GPT-5 OOLONG-Pairs scores rise from **58.0** at depth one to **65.5** at depth two and **76.0** at depth three. Qwen3-Coder drops from **23.1** to **19.0** and **21.1** respectively; the authors associate its deeper-recursion failures with propagated syntax errors. V3 also changes some reported values, including GPT-5 depth-one OOLONG to **56.0**, so version-pinned reporting matters. [Paper v3, Table 1 and §5][rlm-p3].

### Reference architecture and sandbox communication

`RLM` coordinates prompt construction, model requests, code execution, termination, and limits. `BaseLM` defines synchronous/asynchronous completion and usage methods. `BaseEnv`, `NonIsolatedEnv`, and `IsolatedEnv` separate execution providers. The environment factory supports **Local, IPython, Docker, Modal, Prime, Daytona, and E2B**. [core/rlm.py][rlm-core]; [clients/base_lm.py][rlm-baselm]; [environments/base_env.py][rlm-baseenv]; [environment factory][rlm-envfactory].

**Local:** LocalREPL runs `exec` over a persistent namespace and captures stdout/stderr in `StringIO`. Its `llm_query` opens a TCP connection to the host `LMHandler`, a `ThreadingTCPServer` wrapping backend clients. Model credentials remain with those clients, although local code still shares the host process's privileges. [local_repl.py][rlm-local]; [core/lm_handler.py][rlm-handler].

**IPython:** Its default mode uses an in-process `InteractiveShell`; subprocess mode starts an `ipykernel` through `jupyter_client.KernelManager`. A TCP broker handles recursive calls and completion events, tagged with `cell_id` to avoid attributing late results to a subsequent cell. Subprocess mode includes kernel interruption on cell timeout. [ipython_repl.py][rlm-ipython].

The socket framing is **four-byte unsigned big-endian payload length, then UTF-8 JSON**. The typed request fields are:

```python
prompt: str | dict[str, Any] | None = None
prompts: list[str | dict[str, Any]] | None = None
model: str | None = None
depth: int = 0
```

Responses contain `error`, `chat_completion`, or `chat_completions`. Completion records contain `root_model`, `prompt`, `response`, `usage_summary`, and `execution_time`, with optional `metadata` and `error`. Batched requests use `acompletion`, `asyncio.gather`, and a semaphore, whose handler default is **16**. [core/comms_utils.py][rlm-comms]; [core/types.py][rlm-types]; [core/lm_handler.py][rlm-handler].

**Docker:** The inspected implementation uses container HTTP requests to `host.docker.internal`, rather than direct container-to-LM-handler TCP. Its host `LLMProxyHandler` serves `/llm_query`, `/llm_query_batched`, `/rlm_query`, and `/rlm_query_batched`. Plain calls forward to the TCP handler; recursive calls invoke the host callback. Container state persists through `dill` at `/workspace/state.dill`. [docker_repl.py][rlm-docker].

**Modal and Prime:** Generated code POSTs to a Flask broker inside its sandbox. `/enqueue` assigns a request ID and waits on an event; the host polls `/pending`, forwards the request to its LM handler, and POSTs `{id, response}` to `/respond`. Modal exposes an encrypted-port tunnel; Prime uses sandbox port exposure. Broker messages distinguish `type: "single"` from `type: "batched"`, and return `response` or `responses`. State is serialized between execution cells using `dill`, with a pickle fallback. [modal_repl.py][rlm-modal]; [prime_repl.py][rlm-prime].

A compatibility pitfall: the inspected Modal and Prime generated namespaces install `llm_query`, `llm_query_batched`, and `SHOW_VARS`, but omit `rlm_query` and `rlm_query_batched`. The host injects the recursive callback only for Local, IPython, and Docker. Environment registration therefore does not imply equivalent deeper-recursion support. This is a code observation; live failure behavior is **unverified**. [modal_repl.py][rlm-modal]; [prime_repl.py][rlm-prime]; [core/rlm.py][rlm-core].

**Minimal:** `RLM_REPL` loops over `REPLEnv`; `llm_query` invokes `Sub_RLM.completion` directly in-process, without the main repository's handler or broker. Its constructor defaults to **20 iterations**, compared with **30** in the main runtime. Minimal's `depth` attribute is explicitly unused, and cost-summary methods raise `NotImplementedError`. [minimal rlm_repl.py][rlm-minimalcore]; [minimal repl.py][rlm-minimalrepl].

### Feedback, limits, prompts, and trajectories

Main `format_iteration` appends the assistant response and one user message combining execution outputs. Each block's formatted stdout, stderr, and variable-name listing is capped at **20,000 characters**, followed by an omitted-character marker. This trims the model observation after execution, not the worker's underlying output allocation. Minimal caps formatted feedback at **100,000 characters**. [utils/parsing.py][rlm-parsing]; [local_repl.py][rlm-local]; [minimal utils/utils.py][rlm-minimalutils].

The current system prompt includes these load-bearing excerpts:

> REPL outputs over ~20K characters are truncated
>
> Reserve your own tokens for high-level decisions

It explains plain versus recursive queries, encourages batching and decomposition, and supplies an optional orchestrator addendum. Its approximate capacity advice varies between sections, so it should not be treated as an enforced tokenizer limit. [utils/prompts.py][rlm-prompts].

The main loop supports optional budget, timeout, token, and consecutive-error limits. Timeout is checked before iterations; budget/token/error limits are checked after iterations. On iteration exhaustion it requests another model answer. Consequently, these checks do not themselves interrupt a running Python cell or reserve spending before every sub-call. [core/rlm.py, `completion`, `_check_iteration_limits`][rlm-core].

`RLMLogger` captures metadata and iterations in memory and optionally writes JSONL. Iterations include prompts, model responses, code blocks, execution results, and nested call records. The repository includes a **Next.js trajectory viewer**, whose parser loads JSONL metadata and iterations. These are useful inputs for a kyora TUI, but logging locals can also serialize the large `context` payload. [logger/rlm_logger.py][rlm-logger]; [core/types.py][rlm-types]; [visualizer/README.md][rlm-viewer]; [visualizer/src/lib/parse-logs.ts][rlm-viewerparser].

Usage tracks calls and input/output tokens per model; dollar cost is optional provider data. OpenAI-compatible clients extract returned cost fields rather than universally computing cost from a price table. A missing cost becomes zero in the budget check. Additionally, `_subcall` adds child cost to `_cumulative_cost`, while later checks replace that accumulator with the parent handler's usage total, and returned parent usage comes from that handler. **Static inference:** do not assume deep child costs are completely aggregated or budgeted. Batched completions also reuse shared `get_last_usage` data, so per-item usage attribution warrants review. [clients/openai.py][rlm-openaiclient]; [core/types.py][rlm-types]; [core/rlm.py][rlm-core]; [core/lm_handler.py][rlm-handler].

### Limitations and related work

V1 identifies sequential-call latency, deeper recursion, and training native RLM policies as open work. Its negative results include insufficient coding ability, reasoning exhausting output-token budgets, and brittle final-answer tags. The blog additionally notes absent prefix-cache optimization and weak runtime/cost guarantees. V3 adds concerns about exploding sub-call costs and underdeveloped guardrails. [Paper v1, §5 and Appendix A][rlm-p1]; [Blog, Limitations][rlm-blog]; [Paper v3, §7][rlm-p3].

Local execution is not a security boundary: its allegedly safe builtins include unrestricted `__import__` and `open`, and cells execute with host-process access. It also changes process-wide cwd and stdout/stderr, with a per-instance lock. **Static inference:** concurrent or nested LocalREPL instances can interfere through that shared process state. Process isolation should precede claims of safe execution. [local_repl.py][rlm-local].

V3 reports **RLM-Qwen3-8B**, trained on **1,000 filtered trajectories** generated by a Qwen3-Coder root with Qwen3-8B sub-calls on LongBenchPro. The repository now includes a depth-one, subprocess-based training harness integrating with `verifiers` and `prime-rl`. [Paper v3, §3.2][rlm-p3]; [training/README.md][rlm-training].

Relevant comparisons:

- **Context rot:** Chroma's report describes worsening reliability with increasing input length, even within supported windows. RLM's environment approach addresses effective access rather than enlarging model attention. [Chroma report][rlm-rot]; [Paper v1, §1][rlm-p1].
- **ReAct and CodeAct:** ReAct interleaves reasoning and actions; CodeAct represents actions as executable Python with iterative feedback. RLM adds programmatic access to the externalized prompt and model calls within that code. [ReAct paper][rlm-react]; [CodeAct paper][rlm-codeact]; [Paper v1, §2.2][rlm-p1].
- **smolagents CodeAgent:** It likewise emits Python actions, but its local executor interprets an AST with import restrictions and operation limits. Its docs also cover remote execution. This is an executor comparison, not evidence of identical RLM semantics. [Official secure-execution documentation][rlm-smol].
- **SRLM:** A subsequent paper explores uncertainty-aware program selection and argues that recursive calls are not always the source of gains. Include a no-sub-call baseline when assessing kyora's recursive runtime. [SRLM paper][rlm-srlm].

### Implications for kyora

These are design recommendations inferred from the cited evidence:

1. **Separate context storage from model history.** Expose typed context handles, metadata, slices, and output references; keep full values in the worker. Preserve a no-sub-call execution mode for ablations. [Paper v1, §§1, 3][rlm-p1]; [utils/parsing.py][rlm-parsing].
2. **Distinguish plain LM calls from child runtimes.** Specify depth, model routing, child state ownership, and fallback behavior. Publish capabilities per environment rather than assuming every sandbox implements every helper. [core/rlm.py][rlm-core]; [modal_repl.py][rlm-modal].
3. **Use a host-owned RPC service.** Add request IDs, bounded framing, typed errors, cancellation, and authenticated access. The reference socket receiver reads its header with one `recv(4)` and accepts the declared payload length without a cap; improve those boundaries in Rust. [core/comms_utils.py][rlm-comms].
4. **Bound work before dispatch.** Use a shared call ledger, concurrency limits, deadlines, and spending reservations across the recursion tree. Treat unknown dollar cost separately from zero and retain per-call provider usage. [core/rlm.py][rlm-core]; [core/lm_handler.py][rlm-handler]; [core/types.py][rlm-types].
5. **Make completion explicit.** Return a typed completion event or output handle, distinguish successful completion from exhausted iterations, and avoid textual tags inside arbitrary model prose. [minimal utils/utils.py][rlm-minimalutils]; [current completion loop][rlm-core].
6. **Isolate Python and bound output at capture time.** Avoid process-wide cwd/stdout mutations; use worker processes or sandbox backends with resource limits. Observation truncation alone does not bound worker memory. [local_repl.py][rlm-local]; [utils/parsing.py][rlm-parsing].
7. **Log a call tree with provenance.** Record model, depth, prompt/output references, code, errors, usage, and timing. Make large locals opt-in so the TUI can inspect trajectories without duplicating entire corpora. [core/types.py][rlm-types]; [logger/rlm_logger.py][rlm-logger]; [viewer parser][rlm-viewerparser].
8. **Evaluate recursion rather than assuming it helps.** Compare offloading alone, retrieval, compaction, and multiple depths on both sparse and dense tasks; measure tail cost, syntax errors, and repeated verification. [Paper v1, §3][rlm-p1]; [Paper v3, Table 1 and §5][rlm-p3]; [SRLM paper][rlm-srlm].

[rlm-abs]: https://arxiv.org/abs/2512.24601
[rlm-p1]: https://arxiv.org/html/2512.24601v1
[rlm-p3]: https://arxiv.org/html/2512.24601v3
[rlm-blog]: https://alexzhang13.github.io/blog/2025/rlm/
[rlm-repo]: https://github.com/alexzhang13/rlm/tree/d04208afbad29ca675ab13478c40ee8bebc84bfe
[rlm-minimalrepo]: https://github.com/alexzhang13/rlm-minimal/tree/973f8d4acf3af2c86dc170af91607bf8b0c4d0ea
[rlm-pypi]: https://pypi.org/pypi/rlms/json
[rlm-readme]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/README.md
[rlm-project]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/pyproject.toml
[rlm-core]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/core/rlm.py
[rlm-types]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/core/types.py
[rlm-prompts]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/utils/prompts.py
[rlm-local]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/environments/local_repl.py
[rlm-ipython]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/environments/ipython_repl.py
[rlm-baselm]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/clients/base_lm.py
[rlm-baseenv]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/environments/base_env.py
[rlm-envfactory]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/environments/__init__.py
[rlm-handler]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/core/lm_handler.py
[rlm-comms]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/core/comms_utils.py
[rlm-docker]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/environments/docker_repl.py
[rlm-modal]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/environments/modal_repl.py
[rlm-prime]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/environments/prime_repl.py
[rlm-parsing]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/utils/parsing.py
[rlm-logger]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/logger/rlm_logger.py
[rlm-openaiclient]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/rlm/clients/openai.py
[rlm-viewer]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/visualizer/README.md
[rlm-viewerparser]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/visualizer/src/lib/parse-logs.ts
[rlm-training]: https://github.com/alexzhang13/rlm/blob/d04208afbad29ca675ab13478c40ee8bebc84bfe/training/README.md
[rlm-minimalreadme]: https://github.com/alexzhang13/rlm-minimal/blob/973f8d4acf3af2c86dc170af91607bf8b0c4d0ea/README.md
[rlm-minimalcore]: https://github.com/alexzhang13/rlm-minimal/blob/973f8d4acf3af2c86dc170af91607bf8b0c4d0ea/rlm/rlm_repl.py
[rlm-minimalrepl]: https://github.com/alexzhang13/rlm-minimal/blob/973f8d4acf3af2c86dc170af91607bf8b0c4d0ea/rlm/repl.py
[rlm-minimalutils]: https://github.com/alexzhang13/rlm-minimal/blob/973f8d4acf3af2c86dc170af91607bf8b0c4d0ea/rlm/utils/utils.py
[rlm-rot]: https://www.trychroma.com/research/context-rot
[rlm-react]: https://arxiv.org/abs/2210.03629
[rlm-codeact]: https://arxiv.org/abs/2402.01030
[rlm-smol]: https://huggingface.co/docs/smolagents/en/tutorials/secure_code_execution
[rlm-srlm]: https://arxiv.org/abs/2603.15653

## 2. Prime Intellect: RLM environments and harnesses

### Summary

Prime has two materially different implementations. The January-era `RLMEnv` runs a persistent Python worker in a remote Prime sandbox, exposes `call_python_repl(code)`, and implements `llm_batch` through a host HTTP interception server. Its built-in delegation is depth 1. Current `verifiers` instead installs `nano-rlm`, runs it through ACP, and supports supervisor-owned recursive agents with persistent IPython kernels and tree-wide budgets. Both implementations account for child inference separately from the root conversation. [Legacy RLMEnv][pi-legacy], [current RLMHarness][pi-harness], [nano-rlm supervisor][pi-supervisor]

### Source scope and reproducibility

Public repositories were shallow-cloned and read at these commits (`git rev-parse HEAD`):

| Repository | Commit SHA | Source |
|---|---|---|
| `verifiers-v019`, tag `v0.1.9` | `b81cfd338964f1f567058ca9345ede0f2360a708` | [verifiers tree][pi-legacy-tree] |
| `verifiers` | `484e6de6c283e8aeb23d79d3fd2e76a0394bf6ea` | [verifiers tree][pi-vf-tree] |
| `nano-rlm` | `d9784b6b58408b9a20b501875600db2227e05102` | [nano-rlm tree][pi-nano-tree] |
| `prime-rl` | `d9adc1e867db9f8e27438fdd65f99e8dc7a2448a` | [prime-rl tree][pi-rl-tree] |
| `prime-environments` | `cf220ee86500e94b5415d8b226557ce5d950f1db` | [prime-environments tree][pi-community-tree] |
| `research-environments` | `ff40b61dcf898e1cdba7a617ac158a50466797aa` | [research-environments tree][pi-research-tree] |
| `prime` | `b164f22ff92b7094aa4d0088b0b0d5575bd58568` | [Prime SDK tree][pi-sdk-tree] |

The January post's `sebastian/experiment/rlm` branch was unavailable during research. Exact experimental code and plotted values are **unverified**; the release snapshot documents implementation, not exact experimental reproduction. [Post and branch link](https://www.primeintellect.ai/blog/rlm), [v0.1.9 code][pi-legacy]

### Posts: implementation, claims, and experiments

The January 1, 2026 post reports an experimental `verifiers` implementation and compares ordinary LLM, RLM, and RLM with environment-specific tips. It uses GPT-5-mini principally, plus GLM 4.6, GLM 4.5 Air, and INTELLECT-3, across DeepDive, math-python, Oolong, and verbatim-copy. Comparisons use 50 rollouts; Oolong samples are randomized to avoid size-order bias. Reported outcomes: Oolong real and labeled synthetic data improve, unlabeled synthetic data worsens; math-python worsens; DeepDive needs delegation tips; verbatim-copy generally improves for GPT-5-mini. Math timeouts are ablated at 120, 300, and 600 seconds. Root context shrinks while child inference adds tokens and latency. Long-input baseline API rejections complicate efficiency comparisons. The proposal that RL will unlock better context management is a hypothesis, not a demonstrated training result in this post. [January RLM post](https://www.primeintellect.ai/blog/rlm)

The August 5, 2026 Prime Agent post describes a broader coding harness: context and history accessible programmatically, delegation inside a persistent REPL, and editable prompts, skills, memory, and sub-agent specifications. It reports evaluations on ARC-AGI-3 and several long-context tasks, and explicitly says no model had been trained around Prime Agent's core feature set at publication. These are author-reported evaluations, not measurements reproduced here. [Prime Agent post](https://www.primeintellect.ai/blog/prime-agent)

### January-era `verifiers`: exact execution path

#### Python execution and tool schema

`verifiers/envs/experimental/rlm_env.py:RLMEnv` extends `SandboxEnv`. The parent uses `prime_sandboxes`, not an in-process evaluator or locally launched Docker daemon. Defaults request `python:3.11-slim`, 1 CPU core, 2 GB memory, 5 GB disk, and a 60-minute sandbox timeout. The Docker image is the remote environment artifact; this code does not establish the provider's January isolation internals. [RLMEnv constructor][pi-legacy], [SandboxEnv constructor][pi-sandbox-legacy]

The model-visible tool is `call_python_repl`, with one string argument, `code`. The underlying signature includes injected `sandbox_id` and `state`; registration explicitly excludes those arguments. Tool metadata comes from the Python function signature and docstring through the function-to-OpenAI converter. The parent's `bash` tool is removed. [Tool registration and signature][pi-legacy], [schema conversion][pi-tool-schema]

The sandbox runs `/tmp/rlm_worker.py` as a long-lived Python process. A sandbox exec writes JSON `{code, seq}` to `/tmp/rlm_cmd`, then reads `/tmp/rlm_res`. These are named FIFOs. The worker parses code with `ast`, executes statements in a persistent namespace, evaluates a trailing expression, captures stdout/stderr, and returns JSON including sequence number and answer state. This is a custom REPL, not Jupyter. [Worker script and `_execute_code`][pi-legacy]

#### How REPL code calls the model

The injected API is `llm_batch(prompts: list, **kwargs) -> list`. Its `ThreadPoolExecutor` calls `_single_llm_call`, preserves input ordering, prints per-call usage/timing, and returns answer strings. There is no injected `llm()` or `sub_llm()` in this worker namespace. [Worker namespace and batch implementation][pi-legacy]

Each worker request uses `requests.post` to `RLM_INTERCEPTION_URL`, with this payload shape, extracted from `_single_llm_call`: [Worker HTTP payload][pi-legacy]

```python
payload = {
    "model": SUB_MODEL or "default",
    "messages": [{"role": "user", "content": prompt}],
    "_batch_id": batch_id,
    "_request_id": request_id,
}
```

On the host, `aiohttp` serves `POST /rollout/{rollout_id}/v1/chat/completions`, binding `0.0.0.0` on default port 8766. A tunnel pool exposes it when no explicit interception host is supplied. The rollout registry supplies the inference client, root model, optional `sub_model`, and state. The handler prepends its child system prompt, executes inference and optional tools, extracts the boxed answer, and returns `choices` plus `_rlm_metadata`. Model selection comes from registered rollout state. [Interception server and registration][pi-legacy]

A subtle API mismatch: worker `**kwargs` are serialized, but the handler does not forward them to `_run_sub_llm`. Actual inference supplies model, messages, tools, and detected logprobs support. Therefore passing `temperature` or `max_tokens` through `llm_batch` does not configure inference in this snapshot. [Worker, handler, `_call_sub_llm_api`][pi-legacy]

#### Large context and final answers

The regular task prompt remains in the root conversation. Additional data comes from `state["info"][pi-context_key]`, default key `context`. Setup serializes and uploads the payload plus `/tmp/rlm_context.json`; the worker reconstructs it into `extra_data`. Text and JSON have direct loading paths; custom serializers supply deserializer source/function names. Metadata, including type, size, format, and payload path where available, is documented in the root prompt instead of inserting the full payload. [Setup and worker loader][pi-legacy], [`prepare_context_data`][pi-serialization]

The bundled `rlm_arbitrary_data_demo` exercises strings, containers, and Polars DataFrames; its Polars path uploads Parquet and installs a deserializer. Thus `extra_data` need not be a string. [Demo serializer and environment][pi-demo]

Answer submission uses `answer = {"ready": False, "content": ""}`. After each cell the worker writes `/tmp/rlm_answer.json` and returns the dictionary. The harness recognizes `ready`, stores `state["final_answer"]`, and stops through `answer_ready`. Setting the boolean does not interrupt Python immediately, the worker must finish the cell. No `FINAL()` or `FINAL_VAR` is defined. On certain forced endings, the harness reads the saved content even without readiness. [Worker loop, `call_python_repl`, stop/fallback handlers][pi-legacy]

#### Prompt and delegation depth

The root prompt explains iterative execution, encourages inspecting `extra_data`, incremental answer construction, and semantic delegation. Its load-bearing workflow includes: [Root prompt][pi-legacy]

```text
Use the `call_python_repl` tool to execute Python code.
print(type(extra_data))
answer["content"] = "your current best answer"
answer["ready"] = True
```

It also instructs the model to see execution feedback before marking readiness; package and child-tool documentation are appended. An existing dataset system message follows a separate path that appends package/tool documentation rather than automatically inserting the entire default RLM prompt. [Prompt template and `get_prompt_messages`][pi-legacy]

Children receive a short prompt requesting concise assistance and a `\boxed{}` answer. With no `sub_tools`, each child is one plain completion. With `sub_tools`, host Python executes an OpenAI-style tool loop. Those tools are available only to children. Children do not receive the worker, `extra_data`, or `llm_batch` automatically; the built-in path stops at depth 1, although arbitrary user tools could implement additional behavior. [Child prompt, `_run_sub_llm`, `_call_sub_tool`][pi-legacy]

#### Limits and failure behavior

| Limit | Verified default or behavior |
|---|---|
| Root loop | `max_iterations=50`, passed as `max_turns`. [Constructor][pi-legacy] |
| Child loop | `sub_tool_max_turns=5`, then one extra forced final completion without tools, up to six inference calls. [Child loop][pi-legacy] |
| Parallelism | Five worker threads per `llm_batch`; no separate batch-size or total-child-call cap in this class. [Worker][pi-legacy] |
| Execution | 120 seconds per cell; derived default API timeout 96 seconds, worker HTTP timeout 108 seconds. These are per-call limits, not a whole-child tool-loop deadline. [Timeout computation and callers][pi-legacy] |
| Startup | `max_startup_wait_seconds=120`. [Constructor][pi-legacy] |
| Output | First 8192 characters of combined REPL output, followed by a truncation marker; timing is appended afterward. [Output formatter][pi-legacy] |
| Context | Optional `max_seq_len`; warning at 80% of latest root prompt usage; token arrays may be clipped for training. No tree-wide token budget in `RLMEnv`. [Context warning][pi-legacy], [token parser][pi-token-parser] |
| Timeout recovery | Default attempts sandbox recreation and resets REPL state; `abort_on_code_timeout=True` aborts instead. [Execution recovery][pi-legacy] |

Output truncation limits what enters the root prompt, not the worker's in-memory stdout buffer, child-response strings, or saved answer size. Treat it as context management, not a memory quota. [Worker capture and output formatter][pi-legacy]

### Current harness: ACP, IPython, and actual recursive agents

Current `verifiers/v1/harnesses/rlm/harness.py` installs `nano-rlm` at pinned ref `c38e20b60c90bbf625274d3fde2b2a384f390a66`, launches `rlm --acp`, and supplies `ai.prime.rlm/runtime-v1` metadata containing provider endpoint/secret, policy, skills, and role-specific prompt additions. The pinned ref was fetched and compared with the inspected nano-rlm HEAD; only `uv.lock` differs. Execution runtime is configurable, the bundled GSM8K example selects Docker, while `PrimeRuntime` provides remote VM execution. [Harness][pi-harness], [pinned nano-rlm tree][pi-nano-pin], [GSM8K config][pi-gsm8k], [Prime runtime][pi-prime-runtime]

The native tool is `ipython`, requiring string `code` and permitting integer `timeout`. `jupyter_client.KernelManager` starts `ipykernel_launcher` using local IPC. Here, local means inside the selected execution runtime. Variables survive cells and conversation compaction; kernel restart loses them. [IPython schema and kernel startup][pi-ipython]

Inside the kernel, `await rlm.agent.spawn(task=..., name=..., persistent=False)` returns a handle. `await child.result(yield_after=...)` retrieves its result. These calls use a Unix-domain socket to the session supervisor, not the legacy remote HTTP batch endpoint. The protocol is UTF-8 JSON framed by a four-byte big-endian length, with capability and cell-scope identifiers; maximum frames are 1 MiB requests and 16 MiB responses. The supervisor creates another `RLMEngine` with inherited runtime config, incremented depth, working directory, and MCP servers. [Agent API][pi-agent], [broker framing][pi-broker], [supervisor spawning][pi-supervisor]

Children have their own tool-capable engine and kernel. Further delegation is available below `max_depth`; default depth is 1, but larger depths are supported. Default concurrency is four agents, and total spawn count is uncapped unless configured. Policy validation requires concurrency at least depth. Native tools default to IPython; skills/MCP tooling are configurable rather than restricted to children. [Execution policy][pi-policy], [engine setup][pi-engine], [supervisor][pi-supervisor]

Nano-rlm defaults to 300-second execution timeout, with per-IPython-call maximum 600 seconds, 20 KB middle-truncated tool results, and one million tree-wide new tokens. Verifiers overrides the tree token default to ten million; zero explicitly disables that budget. Tree turn and spawn limits are optional. New tokens mean completion plus prompt tokens minus reported cached prompt tokens. Budget checks occur between calls, so they are not strict reservations against concurrent in-flight completions. [IPython timeout][pi-ipython], [policy][pi-policy], [harness policy mapping][pi-harness], [engine accounting][pi-engine]

The current root prompt is assembled from task instructions, role-specific additions, runtime/tool guidance, and optional delegation guidance. It says a final root answer returns control to the caller. A tool-free assistant reply supplies the final answer; the legacy answer dictionary is not the completion mechanism. Automatic compaction preserves the live kernel and makes the append-only message ledger accessible through `history()`. Large inputs are task-specific files/variables rather than a universal injected `extra_data`. [Prompt builder][pi-prompt], [engine loop][pi-engine], [history and lifecycle documentation][pi-nano-readme]

### Training, evaluation, Hub, and trajectory accounting

The Hub presents environments as discoverable artifacts for evaluation and training. The requested `prime-environments` checkout currently identifies itself as community environments. Research tasksets are separately present in `research-environments`; avoid assuming old blog package names map unchanged onto current files. [Environments Hub](https://app.primeintellect.ai/dashboard/environments), [community README][pi-community], [research tree][pi-research-tree]

Current Oolong-real uploads context to `/workspace/context.txt`, requests `/workspace/answer.txt`, and scores the answer file with last-reply fallback. Its task supports deterministic scoring or an optional host judge. Current verbatim-copy instead puts text in the prompt, requires `<answer>` tags, and stops after one turn. These demonstrate that context placement, answer extraction, and termination belong to task design as well as the harness. [Oolong taskset][pi-oolong], [verbatim taskset][pi-verbatim]

`prime-rl` provides concrete RLM training/evaluation configurations. The GLM-4.5-Air search example selects the `rlm` harness and `search` skill for OpenSeeker/RedSearcher training, configures compaction, and evaluates BrowseComp every 20 steps. This establishes integration, not independently verified training gains. [Search training configuration][pi-rl-config]

Legacy accounting records every successful child inference turn with prompt, completion, raw response, optional token arrays, distinct `trajectory_id = batch_id_request_id`, and extras identifying parent turn, batch, request, child-turn index, and tool count. `include_sub_llm_in_trajectory=True` is the default; disabling it still updates metrics. Main and child prompt/completion tokens are accumulated separately from response usage; calls are deduplicated by batch/request identity. Root completion rendering excludes child steps. [Handler, metrics, completion rendering][pi-legacy]

Training data requires more than usage totals: the legacy parser needs prompt token IDs, completion token IDs, and logprobs, otherwise it returns `None`. Thus API evaluations can report child token usage without yielding token-level RL samples. This distinction is explicit in the code. [Token parser][pi-token-parser]

Current model requests go through the host interception endpoint and enter a message graph. ACP publishes request-correlated semantic edges including continuation, child call/return, and compaction. Verifiers attaches those edges and records numeric session metrics. Prime-rl emits one sample per trainable branch, with token IDs, sampled masks, logprobs, and advantages; sampled nodes shared between branches train once. Scalar advantages can be broadcast over sampled tokens across trainable paths. Child calls can therefore participate in training, subject to graph trainability and token availability, rather than being merely invisible tool expenditure. [Interception server][pi-interception], [ACP metadata consumption][pi-acp], [nano-rlm contract][pi-nano-readme], [trajectory conversion][pi-trajectories], [advantage routing][pi-routing]

### Prime Sandboxes API and isolation

The current SDK lives in `prime/packages/prime-sandboxes`. It provides synchronous `SandboxClient` and asynchronous `AsyncSandboxClient`, with `create(CreateSandboxRequest(...))`, `wait_for_creation`, `execute_command`, file transfer, and `delete`. Requests specify image and resources; current boot commands use structured `StartCommand(executable, args)`. This differs from the legacy environment's string startup command, so pin SDK compatibility when reproducing it. [SDK README][pi-sdk-readme], [request models][pi-sdk-models], [legacy sandbox request][pi-sandbox-legacy]

Current async command execution accepts sandbox ID, command, working directory, environment, timeout, and guest username, returning `CommandResponse`; it dispatches through the gateway's Connect RPC path. Live processes also support streamed stdout/stderr and stdin. [SDK `sandbox.py`][pi-sdk-client]

Prime's September 23, 2026 announcement describes hardware-virtualized microVMs with their own Linux guest kernels, Docker images converted to bootable VM images, and Docker support inside the guest. It also describes Prime Tunnels exposing host inference interception servers over HTTPS. These are provider claims; the isolation backend was not audited here, and they do not establish the exact backend used in January. [Sandbox announcement](https://www.primeintellect.ai/blog/sandboxes)

### Implications for kyora

- Separate the Rust agent loop, execution backend, and Python kernel. Implement explicit IPC for cells and a separate broker for recursive calls; Prime's legacy FIFO/HTTP split and current kernel/supervisor split are useful precedents. [Legacy worker][pi-legacy], [broker][pi-broker]
- Model sub-agents as supervisor-owned tasks with stable handles. Preserve ownership, cancellation, and result retrieval independently of Python variables. Support configurable depth rather than equating a batch of completions with arbitrary recursion. [Supervisor][pi-supervisor], [agent API][pi-agent]
- Enforce shared budgets for tokens, calls, depth, concurrency, and wall time. Reserve capacity or document possible in-flight overshoot; distinguish raw billed usage from cached-token-adjusted budget usage. [Policy][pi-policy], [engine accounting][pi-engine]
- Keep large artifacts outside prompts and expose metadata plus selective reads. Define answer extraction per task; a REPL variable, file, and assistant reply are different protocols. [Serialization][pi-serialization], [Oolong task][pi-oolong], [engine finalization][pi-engine]
- Preserve full provenance for root and child requests, tool results, token masks, logprobs, and causal edges. Usage counters alone cannot train the policy; shared prefixes need deduplication. [Legacy token parser][pi-token-parser], [trajectory conversion][pi-trajectories]
- Make reset and truncation semantics explicit. Kernel timeout recovery can destroy Python state; context truncation does not impose memory limits. Retain full artifacts separately from bounded model-visible output. [Legacy recovery][pi-legacy], [IPython tool][pi-ipython], [nano-rlm ledger documentation][pi-nano-readme]
- Test the actual inference argument path. An accepted `**kwargs` surface can silently fail to affect generation, as the legacy HTTP handler demonstrates. [Legacy worker and handler][pi-legacy]
- Provide execution isolation outside the recursive supervisor. Nano-rlm documents shared filesystem access as trusted; handle capabilities control orchestration rather than filesystem access. [Nano-rlm recursion documentation][pi-nano-readme]

[pi-legacy-tree]: https://github.com/PrimeIntellect-ai/verifiers/tree/b81cfd338964f1f567058ca9345ede0f2360a708
[pi-vf-tree]: https://github.com/PrimeIntellect-ai/verifiers/tree/484e6de6c283e8aeb23d79d3fd2e76a0394bf6ea
[pi-nano-tree]: https://github.com/PrimeIntellect-ai/nano-rlm/tree/d9784b6b58408b9a20b501875600db2227e05102
[pi-rl-tree]: https://github.com/PrimeIntellect-ai/prime-rl/tree/d9adc1e867db9f8e27438fdd65f99e8dc7a2448a
[pi-community-tree]: https://github.com/PrimeIntellect-ai/prime-environments/tree/cf220ee86500e94b5415d8b226557ce5d950f1db
[pi-research-tree]: https://github.com/PrimeIntellect-ai/research-environments/tree/ff40b61dcf898e1cdba7a617ac158a50466797aa
[pi-sdk-tree]: https://github.com/PrimeIntellect-ai/prime/tree/b164f22ff92b7094aa4d0088b0b0d5575bd58568
[pi-legacy]: https://github.com/PrimeIntellect-ai/verifiers/blob/b81cfd338964f1f567058ca9345ede0f2360a708/verifiers/envs/experimental/rlm_env.py
[pi-sandbox-legacy]: https://github.com/PrimeIntellect-ai/verifiers/blob/b81cfd338964f1f567058ca9345ede0f2360a708/verifiers/envs/sandbox_env.py
[pi-tool-schema]: https://github.com/PrimeIntellect-ai/verifiers/blob/b81cfd338964f1f567058ca9345ede0f2360a708/verifiers/utils/tool_utils.py
[pi-serialization]: https://github.com/PrimeIntellect-ai/verifiers/blob/b81cfd338964f1f567058ca9345ede0f2360a708/verifiers/utils/rlm_data_serialization_utils.py
[pi-demo]: https://github.com/PrimeIntellect-ai/verifiers/blob/b81cfd338964f1f567058ca9345ede0f2360a708/environments/rlm_arbitrary_data_demo/rlm_arbitrary_data_demo.py
[pi-token-parser]: https://github.com/PrimeIntellect-ai/verifiers/blob/b81cfd338964f1f567058ca9345ede0f2360a708/verifiers/utils/response_utils.py
[pi-harness]: https://github.com/PrimeIntellect-ai/verifiers/blob/484e6de6c283e8aeb23d79d3fd2e76a0394bf6ea/verifiers/v1/harnesses/rlm/harness.py
[pi-gsm8k]: https://github.com/PrimeIntellect-ai/verifiers/blob/484e6de6c283e8aeb23d79d3fd2e76a0394bf6ea/configs/gsm8k_rlm.toml
[pi-prime-runtime]: https://github.com/PrimeIntellect-ai/verifiers/blob/484e6de6c283e8aeb23d79d3fd2e76a0394bf6ea/verifiers/v1/runtimes/prime.py
[pi-interception]: https://github.com/PrimeIntellect-ai/verifiers/blob/484e6de6c283e8aeb23d79d3fd2e76a0394bf6ea/verifiers/v1/interception/server.py
[pi-acp]: https://github.com/PrimeIntellect-ai/verifiers/blob/484e6de6c283e8aeb23d79d3fd2e76a0394bf6ea/verifiers/v1/acp/__init__.py
[pi-nano-pin]: https://github.com/PrimeIntellect-ai/nano-rlm/tree/c38e20b60c90bbf625274d3fde2b2a384f390a66
[pi-ipython]: https://github.com/PrimeIntellect-ai/nano-rlm/blob/d9784b6b58408b9a20b501875600db2227e05102/src/rlm/tools/ipython.py
[pi-agent]: https://github.com/PrimeIntellect-ai/nano-rlm/blob/d9784b6b58408b9a20b501875600db2227e05102/src/rlm/agent.py
[pi-broker]: https://github.com/PrimeIntellect-ai/nano-rlm/blob/d9784b6b58408b9a20b501875600db2227e05102/src/rlm/broker.py
[pi-supervisor]: https://github.com/PrimeIntellect-ai/nano-rlm/blob/d9784b6b58408b9a20b501875600db2227e05102/src/rlm/supervisor.py
[pi-policy]: https://github.com/PrimeIntellect-ai/nano-rlm/blob/d9784b6b58408b9a20b501875600db2227e05102/src/rlm/config.py
[pi-engine]: https://github.com/PrimeIntellect-ai/nano-rlm/blob/d9784b6b58408b9a20b501875600db2227e05102/src/rlm/engine.py
[pi-prompt]: https://github.com/PrimeIntellect-ai/nano-rlm/blob/d9784b6b58408b9a20b501875600db2227e05102/src/rlm/prompt.py
[pi-nano-readme]: https://github.com/PrimeIntellect-ai/nano-rlm/blob/d9784b6b58408b9a20b501875600db2227e05102/README.md
[pi-community]: https://github.com/PrimeIntellect-ai/prime-environments/blob/cf220ee86500e94b5415d8b226557ce5d950f1db/README.md
[pi-oolong]: https://github.com/PrimeIntellect-ai/research-environments/blob/ff40b61dcf898e1cdba7a617ac158a50466797aa/environments/long_context/oolong_real/oolong_real/taskset.py
[pi-verbatim]: https://github.com/PrimeIntellect-ai/research-environments/blob/ff40b61dcf898e1cdba7a617ac158a50466797aa/environments/long_context/verbatim_copy/verbatim_copy/taskset.py
[pi-rl-config]: https://github.com/PrimeIntellect-ai/prime-rl/blob/d9adc1e867db9f8e27438fdd65f99e8dc7a2448a/examples/advanced/glm-4.5-air/search.toml
[pi-trajectories]: https://github.com/PrimeIntellect-ai/prime-rl/blob/d9adc1e867db9f8e27438fdd65f99e8dc7a2448a/src/prime_rl/orchestrator/trajectories.py
[pi-routing]: https://github.com/PrimeIntellect-ai/prime-rl/blob/d9adc1e867db9f8e27438fdd65f99e8dc7a2448a/src/prime_rl/orchestrator/algo/routing.py
[pi-sdk-readme]: https://github.com/PrimeIntellect-ai/prime/blob/b164f22ff92b7094aa4d0088b0b0d5575bd58568/packages/prime-sandboxes/README.md
[pi-sdk-models]: https://github.com/PrimeIntellect-ai/prime/blob/b164f22ff92b7094aa4d0088b0b0d5575bd58568/packages/prime-sandboxes/src/prime_sandboxes/models.py
[pi-sdk-client]: https://github.com/PrimeIntellect-ai/prime/blob/b164f22ff92b7094aa4d0088b0b0d5575bd58568/packages/prime-sandboxes/src/prime_sandboxes/sandbox.py

## 3. Rust harness patterns: Codex CLI, opencode, Claude Code, MCP

### Summary and source scope

Codex separates orchestration, model transport, tool execution, persistence, and presentation. Its reusable patterns are typed command/event boundaries, immutable request snapshots, ordered tool-result recording, and centralized approval/sandbox orchestration. This checkout supports Responses, rejects Chat Completions configuration, and defaults to bubblewrap on Linux rather than Landlock. [codex-rs/core/src/session/mod.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/mod.rs) [codex-rs/model-provider-info/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/model-provider-info/src/lib.rs) [codex-rs/linux-sandbox/src/linux_run_main.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/linux-sandbox/src/linux_run_main.rs)

Snapshots read from shallow clones, identified with `git rev-parse HEAD`:

- OpenAI Codex: `de3721a7be07054c8c2a41102b5a501f34155361`. [codex-rs/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/Cargo.toml)
- anomalyco/opencode: `907b3bc518fa48e90e8ec24dd327d13eee71c36c`. [packages/opencode/package.json](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/package.json)
- Official MCP Rust SDK: `8f9a28ecdb5f9237dd7927f915df82cd4d02d09b`. [README.md](https://github.com/modelcontextprotocol/rust-sdk/blob/8f9a28ecdb5f9237dd7927f915df82cd4d02d09b/README.md)

Every code link below includes its repository, SHA, and path. Claude Code findings describe official documentation fetched on 2026-10-04; internal implementation is **unverified**. Design recommendations are identified as inferences.

### Codex workspace layout

Paths below are relative to `codex-rs/`; this is a selective map of the checked-out workspace. [codex-rs/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/Cargo.toml)

| Crate/path | Responsibility and code citation |
| --- | --- |
| `core` | Session admission, turns, model/tool orchestration. [codex-rs/core/src/session/mod.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/mod.rs) |
| `protocol` | Operations, events, model items, approvals and sandbox data. [codex-rs/protocol/src/protocol.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/protocol/src/protocol.rs) |
| `exec` | Noninteractive frontend, human output or JSONL events, using app-server client. [codex-rs/exec/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/exec/src/lib.rs) |
| `tui` | Interactive terminal frontend. [codex-rs/tui/src/app.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/src/app.rs) |
| `cli` | Top-level command dispatch. [codex-rs/cli/src/main.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/cli/src/main.rs) |
| `codex-api`, `codex-client` | Responses semantics, SSE, HTTP provider/retry primitives. [codex-rs/codex-api/src/sse/responses.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/codex-api/src/sse/responses.rs) [codex-rs/codex-client/src/retry.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/codex-client/src/retry.rs) |
| `model-provider-info` | Provider configuration and wire API selection. [codex-rs/model-provider-info/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/model-provider-info/src/lib.rs) |
| `rmcp-client`, `codex-mcp` | MCP transports/authentication and catalogs/connections. [codex-rs/rmcp-client/src/rmcp_client.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/rmcp-client/src/rmcp_client.rs) [codex-rs/codex-mcp/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/codex-mcp/src/lib.rs) |
| `app-server`, `app-server-protocol` | Client-facing JSON-RPC server and typed API. [codex-rs/app-server/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/app-server/src/lib.rs) [codex-rs/app-server-protocol/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/app-server-protocol/src/lib.rs) |
| `exec-server`, `sandboxing`, `linux-sandbox` | Execution boundary, platform sandbox transformation, Linux enforcement. [codex-rs/exec-server/src/process_sandbox.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/exec-server/src/process_sandbox.rs) [codex-rs/sandboxing/src/manager.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/sandboxing/src/manager.rs) [codex-rs/linux-sandbox/src/linux_run_main.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/linux-sandbox/src/linux_run_main.rs) |
| `execpolicy` | Command rules, parsing and allow/prompt/forbidden evaluation. [codex-rs/execpolicy/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/execpolicy/src/lib.rs) [codex-rs/execpolicy/src/decision.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/execpolicy/src/decision.rs) |
| `apply-patch` | Patch parsing and application. [codex-rs/apply-patch/src/parser.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/apply-patch/src/parser.rs) |
| `file-search` | Ignore-aware path walking and fuzzy matching. [codex-rs/file-search/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/file-search/src/lib.rs) |
| `login` | Authentication management, browser/device login, credential storage interfaces. [codex-rs/login/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/login/src/lib.rs) |
| `responses-api-proxy` | Local proxy forwarding Responses requests with authentication read from stdin. [codex-rs/responses-api-proxy/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/responses-api-proxy/src/lib.rs) |
| `history`, `rollout`, `state`, `thread-store` | History types, JSONL recording/indexing, state storage, thread persistence abstraction. [codex-rs/history/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/history/src/lib.rs) [codex-rs/rollout/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/rollout/src/lib.rs) [codex-rs/state/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/state/src/lib.rs) [codex-rs/thread-store/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/thread-store/src/lib.rs) |
| `code-mode`, `code-mode-host`, `code-mode-runtime` | Code-cell sessions, host process and V8 runtime. [codex-rs/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/Cargo.toml) [codex-rs/code-mode-runtime/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/code-mode-runtime/src/lib.rs) [codex-rs/code-mode-runtime/src/v8_init.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/code-mode-runtime/src/v8_init.rs) |
| `tools`, `utils/pty`, `utils/output-truncation` | Shared tool contracts, PTY/pipe processes, bounded model outputs. [codex-rs/tools/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tools/src/lib.rs) [codex-rs/utils/pty/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/utils/pty/src/lib.rs) [codex-rs/utils/output-truncation/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/utils/output-truncation/src/lib.rs) |

There is no workspace member named `mcp-client` or `mcp-server` here. The CLI exposes `mcp` for managing external servers and `app-server` for serving its application API. Treat older crate names as version-specific, not current integration points. [codex-rs/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/Cargo.toml) [codex-rs/cli/src/main.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/cli/src/main.rs)

### Submission queue and agent loop

`Session::spawn_internal` creates a bounded submission channel and an unbounded event channel. `Op` and `EventMsg` belong to `protocol`; the current `Submission` wrapper belongs to core and adds IDs, ancestry, trace context, and extension initialization. `submission_loop` dispatches operations including turn input, recovery, interruption and approval-related responses. [codex-rs/core/src/session/mod.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/mod.rs) [codex-rs/core/src/session/submission.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/submission.rs) [codex-rs/core/src/session/handlers.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/handlers.rs) [codex-rs/protocol/src/protocol.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/protocol/src/protocol.rs)

`run_turn` repeatedly prepares a sampling request and continues when model output requires follow-up or input remains pending. The stream consumer handles output-item lifecycle, assistant deltas, reasoning, completion and usage. An EOF before completion is an error. `handle_output_item_done` records the tool call, builds a dispatch future, and marks follow-up necessary; `drain_in_flight` appends resulting envelopes to history before subsequent sampling. [codex-rs/core/src/session/turn.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/turn.rs) [codex-rs/core/src/stream_events_utils.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/stream_events_utils.rs)

Tools execute in spawned tasks. An `RwLock<()>` admits parallel-capable calls through a read lock and other calls through a write lock; results drain through `FuturesOrdered`. This combines concurrent execution with recording in call order. Cancellation tokens interrupt streaming and tool dispatch, with aborted outputs produced for interrupted tools. `Op::Interrupt` explicitly preserves background terminal processes; `CleanBackgroundTerminals` is separate. [codex-rs/core/src/tools/parallel.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/parallel.rs) [codex-rs/core/src/session/turn.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/turn.rs) [codex-rs/protocol/src/protocol.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/protocol/src/protocol.rs)

`TurnContext` holds turn-wide state; `StepContext` snapshots settings, environments, MCP bindings and the exact advertised tool router for one model request. Delayed calls retain that step, preventing configuration changes from substituting different tools mid-dispatch. [codex-rs/core/src/session/turn_context.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/turn_context.rs) [codex-rs/core/src/session/step_context.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/step_context.rs) [codex-rs/core/src/tools/parallel.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/parallel.rs)

### Providers, wire APIs and streaming

Providers are built in or configured under `model_providers` in `~/.codex/config.toml`. `ModelProviderInfo` carries endpoint, credential environment variable, headers/query parameters, wire API, retry limits and stream timeout settings. `WireApi` currently contains only `Responses`; deserializing `"chat"` returns an explicit removal error. [codex-rs/model-provider-info/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/model-provider-info/src/lib.rs)

At the API level, Chat Completions uses messages and `choices[].message`; Responses uses `input` and heterogeneous `output` items, including separate function calls and outputs. This is a protocol comparison, not evidence of two implementations in current Codex. [OpenAI migration guide](https://developers.openai.com/api/docs/guides/migrate-to-responses)

The Responses SSE parser uses `eventsource_stream`, deserializes event JSON, and translates events such as `response.output_text.delta`, `response.output_item.done` and `response.completed` into internal `ResponseEvent`s. It enforces an idle timeout. The model client also has a Responses WebSocket path. [codex-rs/codex-api/src/sse/responses.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/codex-api/src/sse/responses.rs) [codex-rs/core/src/client.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/client.rs)

HTTP retries and sampling retries are separate layers. Provider defaults configure four request retries, five stream retries, and a 300,000 ms stream idle timeout. HTTP provider construction enables 5xx/transport retries but disables 429 retries at that layer; `run_with_retry` honors `Retry-After`, otherwise exponential backoff with jitter. Sampling retry handling lives in the turn loop. Do not multiply these into a presumed single retry budget. [codex-rs/model-provider-info/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/model-provider-info/src/lib.rs) [codex-rs/codex-client/src/retry.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/codex-client/src/retry.rs) [codex-rs/core/src/session/turn.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/turn.rs)

### Tools, output limits and approvals

The current shell contract is `exec_command` plus `write_stdin`. Arguments include command, working directory, shell/login choice, `tty`, yield time and maximum output tokens. A running process returns `session_id`; later calls write input or poll it. Unified execution owns process sessions; sandbox spawning selects PTY versus pipes. [codex-rs/core/src/tools/handlers/shell_spec.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/handlers/shell_spec.rs) [codex-rs/core/src/tools/handlers/unified_exec.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/handlers/unified_exec.rs) [codex-rs/core/src/unified_exec/process_manager.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/unified_exec/process_manager.rs) [codex-rs/sandboxing/src/spawn.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/sandboxing/src/spawn.rs)

`ToolRouter` converts model items into calls and dispatches through the registry. `CoreToolRuntime` extends the shared `ToolExecutor<ToolInvocation>` contract with hook, telemetry and argument-streaming behavior. The tool plan depends on model/configuration/environment capabilities. This handler inventory has no dedicated `read_file`, `list_dir` or `grep_files`; shell commands provide filesystem inspection. `file-search` is a separate path-search component, not proof of a model-facing grep tool. [codex-rs/core/src/tools/router.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/router.rs) [codex-rs/core/src/tools/registry.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/registry.rs) [codex-rs/core/src/tools/spec_plan.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/spec_plan.rs) [codex-rs/core/src/tools/handlers/mod.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/handlers/mod.rs) [codex-rs/file-search/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/file-search/src/lib.rs)

`apply_patch` uses its own grammar, including add/delete/update headers, optional move directives and context-based chunks. The parser additionally supports an environment selector. Load-bearing grammar excerpt: [codex-rs/apply-patch/src/parser.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/apply-patch/src/parser.rs)

```text
start: begin_patch environment_id? hunk+ end_patch
begin_patch: "*** Begin Patch" LF
environment_id: "*** Environment ID: " filename LF
end_patch: "*** End Patch" LF?
hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?
```

Tool formatting and history admission both enforce truncation policies. Shared helpers truncate text and structured function/MCP outputs; do not infer a universal fixed output limit from one tool's settings. [codex-rs/core/src/tools/context.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/context.rs) [codex-rs/core/src/context_manager/history.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/context_manager/history.rs) [codex-rs/utils/output-truncation/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/utils/output-truncation/src/lib.rs)

Code-mode nested tool callbacks also retain their originating `StepContext` through a dispatch broker, directly relevant to a REPL that yields and later calls tools again. [codex-rs/core/src/tools/code_mode/delegate.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/code_mode/delegate.rs)

Approval and sandbox are independent axes. `UnlessTrusted` serializes as `untrusted` and requires approval unless an explicit execpolicy rule allows the command. `OnRequest` lets the model request approval; `Never` returns failures without prompting. `Granular` adds category controls. `ToolOrchestrator` centralizes approval, sandbox selection, attempts and escalation handling. [codex-rs/protocol/src/protocol.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/protocol/src/protocol.rs) [codex-rs/core/src/tools/orchestrator.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/orchestrator.rs)

Compatibility sandbox policies include `read-only`, `workspace-write`, `danger-full-access` and `external-sandbox`. Workspace write can add writable roots and controls temporary-directory access; network defaults restricted. Current permission profiles also support finer filesystem policies and protected metadata paths. [codex-rs/protocol/src/protocol.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/protocol/src/protocol.rs) [codex-rs/protocol/src/permissions.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/protocol/src/permissions.rs)

### Platform sandbox enforcement

macOS builds a Seatbelt profile from bundled policy fragments plus resolved filesystem/network permissions, invoking the fixed `/usr/bin/sandbox-exec` path. The base policy starts from deny-by-default and adds required process/system operations; filesystem writes and network access are selectively granted. [codex-rs/sandboxing/src/seatbelt.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/sandboxing/src/seatbelt.rs) [codex-rs/sandboxing/src/seatbelt_base_policy.sbpl](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/sandboxing/src/seatbelt_base_policy.sbpl)

Linux now defaults to bubblewrap filesystem isolation. Its inner execution stage applies `no_new_privs` and seccomp after establishing the filesystem view. `--use-legacy-landlock` explicitly selects Landlock; bubblewrap failure does not automatically fall back to it. Seccomp restricts network operations and VM socket access; managed proxy routing has separate enforcement. [codex-rs/linux-sandbox/src/linux_run_main.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/linux-sandbox/src/linux_run_main.rs) [codex-rs/linux-sandbox/src/landlock.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/linux-sandbox/src/landlock.rs)

Windows supports restricted-token and elevated sandbox backends. The implementation uses `CreateRestrictedToken` and `CreateProcessAsUserW`; elevated setup manages filesystem ACLs and offline firewall configuration. Unsupported restricted-token permission shapes produce errors instead of silently running unsandboxed. [codex-rs/windows-sandbox-rs/src/token.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/windows-sandbox-rs/src/token.rs) [codex-rs/windows-sandbox-rs/src/process.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/windows-sandbox-rs/src/process.rs) [codex-rs/windows-sandbox-rs/src/setup.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/windows-sandbox-rs/src/setup.rs) [codex-rs/sandboxing/src/windows.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/sandboxing/src/windows.rs)

`SandboxManager` transforms a command/permission snapshot into an execution request. `spawn_process` handles Windows-specific launching or invokes PTY/pipe helpers with the transformed argv, environment and working directory. The sandbox boundary is the child process, not merely the model prompt. [codex-rs/sandboxing/src/manager.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/sandboxing/src/manager.rs) [codex-rs/sandboxing/src/spawn.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/sandboxing/src/spawn.rs)

### Sessions, rollout and history

New rollout paths are under the configured Codex home, conventionally `~/.codex/sessions/YYYY/MM/DD/rollout-...jsonl`. Resume opens existing recordings; new recordings can defer file creation. `RolloutItem` lives in `history`, including session metadata, response items, compaction, turn context and event messages, plus newer retained-context/inter-agent records. Serialization uses snake-case `type` and `payload`. [codex-rs/rollout/src/recorder.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/rollout/src/recorder.rs) [codex-rs/history/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/history/src/lib.rs) [codex-rs/history/src/rollout_payload.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/history/src/rollout_payload.rs)

The outer record includes timestamp and optional ordinal, flattening the item. Exact shape: [codex-rs/history/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/history/src/lib.rs)

```rust
pub struct RolloutLine {
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    #[serde(flatten)]
    pub item: RolloutItem,
}
```

`ThreadManager` exposes resume/fork paths and reconstructs initial history, including ancestry and truncation/reversion handling. `~/.codex/history.jsonl` separately stores input history for recall; it is not the complete model transcript. Rollout storage also integrates state/indexing and compression, so JSONL is not the entire persistence subsystem. [codex-rs/core/src/thread_manager.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/thread_manager.rs) [codex-rs/message-history/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/message-history/src/lib.rs) [codex-rs/rollout/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/rollout/src/lib.rs)

### Context management

`ModelInfo::auto_compact_token_limit()` derives a limit at 90% of resolved context size, clamping an explicit model limit to that value. Runtime token status also accounts for configured scope, buffers and post-turn compaction; there is no single universal model-independent threshold. [codex-rs/protocol/src/openai_models.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/protocol/src/openai_models.rs) [codex-rs/core/src/session/context_window.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/context_window.rs)

The bundled compact prompt requests a checkpoint/handoff summary. Local compaction builds replacement history from retained user messages and a summary; context overflow during compaction removes oldest items and retries. The turn loop chooses among local, remote and token-budget compaction paths according to capabilities/configuration. [codex-rs/prompts/templates/compact/prompt.md](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/prompts/templates/compact/prompt.md) [codex-rs/core/src/compact.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/compact.rs) [codex-rs/core/src/session/turn.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/turn.rs)

Before model submission, history normalization ensures call outputs exist, removes orphan outputs and strips unsupported image/audio input. Recording truncates tool outputs under the selected policy. These invariants are especially relevant after interrupted execution or resume. [codex-rs/core/src/context_manager/history.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/context_manager/history.rs) [codex-rs/core/src/context_manager/normalize.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/context_manager/normalize.rs)

### TUI structure

The terminal frontend depends on ratatui and crossterm with event-stream/bracketed-paste support. `Tui` owns terminal input and frame requests; `App` routes application/backend events; `ChatWidget` owns conversation presentation. This snapshot connects through app-server rather than treating the TUI as the core runtime. [codex-rs/tui/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/Cargo.toml) [codex-rs/tui/src/tui.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/src/tui.rs) [codex-rs/tui/src/app.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/src/app.rs) [codex-rs/tui/src/chatwidget.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/src/chatwidget.rs)

Streaming collects newline-gated Markdown into a FIFO render queue. Controllers commit lines into `HistoryCell`s; active cells can mutate during streaming, while committed transcript cells persist. Separate cell implementations cover commands, patches and MCP results. `BottomPane` owns composer/approval views; `ChatComposer` handles editing/input state. [codex-rs/tui/src/streaming/mod.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/src/streaming/mod.rs) [codex-rs/tui/src/history_cell/mod.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/src/history_cell/mod.rs) [codex-rs/tui/src/bottom_pane/mod.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/src/bottom_pane/mod.rs) [codex-rs/tui/src/bottom_pane/chat_composer.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/tui/src/bottom_pane/chat_composer.rs)

### npm distribution and releases

`codex-cli/package.json` maps the `codex` command to `bin/codex.js`. The launcher maps OS/architecture to six native targets: x86_64/aarch64 Linux musl, macOS Darwin, and Windows MSVC. It resolves platform packages such as `@openai/codex-darwin-arm64`, then `vendor/<target>/bin/codex`; a local vendor directory is the fallback. The exact triples are `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`. It asynchronously spawns the binary and forwards signals. [codex-cli/package.json](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-cli/package.json) [codex-cli/bin/codex.js](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-cli/bin/codex.js)

The source manifest does not contain platform optional dependencies. `build_npm_package.py` injects them during packaging, with platform-specific package metadata. The release workflow builds/stages Rust artifacts and npm tarballs, explicitly uses musl Linux targets, installs musl tooling and bundles bubblewrap for primary Linux artifacts. Inspect generated packaging, not only the checked-in manifest. [codex-cli/scripts/build_npm_package.py](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-cli/scripts/build_npm_package.py) [.github/workflows/rust-release.yml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/.github/workflows/rust-release.yml)

### Testing without real models

`core_test_support` is the package at `core/tests/common`. It provides temporary environments, test Codex construction and mock Responses helpers. `responses.rs` uses wiremock, constructs SSE from JSON events, mounts response sequences, captures requests and exposes helpers such as `function_call_output_text`. WebSocket mocks cover alternative transport behavior. [codex-rs/core/tests/common/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/common/Cargo.toml) [codex-rs/core/tests/common/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/common/lib.rs) [codex-rs/core/tests/common/responses.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/common/responses.rs)

`tool_parallelism.rs` sends synthetic tool calls/completions and checks overlapping execution. Suites cover retry-after, truncation, compaction and resume/fork; apply-patch also has filesystem scenario fixtures. These tests exercise protocol continuation and observable state without requiring a real model. No builds or runtime test suites were executed for this source-reading report. [codex-rs/core/tests/suite/tool_parallelism.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/suite/tool_parallelism.rs) [codex-rs/core/tests/suite/retry_after.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/suite/retry_after.rs) [codex-rs/core/tests/suite/compact_resume_fork.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/suite/compact_resume_fork.rs) [codex-rs/core/tests/suite/truncation.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/suite/truncation.rs) [codex-rs/apply-patch/tests/suite/scenarios.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/apply-patch/tests/suite/scenarios.rs)

### Relevant opencode concepts

The cloned repository is `anomalyco/opencode`. Current runtime code uses TypeScript/Bun and Effect services; the TUI package uses OpenTUI/Solid. A historical Go TUI is **unverified** here and does not describe this snapshot. [packages/opencode/package.json](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/package.json) [packages/opencode/src/session/processor.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/session/processor.ts) [packages/tui/package.json](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/tui/package.json)

Sessions have explicit parent IDs and fork operations, with session/message-part storage backed by SQLite services. Compaction combines pruning old tool outputs with summary/recent-context preservation. The processor handles text, reasoning, tool calls/results and decides whether to compact, continue or stop. [packages/opencode/src/session/session.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/session/session.ts) [packages/core/src/database/database.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/core/src/database/database.ts) [packages/opencode/src/session/compaction.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/session/compaction.ts) [packages/opencode/src/session/processor.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/session/processor.ts)

Agent definitions distinguish primary/subagent modes, model selection, prompts and permissions. The `task` tool creates or resumes child sessions and supports foreground/background execution. Providers integrate AI SDK packages; permission rules resolve wildcard matches to allow/ask/deny, using the last matching rule. MCP supports stdio, Streamable HTTP and legacy SSE through the TypeScript SDK. [packages/opencode/src/agent/agent.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/agent/agent.ts) [packages/opencode/src/tool/task.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/tool/task.ts) [packages/opencode/src/provider/provider.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/provider/provider.ts) [packages/opencode/src/permission/index.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/permission/index.ts) [packages/opencode/src/mcp/index.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/mcp/index.ts)

### Relevant Claude Code concepts

Documented tools include `Bash`, `Read`, `Edit`, `Write`, `Glob`, `Grep`, `WebFetch`, `WebSearch`, `NotebookEdit`, and `Agent`; availability varies by environment/version. Current docs use `Agent` for delegation. The historical `Task` name/alias is **unverified** in this review. [Tools reference](https://code.claude.com/docs/en/tools-reference)

Subagents have separate context, instructions, model/tool selection and permission settings; forks can inherit conversation history. Local sessions save JSONL under `~/.claude/projects/`; continue/resume preserves identity, while fork creates a new session. Automatic compaction summarizes context, with persistent instructions and focused `/compact` requests influencing retention. [Subagents](https://code.claude.com/docs/en/sub-agents) [Session/context behavior](https://code.claude.com/docs/en/how-claude-code-works)

Hooks expose lifecycle boundaries such as `PreToolUse`, `PostToolUse`, `SessionStart` and `PreCompact`, with structured input/output and permission decisions. MCP connects local stdio or remote HTTP servers. Documented permission modes include `default`, `acceptEdits`, `plan`, `auto`, `dontAsk` and `bypassPermissions`; these control approval behavior and must not be mistaken for operating-system isolation. [Hooks](https://code.claude.com/docs/en/hooks) [MCP](https://code.claude.com/docs/en/mcp) [Permissions](https://code.claude.com/docs/en/permissions)

### MCP client basics and Rust SDK

The official Rust SDK provides `rmcp` and `rmcp-macros`, with Tokio-based clients/servers. Stdio launches a child server and exchanges protocol messages through pipes; Streamable HTTP connects to an endpoint whose responses may be JSON or SSE. In the legacy lifecycle, initialization precedes discovery with `tools/list` and invocation with `tools/call`; resources and prompts are separate capabilities. Stdio stdout must contain protocol messages only, with logs on stderr. [2025 transport specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) [README.md](https://github.com/modelcontextprotocol/rust-sdk/blob/8f9a28ecdb5f9237dd7927f915df82cd4d02d09b/README.md)

Transport behavior is versioned: the 2025-11-25 specification includes POST/GET and optional sessions, while the 2026-07-28 revision removes the GET stream endpoint and protocol sessions. Do not implement a timeless generic HTTP+SSE contract. [2025 transport specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) [2026 transport specification](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)

Codex pins `rmcp` 3.3.0 to Git revision `3e636cab26c013eca5131103c03d20237f12c4df` in its workspace manifest, using client/child-process/async-I/O/Streamable HTTP features. `RmcpClient` wraps initialization, tool operations, timeout and authentication behavior. Its default compatibility mode prefers 2025-06-18 initialization; opt-in `V20260728` permits newer discovery/lifecycle behavior. New SDK availability does not imply Codex always negotiates its newest protocol. [codex-rs/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/Cargo.toml) [codex-rs/rmcp-client/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/rmcp-client/Cargo.toml) [codex-rs/rmcp-client/src/rmcp_client.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/rmcp-client/src/rmcp_client.rs) [codex-rs/rmcp-client/src/protocol_mode.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/rmcp-client/src/protocol_mode.rs)

### Implications for kyora

The following are design inferences from the cited implementations:

- Separate core/protocol/provider/executor/storage/TUI crates. Give a Python REPL a typed runtime bridge instead of frontend-owned orchestration. [codex-rs/Cargo.toml](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/Cargo.toml) [codex-rs/core/src/session/step_context.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/step_context.rs)
- Snapshot model, tools, permissions and environment per sampling step. Bind recursive calls to explicit parent/root/session IDs and inherited authority. [codex-rs/core/src/session/submission.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/submission.rs) [codex-rs/core/src/session/step_context.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/step_context.rs) [packages/opencode/src/tool/task.ts](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/tool/task.ts)
- Declare tool parallelism explicitly. Preserve call/output correspondence; ordered recording can delay later results behind slow earlier calls. Separate turn cancellation from process cleanup. [codex-rs/core/src/tools/parallel.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/parallel.rs) [codex-rs/core/src/session/turn.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/session/turn.rs) [codex-rs/protocol/src/protocol.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/protocol/src/protocol.rs)
- Centralize approvals and sandbox enforcement for shell, patches, MCP and REPL execution. A shared Python interpreter needs its own isolation boundary; prompt-level permissions are insufficient. [codex-rs/core/src/tools/orchestrator.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/tools/orchestrator.rs) [codex-rs/sandboxing/src/spawn.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/sandboxing/src/spawn.rs)
- Persist replayable events/context snapshots and compaction checkpoints; normalize call pairs before model submission. Keep input recall distinct from authoritative conversation history. [codex-rs/history/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/history/src/lib.rs) [codex-rs/core/src/context_manager/history.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/src/context_manager/history.rs) [codex-rs/message-history/src/lib.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/message-history/src/lib.rs)
- Bound recursive depth, concurrent children, token/spend budgets and outputs. Test synthetic streaming, cancellation races, resume, protocol-version negotiation and sandbox failure without real model calls. [codex-rs/core/tests/common/responses.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/common/responses.rs) [codex-rs/core/tests/suite/tool_parallelism.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/core/tests/suite/tool_parallelism.rs) [codex-rs/rmcp-client/src/protocol_mode.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/rmcp-client/src/protocol_mode.rs)
- Keep packaging-generated dependencies and platform sandbox differences visible in release checks. Do not copy assumptions from an older Codex or opencode layout. [codex-cli/scripts/build_npm_package.py](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-cli/scripts/build_npm_package.py) [codex-rs/linux-sandbox/src/linux_run_main.rs](https://github.com/openai/codex/blob/de3721a7be07054c8c2a41102b5a501f34155361/codex-rs/linux-sandbox/src/linux_run_main.rs) [packages/tui/package.json](https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/tui/package.json)

## 4. Anthropic Messages API: constraints that shape the provider and the loop

The first provider targets the Anthropic Messages API (`POST /v1/messages`). There is no official Rust SDK (official SDKs exist for Python, TypeScript, Java, Go, Ruby, C# and PHP), so kyora speaks raw HTTP plus SSE. Read on 2026-10-04 from the official documentation.

### Models

Current API ids include `claude-opus-5-5`, `claude-sonnet-5-5` and `claude-haiku-4-5` (alias of `claude-haiku-4-5-20251001`). Model capabilities, including the context window (`max_input_tokens`) and output cap (`max_tokens`), are available live from `GET /v1/models/{id}`, so kyora should not hard-code context windows. [Models overview](https://platform.claude.com/docs/en/about-claude/models/overview)

### Streaming

SSE events are `message_start`, `content_block_start`, `content_block_delta` (delta types `text_delta`, `input_json_delta`, `thinking_delta`, `signature_delta`), `content_block_stop`, `message_delta` (stop reason and cumulative usage), `message_stop`, plus `ping` and `error`. An `error` event can arrive mid-stream (for example `overloaded_error`, the streaming equivalent of HTTP 529), so a stream that started successfully can still fail and must be retried as a whole. For thinking blocks a `signature_delta` arrives just before `content_block_stop`; with the default `display: "omitted"` the thinking text is empty but the signature is still present. [Streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)

By default the API buffers and validates each tool-input parameter before streaming it. Setting `eager_input_streaming: true` on a user-defined tool streams fragments as they are generated, which matters for a `python` tool whose `code` argument can be long; the client then owns strict JSON parsing and schema validation, and must not run a tool whose turn ended with `max_tokens` or `refusal`. [Fine-grained tool streaming](https://platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming)

### Tool use

One assistant message may contain several `tool_use` blocks. All `tool_result` blocks for that message go back in a single user message; failures are returned as `tool_result` with `is_error: true`, not dropped. [Tool use overview](https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview), [Implement tool use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/implement-tool-use)

Stop reasons to handle: `end_turn`, `tool_use`, `max_tokens`, `stop_sequence`, `pause_turn`, `refusal` (with `stop_details`), and on compaction calls `compaction` and `model_context_window_exceeded`. [Handling stop reasons](https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons)

Forced `tool_choice` (`any` or a named tool) is rejected on Claude Opus 5.5 and Claude Sonnet 5.5; structured results should use structured outputs (`output_config.format`) or `strict: true` tools instead. [Structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs)

### Thinking and the append-only rule

On Claude Opus 5.5 thinking is always on (adaptive); depth is controlled with `output_config.effort` (`low` to `max`, default `medium` on this model). Thinking and redacted-thinking blocks must be passed back exactly as received during tool loops. [Adaptive thinking](https://platform.claude.com/docs/en/build-with-claude/adaptive-thinking), [Errors](https://platform.claude.com/docs/en/api/errors)

"Preserved thinking" binds each thinking block's signature to the conversation prefix that produced it: the top-level `system`, the `tools` set and every earlier message. Editing, reordering or deleting an earlier turn, rebuilding `system` or `tools` mid-conversation, or injecting and later removing per-request text invalidates every later thinking block. For accounts created on or after 2026-08-31 the API rejects such a request with a 400 by default; older accounts opt in by setting `thinking.block_binding.prefix_mismatch_behavior` (beta `thinking-binding-controls-2026-08-01`), which can also be set to `drop_block`. The documented rule is: keep `system` and `tools` fixed for the session and treat `messages` as append-only. The same rule keeps the prompt cache warm. [Preserved thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking)

Consequences for kyora: per-agent system prompt and tool list are frozen when the agent node starts; dynamic information (remaining budget, REPL resets after resume, limit warnings) is appended as new content (a text block after the tool results, or an appended `role: "system"` message on models that support mid-conversation system messages), never edited into earlier turns.

### Prompt caching

Caching is a prefix match over `tools`, then `system`, then `messages`; up to four breakpoints; a top-level `cache_control` caches the last cacheable block automatically. Hits are visible as `usage.cache_read_input_tokens`. Volatile content (timestamps, ids) must not appear before the last breakpoint. [Prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)

For kyora's recursion this matters twice: every agent turn re-sends the full history, so cached reads dominate input cost, and sibling sub-agents that share a system prompt and tool list share a cacheable prefix if those are byte-identical.

### Compaction and context editing

Server-side compaction on demand (beta `compact-2026-09-04`): send the conversation with `"compaction": {"type": "summarize", "instructions": "..."}`; the response is a single signed `compaction` block (stop reason `compaction`) that goes first in `messages` in place of the summarized turns, exactly as returned, on every later request. The last assistant turn must not end in an unanswered `tool_use`; `compaction` cannot be combined with `context_management` on one request; usage is reported under `usage.iterations`. A threshold variant (beta `compact-2026-01-12`) and rule-based context editing (clearing old tool results or thinking) also exist. Client-side compaction that keeps recent turns verbatim breaks preserved thinking for the kept turns; the documented safe client-side shape is "summary plus the next user turn, nothing else replayed". [Compaction overview](https://platform.claude.com/docs/en/build-with-claude/compaction), [Compaction on demand](https://platform.claude.com/docs/en/build-with-claude/compaction-on-demand), [Context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing)

### Budgets, token counting, errors

- Task budgets (beta `task-budgets-2026-03-13`): `output_config.task_budget = {"type": "tokens", "total": N}` tells the model how many tokens an agentic loop may spend so it can pace itself; it is advisory, distinct from `max_tokens`. A natural fit for passing a sub-agent's remaining budget to the model. [Task budgets](https://platform.claude.com/docs/en/build-with-claude/task-budgets)
- Token counting: `POST /v1/messages/count_tokens` takes the same body shape as a request. [Token counting](https://platform.claude.com/docs/en/build-with-claude/token-counting)
- Errors: 429 `rate_limit_error` normally carries `retry-after`; a spend-cap 429 has no `retry-after` and keeps failing until access resumes, so retries must be bounded. 529 `overloaded_error` and 5xx are transient. The official SDKs retry twice by default with exponential backoff, honoring `retry-after`. [Errors](https://platform.claude.com/docs/en/api/errors), [Rate limits](https://platform.claude.com/docs/en/api/rate-limits)

## 5. Cross-cutting conclusions for kyora

These are design inferences drawn from sections 1 to 4; the design document turns them into decisions.

1. **Two kinds of recursive call.** Every surveyed RLM runtime separates a cheap plain completion (`llm_query`, `llm_batch`) from a full child agent with its own REPL (`rlm_query`, `rlm.agent.spawn`). Both are needed, with different limits. [Section 1](#1-recursive-language-models-paper-and-reference-implementation), [Section 2](#2-prime-intellect-rlm-environments-and-harnesses)
2. **The host owns model access.** In all implementations the sandboxed Python process never holds provider credentials; model calls are brokered by the host (TCP handler, HTTP interception server, Unix-socket supervisor). kyora should do the same over a private channel, which also lets the REPL run with networking disabled.
3. **Framed, bounded, typed IPC.** Prior art uses 4-byte big-endian length plus JSON (rlm TCP broker, nano-rlm supervisor with 1 MiB request and 16 MiB response caps). The rlm reference reads frames without a size cap; kyora should cap frames and validate every message.
4. **Budgets are tree-wide and enforced before dispatch.** nano-rlm and the rlm reference check budgets between calls, so concurrent in-flight work can overshoot, and the rlm reference under-aggregates child cost. kyora should reserve before dispatch and charge every ancestor. nano-rlm also requires `max_concurrent_subagents >= max_depth`, a rule that exists to avoid deadlock when blocked parents hold concurrency slots.
5. **Completion must be explicit and typed.** Text tags (`FINAL(...)`) were brittle in the paper; later runtimes moved to an answer variable or a tool-free final reply. kyora should accept a tool-free final reply and an explicit `final(value)` call that returns a typed value.
6. **Large state lives in the worker, output to the model is bounded.** Every runtime truncates REPL output (8 KiB to 20 KB) before it enters the prompt, and none of them bounds worker memory through that truncation. kyora needs both: bounded observations and OS resource limits.
7. **Trace everything as a tree.** The rlm logger plus viewer and nano-rlm's semantic edges (`subagent_call`, `subagent_return`, `compaction`) show that a recursion tree with per-node usage is the main debugging tool. Large variables must not be copied into logs by default.
8. **Append-only histories.** Codex records tool results in call order and normalizes call/result pairs; the Anthropic API now enforces append-only histories through preserved thinking. Compaction must be designed around this (server-side compaction for Anthropic, whole-history summary elsewhere).
9. **Deterministic tests without models.** Codex tests the core against a wiremock SSE server and synthetic tool calls; kyora needs the same plus a scripted provider that stays deterministic under concurrent sub-agents.
10. **Evaluate, do not assume.** The paper's no-sub-call ablation sometimes beats full RLM, deeper recursion helps GPT-5 but hurts Qwen3-Coder, and SRLM questions where gains come from. kyora should make depth, sub-calls and REPL individually switchable so these ablations are cheap.
