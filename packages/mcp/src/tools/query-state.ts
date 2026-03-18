import { type KyoraDb, stateSnapshots } from "@kyora/db"
import { desc, eq, gte, lte, and } from "drizzle-orm"

export interface QueryStateInput {
  key: string
  since?: string
  until?: string
  limit?: number
}

export async function queryState(db: KyoraDb, input: QueryStateInput) {
  const conditions = [eq(stateSnapshots.key, input.key)]

  if (input.since) {
    conditions.push(gte(stateSnapshots.timestamp, new Date(input.since)))
  }
  if (input.until) {
    conditions.push(lte(stateSnapshots.timestamp, new Date(input.until)))
  }

  return db
    .select()
    .from(stateSnapshots)
    .where(and(...conditions))
    .orderBy(desc(stateSnapshots.timestamp))
    .limit(input.limit ?? 20)
}
