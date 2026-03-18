import type { KyoraDb } from "@kyora/db"
import { docSources } from "@kyora/db/schema"
import { eq, sql } from "drizzle-orm"

export interface IndexStatusInput {
  sourceId?: number
}

export async function indexStatus(db: KyoraDb, input: IndexStatusInput) {
  if (input.sourceId) {
    const [source] = await db
      .select()
      .from(docSources)
      .where(eq(docSources.id, input.sourceId))
    return source ?? { error: "source not found" }
  }

  const stats = await db
    .select({
      status: docSources.status,
      count: sql<number>`count(*)`.as("count"),
    })
    .from(docSources)
    .groupBy(docSources.status)

  const total = await db.select({ count: sql<number>`count(*)` }).from(docSources)

  return {
    total: total[0]?.count ?? 0,
    byStatus: Object.fromEntries(stats.map((s) => [s.status, s.count])),
  }
}
