import { type KyoraDb, events } from "@kyora/db"
import { desc, eq, and } from "drizzle-orm"

export interface GetHttpLogInput {
  method?: string
  limit?: number
  traceId?: string
}

export async function getHttpLog(db: KyoraDb, input: GetHttpLogInput) {
  const conditions = [eq(events.type, "http")]

  if (input.traceId) {
    conditions.push(eq(events.traceId, input.traceId))
  }

  const rows = await db
    .select()
    .from(events)
    .where(and(...conditions))
    .orderBy(desc(events.timestamp))
    .limit(input.limit ?? 10)

  if (input.method) {
    return rows.filter(
      (r) => (r.data as Record<string, unknown>)?.method === input.method,
    )
  }

  return rows
}
