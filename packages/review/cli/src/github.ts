import { renderFindingSection, renderInlineComment, summaryLine } from "./report"
import type { EngineRun, MergedFinding, PrInfo, ReviewContext } from "./types"

const API = "https://api.github.com"

export async function getToken(): Promise<string | null> {
  if (process.env.GITHUB_TOKEN) return process.env.GITHUB_TOKEN
  if (process.env.GH_TOKEN) return process.env.GH_TOKEN
  const result = await Bun.$`gh auth token`.quiet().nothrow()
  if (result.exitCode === 0) {
    const token = result.text().trim()
    if (token) return token
  }
  return null
}

export async function detectRepo(): Promise<{ owner: string; repo: string } | null> {
  const fromEnv = process.env.GITHUB_REPOSITORY
  if (fromEnv?.includes("/")) {
    const [owner, repo] = fromEnv.split("/")
    if (owner && repo) return { owner, repo }
  }
  const result = await Bun.$`git remote get-url origin`.quiet().nothrow()
  if (result.exitCode !== 0) return null
  const match = /github\.com[:/]([^/]+)\/(.+?)(?:\.git)?$/.exec(result.text().trim())
  if (!match) return null
  return { owner: match[1]!, repo: match[2]! }
}

async function api(
  token: string,
  method: string,
  path: string,
  body?: unknown,
): Promise<{ ok: boolean; status: number; json: unknown }> {
  const response = await fetch(`${API}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      accept: "application/vnd.github+json",
      "x-github-api-version": "2022-11-28",
      "user-agent": "kyora-review",
      ...(body ? { "content-type": "application/json" } : {}),
    },
    ...(body ? { body: JSON.stringify(body) } : {}),
  })
  const json = await response.json().catch(() => null)
  return { ok: response.ok, status: response.status, json }
}

export async function fetchPr(
  owner: string,
  repo: string,
  number: number,
  token: string,
): Promise<PrInfo | { error: string }> {
  const result = await api(token, "GET", `/repos/${owner}/${repo}/pulls/${number}`)
  if (!result.ok) return { error: `GET pulls/${number} → ${result.status}` }
  const pr = result.json as { base: { ref: string }; head: { sha: string } }
  return { owner, repo, number, baseRef: pr.base.ref, headSha: pr.head.sha }
}

export interface PostResult {
  posted: boolean
  inline: number
  fallback: boolean
  error?: string
}

/**
 * One review submission with inline comments for findings that anchor to diff
 * lines; everything else lands in the review body. On a 422 (stale anchors,
 * force-push races) retry once with all findings in the body.
 */
export async function postReview(
  ctx: ReviewContext,
  merged: MergedFinding[],
  runs: EngineRun[],
  token: string,
): Promise<PostResult> {
  const pr = ctx.pr
  if (!pr) return { posted: false, inline: 0, fallback: false, error: "no PR context" }

  const inline: MergedFinding[] = []
  const bodyOnly: MergedFinding[] = []
  for (const finding of merged) {
    if (ctx.commentableLines.get(finding.file)?.has(finding.line)) inline.push(finding)
    else bodyOnly.push(finding)
  }

  const header = ["## kyora review", "", summaryLine(ctx, merged, runs)]
  if (inline.length > 0) header.push("", `${inline.length} finding${inline.length === 1 ? "" : "s"} commented inline.`)
  const body = [...header, ...bodyOnly.flatMap((finding) => ["", "---", "", renderFindingSection(finding)])].join("\n")

  const path = `/repos/${pr.owner}/${pr.repo}/pulls/${pr.number}/reviews`
  const payload = {
    commit_id: pr.headSha,
    event: "COMMENT",
    body,
    comments: inline.map((finding) => ({
      path: finding.file,
      line: finding.line,
      side: "RIGHT",
      body: renderInlineComment(finding),
    })),
  }

  const first = await api(token, "POST", path, payload)
  if (first.ok) return { posted: true, inline: inline.length, fallback: false }

  if (first.status === 422 && inline.length > 0) {
    const fullBody = [
      ...header,
      ...merged.flatMap((finding) => ["", "---", "", renderFindingSection(finding)]),
    ].join("\n")
    const second = await api(token, "POST", path, { commit_id: pr.headSha, event: "COMMENT", body: fullBody })
    if (second.ok) return { posted: true, inline: 0, fallback: true }
    return { posted: false, inline: 0, fallback: true, error: `POST reviews → ${second.status}` }
  }
  return { posted: false, inline: 0, fallback: false, error: `POST reviews → ${first.status}: ${JSON.stringify(first.json).slice(0, 300)}` }
}
