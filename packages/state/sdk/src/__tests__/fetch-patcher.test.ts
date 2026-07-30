import { test, expect, beforeEach, afterEach } from "bun:test"
import { Observer, type ObserverEvent } from "../observer"
import { patchFetch, unpatchFetch } from "../patchers/fetch"

let observer: Observer
let received: ObserverEvent[]
let server: ReturnType<typeof Bun.serve>

beforeEach(() => {
  observer = new Observer()
  received = []
  observer.on("http", (e) => received.push(e))

  server = Bun.serve({
    port: 0,
    fetch(req) {
      const url = new URL(req.url)
      if (url.pathname === "/ok") {
        return new Response(JSON.stringify({ ok: true }), {
          headers: { "Content-Type": "application/json" },
        })
      }
      if (url.pathname === "/fail") {
        return new Response("not found", { status: 404 })
      }
      return new Response("not found", { status: 404 })
    },
  })

  patchFetch(observer)
})

afterEach(() => {
  unpatchFetch()
  server.stop()
})

test("captures successful fetch", async () => {
  await fetch(`${server.url}ok`)

  expect(received).toHaveLength(1)
  expect(received[0]!.data.method).toBe("GET")
  expect(received[0]!.data.status).toBe(200)
  expect(received[0]!.data.durationMs).toBeGreaterThan(0)
})

test("captures failed status fetch", async () => {
  await fetch(`${server.url}fail`)

  expect(received).toHaveLength(1)
  expect(received[0]!.data.status).toBe(404)
})

test("captures POST with body", async () => {
  await fetch(`${server.url}ok`, {
    method: "POST",
    body: JSON.stringify({ hello: "world" }),
  })

  expect(received).toHaveLength(1)
  expect(received[0]!.data.method).toBe("POST")
  expect(received[0]!.data.requestBody).toContain("hello")
})
