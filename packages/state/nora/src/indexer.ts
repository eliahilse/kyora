import type { KyoraDb } from "@kyora/db"
import { docSources, docChunks, apiSymbols } from "@kyora/db/schema"
import { eq, sql } from "drizzle-orm"
import type { Embedder } from "./embedder"
import { chunkText } from "./chunker"
import { extractSymbols } from "./extractor"
import { fetchNpmDocs } from "./fetchers/npm"
import { fetchUrl } from "./fetchers/url"
import { fetchLocalFile, fetchLocalDir } from "./fetchers/local"
import type { FetchedDoc } from "./fetchers"

export type SourceType = "npm" | "url" | "file"

export interface IndexResult {
  sourceId: number
  name: string
  chunksIndexed: number
  status: "indexed" | "error"
  error?: string
}

export async function indexSource(
  db: KyoraDb,
  embedder: Embedder,
  type: SourceType,
  reference: string,
): Promise<IndexResult> {
  const name = type === "npm" ? reference : reference.split("/").pop() ?? reference

  const [source] = await db.insert(docSources).values({
    type,
    reference,
    name,
    status: "indexing",
  }).returning()

  try {
    const docs = await fetchDocs(type, reference)

    if (docs.length === 0) {
      await db.update(docSources)
        .set({ status: "error", error: "no docs found" })
        .where(eq(docSources.id, source!.id))
      return { sourceId: source!.id, name, chunksIndexed: 0, status: "error", error: "no docs found" }
    }

    let totalChunks = 0

    for (const doc of docs) {
      const chunks = chunkText(doc.content)
      if (chunks.length === 0) continue

      const texts = chunks.map((c) => c.content)
      const embeddings = await embedder.embed(texts)

      const inserted = await db.insert(docChunks).values(
        chunks.map((chunk, i) => ({
          sourceId: source!.id,
          content: chunk.content,
          embedding: embeddings[i]!,
          metadata: { ...doc.metadata, ...chunk.metadata },
          chunkIndex: chunk.index,
        })),
      ).returning({ id: docChunks.id, content: docChunks.content })

      // populate tsvector
      await db.execute(
        sql`UPDATE doc_chunks SET tsv = to_tsvector('english', content) WHERE source_id = ${source!.id} AND tsv IS NULL`
      )

      // extract and store api symbols
      const docType = (doc.metadata as Record<string, unknown>)?.type as string | undefined
      const packageName = (doc.metadata as Record<string, unknown>)?.package as string | undefined
      const symbols = extractSymbols(doc.content, docType, packageName)

      if (symbols.length > 0) {
        await db.insert(apiSymbols).values(
          symbols.map((sym) => ({
            sourceId: source!.id,
            name: sym.name,
            qualified: sym.qualified,
            kind: sym.kind,
            signature: sym.signature,
            docChunkId: inserted.find((c) => c.content.includes(sym.signature?.slice(0, 40) ?? sym.name))?.id ?? null,
          })),
        )
      }

      totalChunks += chunks.length
    }

    await db.update(docSources)
      .set({ status: "indexed", chunksCount: totalChunks, updatedAt: new Date() })
      .where(eq(docSources.id, source!.id))

    return { sourceId: source!.id, name, chunksIndexed: totalChunks, status: "indexed" }
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err)
    await db.update(docSources)
      .set({ status: "error", error: message, updatedAt: new Date() })
      .where(eq(docSources.id, source!.id))
    return { sourceId: source!.id, name, chunksIndexed: 0, status: "error", error: message }
  }
}

async function fetchDocs(type: SourceType, reference: string): Promise<FetchedDoc[]> {
  switch (type) {
    case "npm":
      return fetchNpmDocs(reference)
    case "url":
      return fetchUrl(reference)
    case "file": {
      const file = Bun.file(reference)
      const stat = await file.exists()
      if (!stat) throw new Error(`not found: ${reference}`)
      // check if it's likely a directory by trying glob
      try {
        return await fetchLocalDir(reference)
      } catch {
        return fetchLocalFile(reference)
      }
    }
  }
}
