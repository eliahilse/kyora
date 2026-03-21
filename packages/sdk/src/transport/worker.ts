import { createDb } from "@kyora/db"
import { events, stateSnapshots, functionCalls } from "@kyora/db/schema"

declare var self: Worker

interface WorkerMessage {
  type: "init" | "event" | "state" | "function_call" | "flush" | "shutdown"
  payload?: unknown
}

let db: Awaited<ReturnType<typeof createDb>>

let eventBuffer: typeof events.$inferInsert[] = []
let stateBuffer: typeof stateSnapshots.$inferInsert[] = []
let fnBuffer: typeof functionCalls.$inferInsert[] = []

const BATCH_SIZE = 50

async function flushAll() {
  if (!db) return
  if (eventBuffer.length > 0) {
    const batch = eventBuffer.splice(0)
    await db.insert(events).values(batch)
  }
  if (stateBuffer.length > 0) {
    const batch = stateBuffer.splice(0)
    await db.insert(stateSnapshots).values(batch)
  }
  if (fnBuffer.length > 0) {
    const batch = fnBuffer.splice(0)
    await db.insert(functionCalls).values(batch)
  }
}

self.onmessage = async (e: MessageEvent<WorkerMessage>) => {
  const { type, payload } = e.data

  switch (type) {
    case "init": {
      const { dataDir } = payload as { dataDir?: string }
      db = await createDb(dataDir)
      self.postMessage({ type: "ready" })
      break
    }
    case "event": {
      eventBuffer.push(payload as typeof events.$inferInsert)
      if (eventBuffer.length >= BATCH_SIZE) await flushAll()
      break
    }
    case "state": {
      stateBuffer.push(payload as typeof stateSnapshots.$inferInsert)
      if (stateBuffer.length >= BATCH_SIZE) await flushAll()
      break
    }
    case "function_call": {
      fnBuffer.push(payload as typeof functionCalls.$inferInsert)
      if (fnBuffer.length >= BATCH_SIZE) await flushAll()
      break
    }
    case "flush": {
      await flushAll()
      self.postMessage({ type: "flushed" })
      break
    }
    case "shutdown": {
      await flushAll()
      self.postMessage({ type: "shutdown_complete" })
      process.exit(0)
    }
  }
}
