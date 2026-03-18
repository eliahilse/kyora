import type { FetchedDoc } from "./npm"

export async function fetchLocalFile(filePath: string): Promise<FetchedDoc[]> {
  const file = Bun.file(filePath)
  if (!(await file.exists())) {
    throw new Error(`local: file not found: ${filePath}`)
  }

  const content = await file.text()
  return [{
    content,
    source: `file://${filePath}`,
    metadata: { type: "local", path: filePath },
  }]
}

export async function fetchLocalDir(dirPath: string): Promise<FetchedDoc[]> {
  const glob = new Bun.Glob("**/*.{md,mdx,txt,rst}")
  const docs: FetchedDoc[] = []

  for await (const path of glob.scan({ cwd: dirPath })) {
    const fullPath = `${dirPath}/${path}`
    const file = Bun.file(fullPath)
    const content = await file.text()
    docs.push({
      content,
      source: `file://${fullPath}`,
      metadata: { type: "local", path: fullPath, relativePath: path },
    })
  }

  return docs
}
