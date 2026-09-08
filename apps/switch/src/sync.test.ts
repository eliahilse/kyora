import { afterEach, beforeEach, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { readSlot, readSnapshot, writeSnapshot } from "./store"
import { sameFiles, syncLiveSlot } from "./sync"
import type { Provider, Snapshot } from "./types"

let dir: string
const previous = process.env.KYORA_SWITCH_DIR

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "kyora-switch-sync-"))
  process.env.KYORA_SWITCH_DIR = dir
})

afterEach(async () => {
  if (previous === undefined) delete process.env.KYORA_SWITCH_DIR
  else process.env.KYORA_SWITCH_DIR = previous
  await rm(dir, { recursive: true, force: true })
})

function snapshot(account: string, token: string): Snapshot {
  return {
    provider: "codex",
    identity: { account, plan: "pro" },
    files: { "auth.json": JSON.stringify({ tokens: { access_token: token } }) },
    capturedAt: 1,
  }
}

function provider(live: Snapshot | null): Provider {
  return {
    id: "codex",
    label: "Codex",
    processName: "codex",
    loginHint: "",
    locations: () => [],
    capture: async () => live,
    forget: async () => {},
    restore: async () => {},
  }
}

test("sameFiles compares contents regardless of key order", () => {
  const a: Snapshot = { ...snapshot("a", "t"), files: { "x.json": "1", "y.json": "2" } }
  const b: Snapshot = { ...snapshot("a", "t"), files: { "y.json": "2", "x.json": "1" } }
  expect(sameFiles(a, b)).toBe(true)
  expect(sameFiles(a, { ...b, files: { "y.json": "2", "x.json": "9" } })).toBe(false)
  expect(sameFiles(a, { ...b, files: { "x.json": "1" } })).toBe(false)
})

test("a slot holding the live account picks up the CLI's newer token", async () => {
  await writeSnapshot("work", snapshot("me@work.dev", "stale"))
  const state = await syncLiveSlot(provider(snapshot("me@work.dev", "fresh")))

  expect(state.synced).toBe("work")
  expect(JSON.parse((await readSnapshot("codex", "work"))!.files["auth.json"]!).tokens.access_token).toBe("fresh")
})

test("an already-current slot is left alone, so its timestamps do not churn", async () => {
  const written = await writeSnapshot("work", snapshot("me@work.dev", "same"))
  const state = await syncLiveSlot(provider(snapshot("me@work.dev", "same")))

  expect(state.synced).toBeNull()
  expect((await readSlot("codex", "work"))?.updatedAt).toBe(written.updatedAt)
})

test("other accounts' slots are never touched", async () => {
  await writeSnapshot("work", snapshot("me@work.dev", "work-token"))
  await writeSnapshot("private", snapshot("me@home.dev", "home-token"))
  const state = await syncLiveSlot(provider(snapshot("me@work.dev", "newer")))

  expect(state.synced).toBe("work")
  expect(JSON.parse((await readSnapshot("codex", "private"))!.files["auth.json"]!).tokens.access_token).toBe("home-token")
})

test("nothing happens when the live account is in no slot, or nothing is logged in", async () => {
  await writeSnapshot("work", snapshot("me@work.dev", "t"))
  expect((await syncLiveSlot(provider(snapshot("stranger@x.dev", "t")))).synced).toBeNull()
  expect((await syncLiveSlot(provider(null))).synced).toBeNull()
  expect(JSON.parse((await readSnapshot("codex", "work"))!.files["auth.json"]!).tokens.access_token).toBe("t")
})
