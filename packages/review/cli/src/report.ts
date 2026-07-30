import type { EngineRun, MergedFinding, ReviewContext, Severity } from "./types"

const SEVERITY_EMOJI: Record<Severity, string> = {
  critical: "🔴",
  major: "🟠",
  minor: "🟡",
  nit: "⚪",
}

const TIER_BADGE = {
  consensus: "consensus",
  verified: "cross-verified",
  single: "single engine",
} as const

export function renderFindingSection(finding: MergedFinding): string {
  const parts = [
    `#### ${SEVERITY_EMOJI[finding.severity]} ${finding.severity} · \`${finding.file}:${finding.line}\` — ${finding.title}`,
    `_${TIER_BADGE[finding.tier]}: ${finding.engines.join(", ")}${finding.category ? ` · ${finding.category}` : ""}_`,
    "",
    finding.body,
  ]
  if (finding.verifyReason) parts.push("", `> verifier: ${finding.verifyReason}`)
  if (finding.suggestion) parts.push("", "```suggestion", finding.suggestion, "```")
  return parts.join("\n")
}

/** Body for an inline PR comment — no file:line header, the anchor carries that. */
export function renderInlineComment(finding: MergedFinding): string {
  const parts = [
    `**${SEVERITY_EMOJI[finding.severity]} ${finding.severity} — ${finding.title}**`,
    `_${TIER_BADGE[finding.tier]}: ${finding.engines.join(", ")}_`,
    "",
    finding.body,
  ]
  if (finding.verifyReason) parts.push("", `> verifier: ${finding.verifyReason}`)
  if (finding.suggestion) parts.push("", "```suggestion", finding.suggestion, "```")
  return parts.join("\n")
}

export function summaryLine(ctx: ReviewContext, merged: MergedFinding[], runs: EngineRun[]): string {
  const consensus = merged.filter((finding) => finding.tier === "consensus").length
  const verified = merged.filter((finding) => finding.tier === "verified").length
  const single = merged.length - consensus - verified
  const engines = runs.map((run) => run.engine).join(", ")
  const tally =
    merged.length === 0
      ? "**no findings**"
      : `**${merged.length} finding${merged.length === 1 ? "" : "s"}** (${consensus} consensus · ${verified} verified · ${single} single)`
  return `${tally} — ${runs.length} engine${runs.length === 1 ? "" : "s"} (${engines}) on \`${ctx.baseDescription}\` → HEAD, ${ctx.changedFiles.length} files`
}

export function renderReport(ctx: ReviewContext, merged: MergedFinding[], runs: EngineRun[]): string {
  const parts = ["## kyora review", "", summaryLine(ctx, merged, runs)]

  if (merged.length === 0) {
    const healthy = runs.filter((run) => run.ok).length
    parts.push("", `${healthy} engine${healthy === 1 ? "" : "s"} reviewed the change and found nothing worth flagging.`)
  } else {
    for (const finding of merged) parts.push("", "---", "", renderFindingSection(finding))
  }

  parts.push("", "<details>", "<summary>engine runs</summary>", "", "| engine | status | findings | time |", "| --- | --- | --- | --- |")
  for (const run of runs) {
    const status = run.ok ? "ok" : `failed: ${run.error ?? "unknown"}`
    parts.push(`| ${run.engine} | ${status.replaceAll("|", "\\|")} | ${run.findings.length} | ${Math.round(run.durationMs / 1000)}s |`)
  }
  parts.push("", "</details>", "")
  return parts.join("\n")
}
