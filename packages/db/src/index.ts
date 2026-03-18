import { PGlite } from "@electric-sql/pglite"
import { vector } from "@electric-sql/pglite/vector"
import { drizzle } from "drizzle-orm/pglite"
import * as schema from "./schema"

export function createDb(dataDir?: string) {
  const client = new PGlite(dataDir ?? ".kyora/data", {
    extensions: { vector },
  })
  return drizzle(client, { schema })
}

export function createMemoryDb() {
  const client = new PGlite({ extensions: { vector } })
  return drizzle(client, { schema })
}

export type KyoraDb = ReturnType<typeof createDb>

export { schema }
export * from "./schema"
