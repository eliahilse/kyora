import { runHook } from "./hook"
import { startServer } from "./server"

/**
 * One binary, dispatched by subcommand. Shipping two bins meant neither runner
 * could resolve one: `npx @kyora-sh/council` failed outright ("could not
 * determine executable to run"), and `bunx @kyora-sh/council kyora-stakes-hook`
 * — the documented hook command — started the MCP server with the hook's name
 * as an argument, so every matching tool call hung until the hook timed out.
 */
const USAGE = `kyora-council — summon other model families from inside your coding agent

usage:
  kyora-council [serve]   start the MCP server over stdio (default)
  kyora-council hook      run the high-stakes PostToolUse watcher hook`

const subcommand = Bun.argv[2]

if (subcommand === "hook") {
  await runHook()
} else if (subcommand === undefined || subcommand === "serve") {
  await startServer()
} else if (subcommand === "-h" || subcommand === "--help" || subcommand === "help") {
  console.log(USAGE)
} else {
  console.error(`kyora-council: unknown command "${subcommand}"\n\n${USAGE}`)
  process.exit(2)
}
