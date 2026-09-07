import { mkdir, readdir, rename, rm } from "node:fs/promises"
import { homedir } from "node:os"
import { join } from "node:path"
import { readJsonIfExists, readTextIfExists, writeFileAtomic } from "./fsx"
import type { Identity, ProviderId, Snapshot } from "./types"

const NAME_PATTERN = /^[a-zA-Z0-9][a-zA-Z0-9._-]*$/
const META_FILE = "slot.json"
const MAX_BACKUPS = 10

export interface Slot {
  name: string
  provider: ProviderId
  identity: Identity
  createdAt: number
  updatedAt: number
}

export function rootDir(): string {
  return process.env.KYORA_SWITCH_DIR ?? join(homedir(), ".kyora", "switch")
}

export function slotDir(provider: ProviderId, name: string): string {
  return join(rootDir(), provider, assertName(name))
}

/** Slot names become path segments, so they are held to a strict alphabet. */
export function assertName(name: string): string {
  if (!NAME_PATTERN.test(name)) {
    throw new Error(`invalid slot name "${name}" — use letters, digits, dot, dash or underscore`)
  }
  return name
}

export async function listSlots(provider: ProviderId): Promise<Slot[]> {
  let names: string[]
  try {
    names = await readdir(join(rootDir(), provider))
  } catch {
    return []
  }
  const slots: Slot[] = []
  for (const name of names.sort()) {
    if (!NAME_PATTERN.test(name)) continue
    const slot = await readSlot(provider, name)
    if (slot) slots.push(slot)
  }
  return slots
}

export async function readSlot(provider: ProviderId, name: string): Promise<Slot | null> {
  const meta = await readJsonIfExists(join(slotDir(provider, name), META_FILE))
  if (!meta) return null
  return {
    name,
    provider,
    identity: (meta.identity as Identity) ?? {},
    createdAt: typeof meta.createdAt === "number" ? meta.createdAt : 0,
    updatedAt: typeof meta.updatedAt === "number" ? meta.updatedAt : 0,
  }
}

export async function readSnapshot(provider: ProviderId, name: string): Promise<Snapshot | null> {
  const slot = await readSlot(provider, name)
  if (!slot) return null
  const dir = slotDir(provider, name)
  const files: Record<string, string> = {}
  for (const file of await readdir(dir)) {
    if (file === META_FILE) continue
    const text = await readTextIfExists(join(dir, file))
    if (text !== null) files[file] = text
  }
  return { provider, identity: slot.identity, files, capturedAt: slot.updatedAt }
}

export async function writeSnapshot(name: string, snapshot: Snapshot): Promise<Slot> {
  const dir = slotDir(snapshot.provider, name)
  const existing = await readSlot(snapshot.provider, name)
  await rm(dir, { recursive: true, force: true })
  await mkdir(dir, { recursive: true, mode: 0o700 })

  for (const [file, contents] of Object.entries(snapshot.files)) {
    await writeFileAtomic(join(dir, file), contents, 0o600)
  }
  const slot: Slot = {
    name,
    provider: snapshot.provider,
    identity: snapshot.identity,
    createdAt: existing?.createdAt || snapshot.capturedAt,
    updatedAt: snapshot.capturedAt,
  }
  await writeFileAtomic(join(dir, META_FILE), `${JSON.stringify(slot, null, 2)}\n`, 0o600)
  return slot
}

export async function removeSlot(provider: ProviderId, name: string): Promise<void> {
  await rm(slotDir(provider, name), { recursive: true, force: true })
}

export async function renameSlot(provider: ProviderId, from: string, to: string): Promise<void> {
  await mkdir(join(rootDir(), provider), { recursive: true, mode: 0o700 })
  await rename(slotDir(provider, from), slotDir(provider, to))
  const slot = await readSlot(provider, to)
  if (slot) {
    await writeFileAtomic(
      join(slotDir(provider, to), META_FILE),
      `${JSON.stringify({ ...slot, name: to }, null, 2)}\n`,
      0o600,
    )
  }
}

/** Parks the outgoing login somewhere recoverable before it is overwritten. */
export async function writeBackup(snapshot: Snapshot): Promise<string> {
  const stamp = new Date().toISOString().replace(/[:.]/g, "-")
  const dir = join(rootDir(), "backups", snapshot.provider, stamp)
  for (const [file, contents] of Object.entries(snapshot.files)) {
    await writeFileAtomic(join(dir, file), contents, 0o600)
  }
  await writeFileAtomic(join(dir, META_FILE), `${JSON.stringify(snapshot.identity, null, 2)}\n`, 0o600)
  await pruneBackups(snapshot.provider)
  return dir
}

async function pruneBackups(provider: ProviderId): Promise<void> {
  const dir = join(rootDir(), "backups", provider)
  let names: string[]
  try {
    names = await readdir(dir)
  } catch {
    return
  }
  for (const name of names.sort().slice(0, Math.max(0, names.length - MAX_BACKUPS))) {
    await rm(join(dir, name), { recursive: true, force: true })
  }
}
