import type { Provider, ProviderId } from "../types"
import { claudeProvider } from "./claude"
import { codexProvider } from "./codex"

export const PROVIDERS: Provider[] = [claudeProvider, codexProvider]

export function providerById(id: ProviderId): Provider {
  const provider = PROVIDERS.find((candidate) => candidate.id === id)
  if (!provider) throw new Error(`unknown provider "${id}"`)
  return provider
}

/**
 * Counts processes still holding this login in memory, matched on executable name.
 * `pgrep -f` would also hit every process that merely mentions `.claude/` in its arguments.
 */
export async function runningSessions(provider: Provider): Promise<number> {
  const result = await Bun.$`pgrep -x ${provider.processName}`.quiet().nothrow()
  if (result.exitCode !== 0) return 0
  return result.stdout
    .toString()
    .split("\n")
    .filter((line) => line.trim().length > 0).length
}
