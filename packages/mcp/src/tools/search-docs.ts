import type { KyoraDb } from "@kyora/db"
import { searchDocs, type Embedder } from "@kyora/nora"

export interface SearchDocsInput {
  query: string
  limit?: number
}

export async function searchDocsHandler(db: KyoraDb, embedder: Embedder, input: SearchDocsInput) {
  return searchDocs(db, embedder, input.query, input.limit ?? 5)
}
