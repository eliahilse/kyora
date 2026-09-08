import { codexCredentials, codexUsage, type LiveUsage } from "@kyora-sh/usage"
import { homedir } from "node:os"
import { join } from "node:path"
import { rm } from "node:fs/promises"
import { readTextIfExists, writeFileAtomic } from "../fsx"
import { codexIdentity } from "../identity"
import type { Provider, Snapshot } from "../types"

const AUTH_FILE = "auth.json"

export function codexDir(): string {
  return process.env.CODEX_HOME ?? join(homedir(), ".codex")
}

function authPath(): string {
  return join(codexDir(), AUTH_FILE)
}

export const codexProvider: Provider = {
  id: "codex",
  label: "Codex",
  processName: "codex",
  loginHint: "run `codex login`",

  locations() {
    return [authPath()]
  },

  async capture(): Promise<Snapshot | null> {
    const text = await readTextIfExists(authPath())
    if (text === null) return null
    let parsed: unknown
    try {
      parsed = JSON.parse(text)
    } catch {
      throw new Error(`${authPath()} is not valid JSON`)
    }
    return { provider: "codex", identity: codexIdentity(parsed), files: { [AUTH_FILE]: text }, capturedAt: Date.now() }
  },

  async forget(): Promise<void> {
    await rm(authPath(), { force: true })
  },

  async quota(snapshot: Snapshot): Promise<LiveUsage | null> {
    const credentials = codexCredentials(snapshot.files[AUTH_FILE] ?? "")
    return credentials ? await codexUsage(credentials.token, credentials.account) : null
  },

  async restore(snapshot: Snapshot): Promise<void> {
    const auth = snapshot.files[AUTH_FILE]
    if (auth === undefined) throw new Error("slot has no Codex credentials")
    await writeFileAtomic(authPath(), auth, 0o600)
  },
}
