export interface StakesVerdict {
  highStakes: boolean
  reason: string
  suggested: string[]
}

export interface TranscriptEntry {
  role?: string
  type?: string
  content?: unknown
}

/**
 * Cheap pre-filter before spending a model call: only tool activity that could
 * plausibly be consequential is worth classifying.
 */
const INTERESTING = /\b(migration|migrate|schema|auth|token|secret|credential|password|encrypt|permission|delete|drop|truncate|deploy|release|payment|billing|rm -rf|force|revoke|cascade)\b/i

const CONSEQUENTIAL_TOOLS = new Set(["Write", "Edit", "MultiEdit", "NotebookEdit", "Bash"])

export function worthClassifying(toolName: string, payload: string): boolean {
  if (!CONSEQUENTIAL_TOOLS.has(toolName)) return false
  if (toolName === "Bash") {
    return INTERESTING.test(payload) || /\b(git (push|reset|rebase)|kubectl|terraform|docker|psql|redis-cli)\b/.test(payload)
  }
  return INTERESTING.test(payload) || payload.length > 1500
}

export const CLASSIFIER_PROMPT = `You watch a coding agent work and decide whether it is at a genuinely high-stakes moment — one where a second opinion from another model family would be worth the cost.

HIGH STAKES means: an architectural commitment that will be expensive to reverse; a security- or data-integrity-sensitive change (auth, secrets, permissions, migrations, deletion); an irreversible or production-affecting operation; or a debugging conclusion the agent is about to act on that rests on an unverified assumption.

NOT high stakes: routine edits, tests, refactors, formatting, docs, exploration, or anything trivially revertible. Most moments are NOT high stakes — say so. False alarms cost the user real money.

Respond with ONLY JSON: {"highStakes": boolean, "reason": "<one sentence>", "suggested": ["<engine ids worth consulting, or empty>"]}`

export function parseVerdict(raw: string): StakesVerdict | null {
  const match = /\{[\s\S]*\}/.exec(raw)
  if (!match) return null
  try {
    const parsed = JSON.parse(match[0]) as Record<string, unknown>
    if (typeof parsed.highStakes !== "boolean") return null
    return {
      highStakes: parsed.highStakes,
      reason: typeof parsed.reason === "string" ? parsed.reason.slice(0, 400) : "",
      suggested: Array.isArray(parsed.suggested)
        ? parsed.suggested.filter((item): item is string => typeof item === "string").slice(0, 4)
        : [],
    }
  } catch {
    return null
  }
}

export interface WatcherConfig {
  baseUrl: string
  apiKey: string
  model: string
}

export function watcherConfig(): WatcherConfig | null {
  const apiKey = process.env.KYORA_WATCHER_KEY ?? process.env.ZAI_API_KEY ?? process.env.QWEN_API_KEY
  if (!apiKey) return null
  const baseUrl =
    process.env.KYORA_WATCHER_BASE_URL ??
    (process.env.KYORA_WATCHER_KEY || process.env.ZAI_API_KEY
      ? "https://api.z.ai/api/paas/v4"
      : "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1")
  const model = process.env.KYORA_WATCHER_MODEL ?? (baseUrl.includes("z.ai") ? "glm-4.5-air" : "qwen3.6-plus")
  return { baseUrl, apiKey, model }
}

/** One cheap chat completion; any failure means "not high stakes" so the hook never blocks work. */
export async function classify(snippet: string, config: WatcherConfig): Promise<StakesVerdict | null> {
  try {
    const response = await fetch(`${config.baseUrl}/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${config.apiKey}` },
      body: JSON.stringify({
        model: config.model,
        max_tokens: 300,
        temperature: 0,
        messages: [
          { role: "system", content: CLASSIFIER_PROMPT },
          { role: "user", content: snippet.slice(-6000) },
        ],
      }),
      signal: AbortSignal.timeout(Number(process.env.KYORA_WATCHER_TIMEOUT_MS ?? 12_000)),
    })
    if (!response.ok) return null
    const payload = (await response.json()) as {
      choices?: { message?: { content?: string } }[]
    }
    const content = payload.choices?.[0]?.message?.content
    return content ? parseVerdict(content) : null
  } catch {
    return null
  }
}
