export interface FetchedDoc {
  content: string
  source: string
  metadata?: Record<string, unknown>
}

interface NpmRegistryData {
  description?: string
  homepage?: string
  repository?: { url?: string } | string
  "dist-tags"?: { latest?: string }
  versions?: Record<string, {
    readme?: string
    types?: string
    typings?: string
  }>
}

export async function fetchNpmDocs(packageName: string): Promise<FetchedDoc[]> {
  const res = await fetch(`https://registry.npmjs.org/${packageName}`)
  if (!res.ok) throw new Error(`npm: failed to fetch ${packageName}: ${res.status}`)

  const data = (await res.json()) as NpmRegistryData
  const docs: FetchedDoc[] = []
  const repo = extractGithubRepo(data)

  if (data.description) {
    docs.push({
      content: data.description,
      source: `npm:${packageName}/description`,
      metadata: { type: "description", package: packageName },
    })
  }

  if (repo) {
    const readme = await fetchGithubReadme(repo)
    if (readme) {
      docs.push({
        content: readme,
        source: `npm:${packageName}/README`,
        metadata: { type: "readme", package: packageName, repo },
      })
    }
  }

  // fetch type declarations from cdn
  const typeDocs = await fetchTypeDeclarations(packageName)
  if (typeDocs) {
    docs.push({
      content: typeDocs,
      source: `npm:${packageName}/types`,
      metadata: { type: "types", package: packageName },
    })
  }

  // try llms.txt from homepage
  if (data.homepage) {
    const llmsTxt = await fetchLlmsTxt(data.homepage)
    if (llmsTxt) {
      docs.push({
        content: llmsTxt,
        source: `npm:${packageName}/llms.txt`,
        metadata: { type: "llms-txt", package: packageName, homepage: data.homepage },
      })
    }
  }

  // fetch docs directory from github
  if (repo) {
    const docFiles = await fetchGithubDocs(repo)
    for (const doc of docFiles) {
      docs.push({
        content: doc.content,
        source: `npm:${packageName}/docs/${doc.path}`,
        metadata: { type: "docs", package: packageName, repo, path: doc.path },
      })
    }
  }

  return docs
}

async function fetchGithubReadme(repo: string): Promise<string | null> {
  for (const branch of ["main", "master"]) {
    for (const filename of ["README.md", "readme.md", "Readme.md"]) {
      const url = `https://raw.githubusercontent.com/${repo}/${branch}/${filename}`
      const res = await fetch(url)
      if (res.ok) return res.text()
    }
  }
  return null
}

// fetches the main .d.ts entry point from jsdelivr
async function fetchTypeDeclarations(packageName: string): Promise<string | null> {
  try {
    // try the flat bundle from jsdelivr
    const url = `https://cdn.jsdelivr.net/npm/${packageName}/+esm`
    const res = await fetch(url, { headers: { accept: "text/plain" } })
    if (!res.ok) return null

    // try fetching the actual .d.ts file instead
    const dtsUrls = [
      `https://cdn.jsdelivr.net/npm/${packageName}/dist/index.d.ts`,
      `https://cdn.jsdelivr.net/npm/${packageName}/index.d.ts`,
      `https://cdn.jsdelivr.net/npm/${packageName}/types/index.d.ts`,
      `https://cdn.jsdelivr.net/npm/@types/${packageName.replace("@", "").replace("/", "__")}/index.d.ts`,
    ]

    for (const dtsUrl of dtsUrls) {
      const dtsRes = await fetch(dtsUrl)
      if (dtsRes.ok) {
        const content = await dtsRes.text()
        if (content.length > 50 && content.length < 500_000) return content
      }
    }

    return null
  } catch {
    return null
  }
}

// checks for llms.txt at the library's homepage
async function fetchLlmsTxt(homepage: string): Promise<string | null> {
  try {
    const base = homepage.endsWith("/") ? homepage : homepage + "/"
    const urls = [
      base + "llms.txt",
      base + "llms-full.txt",
    ]

    for (const url of urls) {
      const res = await fetch(url, { redirect: "follow" })
      if (res.ok) {
        const content = await res.text()
        if (content.length > 100 && !content.includes("<html")) return content
      }
    }

    return null
  } catch {
    return null
  }
}

// fetches markdown files from docs/ directory in github repo
async function fetchGithubDocs(repo: string): Promise<Array<{ path: string; content: string }>> {
  const docs: Array<{ path: string; content: string }> = []

  try {
    for (const branch of ["main", "master"]) {
      const apiUrl = `https://api.github.com/repos/${repo}/git/trees/${branch}?recursive=1`
      const res = await fetch(apiUrl, {
        headers: { "user-agent": "kyora-nora" },
      })
      if (!res.ok) continue

      const tree = (await res.json()) as { tree: Array<{ path: string; type: string; size?: number }> }

      const docFiles = tree.tree.filter((f) =>
        f.type === "blob" &&
        (f.size ?? 0) < 200_000 &&
        /^docs?\/.*\.(md|mdx|txt|rst)$/i.test(f.path),
      )

      // cap at 30 files to avoid rate limits
      for (const file of docFiles.slice(0, 30)) {
        const rawUrl = `https://raw.githubusercontent.com/${repo}/${branch}/${file.path}`
        const fileRes = await fetch(rawUrl)
        if (fileRes.ok) {
          docs.push({ path: file.path, content: await fileRes.text() })
        }
      }

      if (docs.length > 0) break
    }
  } catch {
    // github api rate limit or other error, skip silently
  }

  return docs
}

export function extractGithubRepo(data: NpmRegistryData): string | null {
  const repoField = data.repository
  const url = typeof repoField === "string" ? repoField : repoField?.url
  if (!url) return null

  const match = url.match(/github\.com[/:]([^/]+\/[^/.#]+)/)
  return match?.[1] ?? null
}
