import { createDb } from "@kyora/db"
import { events } from "@kyora/db/schema"
import { desc } from "drizzle-orm"

const DATA_DIR = import.meta.dir + "/../.kyora"
const db = await createDb(DATA_DIR)

const rows = await db.select().from(events).orderBy(desc(events.timestamp))
console.log(`\n=== ${rows.length} events recorded ===\n`)

for (const row of rows) {
  const data = row.data as Record<string, unknown>
  console.log(`[${row.type}] ${row.timestamp}`)
  console.log(`  ${JSON.stringify(data).slice(0, 120)}`)
  console.log()
}

process.exit(0)
