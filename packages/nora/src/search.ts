import type { KyoraDb } from "@kyora/db"
import { docChunks, docSources } from "@kyora/db/schema"
import { sql, eq, desc } from "drizzle-orm"
import type { Embedder } from "./embedder"

export interface SearchResult {
  content: string
  source: string
  sourceName: string
  score: number
  metadata: Record<string, unknown> | null
}

export async function searchDocs(
  db: KyoraDb,
  embedder: Embedder,
  query: string,
  limit = 5,
): Promise<SearchResult[]> {
  const [queryEmbedding] = await embedder.embed([query])

  const vectorStr = `[${queryEmbedding!.join(",")}]`

  const results = await db
    .select({
      content: docChunks.content,
      metadata: docChunks.metadata,
      sourceId: docChunks.sourceId,
      score: sql<number>`1 - (${docChunks.embedding} <=> ${vectorStr}::vector)`.as("score"),
    })
    .from(docChunks)
    .orderBy(sql`${docChunks.embedding} <=> ${vectorStr}::vector`)
    .limit(limit)

  if (results.length === 0) return []

  const sourceIds = [...new Set(results.map((r) => r.sourceId))]
  const sources = await db
    .select({ id: docSources.id, name: docSources.name, reference: docSources.reference })
    .from(docSources)
    .where(sql`${docSources.id} IN ${sourceIds}`)

  const sourceMap = new Map(sources.map((s) => [s.id, s]))

  return results.map((r) => {
    const src = sourceMap.get(r.sourceId)
    return {
      content: r.content,
      source: src?.reference ?? "unknown",
      sourceName: src?.name ?? "unknown",
      score: r.score,
      metadata: r.metadata as Record<string, unknown> | null,
    }
  })
}
