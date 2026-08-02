import { mkdirSync, readFileSync, writeFileSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"
import { classify, watcherConfig, worthClassifying } from "./stakes"

interface HookInput {
  session_id?: string
  transcript_path?: string
  cwd?: string
  tool_name?: string
  tool_input?: unknown
}

interface WatchState {
  lastCheck?: number
  lastNudge?: number
  checks?: number
}

function statePath(sessionId: string): string {
  const dir =
    process.env.KYORA_COUNCIL_STATE_DIR ??
    join(process.env.XDG_STATE_HOME ?? join(homedir(), ".local", "state"), "kyora-council")
  mkdirSync(dir, { recursive: true })
  return join(dir, `watch-${sessionId.replace(/[^\w-]/g, "")}.json`)
}

function readState(path: string): WatchState {
  try {
    return JSON.parse(readFileSync(path, "utf8")) as WatchState
  } catch {
    return {}
  }
}

function writeState(path: string, state: WatchState): void {
  try {
    writeFileSync(path, JSON.stringify(state))
  } catch {}
}

/** Emitting nothing lets the agent proceed untouched — the default for every failure path. */
function pass(): never {
  process.exit(0)
}

function nudge(reason: string, suggested: string[]): never {
  const who = suggested.length > 0 ? ` Suggested members: ${suggested.join(", ")}.` : ""
  console.log(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PostToolUse",
        additionalContext: `[kyora council] This looks like a high-stakes moment: ${reason} Before committing to it, consider consulting other model families — \`council_convene\` for independent takes across lineages, or \`council_ask\` for one specific perspective. If you have already validated this, ignore this note and continue.${who}`,
      },
    }),
  )
  process.exit(0)
}

const MIN_GAP_MS = Number(process.env.KYORA_WATCHER_MIN_GAP_MS ?? 120_000)
const NUDGE_GAP_MS = Number(process.env.KYORA_WATCHER_NUDGE_GAP_MS ?? 900_000)

export async function runHook(): Promise<void> {
  const raw = await Bun.stdin.text().catch(() => "")
  if (!raw.trim()) pass()
  let input: HookInput
  try {
    input = JSON.parse(raw) as HookInput
  } catch {
    pass()
  }

  const toolName = input.tool_name ?? ""
  const payload = JSON.stringify(input.tool_input ?? {})
  if (!worthClassifying(toolName, payload)) pass()

  const config = watcherConfig()
  if (!config) pass()

  const path = statePath(input.session_id ?? "default")
  const state = readState(path)
  const now = Date.now()
  if (state.lastCheck && now - state.lastCheck < MIN_GAP_MS) pass()
  if (state.lastNudge && now - state.lastNudge < NUDGE_GAP_MS) pass()

  writeState(path, { ...state, lastCheck: now, checks: (state.checks ?? 0) + 1 })

  let tail = ""
  if (input.transcript_path) {
    try {
      const lines = readFileSync(input.transcript_path, "utf8").trim().split("\n")
      tail = lines.slice(-12).join("\n")
    } catch {}
  }
  const snippet = `RECENT AGENT ACTIVITY:\n${tail}\n\nCURRENT ACTION — ${toolName}:\n${payload.slice(0, 2000)}`

  const verdict = await classify(snippet, config)
  if (!verdict?.highStakes) pass()

  writeState(path, { ...readState(path), lastNudge: now })
  nudge(verdict.reason, verdict.suggested)
}
