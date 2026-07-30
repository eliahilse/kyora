import { test, expect } from "bun:test"
import { scanSource } from "../observer/comment-scanner"

test("parses @kyora.watch with explicit target", () => {
  const source = `// @kyora.watch cart
const cart = { items: [], total: 0 }`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(1)
  expect(directives[0]).toEqual({
    type: "watch",
    target: "cart",
    file: "test.ts",
    line: 1,
  })
})

test("parses @kyora.trace with explicit target", () => {
  const source = `// @kyora.trace processPayment
async function processPayment(amount: number) {}`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(1)
  expect(directives[0]!.type).toBe("trace")
  expect(directives[0]!.target).toBe("processPayment")
})

test("parses @kyora.track with expression", () => {
  const source = `// @kyora.track queue.length
const queue = new Queue()`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(1)
  expect(directives[0]!.type).toBe("track")
  expect(directives[0]!.target).toBe("queue.length")
})

test("infers watch target from next line variable declaration", () => {
  const source = `// @kyora.watch
const state = { count: 0 }`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(1)
  expect(directives[0]!.target).toBe("state")
})

test("infers trace target from next line function declaration", () => {
  const source = `// @kyora.trace
async function handleRequest(req: Request) {}`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(1)
  expect(directives[0]!.target).toBe("handleRequest")
})

test("infers trace target from arrow function", () => {
  const source = `// @kyora.trace
const fetchData = async (url: string) => {}`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(1)
  expect(directives[0]!.target).toBe("fetchData")
})

test("parses multiple directives", () => {
  const source = `// @kyora.watch cart
const cart = {}

// @kyora.trace
function checkout() {}

// @kyora.track items.length
const items = []`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(3)
  expect(directives.map((d) => d.type)).toEqual(["watch", "trace", "track"])
})

test("ignores non-directive comments", () => {
  const source = `// this is a normal comment
// @kyora.watch cart
const cart = {}
// another comment`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(1)
})

test("returns empty for source with no directives", () => {
  const source = `const x = 1
function foo() {}`

  const directives = scanSource(source, "test.ts")
  expect(directives).toHaveLength(0)
})
