import type { KyoraDb } from "@kyora/db"
import { docChunks, docSources, apiSymbols } from "@kyora/db/schema"
import { sql, eq, or, ilike } from "drizzle-orm"
import type { Embedder } from "./embedder"

export interface SearchResult {
  content: string
  source: string
  sourceName: string
  score: number
  metadata: Record<string, unknown> | null
}

export interface SymbolResult {
  name: string
  qualified: string
  kind: string
  signature: string | null
  source: string
  sourceName: string
  context: string | null
}

// hybrid search: vector similarity + keyword matching
export async function searchDocs(
  db: KyoraDb,
  embedder: Embedder,
  query: string,
  limit = 5,
): Promise<SearchResult[]> {
  const [queryEmbedding] = await embedder.embed([query])
  const vectorStr = `[${queryEmbedding!.join(",")}]`

  const results = await db.execute(sql`
    SELECT
      dc.id,
      dc.content,
      dc.metadata,
      dc.source_id,
      (1 - (dc.embedding <=> ${vectorStr}::vector)) AS vector_score,
      COALESCE(ts_rank(dc.tsv, websearch_to_tsquery('english', ${query})), 0) AS text_score
    FROM doc_chunks dc
    WHERE dc.embedding IS NOT NULL
    ORDER BY
      (0.7 * (1 - (dc.embedding <=> ${vectorStr}::vector))
       + 0.3 * COALESCE(ts_rank(dc.tsv, websearch_to_tsquery('english', ${query})), 0))
    DESC
    LIMIT ${limit}
  `)

  if (results.rows.length === 0) return []

  const sourceIds = [...new Set(results.rows.map((r: any) => r.source_id))]
  const sources = await db
    .select({ id: docSources.id, name: docSources.name, reference: docSources.reference })
    .from(docSources)
    .where(sql`${docSources.id} IN ${sourceIds}`)

  const sourceMap = new Map(sources.map((s) => [s.id, s]))

  return results.rows.map((r: any) => {
    const src = sourceMap.get(r.source_id)
    return {
      content: r.content,
      source: src?.reference ?? "unknown",
      sourceName: src?.name ?? "unknown",
      score: Number(r.vector_score) + Number(r.text_score),
      metadata: r.metadata as Record<string, unknown> | null,
    }
  })
}

// direct kv lookup by symbol name
export async function lookupSymbol(
  db: KyoraDb,
  query: string,
  limit = 10,
): Promise<SymbolResult[]> {
  const results = await db
    .select({
      name: apiSymbols.name,
      qualified: apiSymbols.qualified,
      kind: apiSymbols.kind,
      signature: apiSymbols.signature,
      sourceId: apiSymbols.sourceId,
      docChunkId: apiSymbols.docChunkId,
    })
    .from(apiSymbols)
    .where(or(
      eq(apiSymbols.qualified, query),
      eq(apiSymbols.name, query),
      ilike(apiSymbols.qualified, `%${query}%`),
    ))
    .limit(limit)

  if (results.length === 0) return []

  const sourceIds = [...new Set(results.map((r) => r.sourceId))]
  const sources = await db
    .select({ id: docSources.id, name: docSources.name, reference: docSources.reference })
    .from(docSources)
    .where(sql`${docSources.id} IN ${sourceIds}`)
  const sourceMap = new Map(sources.map((s) => [s.id, s]))

  const chunkIds = results.map((r) => r.docChunkId).filter((id): id is number => id !== null)
  let chunkMap = new Map<number, string>()
  if (chunkIds.length > 0) {
    const chunks = await db
      .select({ id: docChunks.id, content: docChunks.content })
      .from(docChunks)
      .where(sql`${docChunks.id} IN ${chunkIds}`)
    chunkMap = new Map(chunks.map((c) => [c.id, c.content]))
  }

  return results.map((r) => {
    const src = sourceMap.get(r.sourceId)
    return {
      name: r.name,
      qualified: r.qualified,
      kind: r.kind,
      signature: r.signature,
      source: src?.reference ?? "unknown",
      sourceName: src?.name ?? "unknown",
      context: r.docChunkId ? (chunkMap.get(r.docChunkId) ?? null) : null,
    }
  })
}
