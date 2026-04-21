import { createDb } from "@kyora/db"
import { createLocalEmbedder } from "@kyora/nora"
import { indexSource, searchDocs } from "@kyora/nora"

const db = await createDb(import.meta.dir + "/../.kyora")
const embedder = createLocalEmbedder()

console.log("indexing zod from npm...")
const result = await indexSource(db, embedder, "npm", "zod")
console.log("result:", result)

console.log("\nsearching: 'how to validate an email'...")
const results = await searchDocs(db, embedder, "how to validate an email", 3)
for (const r of results) {
  console.log(`\n[${r.score.toFixed(3)}] ${r.source}`)
  console.log(r.content.slice(0, 200))
}

process.exit(0)
