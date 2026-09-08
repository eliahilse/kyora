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

const STALE_KEYS = [
  "additionalModelCostsCache",
  "additionalModelOptionsCache",
  "cachedExtraUsageDisabledReason",
  "groveConfigCache",
  "hasAvailableSubscription",
  "metricsStatusCache",
  "modelAccessCache",
  "orgModelDefaultCache",
  "overageCreditGrantCache",
  "passesEligibilityCache",
  "passesLastSeenRemaining",
  "subscriptionNoticeCount",
]

const SIDE_FILES = ["policy-limits.json", "remote-settings.json"]

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
 * Swaps the account identity and drops the previous account's entitlement caches.
 * Everything else in the config, including projects and machine state, is kept.
 */
export function mergeAccountIntoConfig(
  config: Record<string, unknown>,
  account: Record<string, unknown>,
): Record<string, unknown> {
  const merged = { ...config }
  if (ACCOUNT_KEY in account) merged[ACCOUNT_KEY] = account[ACCOUNT_KEY]
  else delete merged[ACCOUNT_KEY]
  for (const key of STALE_KEYS) delete merged[key]
  return merged
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
    return [store, claudeConfigPath(), ...SIDE_FILES.map((file) => join(claudeDir(), file))]
  },

  async capture(): Promise<Snapshot | null> {
    const credentials = await readCredentials()
    if (credentials === null) return null

    const slice = accountSlice(await readJsonIfExists(claudeConfigPath()))
    const files: Record<string, string> = {
      [CREDENTIALS_FILE]: credentials,
      [ACCOUNT_FILE]: `${JSON.stringify(slice, null, 2)}\n`,
    }
    for (const name of SIDE_FILES) {
      const text = await readTextIfExists(join(claudeDir(), name))
      if (text !== null) files[name] = text
    }
    return { provider: "claude", identity: claudeIdentity(slice), files, capturedAt: Date.now() }
  },

  async forget(): Promise<void> {
    await rm(credentialsPath(), { force: true })
    if (keychainSupported()) await keychainDelete()

    const config = await readJsonIfExists(claudeConfigPath())
    if (config) await writeFileAtomic(claudeConfigPath(), `${JSON.stringify(mergeAccountIntoConfig(config, {}), null, 2)}\n`)
    for (const name of SIDE_FILES) await rm(join(claudeDir(), name), { force: true })
  },

  credentialExpiry(snapshot: Snapshot): number | undefined {
    try {
      const expiresAt = JSON.parse(snapshot.files[CREDENTIALS_FILE] ?? "{}")?.claudeAiOauth?.expiresAt
      return typeof expiresAt === "number" ? expiresAt : undefined
    } catch {
      return undefined
    }
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

    for (const name of SIDE_FILES) {
      const path = join(claudeDir(), name)
      const value = snapshot.files[name]
      if (value === undefined) await rm(path, { force: true })
      else await writeFileAtomic(path, value, 0o600)
    }
  },
}
