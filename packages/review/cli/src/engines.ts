import { readFileSync } from "node:fs"
import { mkdtemp, rm } from "node:fs/promises"
import { homedir, tmpdir } from "node:os"
import { join } from "node:path"
import type { EngineOverride, ReviewConfig } from "./types"

export interface EngineDef {
  id: string
  label: string
  bin: string
  /** env vars that must all be present for this engine to be selectable */
  requiresEnv?: string[]
  /** extra readiness check; returns a reason when unavailable, null when ready */
  ready?: () => string | null
  /** tokens {prompt} {schema} {out} are substituted; first element is replaced by the resolved bin */
  args: string[]
  env?: () => Record<string, string | undefined>
  /** engine writes its final message to the {out} file instead of stdout */
  readsOutFile?: boolean
  authHint: string
}

function zaiKey(): string | undefined {
  if (process.env.ZAI_API_KEY) return process.env.ZAI_API_KEY
  try {
    const auth = JSON.parse(readFileSync(join(homedir(), ".local/share/opencode/auth.json"), "utf8"))
    const entry = auth["zai-coding-plan"]
    return entry?.key ?? entry?.apiKey ?? undefined
  } catch {
    return undefined
  }
}

const CLAUDE_DENIED = [
  "Write", "Edit", "MultiEdit", "NotebookEdit", "WebFetch", "WebSearch",
  "Bash(bun test:*)", "Bash(bun run:*)", "Bash(bun install:*)", "Bash(bunx turbo:*)",
  "Bash(npm test:*)", "Bash(npm run:*)", "Bash(npm install:*)", "Bash(npx turbo:*)",
  "Bash(pnpm test:*)", "Bash(pnpm run:*)", "Bash(pnpm install:*)",
  "Bash(yarn:*)", "Bash(turbo:*)", "Bash(tsc:*)", "Bash(next:*)",
  "Bash(vitest:*)", "Bash(jest:*)", "Bash(pytest:*)", "Bash(go test:*)", "Bash(cargo test:*)",
  "Bash(make:*)", "Bash(pip install:*)", "Bash(rm:*)", "Bash(mv:*)",
  "Bash(git push:*)", "Bash(git commit:*)", "Bash(git checkout:*)", "Bash(git reset:*)", "Bash(git stash:*)",
].join(",")

const CLAUDE_ARGS = [
  "-p",
  "{prompt}",
  "--output-format",
  "json",
  "--max-turns",
  "40",
  "--allowedTools",
  "Read,Grep,Glob,Bash",
  "--disallowedTools",
  CLAUDE_DENIED,
]

export const ENGINES: EngineDef[] = [
  {
    id: "codex",
    label: "Codex (OpenAI)",
    bin: "codex",
    args: ["exec", "--sandbox", "read-only", "--output-schema", "{schema}", "-o", "{out}", "{prompt}"],
    readsOutFile: true,
    authHint: "run `codex login` (ChatGPT subscription) or set OPENAI_API_KEY — CI: seed the CODEX_AUTH_JSON secret",
  },
  {
    id: "claude",
    label: "Claude Code (Anthropic)",
    bin: "claude",
    args: CLAUDE_ARGS,
    authHint: "log in once via `claude`, or set CLAUDE_CODE_OAUTH_TOKEN (created with `claude setup-token`)",
  },
  {
    id: "kimi",
    label: "Kimi (Moonshot, via Claude Code)",
    bin: "claude",
    requiresEnv: ["KIMI_API_KEY"],
    args: CLAUDE_ARGS,
    env: () => ({
      ANTHROPIC_BASE_URL: process.env.KIMI_BASE_URL ?? "https://api.moonshot.ai/anthropic",
      ANTHROPIC_AUTH_TOKEN: process.env.KIMI_API_KEY,
      ANTHROPIC_MODEL: process.env.KIMI_MODEL ?? "kimi-k3",
      ANTHROPIC_API_KEY: undefined,
    }),
    authHint: "set KIMI_API_KEY (Kimi membership / platform.kimi.ai); optional KIMI_BASE_URL, KIMI_MODEL",
  },
  {
    id: "glm",
    label: "GLM 5.2 (Z.ai, via Claude Code)",
    bin: "claude",
    ready: () => (zaiKey() ? null : "no Z.ai key (set ZAI_API_KEY or run `opencode auth login` → Z.AI Coding Plan)"),
    args: CLAUDE_ARGS,
    env: () => ({
      ANTHROPIC_BASE_URL: process.env.ZAI_BASE_URL ?? "https://api.z.ai/api/anthropic",
      ANTHROPIC_AUTH_TOKEN: zaiKey(),
      ANTHROPIC_MODEL: process.env.ZAI_MODEL ?? "glm-5.2",
      ANTHROPIC_API_KEY: undefined,
    }),
    authHint: "set ZAI_API_KEY (GLM Coding Plan), or log in once via `opencode auth login` — the key is picked up from there",
  },
  {
    id: "grok",
    label: "Grok Build (xAI)",
    bin: "grok",
    args: ["--verbatim", "--reasoning-effort", "high", "--output-format", "json", "--json-schema", "{schemaJson}", "-p", "{prompt}"],
    authHint: "log in via `grok login`, or set GROK_API_KEY / XAI_API_KEY (console.x.ai)",
  },
  {
    id: "qwen",
    label: "Qwen Code (Alibaba)",
    bin: "qwen",
    args: ["-p", "{prompt}"],
    authHint: "run `qwen` once to log in (Coding Plan), or configure an API key per qwen-code docs",
  },
]

