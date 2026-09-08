export interface LiveUsage {
  remainingPct: number
  detail: string
  windows?: UsageWindow[]
}

export interface UsageWindow {
  label: string
  usedPct: number
  resetsAt?: number
  active?: boolean
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
 * Claude's OAuth usage payload carries a `limits` array covering session, weekly
 * and per-model weekly windows; the older top-level keys are the fallback.
 */
export function parseClaudeOauthUsage(payload: unknown): LiveUsage | null {
  if (payload === null || typeof payload !== "object") return null
  const obj = payload as Record<string, unknown>
  const windows = Array.isArray(obj.limits) ? claudeLimitWindows(obj.limits) : []
  return summarize(windows.length > 0 ? windows : claudeLegacyWindows(obj))
}

function summarize(windows: UsageWindow[]): LiveUsage | null {
  if (windows.length === 0) return null
  const worst = Math.max(...windows.map((window) => window.usedPct))
  return {
    remainingPct: Math.max(0, Math.round(100 - worst)),
    detail: windows.map((window) => `${window.label} ${window.usedPct}% used`).join(" · "),
    windows,
  }
}

function claudeLimitWindows(limits: unknown[]): UsageWindow[] {
  const windows: UsageWindow[] = []
  for (const entry of limits) {
    if (entry === null || typeof entry !== "object") continue
    const limit = entry as Record<string, unknown>
    if (typeof limit.percent !== "number") continue
    windows.push({
      label: claudeLimitLabel(limit),
      usedPct: Math.round(limit.percent),
      resetsAt: parseTimestamp(limit.resets_at),
      active: limit.is_active === true,
    })
  }
  return windows
}

function claudeLimitLabel(limit: Record<string, unknown>): string {
  const kind = typeof limit.kind === "string" ? limit.kind : "limit"
  const scope = limit.scope as Record<string, unknown> | null | undefined
  const model = scope?.model as Record<string, unknown> | null | undefined
  const name = typeof model?.display_name === "string" ? model.display_name : undefined
  if (kind === "session") return "session"
  if (kind === "weekly_all") return "weekly"
  if (kind === "weekly_scoped") return name ? `weekly ${name}` : "weekly scoped"
  return name ? `${kind} ${name}` : kind
}

function claudeLegacyWindows(obj: Record<string, unknown>): UsageWindow[] {
  const windows: UsageWindow[] = []
  for (const [key, label] of [["five_hour", "5h"], ["seven_day", "7d"]] as const) {
    const window = obj[key]
    if (window === null || typeof window !== "object") continue
    const util = utilizationOf(window as Record<string, unknown>)
    if (util === null) continue
    windows.push({ label, usedPct: Math.round(util), resetsAt: resetsAtOf(window as Record<string, unknown>) })
  }
  return windows
}

function parseTimestamp(raw: unknown): number | undefined {
  if (typeof raw === "number") return raw > 1e11 ? raw : raw * 1000
  if (typeof raw !== "string") return undefined
  const parsed = Date.parse(raw)
  return Number.isNaN(parsed) ? undefined : parsed
}

function resetsAtOf(window: Record<string, unknown>): number | undefined {
  return parseTimestamp(window.resets_at)
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

function windowLabel(seconds: unknown): string {
  if (typeof seconds !== "number" || seconds <= 0) return "window"
  const hours = Math.round(seconds / 3600)
  return hours < 24 ? `${hours}h` : `${Math.round(hours / 24)}d`
}

function codexWindows(rateLimit: unknown, prefix = ""): UsageWindow[] {
  if (rateLimit === null || typeof rateLimit !== "object") return []
  const limit = rateLimit as Record<string, unknown>
  const windows: UsageWindow[] = []
  for (const key of ["primary_window", "secondary_window"]) {
    const window = limit[key]
    if (window === null || typeof window !== "object") continue
    const entry = window as Record<string, unknown>
    if (typeof entry.used_percent !== "number") continue
    windows.push({
      label: `${prefix}${windowLabel(entry.limit_window_seconds)}`,
      usedPct: Math.round(entry.used_percent),
      resetsAt: parseTimestamp(entry.reset_at),
    })
  }
  return windows
}

/** Reads the plan window plus any model-scoped limits out of Codex's usage payload. */
export function parseCodexUsage(payload: unknown): LiveUsage | null {
  if (payload === null || typeof payload !== "object") return null
  const obj = payload as Record<string, unknown>
  const windows = codexWindows(obj.rate_limit)

  if (Array.isArray(obj.additional_rate_limits)) {
    for (const entry of obj.additional_rate_limits) {
      if (entry === null || typeof entry !== "object") continue
      const extra = entry as Record<string, unknown>
      const name = typeof extra.limit_name === "string" ? `${extra.limit_name} ` : ""
      windows.push(...codexWindows(extra.rate_limit, name))
    }
  }
  return summarize(windows)
}

/** Pulls the ChatGPT access token and account id out of a Codex auth.json blob. */
export function codexCredentials(auth: string): { token: string; account: string } | null {
  try {
    const tokens = JSON.parse(auth)?.tokens
    const token = tokens?.access_token
    const account = tokens?.account_id
    return typeof token === "string" && typeof account === "string" ? { token, account } : null
  } catch {
    return null
  }
}

/** Asks ChatGPT what is left on the Codex plan windows for one account. */
export async function codexUsage(token: string, account: string): Promise<LiveUsage | null> {
  const payload = await probeJson(process.env.KYORA_CODEX_USAGE_URL ?? "https://chatgpt.com/backend-api/codex/usage", {
    authorization: `Bearer ${token}`,
    "chatgpt-account-id": account,
    accept: "application/json",
  })
  return payload ? parseCodexUsage(payload) : null
}
