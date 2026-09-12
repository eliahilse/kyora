import { afterEach, beforeEach, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import {
  assertName,
  listSlots,
  readSlot,
  readSnapshot,
  removeSlot,
  renameSlot,
  rootDir,
  writeBackup,
  writeSnapshot,
} from "./store"
import type { Snapshot } from "./types"

let dir: string
const previous = process.env.KYORA_SWITCH_DIR

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "kyora-switch-test-"))
  process.env.KYORA_SWITCH_DIR = dir
})

afterEach(async () => {
  if (previous === undefined) delete process.env.KYORA_SWITCH_DIR
  else process.env.KYORA_SWITCH_DIR = previous
  await rm(dir, { recursive: true, force: true })
})

function snapshot(overrides: Partial<Snapshot> = {}): Snapshot {
  return {
    provider: "codex",
    identity: { account: "me@example.com", plan: "pro" },
    files: { "auth.json": '{"tokens":{}}' },
    capturedAt: 1,
    ...overrides,
  }
}

test("rootDir honours KYORA_SWITCH_DIR", () => {
  expect(rootDir()).toBe(dir)
})

test("slot names are restricted to safe path segments", () => {
  expect(assertName("work-2")).toBe("work-2")
  expect(() => assertName("../escape")).toThrow(/invalid slot name/)
  expect(() => assertName("a/b")).toThrow(/invalid slot name/)
  expect(() => assertName("")).toThrow(/invalid slot name/)
  expect(() => assertName(".hidden")).toThrow(/invalid slot name/)
})

test("a snapshot round-trips through save and read", async () => {
  await writeSnapshot("work", snapshot())
  expect((await readSlot("codex", "work"))?.identity.account).toBe("me@example.com")

  const restored = await readSnapshot("codex", "work")
  expect(restored?.files).toEqual({ "auth.json": '{"tokens":{}}' })
  expect(restored?.identity.account).toBe("me@example.com")
})

test("slots are namespaced per provider, so the same name holds two accounts", async () => {
  await writeSnapshot("work", snapshot({ identity: { account: "codex@work.dev" } }))
  await writeSnapshot(
    "work",
    snapshot({ provider: "claude", identity: { account: "claude@work.dev" }, files: { "credentials.json": "{}" } }),
  )

  expect((await readSlot("codex", "work"))?.identity.account).toBe("codex@work.dev")
  expect((await readSlot("claude", "work"))?.identity.account).toBe("claude@work.dev")
  expect((await listSlots("codex")).length).toBe(1)
})

test("re-saving a slot drops files the new snapshot no longer has", async () => {
  await writeSnapshot("work", snapshot({ files: { "auth.json": "{}", "stale.json": "{}" } }))
  await writeSnapshot("work", snapshot({ files: { "auth.json": "{}" } }))

  expect(Object.keys((await readSnapshot("codex", "work"))!.files)).toEqual(["auth.json"])
})

test("createdAt survives a re-save", async () => {
  const first = await writeSnapshot("work", snapshot())
  const second = await writeSnapshot("work", snapshot({ capturedAt: 999 }))
  expect(second.createdAt).toBe(first.createdAt)
  expect(second.updatedAt).toBe(999)
})

test("listSlots is sorted, readSlot is null for unknown names", async () => {
  await writeSnapshot("work", snapshot())
  await writeSnapshot("private", snapshot())
  expect((await listSlots("codex")).map((slot) => slot.name)).toEqual(["private", "work"])
  expect(await listSlots("claude")).toEqual([])
  expect(await readSlot("codex", "nope")).toBeNull()
  expect(await readSnapshot("codex", "nope")).toBeNull()
})

test("remove and rename move the whole slot", async () => {
  await writeSnapshot("work", snapshot())
  await renameSlot("codex", "work", "day-job")
  expect((await readSlot("codex", "day-job"))?.name).toBe("day-job")
  expect(await readSlot("codex", "work")).toBeNull()

  await removeSlot("codex", "day-job")
  expect(await listSlots("codex")).toEqual([])
})

test("writeBackup stores the outgoing files under the provider", async () => {
  const path = await writeBackup(snapshot())
  expect(path.startsWith(join(dir, "backups", "codex"))).toBe(true)
  expect(await Bun.file(join(path, "auth.json")).text()).toBe('{"tokens":{}}')
})
