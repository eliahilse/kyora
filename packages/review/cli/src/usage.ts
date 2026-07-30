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

export interface LiveUsage {
  remainingPct: number
  detail: string
}

/**
 * Tolerant extraction of used/total credit figures from the undocumented
 * Bailian token-plan console payload — field casing and nesting drift, so any
 * object carrying a used+total numeric pair counts.
 */
export function parseTokenPlanUsage(payload: unknown): LiveUsage | null {
  const found = findUsagePair(payload)
  if (!found || found.total <= 0) return null
  const remaining = Math.max(0, found.total - found.used)
  return {
    remainingPct: Math.round((remaining / found.total) * 100),
    detail: `${remaining.toLocaleString()} of ${found.total.toLocaleString()} credits left`,
  }
}

function findUsagePair(node: unknown): { used: number; total: number } | null {
  if (node === null || typeof node !== "object") return null
  const obj = node as Record<string, unknown>
  let used: number | null = null
  let total: number | null = null
  for (const [key, value] of Object.entries(obj)) {
    if (typeof value !== "number") continue
    const lower = key.toLowerCase()
    if (lower.includes("used")) used = value
    else if (lower.includes("total")) total = value
  }
  if (used !== null && total !== null) return { used, total }
  for (const value of Object.values(obj)) {
    const nested = findUsagePair(value)
    if (nested) return nested
  }
  return null
}

/**
 * Generic quota-window extraction for vendor usage payloads (kimi, z.ai):
 * collects objects carrying a used+limit numeric pair; the tightest window
 * determines the remaining percentage.
 */
export function parseQuotaWindows(payload: unknown): LiveUsage | null {
  const windows: { used: number; limit: number; label: string }[] = []
  collectWindows(payload, windows)
  let worst = -1
  const parts: string[] = []
  for (const window of windows.slice(0, 3)) {
    const pct = Math.max(0, Math.round((1 - window.used / window.limit) * 100))
    worst = worst === -1 ? pct : Math.min(worst, pct)
    parts.push(`${window.used.toLocaleString()}/${window.limit.toLocaleString()}${window.label ? ` ${window.label}` : ""}`)
  }
  if (worst === -1) return null
  return { remainingPct: worst, detail: parts.join(" · ") }
}

function collectWindows(node: unknown, out: { used: number; limit: number; label: string }[]): void {
  if (node === null || typeof node !== "object") return
  if (Array.isArray(node)) {
    for (const item of node) collectWindows(item, out)
    return
  }
  const obj = node as Record<string, unknown>
  let used: number | null = null
  let limit: number | null = null
  for (const [key, value] of Object.entries(obj)) {
    if (typeof value !== "number") continue
    const lower = key.toLowerCase()
    if (lower.includes("used") || lower === "usage") used = value
    else if (lower.includes("limit") || lower.includes("total") || lower.includes("quota")) limit = value
  }
  if (used !== null && limit !== null && limit > 0) {
    const label = ["window", "scope", "name", "type", "period"]
      .map((key) => obj[key])
      .find((value): value is string => typeof value === "string")
    out.push({ used, limit, label: label ?? "" })
    return
  }
  for (const value of Object.values(obj)) collectWindows(value, out)
}

/**
 * Z.ai's monitor payload reports per-window percentages (TIME_LIMIT /
 * TOKENS_LIMIT entries with `percentage` used); the tightest window wins.
 */
export function parseZaiQuota(payload: unknown): LiveUsage | null {
  const windows: { pct: number; label: string }[] = []
  collectPercentages(payload, windows)
  if (windows.length === 0) return parseQuotaWindows(payload)
  const worst = Math.max(...windows.map((window) => window.pct))
  return {
    remainingPct: Math.max(0, Math.round(100 - worst)),
    detail: windows
      .slice(0, 3)
      .map((window) => `${window.label}${Math.round(window.pct)}% used`)
      .join(" · "),
  }
}

function collectPercentages(node: unknown, out: { pct: number; label: string }[]): void {
  if (node === null || typeof node !== "object") return
  if (Array.isArray(node)) {
    for (const item of node) collectPercentages(item, out)
    return
  }
  const obj = node as Record<string, unknown>
  if (typeof obj.percentage === "number") {
    const label = typeof obj.type === "string" ? `${obj.type.toLowerCase().replace("_limit", "")} ` : ""
    out.push({ pct: obj.percentage, label })
  }
  for (const value of Object.values(obj)) collectPercentages(value, out)
}

/**
 * Claude's OAuth usage payload reports window utilization rather than raw
 * counts: five_hour / seven_day objects with a percentage (0-1 or 0-100).
 */
export function parseClaudeOauthUsage(payload: unknown): LiveUsage | null {
  if (payload === null || typeof payload !== "object") return null
  const obj = payload as Record<string, unknown>
  const parts: string[] = []
  let worst = -1
  for (const [key, label] of [["five_hour", "5h"], ["seven_day", "7d"]] as const) {
    const window = obj[key]
    if (window === null || typeof window !== "object") continue
    const util = utilizationOf(window as Record<string, unknown>)
    if (util === null) continue
    worst = Math.max(worst, util)
    parts.push(`${label} ${Math.round(util)}% used`)
  }
  if (worst === -1) return null
  return { remainingPct: Math.max(0, Math.round(100 - worst)), detail: parts.join(" · ") }
}

function utilizationOf(window: Record<string, unknown>): number | null {
  for (const [key, value] of Object.entries(window)) {
    if (typeof value !== "number") continue
    if (/util|percent|pct/i.test(key)) return value < 1 ? value * 100 : value
  }
  return null
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
