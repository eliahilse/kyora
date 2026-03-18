import { type KyoraDb, events } from "@kyora/db"
import { desc, eq } from "drizzle-orm"

export interface GetRecentErrorsInput {
  limit?: number
}

export async function getRecentErrors(db: KyoraDb, input: GetRecentErrorsInput) {
  return db
    .select()
    .from(events)
    .where(eq(events.type, "error"))
    .orderBy(desc(events.timestamp))
    .limit(input.limit ?? 5)
}
