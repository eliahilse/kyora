import { tasks } from "./tasks"
import type { RunResult } from "./types"

const args = process.argv.slice(2)
const taskFilter = args.includes("--task") ? args[args.indexOf("--task") + 1] : null
const mode = args.includes("--mode") ? args[args.indexOf("--mode") + 1] as "kyora" | "blind" | "both" : "both"
const model = args.includes("--model") ? args[args.indexOf("--model") + 1]! : "sonnet"
const timeoutSec = args.includes("--timeout") ? parseInt(args[args.indexOf("--timeout") + 1]!) : 900
const source = args.includes("--source") ? args[args.indexOf("--source") + 1]! as "local" | "npm" : "local"
const noCache = args.includes("--no-cache")

const selected = taskFilter ? tasks.filter((t) => t.name === taskFilter) : tasks
if (selected.length === 0) {
  console.error(`task not found: ${taskFilter}`)
  process.exit(1)
}

console.log(`model: ${model}  |  source: ${source}  |  cache: ${noCache ? "disabled" : "on"}  |  timeout: ${timeoutSec}s  |  tasks: ${selected.map((t) => t.name).join(", ")}`)

const results: RunResult[] = []

for (const task of selected) {
  const modes = mode === "both" ? ["blind", "kyora"] as const : [mode] as const

  for (const m of modes) {
    console.log(`\n--- ${task.name} [${m}] ---`)
    const result = await runTask(task.name, task.dir, m)
    results.push(result)
    console.log(
      `  ${result.passed}/${result.total} passed  ${(result.timeMs / 1000).toFixed(1)}s  ` +
      `${result.numTurns} turns  ` +
      `in=${fmt(result.inputTokens)} out=${fmt(result.outputTokens)} ` +
      `cacheR=${fmt(result.cacheReadTokens)} cacheW=${fmt(result.cacheWriteTokens)}  ` +
      `$${result.costUsd.toFixed(4)}`
    )
  }
}

console.log("\n=== results ===\n")
const header = "task".padEnd(28) + "mode".padEnd(7) + "pass".padEnd(7) + "time".padEnd(8) + "turns".padEnd(7) + "in".padEnd(9) + "out".padEnd(8) + "cacheR".padEnd(10) + "cost"
console.log(header)
console.log("-".repeat(header.length))
for (const r of results) {
  console.log(
    r.task.padEnd(28) + r.mode.padEnd(7) +
    `${r.passed}/${r.total}`.padEnd(7) +
    `${(r.timeMs / 1000).toFixed(0)}s`.padEnd(8) +
    String(r.numTurns).padEnd(7) +
    fmt(r.inputTokens).padEnd(9) +
    fmt(r.outputTokens).padEnd(8) +
    fmt(r.cacheReadTokens).padEnd(10) +
    `$${r.costUsd.toFixed(4)}`
  )
}

const report = { timestamp: new Date().toISOString(), model, source, noCache, results }
const outPath = `${import.meta.dir}/results/${Date.now()}.json`
await Bun.write(outPath, JSON.stringify(report, null, 2))
console.log(`\nresults saved to ${outPath}`)

