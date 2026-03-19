---
name: debug
description: Query runtime state to debug issues. Shows recent errors, HTTP logs, and state changes captured by kyora.
user_invocable: true
---

The user wants to debug a runtime issue using kyora's captured data.

Use the following MCP tools to gather context:
1. `kyora_get_recent_errors` — check for recent errors first
2. `kyora_get_http_log` — check recent HTTP activity if relevant
3. `kyora_query_state` — query specific state if the user mentions a variable

If the user provides a specific focus (e.g. "why is the cart empty", "what's failing in checkout"), tailor your queries accordingly.

Present findings clearly with timestamps and relevant context.

The user's input: $ARGUMENTS
