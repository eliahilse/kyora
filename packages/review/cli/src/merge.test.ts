import { describe, expect, test } from "bun:test"
import { mergeFindings } from "./merge"
import type { EngineFinding } from "./types"

const finding = (overrides: Partial<EngineFinding>): EngineFinding => ({
  engine: "codex",
  file: "src/a.ts",
  line: 10,
  severity: "minor",
  title: "Unchecked null return",
  body: "getUser can return null and this dereferences it.",
  ...overrides,
})

describe("mergeFindings", () => {
  test("clusters the same issue from two engines into consensus", () => {
    const merged = mergeFindings([
      finding({ engine: "codex", line: 10, severity: "major" }),
      finding({ engine: "claude", line: 12, title: "Null return not checked", body: "Dereference of possibly-null getUser result." }),
    ])
    expect(merged).toHaveLength(1)
    expect(merged[0]!.tier).toBe("consensus")
    expect(merged[0]!.engines).toEqual(["claude", "codex"])
    expect(merged[0]!.severity).toBe("major")
  })

  test("keeps unrelated findings separate", () => {
    const merged = mergeFindings([
      finding({ engine: "codex" }),
      finding({ engine: "claude", file: "src/b.ts", title: "SQL injection in query builder", body: "User input concatenated into SQL." }),
      finding({ engine: "claude", line: 300, title: "Race on cache write", body: "Two writers, no lock." }),
    ])
    expect(merged).toHaveLength(3)
    expect(merged.every((item) => item.tier === "single")).toBe(true)
  })

  test("does not cluster dissimilar issues on nearby lines", () => {
    const merged = mergeFindings([
      finding({ engine: "codex", line: 10, title: "Memory leak in listener", body: "Event listener never removed on unmount." }),
      finding({ engine: "claude", line: 11, title: "Wrong HTTP status code", body: "Returns 200 for a validation failure." }),
    ])
    expect(merged).toHaveLength(2)
  })

  test("sorts consensus first, then by severity", () => {
    const merged = mergeFindings([
      finding({ engine: "codex", file: "z.ts", line: 1, severity: "critical", title: "Data loss on retry", body: "Retries drop the payload." }),
      finding({ engine: "codex", file: "a.ts", line: 5, severity: "major" }),
      finding({ engine: "claude", file: "a.ts", line: 6, severity: "major" }),
    ])
    expect(merged[0]!.tier).toBe("consensus")
    expect(merged[1]!.severity).toBe("critical")
  })
})
