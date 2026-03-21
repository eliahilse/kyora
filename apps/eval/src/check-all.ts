import { createDb } from "@kyora/db"
import { events, stateSnapshots, functionCalls } from "@kyora/db/schema"
import { desc } from "drizzle-orm"

const db = await createDb(import.meta.dir + "/../.kyora")

const evts = await db.select().from(events).orderBy(desc(events.timestamp))
console.log(`\n=== ${evts.length} events ===`)
for (const e of evts.slice(0, 5)) {
  const d = e.data as Record<string, unknown>
  console.log(`  [${e.type}] ${d.method ?? d.level ?? ""} ${d.url ?? (d.args as any)?.[0] ?? ""}`)
}

const snaps = await db.select().from(stateSnapshots).orderBy(desc(stateSnapshots.timestamp))
console.log(`\n=== ${snaps.length} state snapshots ===`)
for (const s of snaps) {
  console.log(`  [${s.key}] ${JSON.stringify(s.value)}`)
}

const fns = await db.select().from(functionCalls).orderBy(desc(functionCalls.timestamp))
console.log(`\n=== ${fns.length} function calls ===`)
for (const f of fns) {
  console.log(`  ${f.name}(${JSON.stringify(f.args)}) → ${JSON.stringify(f.returnValue)} ${f.error ? `ERR: ${f.error}` : ""} [${f.durationMs?.toFixed(2)}ms]`)
}

process.exit(0)
