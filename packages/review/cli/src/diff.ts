import type { PrInfo, ReviewContext } from "./types"

export async function repoRoot(): Promise<string | null> {
  const result = await Bun.$`git rev-parse --show-toplevel`.quiet().nothrow()
  if (result.exitCode !== 0) return null
  return result.text().trim()
}

async function resolveBase(base: string): Promise<string | null> {
  for (const candidate of [base, `origin/${base}`]) {
    const result = await Bun.$`git rev-parse --verify --quiet ${candidate}`.quiet().nothrow()
    if (result.exitCode === 0) return candidate
  }
  const fetched = await Bun.$`git fetch origin ${base}`.quiet().nothrow()
  if (fetched.exitCode === 0) return "FETCH_HEAD"
  return null
}

export async function buildContext(opts: {
  root: string
  base: string
  maxDiffBytes: number
  pr?: PrInfo
}): Promise<ReviewContext | { error: string }> {
  const baseName = opts.pr?.baseRef ?? opts.base
  const resolved = await resolveBase(baseName)
  if (!resolved) return { error: `cannot resolve base ref "${baseName}" (tried locally, origin/, and git fetch)` }

  const mergeBase = (await Bun.$`git merge-base ${resolved} HEAD`.quiet().nothrow().text()).trim()
  if (!mergeBase) return { error: `no merge-base between ${baseName} and HEAD` }

  let diff = await Bun.$`git diff --no-color ${mergeBase} HEAD`.quiet().text()
  const changedFiles = (await Bun.$`git diff --name-only ${mergeBase} HEAD`.quiet().text())
    .split("\n")
    .filter(Boolean)

  const commentableLines = parseCommentableLines(diff)

  if (diff.length > opts.maxDiffBytes) {
    diff = `${diff.slice(0, opts.maxDiffBytes)}\n\n[diff truncated at ${opts.maxDiffBytes} bytes — run \`git diff ${mergeBase.slice(0, 12)} HEAD\` yourself for the rest]`
  }

  return {
    repoRoot: opts.root,
    baseDescription: `${baseName}@${mergeBase.slice(0, 12)}`,
    diff,
    changedFiles,
    commentableLines,
    ...(opts.pr ? { pr: opts.pr } : {}),
  }
}

/**
 * NEW-side line numbers that appear inside diff hunks, per file — the only
 * positions GitHub accepts for inline PR review comments (side=RIGHT).
 */
export function parseCommentableLines(diff: string): Map<string, Set<number>> {
  const result = new Map<string, Set<number>>()
  let file: string | null = null
  let newLine = 0
  let inHunk = false
  for (const line of diff.split("\n")) {
    if (line.startsWith("+++ ")) {
      const path = line.slice(4).trim()
      file = path === "/dev/null" ? null : path.replace(/^b\//, "")
      inHunk = false
      continue
    }
    const hunk = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line)
    if (hunk) {
      newLine = parseInt(hunk[1]!, 10)
      inHunk = true
      continue
    }
    if (!inHunk || !file) continue
    if (line.startsWith("diff --git")) {
      inHunk = false
      continue
    }
    if (line.startsWith("+") || line.startsWith(" ") || line === "") {
      let lines = result.get(file)
      if (!lines) {
        lines = new Set()
        result.set(file, lines)
      }
      lines.add(newLine)
      newLine++
    }
    // "-" lines belong to the old side and don't advance the new-side counter
  }
  return result
}
