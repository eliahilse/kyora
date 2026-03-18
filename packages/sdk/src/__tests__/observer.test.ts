import { test, expect, beforeEach } from "bun:test"
import { Observer, type ObserverEvent } from "../observer"

let observer: Observer

beforeEach(() => {
  observer = new Observer()
})

test("emits events to typed listeners", () => {
  const received: ObserverEvent[] = []
  observer.on("http", (e) => received.push(e))

  observer.emit({ type: "http", timestamp: Date.now(), data: { url: "/test" } })
  observer.emit({ type: "console", timestamp: Date.now(), data: { level: "log" } })

  expect(received).toHaveLength(1)
  expect(received[0]!.data.url).toBe("/test")
})

test("wildcard listener receives all events", () => {
  const received: ObserverEvent[] = []
  observer.on("*", (e) => received.push(e))

  observer.emit({ type: "http", timestamp: Date.now(), data: {} })
  observer.emit({ type: "error", timestamp: Date.now(), data: {} })

  expect(received).toHaveLength(2)
})

test("off removes listener", () => {
  const received: ObserverEvent[] = []
  const listener = (e: ObserverEvent) => received.push(e)

  observer.on("http", listener)
  observer.emit({ type: "http", timestamp: Date.now(), data: {} })
  observer.off("http", listener)
  observer.emit({ type: "http", timestamp: Date.now(), data: {} })

  expect(received).toHaveLength(1)
})
