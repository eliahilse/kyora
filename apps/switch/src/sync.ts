import { listSlots, readSnapshot, writeSnapshot, type Slot } from "./store"
import type { Provider, Snapshot } from "./types"

export interface LiveState {
  active: Snapshot | null
  slots: Slot[]
  synced: string | null
}

export function sameFiles(a: Snapshot, b: Snapshot): boolean {
  const keys = Object.keys(a.files).sort()
  if (keys.join("\n") !== Object.keys(b.files).sort().join("\n")) return false
  return keys.every((key) => a.files[key] === b.files[key])
}

/**
 * Copies the live login back into the slot that holds the same account, so the
 * CLI's own token refreshes never leave that slot behind. No network, no keychain
 * write — it only reads what every other command already reads.
 */
export async function syncLiveSlot(provider: Provider): Promise<LiveState> {
  const active = await provider.capture()
  let slots = await listSlots(provider.id)
  const match = active ? slots.find((slot) => slot.identity.account === active.identity.account) : undefined
  if (!active || !match) return { active, slots, synced: null }

  const stored = await readSnapshot(provider.id, match.name)
  if (stored && sameFiles(stored, active)) return { active, slots, synced: null }

  await writeSnapshot(match.name, active)
  slots = await listSlots(provider.id)
  return { active, slots, synced: match.name }
}
