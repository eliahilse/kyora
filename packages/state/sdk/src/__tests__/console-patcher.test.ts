import { test, expect, beforeEach, afterEach } from "bun:test"
import { Observer, type ObserverEvent } from "../observer"
import { patchConsole, unpatchConsole } from "../patchers/console"

let observer: Observer
let received: ObserverEvent[]

beforeEach(() => {
  observer = new Observer()
  received = []
  observer.on("console", (e) => received.push(e))
  patchConsole(observer)
})

afterEach(() => {
  unpatchConsole()
})

test("captures console.log", () => {
  console.log("test message")

  expect(received).toHaveLength(1)
  expect(received[0]!.data.level).toBe("log")
  expect(received[0]!.data.args).toEqual(["test message"])
})

test("captures console.error", () => {
  console.error("something broke")

  expect(received).toHaveLength(1)
  expect(received[0]!.data.level).toBe("error")
})

test("captures console.warn", () => {
  console.warn("heads up")

  expect(received).toHaveLength(1)
  expect(received[0]!.data.level).toBe("warn")
})

test("captures multiple args", () => {
  console.log("a", 1, { key: "val" })

  expect(received).toHaveLength(1)
  expect(received[0]!.data.args).toEqual(["a", 1, { key: "val" }])
})
