import { type KyoraDb, events } from "@kyora/db"
import { desc, eq, and } from "drizzle-orm"

export interface GetRecentErrorsInput {
  limit?: number
  traceId?: string
}

export async function getRecentErrors(db: KyoraDb, input: GetRecentErrorsInput) {
  const conditions = [eq(events.type, "error")]

  if (input.traceId) {
    conditions.push(eq(events.traceId, input.traceId))
  }

  return db
    .select()
    .from(events)
    .where(and(...conditions))
    .orderBy(desc(events.timestamp))
    .limit(input.limit ?? 5)
}
