export interface LiveUsage {
  remainingPct: number
  detail: string
  windows?: UsageWindow[]
}

export interface UsageWindow {
  label: string
  usedPct: number
  resetsAt?: number
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
  const windows: UsageWindow[] = []
  let worst = -1
  for (const [key, label] of [["five_hour", "5h"], ["seven_day", "7d"]] as const) {
    const window = obj[key]
    if (window === null || typeof window !== "object") continue
    const util = utilizationOf(window as Record<string, unknown>)
    if (util === null) continue
    worst = Math.max(worst, util)
    parts.push(`${label} ${Math.round(util)}% used`)
    windows.push({ label, usedPct: Math.round(util), resetsAt: resetsAtOf(window as Record<string, unknown>) })
  }
  if (worst === -1) return null
  return { remainingPct: Math.max(0, Math.round(100 - worst)), detail: parts.join(" · "), windows }
}

function resetsAtOf(window: Record<string, unknown>): number | undefined {
  const raw = window.resets_at
  if (typeof raw !== "string") return undefined
  const parsed = Date.parse(raw)
  return Number.isNaN(parsed) ? undefined : parsed
}

function utilizationOf(window: Record<string, unknown>): number | null {
  for (const [key, value] of Object.entries(window)) {
    if (typeof value !== "number") continue
    if (/util|percent|pct/i.test(key)) return value < 1 ? value * 100 : value
  }
  return null
}

async function probeJson(url: string, headers: Record<string, string>): Promise<unknown | null> {
  try {
    const response = await fetch(url, { headers, redirect: "error", signal: AbortSignal.timeout(5000) })
    if (!response.ok) return null
    return await response.json()
  } catch {
    return null
  }
}

/** Pulls the OAuth access token out of a Claude Code credentials blob. */
export function claudeAccessToken(credentials: string): string | undefined {
  try {
    const token = JSON.parse(credentials)?.claudeAiOauth?.accessToken
    return typeof token === "string" && token.length > 0 ? token : undefined
  } catch {
    return undefined
  }
}

/** Asks Anthropic what is left on the five-hour and seven-day windows for one token. */
export async function claudeOauthUsage(token: string): Promise<LiveUsage | null> {
  const payload = await probeJson(process.env.KYORA_CLAUDE_USAGE_URL ?? "https://api.anthropic.com/api/oauth/usage", {
    authorization: `Bearer ${token}`,
    "anthropic-beta": "oauth-2025-04-20",
  })
  return payload ? parseClaudeOauthUsage(payload) : null
}

export async function bearerUsage(
  url: string,
  key: string,
  parse: (payload: unknown) => LiveUsage | null,
): Promise<LiveUsage | null> {
  const payload = await probeJson(url, { authorization: `Bearer ${key}` })
  return payload ? parse(payload) : null
}