export function engineById(id: string): EngineDef | undefined {
  return ENGINES.find((engine) => engine.id === id)
}

export interface EngineStatus {
  engine: EngineDef
  available: boolean
  reason: string
}

export function engineStatus(engine: EngineDef, override: EngineOverride | undefined): EngineStatus {
  const bin = override?.bin ?? engine.bin
  if (!Bun.which(bin)) {
    return { engine, available: false, reason: `\`${bin}\` not on PATH` }
  }
  for (const name of engine.requiresEnv ?? []) {
    if (!process.env[name]) {
      return { engine, available: false, reason: `${name} not set` }
    }
  }
  const notReady = engine.ready?.()
  if (notReady) return { engine, available: false, reason: notReady }
  return { engine, available: true, reason: "ready" }
}

export interface RawRun {
  ok: boolean
  raw: string
  error?: string
  durationMs: number
}

/** Run an engine CLI headless in the repo checkout and capture whatever it printed. */
export async function runEngineRaw(
  engine: EngineDef,
  prompt: string,
  schema: unknown,
  cwd: string,
  config: ReviewConfig,
): Promise<RawRun> {
  const override = config.overrides[engine.id]
  const started = Date.now()
  const workDir = await mkdtemp(join(tmpdir(), `kyora-review-${engine.id}-`))
  try {
    const schemaPath = join(workDir, "schema.json")
    const outPath = join(workDir, "out.json")
    await Bun.write(schemaPath, JSON.stringify(schema))

    const bin = override?.bin ?? engine.bin
    const argTemplate = override?.args ?? engine.args
    const args = argTemplate.map((arg) =>
      arg
        .replace("{schemaJson}", () => JSON.stringify(schema))
        .replace("{schema}", () => schemaPath)
        .replace("{out}", () => outPath)
        .replace("{prompt}", () => prompt),
    )

    const env: Record<string, string> = {}
    for (const [key, value] of Object.entries({ ...process.env, ...engine.env?.(), ...override?.env })) {
      if (value !== undefined) env[key] = value
    }

    const proc = Bun.spawn({ cmd: [bin, ...args], cwd, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" })
    let timedOut = false
    const timer = setTimeout(() => {
      timedOut = true
      proc.kill()
    }, config.timeoutMs)
    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ])
    clearTimeout(timer)

    let raw = stdout
    if (engine.readsOutFile) {
      const outFile = Bun.file(outPath)
      if (await outFile.exists()) {
        const content = (await outFile.text()).trim()
        if (content) raw = `${content}\n${stdout}`
      }
    }

    const durationMs = Date.now() - started
    if (timedOut) {
      return { ok: false, raw, error: `timed out after ${Math.round(config.timeoutMs / 1000)}s`, durationMs }
    }
    if (exitCode !== 0 && !raw.trim()) {
      const tail = stderr.trim().split("\n").slice(-4).join("\n")
      return { ok: false, raw, error: `exit ${exitCode}: ${tail || "no output"}`, durationMs }
    }
    return { ok: true, raw, durationMs }
  } catch (error) {
    return { ok: false, raw: "", error: String(error), durationMs: Date.now() - started }
  } finally {
    await rm(workDir, { recursive: true, force: true })
  }
}
