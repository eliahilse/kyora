import { type KyoraDb, events } from "@kyora/db"
import { desc, eq } from "drizzle-orm"

export interface GetHttpLogInput {
  method?: string
  limit?: number
}

export async function getHttpLog(db: KyoraDb, input: GetHttpLogInput) {
  const rows = await db
    .select()
    .from(events)
    .where(eq(events.type, "http"))
    .orderBy(desc(events.timestamp))
    .limit(input.limit ?? 10)

  if (input.method) {
    return rows.filter(
      (r) => (r.data as Record<string, unknown>)?.method === input.method,
    )
  }

  return rows
}
