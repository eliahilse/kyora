import type { KyoraDb } from "@kyora/db"
import { lookupSymbol } from "@kyora/nora"

export interface LookupSymbolInput {
  name: string
  limit?: number
}

export async function lookupSymbolHandler(db: KyoraDb, input: LookupSymbolInput) {
  return lookupSymbol(db, input.name, input.limit ?? 10)
}
