export interface Embedder {
  embed(texts: string[]): Promise<number[][]>
  dimensions: number
}

let pipeline: any = null

async function getPipeline() {
  if (pipeline) return pipeline
  const { pipeline: createPipeline } = await import("@huggingface/transformers")
  pipeline = await createPipeline("feature-extraction", "Xenova/all-MiniLM-L6-v2")
  return pipeline
}

export function createLocalEmbedder(): Embedder {
  return {
    dimensions: 384,
    async embed(texts: string[]): Promise<number[][]> {
      const pipe = await getPipeline()
      const results: number[][] = []

      for (const text of texts) {
        const output = await pipe(text, { pooling: "mean", normalize: true })
        results.push(Array.from(output.data as Float32Array).slice(0, 384))
      }

      return results
    },
  }
}
