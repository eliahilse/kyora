import { createDb } from "@kyora/db"
import { events } from "@kyora/db/schema"

const DATA_DIR = import.meta.dir + "/../.kyora"

console.log("creating db at", DATA_DIR)
const db = await createDb(DATA_DIR)

console.log("inserting test event...")
await db.insert(events).values({
  type: "console",
  data: { level: "log", args: ["hello from test"] },
  timestamp: new Date(),
})

console.log("querying events...")
const rows = await db.select().from(events)
console.log("events:", JSON.stringify(rows, null, 2))

process.exit(0)
