import { describe, expect, test } from "bun:test"
import { extractFindings, extractPayload } from "./extract"

const payload = {
  findings: [
    { file: "src/a.ts", line: 10, severity: "major", title: "Off-by-one in pagination", body: "The loop skips the last page." },
  ],
}

describe("extractPayload", () => {
  test("parses bare JSON", () => {
    expect(extractPayload(JSON.stringify(payload), "findings")).toEqual(payload)
  })

  test("unwraps the claude -p json envelope", () => {
    const envelope = JSON.stringify({ type: "result", result: JSON.stringify(payload) })
    expect(extractPayload(envelope, "findings")).toEqual(payload)
  })

  test("unwraps the grok structuredOutput envelope", () => {
    const envelope = JSON.stringify({ status: "done", structuredOutput: payload })
    expect(extractPayload(envelope, "findings")).toEqual(payload)
  })

  test("finds fenced json in prose", () => {
    const raw = `Here is my review:\n\`\`\`json\n${JSON.stringify(payload)}\n\`\`\`\nDone.`
    expect(extractPayload(raw, "findings")).toEqual(payload)
  })

  test("scans NDJSON event streams from the end", () => {
    const raw = [
      JSON.stringify({ type: "step_start" }),
      JSON.stringify({ type: "text", text: "thinking..." }),
      JSON.stringify({ type: "text", text: JSON.stringify(payload) }),
    ].join("\n")
    expect(extractPayload(raw, "findings")).toEqual(payload)
  })

  test("recovers a balanced object embedded in prose", () => {
    const raw = `Review complete. ${JSON.stringify(payload)} — that's everything.`
    expect(extractPayload(raw, "findings")).toEqual(payload)
  })

  test("returns null for garbage", () => {
    expect(extractPayload("no json here", "findings")).toBeNull()
    expect(extractPayload("", "findings")).toBeNull()
  })
})

describe("extractFindings", () => {
  test("tags findings with the engine and normalizes paths", () => {
    const raw = JSON.stringify({
      findings: [{ file: "b/src/a.ts", line: "12", severity: "HIGH", title: "T", body: "B" }],
    })
    const findings = extractFindings(raw, "codex", 20)
    expect(findings).toHaveLength(1)
    expect(findings[0]!.engine).toBe("codex")
    expect(findings[0]!.file).toBe("src/a.ts")
    expect(findings[0]!.line).toBe(12)
    expect(findings[0]!.severity).toBe("critical")
  })

  test("treats null optional fields as absent", () => {
    const raw = JSON.stringify({
      findings: [{ file: "a.ts", line: 3, endLine: null, severity: "major", category: null, title: "T", body: "B", suggestion: null }],
    })
    const findings = extractFindings(raw, "codex", 20)
    expect(findings).toHaveLength(1)
    expect(findings[0]!).not.toHaveProperty("endLine")
    expect(findings[0]!).not.toHaveProperty("suggestion")
  })

  test("drops malformed findings and honors the limit", () => {
    const raw = JSON.stringify({
      findings: [
        { file: "a.ts", line: 1, severity: "minor", title: "one", body: "x" },
        { title: "no file or line" },
        { file: "a.ts", line: 2, severity: "weird", title: "two", body: "y" },
        { file: "a.ts", line: 3, severity: "nit", title: "three", body: "z" },
      ],
    })
    const findings = extractFindings(raw, "grok", 2)
    expect(findings).toHaveLength(2)
    expect(findings[1]!.severity).toBe("minor")
  })
})
