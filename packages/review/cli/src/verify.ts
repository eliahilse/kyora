import { runEngineRaw, type EngineDef } from "./engines"
import { extractPayload } from "./extract"
import { VERDICTS_SCHEMA, verifyPrompt, type VerifyClaim } from "./schema"
import type { MergedFinding, ReviewConfig, ReviewContext } from "./types"

interface Batch {
  verifier: EngineDef
  claims: VerifyClaim[]
  mergedIndexes: number[]
}

/**
 * Findings reported by a single engine get cross-examined by a different one.
 * Confirmed → promoted to "verified"; refuted → dropped; if verification
 * fails to run or answer, the finding stays "single" (never silently upgraded).
 */
export async function verifySingles(
  merged: MergedFinding[],
  available: EngineDef[],
  ctx: ReviewContext,
  config: ReviewConfig,
  log: (message: string) => void,
): Promise<MergedFinding[]> {
  const batches = new Map<string, Batch>()
  merged.forEach((finding, index) => {
    if (finding.tier !== "single") return
    const verifier = available.find((engine) => !finding.engines.includes(engine.id))
    if (!verifier) return
    let batch = batches.get(verifier.id)
    if (!batch) {
      batch = { verifier, claims: [], mergedIndexes: [] }
      batches.set(verifier.id, batch)
    }
    batch.claims.push({
      index: batch.claims.length,
      engine: finding.engines.join("+"),
      file: finding.file,
      line: finding.line,
      severity: finding.severity,
      title: finding.title,
      body: finding.body,
    })
    batch.mergedIndexes.push(index)
  })
  if (batches.size === 0) return merged

  const drop = new Set<number>()
  const updated = [...merged]
  await Promise.all(
    [...batches.values()].map(async (batch) => {
      log(`verify: ${batch.verifier.id} cross-examining ${batch.claims.length} single-engine finding(s)`)
      const run = await runEngineRaw(batch.verifier, verifyPrompt(batch.claims), VERDICTS_SCHEMA, ctx.repoRoot, config)
      const payload = run.ok ? extractPayload(run.raw, "verdicts") : null
      if (!payload) {
        log(`verify: ${batch.verifier.id} did not return verdicts (${run.error ?? "unparseable output"}) — findings stay unverified`)
        return
      }
      const verdicts = payload.verdicts as { index?: unknown; verdict?: unknown; reason?: unknown }[]
      for (const verdict of verdicts) {
        const local = typeof verdict.index === "number" ? verdict.index : -1
        const mergedIndex = batch.mergedIndexes[local]
        if (mergedIndex === undefined) continue
        const finding = updated[mergedIndex]!
        const reason = typeof verdict.reason === "string" ? verdict.reason.slice(0, 1000) : ""
        if (verdict.verdict === "confirmed") {
          updated[mergedIndex] = { ...finding, tier: "verified", verifyReason: reason }
        } else if (verdict.verdict === "refuted") {
          drop.add(mergedIndex)
          log(`verify: dropped "${finding.title}" (${finding.file}:${finding.line}) — ${reason || "refuted"}`)
        }
      }
    }),
  )
  return updated.filter((_, index) => !drop.has(index))
}
