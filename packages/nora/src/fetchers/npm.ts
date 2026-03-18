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
  versions?: Record<string, { readme?: string }>
}

export async function fetchNpmDocs(packageName: string): Promise<FetchedDoc[]> {
  const res = await fetch(`https://registry.npmjs.org/${packageName}`)
  if (!res.ok) throw new Error(`npm: failed to fetch ${packageName}: ${res.status}`)

  const data = (await res.json()) as NpmRegistryData
  const docs: FetchedDoc[] = []

  if (data.description) {
    docs.push({
      content: data.description,
      source: `npm:${packageName}/description`,
      metadata: { type: "description", package: packageName },
    })
  }

  // try to get readme from github
  const repo = extractGithubRepo(data)
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

  return docs
}

async function fetchGithubReadme(repo: string): Promise<string | null> {
  // try raw readme from github
  for (const branch of ["main", "master"]) {
    for (const filename of ["README.md", "readme.md", "Readme.md"]) {
      const url = `https://raw.githubusercontent.com/${repo}/${branch}/${filename}`
      const res = await fetch(url)
      if (res.ok) return res.text()
    }
  }
  return null
}

export function extractGithubRepo(data: NpmRegistryData): string | null {
  const repoField = data.repository
  const url = typeof repoField === "string" ? repoField : repoField?.url
  if (!url) return null

  const match = url.match(/github\.com[/:]([^/]+\/[^/.#]+)/)
  return match?.[1] ?? null
}
