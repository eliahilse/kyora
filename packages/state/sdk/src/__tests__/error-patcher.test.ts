import { test, expect, beforeEach, afterEach } from "bun:test"
import { Observer, type ObserverEvent } from "../observer"
import { patchErrors, unpatchErrors } from "../patchers/error"

let observer: Observer
let received: ObserverEvent[]

beforeEach(() => {
  observer = new Observer()
  received = []
  observer.on("error", (e) => received.push(e))
  patchErrors(observer)
})

afterEach(() => {
  unpatchErrors()
})

test("registers and removes error listeners without crashing", () => {
  expect(received).toHaveLength(0)
  unpatchErrors()
  unpatchErrors()
})

test("emits error events when manually dispatching", () => {
  const event = new ErrorEvent("error", {
    message: "test error",
    error: new Error("test error"),
  })
  globalThis.dispatchEvent(event)

  expect(received).toHaveLength(1)
  expect(received[0]!.data.message).toBe("test error")
  expect(received[0]!.data.unhandled).toBe(true)
})
