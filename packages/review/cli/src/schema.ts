import type { ReviewContext } from "./types"

/** JSON Schema handed to engines that support schema-constrained output (codex --output-schema, grok --json-schema). */
export const FINDINGS_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["findings"],
  properties: {
    findings: {
      type: "array",
      items: {
        // OpenAI strict structured outputs (codex --output-schema) reject any
        // property missing from `required`, so optional fields are nullable
        type: "object",
        additionalProperties: false,
        required: ["file", "line", "endLine", "severity", "category", "title", "body", "suggestion"],
        properties: {
          file: { type: "string", description: "repo-relative path" },
          line: { type: "integer", description: "line number in the NEW version of the file" },
          endLine: { type: ["integer", "null"] },
          severity: { type: "string", enum: ["critical", "major", "minor", "nit"] },
          category: { type: ["string", "null"] },
          title: { type: "string", description: "one-line summary of the issue" },
          body: { type: "string", description: "explanation: what breaks, when, and why" },
          suggestion: { type: ["string", "null"], description: "replacement code for the flagged lines, if applicable" },
        },
      },
    },
  },
} as const

export const VERDICTS_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["verdicts"],
  properties: {
    verdicts: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["index", "verdict", "reason"],
        properties: {
          index: { type: "integer" },
          verdict: { type: "string", enum: ["confirmed", "refuted"] },
          reason: { type: "string" },
        },
      },
    },
  },
} as const

export function reviewPrompt(ctx: ReviewContext, maxFindings: number): string {
  return `You are one reviewer on a multi-model code review panel. Different models review the same change independently; findings are cross-checked afterwards, so precision matters more than volume.

You are inside the repository checkout (current working directory). The change under review is the diff below. Before reporting a finding, verify it against the actual code: read the surrounding file, check callers and tests with grep, confirm the claim holds. Never report an issue you could have disproven by reading the repo. Do not modify any files.

Report only issues that matter: bugs, correctness, security, data loss, races, broken error handling, API misuse, significant performance problems. No style or formatting nits unless they hide a defect. Report at most ${maxFindings} findings — fewer, well-verified findings beat many speculative ones. If the change looks correct, return an empty findings array.

Severity: "critical" = will break production or lose/leak data; "major" = real bug or vulnerability with limited blast radius; "minor" = genuine defect, low impact; "nit" = only if it masks a defect.

For each finding: "file" is the repo-relative path, "line" the line number in the NEW version of the file (must be a line in or near the diff). "suggestion" is optional replacement code for the flagged lines only.

OUTPUT: respond with ONLY a JSON object matching this shape, no prose before or after:
{"findings": [{"file": "...", "line": 1, "severity": "major", "category": "correctness", "title": "...", "body": "...", "suggestion": "..."}]}

CHANGE UNDER REVIEW (${ctx.baseDescription} → HEAD, ${ctx.changedFiles.length} files):

${ctx.diff}`
}

export interface VerifyClaim {
  index: number
  engine: string
  file: string
  line: number
  severity: string
  title: string
  body: string
}

export function verifyPrompt(claims: VerifyClaim[]): string {
  const list = claims
    .map(
      (claim) =>
        `[${claim.index}] ${claim.file}:${claim.line} (${claim.severity}, reported by ${claim.engine})\n${claim.title}\n${claim.body}`,
    )
    .join("\n\n")
  return `You are an adversarial verifier on a code review panel. Each claim below was reported by only ONE reviewer, so it is suspect. You are inside the repository checkout: for each claim, read the actual code and decide whether the issue is real.

Actively try to refute each claim. "confirmed" only if you can point at the code path that makes it true; if the claim is speculative, already handled elsewhere, or you cannot reproduce the reasoning from the code, return "refuted". Do not modify any files.

OUTPUT: respond with ONLY a JSON object, no prose, one verdict per claim index:
{"verdicts": [{"index": 0, "verdict": "confirmed", "reason": "..."}]}

CLAIMS:

${list}`
}
