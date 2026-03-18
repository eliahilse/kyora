import { test, expect } from "bun:test"
import { fetchNpmDocs } from "../fetchers/npm"
import { fetchLocalFile } from "../fetchers/local"

test("fetches npm docs for a known package", async () => {
  const docs = await fetchNpmDocs("express")

  expect(docs.length).toBeGreaterThan(0)
  const readme = docs.find((d) => d.metadata?.type === "readme")
  expect(readme).toBeDefined()
  expect(readme!.content.length).toBeGreaterThan(100)
})

test("throws for non-existent npm package", async () => {
  await expect(
    fetchNpmDocs("this-package-definitely-does-not-exist-xyz-123"),
  ).rejects.toThrow()
})

test("fetches local file", async () => {
  // use this test file itself as the target
  const docs = await fetchLocalFile(import.meta.path)

  expect(docs).toHaveLength(1)
  expect(docs[0]!.content).toContain("fetches local file")
})

test("throws for non-existent local file", async () => {
  await expect(fetchLocalFile("/tmp/nope-does-not-exist.txt")).rejects.toThrow()
})
