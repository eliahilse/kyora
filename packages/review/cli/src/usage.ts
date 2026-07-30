import { mkdirSync, readFileSync, writeFileSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"

export type RunOutcome = "ok" | "rate_limited" | "error"

export interface EngineUsage {
  lastRun?: number
  lastOutcome?: RunOutcome
  cooldownUntil?: number
  runs?: number
}

export interface UsageState {
  engines: Record<string, EngineUsage>
}

function stateDir(): string {
  return (
    process.env.KYORA_REVIEW_STATE_DIR ??
    join(process.env.XDG_STATE_HOME ?? join(homedir(), ".local", "state"), "kyora-review")
  )
}

export function loadUsage(): UsageState {
  try {
    const parsed = JSON.parse(readFileSync(join(stateDir(), "usage.json"), "utf8")) as UsageState
    return parsed.engines ? parsed : { engines: {} }
  } catch {
    return { engines: {} }
  }
}

export function markRun(engineId: string, outcome: RunOutcome, cooldownMs: number): void {
  const state = loadUsage()
  const entry: EngineUsage = state.engines[engineId] ?? {}
  entry.lastRun = Date.now()
  entry.lastOutcome = outcome
  entry.runs = (entry.runs ?? 0) + 1
  if (outcome === "rate_limited") entry.cooldownUntil = Date.now() + cooldownMs
  else delete entry.cooldownUntil
  state.engines[engineId] = entry
  try {
    mkdirSync(stateDir(), { recursive: true })
    writeFileSync(join(stateDir(), "usage.json"), JSON.stringify(state, null, 2))
  } catch {}
}

export function cooldownRemainingMs(engineId: string, state: UsageState = loadUsage()): number {
  const until = state.engines[engineId]?.cooldownUntil ?? 0
  return Math.max(0, until - Date.now())
}

export function lastRunAt(engineId: string, state: UsageState = loadUsage()): number {
  return state.engines[engineId]?.lastRun ?? 0
}

const RATE_LIMIT_PATTERN =
  /rate.?limit|too many requests|\b429\b|usage.?limit|quota (?:exceeded|reached|exhausted)|(?:exceeded|reached|exhausted) (?:your )?quota|limit (?:reached|exceeded)|insufficient[_ ](?:quota|credits?|balance)|out of credits|overloaded_error/i

export function looksRateLimited(text: string): boolean {
  return RATE_LIMIT_PATTERN.test(text)
}

/** best-effort parse of "try again in 3 hours" / "retry after 90 seconds" style hints */
export function resetHintMs(text: string): number | null {
  const match = /(?:try again|retry|resets?)[^\d\n]{0,24}(\d+(?:\.\d+)?)\s*(hours?|h\b|minutes?|min\b|m\b|seconds?|s\b)/i.exec(text)
  if (!match) return null
  const value = parseFloat(match[1]!)
  const unit = match[2]!.toLowerCase()
  if (unit.startsWith("h")) return value * 3_600_000
  if (unit.startsWith("m")) return value * 60_000
  return value * 1000
}
