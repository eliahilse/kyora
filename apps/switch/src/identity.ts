import type { Identity } from "./types"

/** Decode a JWT payload without verifying the signature — this is display only. */
export function jwtPayload(token: string): Record<string, unknown> | null {
  const segment = token.split(".")[1]
  if (!segment) return null
  try {
    const json = Buffer.from(segment.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString("utf8")
    const parsed: unknown = JSON.parse(json)
    return parsed !== null && typeof parsed === "object" ? (parsed as Record<string, unknown>) : null
  } catch {
    return null
  }
}

function str(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined
}

function record(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" ? (value as Record<string, unknown>) : undefined
}

/** Pull the account out of `~/.codex/auth.json` — the id_token carries it, an API key does not. */
export function codexIdentity(auth: unknown): Identity {
  const root = record(auth) ?? {}
  const tokens = record(root.tokens) ?? {}
  const idToken = str(tokens.id_token)
  const payload = idToken ? (jwtPayload(idToken) ?? {}) : {}
  const claims = record(payload["https://api.openai.com/auth"]) ?? {}
  const account = str(payload.email) ?? str(claims.chatgpt_account_id) ?? str(tokens.account_id)
  const plan = str(claims.chatgpt_plan_type) ?? (str(root.OPENAI_API_KEY) ? "api key" : undefined)
  return { account, plan }
}

/** Pull the account out of the `oauthAccount` slice of `~/.claude.json`. */
export function claudeIdentity(account: unknown): Identity {
  const oauth = record(record(account)?.oauthAccount) ?? {}
  return {
    account: str(oauth.emailAddress) ?? str(oauth.accountUuid),
    org: str(oauth.organizationName),
    plan: str(oauth.seatTier) ?? str(oauth.billingType),
  }
}
