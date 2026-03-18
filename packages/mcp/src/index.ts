import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js"
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js"
import { z } from "zod"
import { createDb } from "@kyora/db"
import { createLocalEmbedder } from "@kyora/nora"
import { queryState } from "./tools/query-state"
import { getRecentErrors } from "./tools/get-recent-errors"
import { getHttpLog } from "./tools/get-http-log"
import { searchDocsHandler } from "./tools/search-docs"
import { listIndexed } from "./tools/list-indexed"
import { indexSourceHandler } from "./tools/index-source"
import { indexStatus } from "./tools/index-status"

const db = createDb(process.env.KYORA_DATA_DIR)
const embedder = createLocalEmbedder()

const server = new McpServer({
  name: "kyora",
  version: "0.0.1",
})

// observability tools

server.tool(
  "kyora_query_state",
  "query state snapshots for a watched variable over time",
  {
    key: z.string().describe("variable name or path to query"),
    since: z.string().optional().describe("start time (ISO string or relative like '30s ago')"),
    until: z.string().optional().describe("end time (ISO string or relative like 'now')"),
    limit: z.number().optional().describe("max results (default 20)"),
  },
  async (input) => {
    const results = await queryState(db, input)
    return { content: [{ type: "text" as const, text: JSON.stringify(results, null, 2) }] }
  },
)

server.tool(
  "kyora_get_recent_errors",
  "get the most recent errors with stack traces",
  {
    limit: z.number().optional().describe("max results (default 5)"),
  },
  async (input) => {
    const results = await getRecentErrors(db, input)
    return { content: [{ type: "text" as const, text: JSON.stringify(results, null, 2) }] }
  },
)

server.tool(
  "kyora_get_http_log",
  "get recent HTTP requests and responses",
  {
    method: z.string().optional().describe("filter by HTTP method (GET, POST, etc.)"),
    limit: z.number().optional().describe("max results (default 10)"),
  },
  async (input) => {
    const results = await getHttpLog(db, input)
    return { content: [{ type: "text" as const, text: JSON.stringify(results, null, 2) }] }
  },
)

// nora: doc indexing tools

server.tool(
  "kyora_search_docs",
  "semantic search across indexed documentation — use this to find relevant docs for any library or concept",
  {
    query: z.string().describe("natural language search query"),
    limit: z.number().optional().describe("max results (default 5)"),
  },
  async (input) => {
    const results = await searchDocsHandler(db, embedder, input)
    return { content: [{ type: "text" as const, text: JSON.stringify(results, null, 2) }] }
  },
)

server.tool(
  "kyora_list_indexed",
  "list all indexed documentation sources and their status",
  {},
  async () => {
    const results = await listIndexed(db)
    return { content: [{ type: "text" as const, text: JSON.stringify(results, null, 2) }] }
  },
)

server.tool(
  "nora_index_source",
  "index documentation from an npm package, URL, or local file/directory for semantic search",
  {
    type: z.enum(["npm", "url", "file"]).describe("source type: 'npm' for npm package, 'url' for any URL, 'file' for local file or directory"),
    reference: z.string().describe("the npm package name, URL, or file path to index"),
  },
  async (input) => {
    const result = await indexSourceHandler(db, embedder, input)
    return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }] }
  },
)

server.tool(
  "kyora_index_status",
  "check the status of doc indexing — overall stats or a specific source",
  {
    sourceId: z.number().optional().describe("specific source ID to check, or omit for overall stats"),
  },
  async (input) => {
    const result = await indexStatus(db, input)
    return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }] }
  },
)

const transport = new StdioServerTransport()
await server.connect(transport)
