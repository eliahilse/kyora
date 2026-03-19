---
name: index-docs
description: Index documentation for a library so you can search it later. Pass an npm package name, URL, or local file path.
user_invocable: true
---

The user wants to index documentation for a library or source.

Use the `nora_index_source` MCP tool to index the provided reference. Determine the type:
- If it looks like an npm package name (e.g. `express`, `drizzle-orm`, `@types/node`), use type `npm`
- If it starts with `http://` or `https://`, use type `url`
- Otherwise, treat it as a local file path with type `file`

After indexing, report the result: how many chunks were indexed, or any errors.

The user's input: $ARGUMENTS
