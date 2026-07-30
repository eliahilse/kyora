import { describe, expect, test } from "bun:test"
import { parseVerdict, worthClassifying } from "./stakes"

describe("worthClassifying", () => {
  test("ignores read-only and trivial tools", () => {
    expect(worthClassifying("Read", '{"file_path":"src/auth.ts"}')).toBe(false)
    expect(worthClassifying("Grep", '{"pattern":"password"}')).toBe(false)
    expect(worthClassifying("Edit", '{"file_path":"README.md","new_string":"typo fix"}')).toBe(false)
  })

  test("flags sensitive edits and consequential commands", () => {
    expect(worthClassifying("Edit", '{"file_path":"src/auth/session.ts","new_string":"token"}')).toBe(true)
    expect(worthClassifying("Write", '{"file_path":"migrations/001.sql","content":"DROP TABLE users"}')).toBe(true)
    expect(worthClassifying("Bash", '{"command":"git push --force origin main"}')).toBe(true)
    expect(worthClassifying("Bash", '{"command":"terraform apply"}')).toBe(true)
  })

  test("flags very large edits even without keywords", () => {
    expect(worthClassifying("Write", `{"content":"${"x".repeat(1600)}"}`)).toBe(true)
    expect(worthClassifying("Bash", '{"command":"ls -la"}')).toBe(false)
  })
})

describe("parseVerdict", () => {
  test("parses a verdict embedded in prose", () => {
    const verdict = parseVerdict('Sure: {"highStakes": true, "reason": "Dropping a table.", "suggested": ["codex"]}')
    expect(verdict).toEqual({ highStakes: true, reason: "Dropping a table.", suggested: ["codex"] })
  })

  test("rejects malformed or non-verdict output", () => {
    expect(parseVerdict("no json here")).toBeNull()
    expect(parseVerdict('{"reason":"missing the boolean"}')).toBeNull()
  })

  test("tolerates missing optional fields", () => {
    expect(parseVerdict('{"highStakes": false}')).toEqual({ highStakes: false, reason: "", suggested: [] })
  })
})
