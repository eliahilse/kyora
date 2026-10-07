# MCP servers

`kyora run` connects to the Model Context Protocol servers listed in `$KYORA_HOME/config.toml` and offers their tools next to the built-in tools. Each server tool becomes an ordinary tool named `mcp__<server>__<tool>`, so `--tools` and a child agent's tool selection include or exclude it like any other tool.

## Configuration

```toml
# stdio: kyora spawns the command and speaks JSON-RPC over its stdin and stdout.
[mcp.servers.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = { LOG_LEVEL = "warn" }    # literal values, never credentials
env_vars = ["GITHUB_TOKEN"]     # copied from kyora's environment
cwd = "."                       # relative to the workspace (the default)
tool_timeout_s = 60

# Streamable HTTP.
[mcp.servers.tracker]
url = "https://mcp.example.com/mcp"
bearer_token_env = "TRACKER_TOKEN"            # sent as Authorization: Bearer <value>
headers = { X-Team = "core" }                 # literal, non-secret headers
env_headers = { X-Api-Key = "TRACKER_KEY" }   # header values read from the environment
allow_tools = ["search", "get_issue"]
deny_tools = ["delete_issue"]
```

| Key | Applies to | Meaning |
|---|---|---|
| `command`, `args` | stdio | Executable (looked up on PATH) and its arguments. |
| `env`, `env_vars`, `cwd` | stdio | Literal variables, variables copied from kyora's environment, working directory. |
| `url` | HTTP | Endpoint; `http` or `https`, without user info. |
| `bearer_token_env`, `headers`, `env_headers` | HTTP | Bearer token variable, literal headers, headers read from variables. |
| `startup_timeout_s` | both | Spawn or connect, initialize and list tools (default 30). |
| `tool_timeout_s` | both | One tool call (default 120). |
| `allow_tools`, `deny_tools` | both | Server-side tool names to keep or drop; deny wins. |
| `enabled` | both | `false` keeps the entry without starting the server. |

Exactly one of `command` and `url` is set. Credentials are never written into the file: `env` keys and `headers` names that look like credentials (containing KEY, TOKEN, SECRET, PASSWORD, CREDENTIAL or AUTH) are rejected; use `env_vars`, `bearer_token_env` or `env_headers`. A stdio server inherits only PATH, HOME, USER, LOGNAME, SHELL, LANG, TERM, TMPDIR, TZ and LC_* from kyora, minus credential-like names, so provider API keys never reach it unless listed in `env_vars`.

Server names are 1 to 32 ASCII letters, digits, `-` or `_`, without `__` or a trailing `_`, which keeps every tool name unambiguous. Tool name characters outside `[A-Za-z0-9_-]` become `_`, and names are kept within the 64 characters providers accept; a name that had to change gets eight hex digits of a hash of the original, so distinct server tools stay distinct.

## Behavior

- **Startup.** Enabled servers start concurrently before the run. kyora sends `initialize` offering protocol version 2025-11-25 (it accepts servers that answer with 2024-11-05 or later), then pages through `tools/list`. A server that fails or exceeds its startup timeout is reported on stderr and left out; the run continues. Naming its tools in `--tools` is a usage error. When `--tools` is given, only servers it names tools of are started.
- **Schemas and effects.** Input schemas are passed through unchanged. A tool counts as read-only only when the server sets `readOnlyHint: true`; every other tool is treated as mutating. On cancellation the runtime abandons a read-only call at once and waits for a mutating call to stop, which an MCP call does right after sending its cancellation notice.
- **Results.** Text content passes through. Images and audio are described by MIME type and size, embedded text resources are inlined, binary resources and resource links are described, and `structuredContent` is used when there is no other content. `isError: true` becomes an error result the model sees, as do JSON-RPC errors.
- **Timeouts and cancellation.** A call ends at its tool timeout or the node deadline, or when the run is cancelled. In each case kyora sends `notifications/cancelled` for the request; the server may already have applied part of a mutating call.
- **Changing tool lists.** On `notifications/tools/list_changed` kyora lists the server's tools again. Agents that start afterwards see the new list; running agents keep the list they started with.
- **Failures during a run.** A server that exits mid-call turns that call and later calls into tool errors. kyora does not restart it.
- **Shutdown.** When the run ends, HTTP sessions are deleted and stdio servers see end of file on stdin, then SIGTERM after 2 s and SIGKILL after 2 more seconds. Signals go to the server's whole process group, which also ends its child processes. A second Ctrl-C kills every server group immediately.

Not supported yet: resources, prompts, sampling, roots, elicitation, OAuth and server-provided instructions.