function fmt(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`
  return String(n)
}

async function runTask(name: string, dir: string, mode: "kyora" | "blind"): Promise<RunResult> {
  const tmpDir = `${import.meta.dir}/results/.workspace-${name}-${mode}`
  await Bun.$`rm -rf ${tmpDir}`.quiet()
  await Bun.$`mkdir -p ${tmpDir}`.quiet()

  // copy task files (exclude solution.ts and tests.ts — tests are hidden from agent)
  const files = new Bun.Glob("*").scanSync(dir)
  for (const file of files) {
    if (file === "solution.ts" || file === "tests.ts") continue
    await Bun.$`cp ${dir}/${file} ${tmpDir}/${file}`.quiet()
  }

  // minimal package.json so bun test + kyora imports work
  const pkgJson: Record<string, unknown> = {
    name: `reckon-${name}-${mode}`,
    private: true,
    type: "module",
  }
  if (mode === "kyora") {
    pkgJson.dependencies = source === "local"
      ? { "@kyora/sdk": `file:${import.meta.dir}/../../../packages/sdk` }
      : { "@kyora-sh/sdk": "latest" }
  }
  await Bun.write(`${tmpDir}/package.json`, JSON.stringify(pkgJson, null, 2))

  if (mode === "kyora") {
    console.log(`  installing kyora sdk (${source})...`)
    await Bun.$`bun install`.cwd(tmpDir).quiet().nothrow()
  }

  const prompt = mode === "kyora"
    ? `Fix the MULTIPLE bugs in code.ts. Use kyora: write a harness.ts that imports from code.ts, calls init({dataDir:'.kyora'}) from '@kyora/sdk', wraps key state with watch() and key functions with trace(), then runs representative scenarios. Run the harness, then use the kyora MCP tools (kyora_query_state, kyora_get_recent_errors, etc.) to inspect runtime state. Fix bugs based on what you observe. Tests are hidden — reading alone won't reveal all bugs.`
    : `Fix the MULTIPLE bugs in code.ts. Read README.md for symptoms. Tests are hidden — run \`bun code.ts\` to see program output and iterate.`

  const claudeArgs = [
    "-p", prompt,
    "--max-turns", "25",
    "--model", model,
    "--output-format", "stream-json",
    "--verbose",
    "--permission-mode", "bypassPermissions",
  ]

  const mcpConfig = mode === "kyora"
    ? {
        mcpServers: {
          kyora: source === "local"
            ? { command: "bun", args: [`${import.meta.dir}/../../../packages/mcp/src/index.ts`], env: { KYORA_DATA_DIR: `${tmpDir}/.kyora` } }
            : { command: "bunx", args: ["@kyora-sh/mcp"], env: { KYORA_DATA_DIR: `${tmpDir}/.kyora` } },
        },
      }
    : { mcpServers: {} }
  const mcpConfigPath = `${tmpDir}/.mcp.json`
  await Bun.write(mcpConfigPath, JSON.stringify(mcpConfig))
  claudeArgs.push("--mcp-config", mcpConfigPath, "--strict-mcp-config")

  console.log(`  invoking claude (timeout ${timeoutSec}s)...`)
  const start = Date.now()
  let stdout = ""
  let stderr = ""

  try {
    const env: Record<string, string> = { ...process.env as Record<string, string> }
    if (noCache) {
      env.DISABLE_PROMPT_CACHING = "1"
      env.CLAUDE_CODE_DISABLE_PROMPT_CACHING = "1"
    }

    const proc = Bun.spawn(["claude", ...claudeArgs], {
      cwd: tmpDir,
      stdout: "pipe",
      stderr: "pipe",
      env,
      signal: AbortSignal.timeout(timeoutSec * 1000),
    })

    // stream stderr live (progress indicator)
    const stderrReader = (async () => {
      const reader = proc.stderr.getReader()
      const decoder = new TextDecoder()
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        const chunk = decoder.decode(value)
        stderr += chunk
        for (const line of chunk.split("\n")) {
          if (line.trim()) console.log(`  [claude] ${line.trim().slice(0, 200)}`)
        }
      }
    })()

    const stdoutReader = (async () => {
      const reader = proc.stdout.getReader()
      const decoder = new TextDecoder()
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        stdout += decoder.decode(value)
      }
    })()

    await Promise.all([proc.exited, stderrReader, stdoutReader])
  } catch (err) {
    console.log(`  agent errored: ${err instanceof Error ? err.message : err}`)
  }

  const timeMs = Date.now() - start

  const transcriptPath = `${tmpDir}/transcript.jsonl`
  await Bun.write(transcriptPath, stdout)

  const { usage, toolCalls, finalMessage } = parseStream(stdout)

  // now copy the hidden tests into the workspace for scoring
  await Bun.$`cp ${dir}/tests.ts ${tmpDir}/code.test.ts`.quiet().nothrow()
  const { passed, total } = await countTests(tmpDir)

  const kyoraCalls = toolCalls.filter((t) => t.name.startsWith("mcp__kyora__") || t.name.startsWith("kyora_"))
  const toolSummary = summarizeTools(toolCalls)

  console.log(`  tool calls: ${toolCalls.length} total  (${toolSummary})`)
  if (mode === "kyora") {
    console.log(`  kyora MCP calls: ${kyoraCalls.length}`)
    for (const c of kyoraCalls.slice(0, 10)) {
      const shortName = c.name.replace(/^mcp__kyora__|^kyora_/, "")
      console.log(`    - ${shortName}  ${c.input.slice(0, 80)}`)
    }
  }
  if (finalMessage) {
    console.log(`  final: ${finalMessage.slice(0, 160).replace(/\n/g, " ")}`)
  }
  console.log(`  transcript: ${transcriptPath}`)

  return { task: name, mode, passed, total, timeMs, ...usage }
}

