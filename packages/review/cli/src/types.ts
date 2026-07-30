export type Severity = "critical" | "major" | "minor" | "nit"

export const SEVERITIES: Severity[] = ["critical", "major", "minor", "nit"]

export const SEVERITY_RANK: Record<Severity, number> = {
  critical: 3,
  major: 2,
  minor: 1,
  nit: 0,
}

export interface Finding {
  file: string
  line: number
  endLine?: number
  severity: Severity
  category?: string
  title: string
  body: string
  suggestion?: string
}

export interface EngineFinding extends Finding {
  engine: string
}

export type Tier = "consensus" | "verified" | "single"

export interface MergedFinding extends Finding {
  engines: string[]
  tier: Tier
  verifyReason?: string
}

export interface EngineRun {
  engine: string
  ok: boolean
  findings: EngineFinding[]
  error?: string
  durationMs: number
}

export interface PrInfo {
  owner: string
  repo: string
  number: number
  baseRef: string
  headSha: string
}

export interface ReviewContext {
  repoRoot: string
  baseDescription: string
  diff: string
  changedFiles: string[]
  /** file → set of NEW-side line numbers that appear in diff hunks (valid anchors for PR comments) */
  commentableLines: Map<string, Set<number>>
  pr?: PrInfo
}

export interface EngineOverride {
  bin?: string
  args?: string[]
  env?: Record<string, string>
  model?: string
}

export interface ReviewConfig {
  engines: string[]
  verify: boolean
  post: boolean
  base: string
  failOn: Severity | "none"
  maxDiffBytes: number
  timeoutMs: number
  maxFindingsPerEngine: number
  overrides: Record<string, EngineOverride>
}
