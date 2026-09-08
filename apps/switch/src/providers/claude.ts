import { claudeAccessToken, claudeOauthUsage, type LiveUsage } from "@kyora-sh/usage"
import { rm } from "node:fs/promises"
import { homedir } from "node:os"
import { join } from "node:path"
import { readJsonIfExists, readTextIfExists, writeFileAtomic } from "../fsx"
import { claudeIdentity } from "../identity"
import { KEYCHAIN_SERVICE, keychainAccount, keychainDelete, keychainRead, keychainSupported, keychainWrite } from "../keychain"
import type { Provider, Snapshot } from "../types"

const CREDENTIALS_FILE = "credentials.json"
const ACCOUNT_FILE = "account.json"
const ACCOUNT_KEY = "oauthAccount"

const tokenUrl = () => process.env.KYORA_CLAUDE_TOKEN_URL ?? "https://platform.claude.com/v1/oauth/token"
const clientId = () => process.env.KYORA_CLAUDE_CLIENT_ID ?? "9d1c250a-e61b-44d9-88ed-5944d1962f5e"

export function claudeDir(): string {
  return process.env.CLAUDE_CONFIG_DIR ?? join(homedir(), ".claude")
}

export function claudeConfigPath(): string {
  const dir = process.env.CLAUDE_CONFIG_DIR
  return dir ? join(dir, ".claude.json") : join(homedir(), ".claude.json")
}

function credentialsPath(): string {
  return join(claudeDir(), ".credentials.json")
}

async function readCredentials(): Promise<string | null> {
  const onDisk = await readTextIfExists(credentialsPath())
  if (onDisk !== null) return onDisk
  return keychainSupported() ? await keychainRead() : null
}

async function writeCredentials(value: string): Promise<void> {
  if ((await readTextIfExists(credentialsPath())) !== null) {
    await writeFileAtomic(credentialsPath(), value, 0o600)
    return
  }
  if (keychainSupported()) {
    await keychainWrite(value)
    return
  }
  await writeFileAtomic(credentialsPath(), value, 0o600)
}

/**
 * Swaps the account identity and nothing else, so projects, machine state and the
 * caches the CLI refetches for itself all survive a switch untouched.
 */
export function mergeAccountIntoConfig(
  config: Record<string, unknown>,
  account: Record<string, unknown>,
): Record<string, unknown> {
  const merged = { ...config }
  if (ACCOUNT_KEY in account) merged[ACCOUNT_KEY] = account[ACCOUNT_KEY]
  else delete merged[ACCOUNT_KEY]
  return merged
}

interface OauthBlob {
  accessToken?: string
  refreshToken?: string
  expiresAt?: number
  refreshTokenExpiresAt?: number
  scopes?: string[]
}

/**
 * Trades a stored refresh token for a fresh access token, the same call the CLI
 * makes when its own token ages out. The refresh token rotates, so the caller
 * must persist the result or the slot is left holding a spent credential.
 */
export async function refreshCredentials(credentials: string): Promise<string | null> {
  let parsed: Record<string, unknown>
  try {
    parsed = JSON.parse(credentials)
  } catch {
    return null
  }
  const oauth = parsed.claudeAiOauth as OauthBlob | undefined
  if (!oauth?.refreshToken) return null

  const response = await fetch(tokenUrl(), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      grant_type: "refresh_token",
      refresh_token: oauth.refreshToken,
      client_id: clientId(),
      scope: (oauth.scopes ?? []).join(" "),
    }),
    signal: AbortSignal.timeout(30_000),
  }).catch(() => null)
  if (!response?.ok) return null

  const data = (await response.json().catch(() => null)) as Record<string, unknown> | null
  if (typeof data?.access_token !== "string") return null

  const now = Date.now()
  const next: OauthBlob = {
    ...oauth,
    accessToken: data.access_token,
    refreshToken: typeof data.refresh_token === "string" ? data.refresh_token : oauth.refreshToken,
    expiresAt: typeof data.expires_in === "number" ? now + data.expires_in * 1000 : oauth.expiresAt,
    refreshTokenExpiresAt:
      typeof data.refresh_token_expires_in === "number"
        ? now + data.refresh_token_expires_in * 1000
        : oauth.refreshTokenExpiresAt,
    scopes: typeof data.scope === "string" ? data.scope.split(" ") : oauth.scopes,
  }
  return JSON.stringify({ ...parsed, claudeAiOauth: next })
}

export function accountSlice(config: Record<string, unknown> | null): Record<string, unknown> {
  return config && ACCOUNT_KEY in config ? { [ACCOUNT_KEY]: config[ACCOUNT_KEY] } : {}
}

export const claudeProvider: Provider = {
  id: "claude",
  label: "Claude Code",
  processName: "claude",
  loginHint: "run `claude` and use /login",

  locations() {
    const store = keychainSupported()
      ? `macOS keychain: ${KEYCHAIN_SERVICE} (${keychainAccount()})`
      : credentialsPath()
    return [store, claudeConfigPath()]
  },

  async capture(): Promise<Snapshot | null> {
    const credentials = await readCredentials()
    if (credentials === null) return null

    const slice = accountSlice(await readJsonIfExists(claudeConfigPath()))
    const files: Record<string, string> = {
      [CREDENTIALS_FILE]: credentials,
      [ACCOUNT_FILE]: `${JSON.stringify(slice, null, 2)}\n`,
    }
    return { provider: "claude", identity: claudeIdentity(slice), files, capturedAt: Date.now() }
  },

  async forget(): Promise<void> {
    await rm(credentialsPath(), { force: true })
    if (keychainSupported()) await keychainDelete()

    const config = await readJsonIfExists(claudeConfigPath())
    if (config) await writeFileAtomic(claudeConfigPath(), `${JSON.stringify(mergeAccountIntoConfig(config, {}), null, 2)}\n`)
  },

  credentialExpiry(snapshot: Snapshot): number | undefined {
    try {
      const expiresAt = JSON.parse(snapshot.files[CREDENTIALS_FILE] ?? "{}")?.claudeAiOauth?.expiresAt
      return typeof expiresAt === "number" ? expiresAt : undefined
    } catch {
      return undefined
    }
  },

  async refresh(snapshot: Snapshot): Promise<Snapshot | null> {
    const credentials = snapshot.files[CREDENTIALS_FILE]
    if (credentials === undefined) return null
    const refreshed = await refreshCredentials(credentials)
    if (refreshed === null) return null
    return { ...snapshot, files: { ...snapshot.files, [CREDENTIALS_FILE]: refreshed } }
  },

  async quota(snapshot: Snapshot): Promise<LiveUsage | null> {
    const token = claudeAccessToken(snapshot.files[CREDENTIALS_FILE] ?? "")
    return token ? await claudeOauthUsage(token) : null
  },

  async restore(snapshot: Snapshot): Promise<void> {
    const credentials = snapshot.files[CREDENTIALS_FILE]
    if (credentials === undefined) throw new Error("slot has no Claude credentials")
    await writeCredentials(credentials)

    const account = JSON.parse(snapshot.files[ACCOUNT_FILE] ?? "{}") as Record<string, unknown>
    const config = (await readJsonIfExists(claudeConfigPath())) ?? {}
    await writeFileAtomic(claudeConfigPath(), `${JSON.stringify(mergeAccountIntoConfig(config, account), null, 2)}\n`)
  },
}
