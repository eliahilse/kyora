import { describe, expect, test } from "bun:test"
import { argsFor, type EngineDef } from "./engines"
import type { EngineOverride } from "./types"

const ENGINE = {
  id: "test",
  label: "Test",
  bin: "test-cli",
  args: ["review", "{prompt}"],
  argsWrite: ["write", "{prompt}"],
  argsChat: ["chat", "{prompt}"],
  authHint: "",
} satisfies EngineDef

describe("argsFor", () => {
  test("falls back to the engine's own template for each mode", () => {
    expect(argsFor(ENGINE, undefined, "read")).toEqual(["review", "{prompt}"])
    expect(argsFor(ENGINE, undefined, "write")).toEqual(["write", "{prompt}"])
    expect(argsFor(ENGINE, undefined, "chat")).toEqual(["chat", "{prompt}"])
  })

  test("a mode-specific override beats the engine's built-in template", () => {
    const override: EngineOverride = { argsWrite: ["new-write"], argsChat: ["new-chat"] }
    expect(argsFor(ENGINE, override, "write")).toEqual(["new-write"])
    expect(argsFor(ENGINE, override, "chat")).toEqual(["new-chat"])
  })

  test("a plain args override stands in wherever the engine has no mode template", () => {
    const bare = { ...ENGINE, argsWrite: undefined, argsChat: undefined } satisfies EngineDef
    const override: EngineOverride = { args: ["configured"] }
    expect(argsFor(bare, override, "read")).toEqual(["configured"])
    expect(argsFor(bare, override, "write")).toEqual(["configured"])
    expect(argsFor(bare, override, "chat")).toEqual(["configured"])
  })

  test("a plain args override does not leak into modes the engine defines itself", () => {
    expect(argsFor(ENGINE, { args: ["configured"] }, "write")).toEqual(["write", "{prompt}"])
  })
})
