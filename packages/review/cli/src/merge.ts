import { SEVERITY_RANK, type EngineFinding, type MergedFinding } from "./types"

const STOPWORDS = new Set([
  "the", "a", "an", "of", "in", "on", "to", "is", "are", "for", "and", "or",
  "with", "this", "that", "be", "it", "as", "at", "by", "from", "not", "when",
])

function tokens(text: string): Set<string> {
  return new Set(
    text
      .toLowerCase()
      .split(/[^a-z0-9_]+/)
      .filter((token) => token.length > 1 && !STOPWORDS.has(token)),
  )
}

export function similarity(a: string, b: string): number {
  const ta = tokens(a)
  const tb = tokens(b)
  if (ta.size === 0 || tb.size === 0) return 0
  let shared = 0
  for (const token of ta) if (tb.has(token)) shared++
  return shared / (ta.size + tb.size - shared)
}

const LINE_SLACK = 3

function rangesOverlap(a: EngineFinding, b: EngineFinding): boolean {
  const aEnd = a.endLine ?? a.line
  const bEnd = b.endLine ?? b.line
  return a.line - LINE_SLACK <= bEnd && b.line - LINE_SLACK <= aEnd
}

function sameIssue(a: EngineFinding, b: EngineFinding): boolean {
  if (a.file !== b.file || !rangesOverlap(a, b)) return false
  return similarity(`${a.title} ${a.body}`, `${b.title} ${b.body}`) >= 0.2
}

/**
 * Greedy clustering: findings from different engines that point at the same
 * file/lines and describe the same issue collapse into one merged finding.
 * Two or more engines agreeing promotes the cluster to the "consensus" tier.
 */
export function mergeFindings(all: EngineFinding[]): MergedFinding[] {
  const clusters: EngineFinding[][] = []
  for (const finding of all) {
    const home = clusters.find((cluster) => cluster.some((member) => sameIssue(member, finding)))
    if (home) home.push(finding)
    else clusters.push([finding])
  }

  const merged = clusters.map((cluster) => {
    const canonical = cluster.reduce((best, candidate) => {
      if (SEVERITY_RANK[candidate.severity] !== SEVERITY_RANK[best.severity]) {
        return SEVERITY_RANK[candidate.severity] > SEVERITY_RANK[best.severity] ? candidate : best
      }
      return candidate.body.length > best.body.length ? candidate : best
    })
    const engines = [...new Set(cluster.map((member) => member.engine))].sort()
    const { engine: _engine, ...finding } = canonical
    return {
      ...finding,
      engines,
      tier: engines.length > 1 ? ("consensus" as const) : ("single" as const),
    }
  })

  return merged.sort((a, b) => {
    if (a.tier !== b.tier) return a.tier === "consensus" ? -1 : 1
    if (a.severity !== b.severity) return SEVERITY_RANK[b.severity] - SEVERITY_RANK[a.severity]
    return a.file === b.file ? a.line - b.line : a.file.localeCompare(b.file)
  })
}
