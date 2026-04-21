import { createDb } from "@kyora/db"
import { docSources, docChunks } from "@kyora/db/schema"
import { eq } from "drizzle-orm"

const db = await createDb(import.meta.dir + "/../.kyora")

const sources = await db.select().from(docSources)
console.log(`${sources.length} sources:`)
for (const s of sources) {
  console.log(`  [${s.status}] ${s.name} — ${s.chunksCount} chunks`)
}

const chunks = await db.select({
  content: docChunks.content,
  metadata: docChunks.metadata,
}).from(docChunks).limit(5)

console.log(`\nfirst 5 chunks:`)
for (const c of chunks) {
  const meta = c.metadata as Record<string, unknown>
  console.log(`  [${meta?.type}] ${(c.content).slice(0, 80)}...`)
}

process.exit(0)
