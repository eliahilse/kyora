import { join } from "node:path"
import { repoRoot } from "./diff"
import type { ReviewConfig } from "./types"

export const CONFIG_FILE = "kyora-review.config.json"

export interface LoadedConfig {
  config: Partial<ReviewConfig>
  /** set when the file exists but could not be read as JSON; callers decide whether that is fatal */
  error?: string
}

/**
 * Engine configuration is shared by every consumer of the engine pool — the
 * review CLI and the council MCP server both run the same vendor CLIs, so a
 * `bin`/`args`/`env` override has to mean the same thing to both.
 */
export async function loadConfigFile(root: string): Promise<LoadedConfig> {
  const file = Bun.file(join(root, CONFIG_FILE))
  if (!(await file.exists())) return { config: {} }
  try {
    return { config: (await file.json()) as Partial<ReviewConfig> }
  } catch {
    return { config: {}, error: `${CONFIG_FILE} exists but is not valid JSON` }
  }
}

/** Config that applies to work happening inside `cwd` — its repo root, or `cwd` itself if it is not a checkout. */
export async function configForCwd(cwd: string): Promise<LoadedConfig> {
  return loadConfigFile((await repoRoot(cwd)) ?? cwd)
}
