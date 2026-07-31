import { describe, expect, test } from "bun:test"
import { assignTasks } from "./assign"

const POOL = ["glm", "codex", "grok"]

describe("assignTasks", () => {
  test("spreads unpinned tasks across distinct families", () => {
    const { assignments } = assignTasks([{ task: "a" }, { task: "b" }, { task: "c" }], POOL)
    expect(assignments.map((item) => item.engine).sort()).toEqual(["codex", "glm", "grok"])
  })

  test("honors pinned engines and leaves them out of the spread", () => {
    const { assignments } = assignTasks([{ task: "a", engine: "grok" }, { task: "b" }], POOL)
    expect(assignments.find((item) => item.task.task === "a")!.engine).toBe("grok")
    expect(assignments.find((item) => item.task.task === "b")!.engine).not.toBe("grok")
  })

  test("reuses families once every one is taken", () => {
    const { assignments } = assignTasks([{ task: "a" }, { task: "b" }, { task: "c" }, { task: "d" }], POOL)
    expect(assignments).toHaveLength(4)
    expect(new Set(assignments.map((item) => item.engine)).size).toBe(3)
  })

  test("reports tasks pinned to an unavailable family instead of silently rerouting", () => {
    const { assignments, unassigned } = assignTasks([{ task: "a", engine: "kimi" }, { task: "b" }], POOL)
    expect(unassigned).toEqual(["a"])
    expect(assignments).toHaveLength(1)
  })

  test("assigns nothing when no family has quota", () => {
    const { assignments, unassigned } = assignTasks([{ task: "a" }], [])
    expect(assignments).toHaveLength(0)
    expect(unassigned).toEqual(["a"])
  })
})
