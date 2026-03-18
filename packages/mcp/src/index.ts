import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js"
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js"
import { z } from "zod"
import { createDb } from "@kyora/db"
import { queryState } from "./tools/query-state"
import { getRecentErrors } from "./tools/get-recent-errors"
import { getHttpLog } from "./tools/get-http-log"

const db = createDb(process.env.KYORA_DATA_DIR)

const server = new McpServer({
  name: "kyora",
  version: "0.0.1",
})

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

const transport = new StdioServerTransport()
await server.connect(transport)
