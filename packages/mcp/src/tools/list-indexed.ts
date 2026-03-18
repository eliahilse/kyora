import type { KyoraDb } from "@kyora/db"
import { docSources } from "@kyora/db/schema"
import { desc } from "drizzle-orm"

export async function listIndexed(db: KyoraDb) {
  return db
    .select()
    .from(docSources)
    .orderBy(desc(docSources.createdAt))
}
