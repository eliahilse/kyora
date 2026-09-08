import { beforeEach, describe, expect, test } from "bun:test"
import { mkdtempSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { cooldownRemainingMs, lastRunAt, loadUsage, looksRateLimited, markRun, resetHintMs } from "./state"
import { codexCredentials, parseClaudeOauthUsage, parseCodexUsage, parseQuotaWindows, parseTokenPlanUsage, parseZaiQuota } from "./quota"

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

describe("parseClaudeOauthUsage", () => {
  test("prefers the limits array, so a model-scoped weekly window is not missed", () => {
    const parsed = parseClaudeOauthUsage({
      five_hour: { utilization: 49, resets_at: "2026-09-08T03:40:00.011853+00:00" },
      seven_day: { utilization: 32, resets_at: "2026-09-12T09:00:00.011875+00:00" },
      limits: [
        { kind: "session", group: "session", percent: 49, resets_at: "2026-09-08T03:40:00.011853+00:00", scope: null, is_active: false },
        { kind: "weekly_all", group: "weekly", percent: 32, resets_at: "2026-09-12T09:00:00.011875+00:00", scope: null, is_active: false },
        {
          kind: "weekly_scoped",
          group: "weekly",
          percent: 62,
          resets_at: "2026-09-12T09:00:00.012118+00:00",
          scope: { model: { id: null, display_name: "Fable" }, surface: null },
          is_active: true,
        },
      ],
    })
    expect(parsed?.windows?.map((w) => w.label)).toEqual(["session", "weekly", "weekly Fable"])
    expect(parsed?.windows?.find((w) => w.label === "weekly Fable")).toEqual({
      label: "weekly Fable",
      usedPct: 62,
      resetsAt: Date.parse("2026-09-12T09:00:00.012118+00:00"),
      active: true,
    })
    expect(parsed?.remainingPct).toBe(38)
  })

  test("falls back to the top-level windows when there is no limits array", () => {
    const parsed = parseClaudeOauthUsage({
      five_hour: { utilization: 32, resets_at: "2026-09-08T03:39:59.660912+00:00" },
      seven_day: { utilization: 30 },
    })
    expect(parsed?.remainingPct).toBe(68)
    expect(parsed?.windows).toEqual([
      { label: "5h", usedPct: 32, resetsAt: Date.parse("2026-09-08T03:39:59.660912+00:00") },
      { label: "7d", usedPct: 30, resetsAt: undefined },
    ])
  })

  test("skips windows the payload leaves null and reports nothing when all are", () => {
    expect(parseClaudeOauthUsage({ five_hour: { utilization: 45 }, seven_day: null })?.remainingPct).toBe(55)
    expect(parseClaudeOauthUsage({ five_hour: null, seven_day: null })).toBeNull()
    expect(parseClaudeOauthUsage(null)).toBeNull()
  })
})

describe("parseCodexUsage", () => {
  const payload = {
    plan_type: "pro",
    rate_limit: {
      primary_window: { used_percent: 73, limit_window_seconds: 604800, reset_at: 1789321995 },
      secondary_window: null,
    },
    additional_rate_limits: [
      {
        limit_name: "GPT-5.3-Codex-Spark",
        rate_limit: {
          primary_window: { used_percent: 4, limit_window_seconds: 18000, reset_at: 1788843816 },
          secondary_window: { used_percent: 9, limit_window_seconds: 604800, reset_at: 1789430616 },
        },
      },
    ],
  }

  test("labels windows by length and keeps named sub-limits", () => {
    const parsed = parseCodexUsage(payload)
    expect(parsed?.windows).toEqual([
      { label: "7d", usedPct: 73, resetsAt: 1789321995000 },
      { label: "GPT-5.3-Codex-Spark 5h", usedPct: 4, resetsAt: 1788843816000 },
      { label: "GPT-5.3-Codex-Spark 7d", usedPct: 9, resetsAt: 1789430616000 },
    ])
    expect(parsed?.remainingPct).toBe(27)
  })

  test("returns null when no window carries a percentage", () => {
    expect(parseCodexUsage({ rate_limit: { primary_window: null, secondary_window: null } })).toBeNull()
    expect(parseCodexUsage(null)).toBeNull()
  })
})

describe("codexCredentials", () => {
  test("takes the access token and account id, rejecting anything else", () => {
    expect(codexCredentials(JSON.stringify({ tokens: { access_token: "t", account_id: "a" } }))).toEqual({
      token: "t",
      account: "a",
    })
    expect(codexCredentials(JSON.stringify({ tokens: { access_token: "t" } }))).toBeNull()
    expect(codexCredentials("not json")).toBeNull()
  })
})
