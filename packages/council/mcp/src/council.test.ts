import { mkdtemp, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { afterAll, describe, expect, test } from "bun:test"
import { DEFAULT_COOLDOWN_MINUTES, DEFAULT_TIMEOUT_MS, newJobId, reconcile, runConfig, type Job } from "./council"

const running: Job = {
  id: "council-1",
  kind: "council",
  status: "running",
  question: "?",
  engines: ["glm"],
  startedAt: Date.now() - 60_000,
  deadlineAt: Date.now() + 60_000,
}

describe("reconcile", () => {
  test("leaves a job that is still within its deadline alone", () => {
    expect(reconcile(running).status).toBe("running")
  })

  test("fails a job that outlived its deadline without recording a result", () => {
    const stranded = reconcile({ ...running, deadlineAt: Date.now() - 1 })
    expect(stranded.status).toBe("failed")
    expect(stranded.error).toContain("deadline")
  })

  test("never rewrites a job that already finished", () => {
    const done: Job = { ...running, status: "done", deadlineAt: Date.now() - 1, replies: [] }
    expect(reconcile(done).status).toBe("done")
  })
})

describe("newJobId", () => {
  test("is unique for jobs started in the same millisecond", () => {
    const stamp = Date.now()
    const ids = [newJobId("council", stamp), newJobId("council", stamp), newJobId("council", stamp)]
    expect(new Set(ids).size).toBe(3)
  })

  test("stays filename-safe so it round-trips through the job store", () => {
    expect(newJobId("agent")).toMatch(/^agent-[0-9a-z]+-[0-9a-z]+$/)
  })
})

const roots: string[] = []

async function checkout(config?: unknown): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "kyora-council-test-"))
  roots.push(root)
  await Bun.$`git -C ${root} init --quiet`.quiet().nothrow()
  if (config) await writeFile(join(root, "kyora-review.config.json"), JSON.stringify(config))
  return root
}

afterAll(async () => {
  await Promise.all(roots.map((root) => rm(root, { recursive: true, force: true })))
})

describe("runConfig", () => {
  test("falls back to defaults in a checkout with no config", async () => {
    const config = await runConfig(await checkout())
    expect(config).toEqual({
      timeoutMs: DEFAULT_TIMEOUT_MS,
      cooldownMinutes: DEFAULT_COOLDOWN_MINUTES,
      overrides: {},
    })
  })

  test("picks up the same engine overrides kyora-review reads", async () => {
    const root = await checkout({
      timeoutMs: 30_000,
      cooldownMinutes: 5,
      overrides: { codex: { bin: "/opt/codex", argsChat: ["talk", "{prompt}"] } },
    })
    const config = await runConfig(root)
    expect(config.timeoutMs).toBe(30_000)
    expect(config.cooldownMinutes).toBe(5)
    expect(config.overrides.codex?.bin).toBe("/opt/codex")
    expect(config.overrides.codex?.argsChat).toEqual(["talk", "{prompt}"])
  })

  test("ignores review-only keys rather than carrying them into a run", async () => {
    const config = await runConfig(await checkout({ base: "develop", failOn: "major", maxDiffBytes: 5 }))
    expect(config).not.toHaveProperty("base")
    expect(config).not.toHaveProperty("failOn")
  })
})
