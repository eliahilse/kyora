import { beforeEach, describe, expect, test } from "bun:test"
import { mkdtempSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { cooldownRemainingMs, lastRunAt, loadUsage, looksRateLimited, markRun, parseClaudeOauthUsage, parseQuotaWindows, parseTokenPlanUsage, parseZaiQuota, resetHintMs } from "./usage"

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

describe("parseTokenPlanUsage", () => {
  test("finds used/total pairs regardless of casing and nesting", () => {
    const payload = { code: 200, data: { subscription: { UsedCredit: 250, TotalCredit: 1000 } } }
    expect(parseTokenPlanUsage(payload)).toEqual({ remainingPct: 75, detail: "750 of 1,000 credits left" })
  })

  test("returns null for empty or zero-total payloads", () => {
    expect(parseTokenPlanUsage({ data: { totalCount: 0 } })).toBeNull()
    expect(parseTokenPlanUsage(null)).toBeNull()
    expect(parseTokenPlanUsage("<html>login</html>")).toBeNull()
  })
})

describe("parseQuotaWindows", () => {
  test("collects windows and reports the tightest one", () => {
    const payload = {
      usage: { limit: 2048, used: 512, remaining: 1536, resetTime: "2026-08-03T00:00:00Z" },
      limits: [{ window: "5h", limit: 200, used: 190, remaining: 10 }],
    }
    const live = parseQuotaWindows(payload)
    expect(live!.remainingPct).toBe(5)
    expect(live!.detail).toContain("512/2,048")
    expect(live!.detail).toContain("190/200 5h")
  })

  test("returns null when no used/limit pairs exist", () => {
    expect(parseQuotaWindows({ message: "ok" })).toBeNull()
    expect(parseQuotaWindows(null)).toBeNull()
  })
})

describe("parseZaiQuota", () => {
  test("reads percentage windows from the live monitor shape", () => {
    const payload = {
      code: 200,
      data: {
        limits: [
          { type: "TIME_LIMIT", unit: 5, number: 1, usage: 100, currentValue: 0, remaining: 100, percentage: 12 },
          { type: "TOKENS_LIMIT", unit: 3, number: 5, percentage: 87 },
        ],
      },
    }
    const live = parseZaiQuota(payload)
    expect(live!.remainingPct).toBe(13)
    expect(live!.detail).toBe("time 12% used · tokens 87% used")
  })

  test("falls back to used/limit pairs when no percentages exist", () => {
    expect(parseZaiQuota({ data: { used: 30, limit: 100 } })!.remainingPct).toBe(70)
  })
})

describe("parseClaudeOauthUsage", () => {
  test("reads window utilization in fraction or percent form", () => {
    const live = parseClaudeOauthUsage({ five_hour: { utilization: 0.42 }, seven_day: { utilization: 61 } })
    expect(live!.remainingPct).toBe(39)
    expect(live!.detail).toBe("5h 42% used · 7d 61% used")
  })

  test("returns null without usable windows", () => {
    expect(parseClaudeOauthUsage({ subscriptionType: "max" })).toBeNull()
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