function summarizeTools(calls: Array<{ name: string; input: string }>): string {
  const counts: Record<string, number> = {}
  for (const c of calls) {
    const short = c.name.replace(/^mcp__kyora__|^kyora_/, "kyora:").replace(/^[A-Z]/, (m) => m.toLowerCase())
    counts[short] = (counts[short] ?? 0) + 1
  }
  return Object.entries(counts)
    .sort((a, b) => b[1] - a[1])
    .map(([n, c]) => `${n}×${c}`)
    .join(" ")
}

function parseStream(stdout: string): {
  usage: {
    inputTokens: number
    outputTokens: number
    cacheReadTokens: number
    cacheWriteTokens: number
    numTurns: number
    costUsd: number
  }
  toolCalls: Array<{ name: string; input: string }>
  finalMessage: string
} {
  const empty = {
    usage: { inputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, numTurns: 0, costUsd: 0 },
    toolCalls: [] as Array<{ name: string; input: string }>,
    finalMessage: "",
  }

  const toolCalls: Array<{ name: string; input: string }> = []
  let finalMessage = ""
  let usage = empty.usage

  for (const line of stdout.split("\n")) {
    if (!line.trim()) continue
    try {
      const evt = JSON.parse(line)

      if (evt.type === "assistant" && evt.message?.content) {
        for (const block of evt.message.content) {
          if (block.type === "tool_use") {
            toolCalls.push({
              name: block.name,
              input: JSON.stringify(block.input ?? {}).slice(0, 200),
            })
          }
          if (block.type === "text" && block.text) {
            finalMessage = block.text
          }
        }
      }

      if (evt.type === "result") {
        usage = {
          inputTokens: evt.usage?.input_tokens ?? 0,
          outputTokens: evt.usage?.output_tokens ?? 0,
          cacheReadTokens: evt.usage?.cache_read_input_tokens ?? 0,
          cacheWriteTokens: evt.usage?.cache_creation_input_tokens ?? 0,
          numTurns: evt.num_turns ?? 0,
          costUsd: evt.total_cost_usd ?? 0,
        }
      }
    } catch {
      // skip malformed lines
    }
  }

  return { usage, toolCalls, finalMessage }
}

async function countTests(dir: string): Promise<{ passed: number; total: number }> {
  try {
    const result = await Bun.$`bun test code.test.ts 2>&1`.cwd(dir).quiet().nothrow()
    const output = result.text()

    const passMatch = output.match(/(\d+) pass/)
    const failMatch = output.match(/(\d+) fail/)
    const passed = passMatch ? parseInt(passMatch[1]!) : 0
    const failed = failMatch ? parseInt(failMatch[1]!) : 0

    return { passed, total: passed + failed }
  } catch {
    return { passed: 0, total: 0 }
  }
}
