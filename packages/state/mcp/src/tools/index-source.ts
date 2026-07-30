import type { KyoraDb } from "@kyora/db"
import { indexSource, type SourceType, type Embedder } from "@kyora/nora"

export interface IndexSourceInput {
  type: "npm" | "url" | "file"
  reference: string
}

export async function indexSourceHandler(db: KyoraDb, embedder: Embedder, input: IndexSourceInput) {
  return indexSource(db, embedder, input.type as SourceType, input.reference)
}
