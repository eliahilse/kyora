import { beforeEach, describe, expect, test } from "bun:test"
import { mkdtempSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { cooldownRemainingMs, lastRunAt, loadUsage, looksRateLimited, markRun, resetHintMs } from "./usage"

beforeEach(() => {
  process.env.KYORA_REVIEW_STATE_DIR = mkdtempSync(join(tmpdir(), "kyora-usage-"))
})

describe("usage state", () => {
  test("rate_limited sets a cooldown, ok clears it", () => {
    markRun("codex", "rate_limited", 60_000)
    expect(cooldownRemainingMs("codex")).toBeGreaterThan(0)
    markRun("codex", "ok", 60_000)
    expect(cooldownRemainingMs("codex")).toBe(0)
    expect(loadUsage().engines.codex!.runs).toBe(2)
  })

  test("lastRunAt orders engines for rotation", () => {
    markRun("grok", "ok", 0)
    expect(lastRunAt("grok")).toBeGreaterThan(0)
    expect(lastRunAt("never-ran")).toBe(0)
  })

  test("missing state file reads as empty", () => {
    expect(loadUsage()).toEqual({ engines: {} })
    expect(cooldownRemainingMs("codex")).toBe(0)
  })
})

describe("looksRateLimited", () => {
  test("matches vendor limit messages", () => {
    expect(looksRateLimited("Claude usage limit reached. Your limit resets at 7pm")).toBe(true)
    expect(looksRateLimited("HTTP 429 Too Many Requests")).toBe(true)
    expect(looksRateLimited("insufficient_quota: You exceeded your current quota")).toBe(true)
    expect(looksRateLimited("You've hit your usage limit.")).toBe(true)
  })

  test("ignores ordinary failures", () => {
    expect(looksRateLimited("SyntaxError: unexpected token")).toBe(false)
    expect(looksRateLimited("exit 1: command not found")).toBe(false)
  })
})

describe("resetHintMs", () => {
  test("parses duration hints", () => {
    expect(resetHintMs("Please try again in 3 hours.")).toBe(3 * 3_600_000)
    expect(resetHintMs("retry after 90 seconds")).toBe(90_000)
    expect(resetHintMs("resets in 45 minutes")).toBe(45 * 60_000)
  })

  test("returns null without a parseable duration", () => {
    expect(resetHintMs("limit resets at 7pm")).toBeNull()
    expect(resetHintMs("try again later")).toBeNull()
  })
})
