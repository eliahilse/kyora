import { SEVERITIES, type EngineFinding, type Finding, type Severity } from "./types"

/**
 * Engines differ in how faithfully they honor "output only JSON" (and some CLIs
 * wrap the model output in envelopes or NDJSON event streams), so extraction is
 * layered: exact parse → known envelopes → fenced blocks → balanced-brace scan.
 */
export function extractPayload(raw: string, key: string): Record<string, unknown> | null {
  const trimmed = raw.trim()
  if (!trimmed) return null

  const direct = tryParse(trimmed)
  const fromDirect = fromParsed(direct, key)
  if (fromDirect) return fromDirect

  // NDJSON / stream-json: scan lines from the end
  const lines = trimmed.split("\n")
  if (lines.length > 1) {
    for (let i = lines.length - 1; i >= 0; i--) {
      const found = fromParsed(tryParse(lines[i]!.trim()), key)
      if (found) return found
    }
  }

  // fenced ```json blocks, last first
  const fences = [...trimmed.matchAll(/```(?:json)?\s*([\s\S]*?)```/g)]
  for (let i = fences.length - 1; i >= 0; i--) {
    const found = fromParsed(tryParse(fences[i]![1]!.trim()), key)
    if (found) return found
  }

  // balanced-brace scan around the last occurrences of the key
  let at = trimmed.lastIndexOf(`"${key}"`)
  while (at !== -1) {
    const start = trimmed.lastIndexOf("{", at)
    if (start !== -1) {
      const candidate = balancedSlice(trimmed, start)
      const found = fromParsed(tryParse(candidate), key)
      if (found) return found
    }
    at = trimmed.lastIndexOf(`"${key}"`, at - 1)
  }
  return null
}

function fromParsed(parsed: unknown, key: string): Record<string, unknown> | null {
  if (parsed === null || typeof parsed !== "object") return null
  const obj = parsed as Record<string, unknown>
  if (Array.isArray(obj[key])) return obj
  // claude -p --output-format json envelope: { type: "result", result: "<text>" }
  if (typeof obj.result === "string") return extractPayload(obj.result, key)
  // codex/grok event stream items sometimes nest under message/text/content
  for (const nested of ["message", "text", "content", "last_message"]) {
    if (typeof obj[nested] === "string") {
      const found = extractPayload(obj[nested], key)
      if (found) return found
    }
  }
  return null
}

function balancedSlice(text: string, start: number): string | null {
  let depth = 0
  let inString = false
  let escaped = false
  for (let i = start; i < text.length; i++) {
    const ch = text[i]
    if (escaped) {
      escaped = false
    } else if (ch === "\\") {
      escaped = true
    } else if (ch === '"') {
      inString = !inString
    } else if (!inString) {
      if (ch === "{") depth++
      else if (ch === "}") {
        depth--
        if (depth === 0) return text.slice(start, i + 1)
      }
    }
  }
  return null
}

function tryParse(text: string | null | undefined): unknown {
  if (!text) return null
  try {
    return JSON.parse(text)
  } catch {
    return null
  }
}

const MAX_TEXT = 4000

export function extractFindings(raw: string, engine: string, limit: number): EngineFinding[] {
  const payload = extractPayload(raw, "findings")
  if (!payload) return []
  const items = payload.findings as unknown[]
  const findings: EngineFinding[] = []
  for (const item of items) {
    const finding = coerceFinding(item)
    if (finding) findings.push({ ...finding, engine })
    if (findings.length >= limit) break
  }
  return findings
}

function coerceFinding(item: unknown): Finding | null {
  if (item === null || typeof item !== "object") return null
  const obj = item as Record<string, unknown>
  const file = normalizePath(str(obj.file))
  const title = str(obj.title) || str(obj.summary)
  const body = str(obj.body) || str(obj.description) || title
  const line = int(obj.line) ?? int(obj.startLine)
  if (!file || !title || line === null) return null
  const endLine = int(obj.endLine)
  const severity = coerceSeverity(str(obj.severity))
  const suggestion = str(obj.suggestion) || undefined
  const category = str(obj.category) || undefined
  return {
    file,
    line,
    ...(endLine !== null && endLine > line ? { endLine } : {}),
    severity,
    ...(category ? { category } : {}),
    title: title.slice(0, 200),
    body: body.slice(0, MAX_TEXT),
    ...(suggestion ? { suggestion: suggestion.slice(0, MAX_TEXT) } : {}),
  }
}

function coerceSeverity(value: string): Severity {
  const lower = value.toLowerCase()
  if ((SEVERITIES as string[]).includes(lower)) return lower as Severity
  if (["blocker", "high", "error"].includes(lower)) return "critical"
  if (["medium", "warning", "moderate"].includes(lower)) return "major"
  if (["low", "info", "style", "suggestion"].includes(lower)) return "nit"
  return "minor"
}

function normalizePath(path: string): string {
  return path.replace(/^\.\//, "").replace(/^[ab]\//, "")
}

function str(value: unknown): string {
  return typeof value === "string" ? value.trim() : ""
}

function int(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return Math.max(1, Math.round(value))
  if (typeof value === "string" && /^\d+$/.test(value.trim())) return Math.max(1, parseInt(value, 10))
  return null
}
