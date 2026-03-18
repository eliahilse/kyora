export interface Chunk {
  content: string
  index: number
  metadata?: Record<string, unknown>
}

export function chunkText(
  text: string,
  maxChunkSize = 512,
  overlap = 64,
): Chunk[] {
  const chunks: Chunk[] = []
  const paragraphs = text.split(/\n\n+/)
  let current = ""
  let index = 0

  for (const paragraph of paragraphs) {
    if (current.length + paragraph.length > maxChunkSize && current.length > 0) {
      chunks.push({ content: current.trim(), index: index++ })
      // keep overlap from the end of the current chunk
      const words = current.split(/\s+/)
      const overlapWords = words.slice(-Math.ceil(overlap / 5))
      current = overlapWords.join(" ") + "\n\n" + paragraph
    } else {
      current = current ? current + "\n\n" + paragraph : paragraph
    }
  }

  if (current.trim()) {
    chunks.push({ content: current.trim(), index: index++ })
  }

  return chunks
}
