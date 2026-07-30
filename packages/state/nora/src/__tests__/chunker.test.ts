import { test, expect } from "bun:test"
import { chunkText } from "../chunker"

test("chunks text into multiple pieces", () => {
  const text = Array(20).fill("This is a paragraph with enough text to fill space.").join("\n\n")
  const chunks = chunkText(text, 200)

  expect(chunks.length).toBeGreaterThan(1)
  chunks.forEach((chunk, i) => {
    expect(chunk.index).toBe(i)
    expect(chunk.content.length).toBeGreaterThan(0)
  })
})

test("returns single chunk for short text", () => {
  const chunks = chunkText("Hello world", 512)

  expect(chunks).toHaveLength(1)
  expect(chunks[0]!.content).toBe("Hello world")
  expect(chunks[0]!.index).toBe(0)
})

test("returns empty array for empty text", () => {
  const chunks = chunkText("")
  expect(chunks).toHaveLength(0)
})

test("preserves content across chunks", () => {
  const paragraphs = ["First paragraph.", "Second paragraph.", "Third paragraph."]
  const text = paragraphs.join("\n\n")
  const chunks = chunkText(text, 50, 0)

  const allContent = chunks.map((c) => c.content).join(" ")
  for (const p of paragraphs) {
    expect(allContent).toContain(p)
  }
})
