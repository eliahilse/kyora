import { readFileSync } from "node:fs"
import { mkdtemp, rm } from "node:fs/promises"
import { homedir, tmpdir } from "node:os"
import { join } from "node:path"
import type { EngineOverride, ReviewConfig } from "./types"
import {
  looksRateLimited,
  markRun,
  parseClaudeOauthUsage,
  parseQuotaWindows,
  parseTokenPlanUsage,
  parseZaiQuota,
  resetHintMs,
  type LiveUsage,
} from "./usage"

async function probeJson(url: string, headers: Record<string, string>): Promise<unknown | null> {
  try {
    const response = await fetch(url, { headers, redirect: "error", signal: AbortSignal.timeout(5000) })
    if (!response.ok) return null
    return await response.json()
  } catch {
    return null
  }
}

async function claudeToken(): Promise<string | undefined> {
  try {
    const creds = JSON.parse(readFileSync(join(homedir(), ".claude", ".credentials.json"), "utf8"))
    if (creds?.claudeAiOauth?.accessToken) return creds.claudeAiOauth.accessToken
  } catch {}
  if (process.platform === "darwin") {
    const result = await Bun.$`security find-generic-password -s "Claude Code-credentials" -w`.quiet().nothrow()
    if (result.exitCode === 0) {
      try {
        return JSON.parse(result.text().trim())?.claudeAiOauth?.accessToken ?? undefined
      } catch {}
    }
  }
  return undefined
}

async function claudeUsageProbe(): Promise<LiveUsage | null> {
  const token = await claudeToken()
  if (!token) return null
  const payload = await probeJson("https://api.anthropic.com/api/oauth/usage", {
    authorization: `Bearer ${token}`,
    "anthropic-beta": "oauth-2025-04-20",
  })
  return payload ? parseClaudeOauthUsage(payload) : null
}

async function kimiUsageProbe(): Promise<LiveUsage | null> {
  const key = process.env.KIMI_API_KEY
  if (!key) return null
  const url = process.env.KIMI_USAGE_URL ?? "https://api.kimi.com/coding/v1/usages"
  const payload = await probeJson(url, { authorization: `Bearer ${key}` })
  return payload ? parseQuotaWindows(payload) : null
}

async function glmUsageProbe(): Promise<LiveUsage | null> {
  const key = zaiKey()
  if (!key) return null
  const url = process.env.ZAI_USAGE_URL ?? "https://api.z.ai/api/monitor/usage/quota/limit"
  const payload = await probeJson(url, { authorization: `Bearer ${key}` })
  return payload ? parseZaiQuota(payload) : null
}

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
  /** args for delegated work that may edit files; absent = engine is read-only */
  argsWrite?: string[]
  /** args for free-form prose answers, without schema-constrained output */
  argsChat?: string[]
  env?: () => Record<string, string | undefined>
  /** engine writes its final message to the {out} file instead of stdout */
  readsOutFile?: boolean
  /** live remaining-quota query where the vendor exposes one; null = unavailable */
  usageProbe?: () => Promise<LiveUsage | null>
  /** selectable models; first entry is the default */
  models?: string[]
  /** extra args to select a model, when the CLI takes it as a flag */
  modelArgs?: (model: string) => string[]
  /** env var carrying the model id, for engines routed through another vendor's CLI */
  modelEnv?: string
  /** extra args to set reasoning effort, when supported */
  effortArgs?: (effort: string) => string[]
  authHint: string
}

export interface RunOptions {
  mode?: EngineMode
  model?: string
  effort?: string
}

async function qwenUsageProbe(): Promise<LiveUsage | null> {
  const cookie = process.env.QWEN_USAGE_COOKIE
  if (!cookie) return null
  const host = process.env.QWEN_CONSOLE_HOST ?? "bailian-singapore-cs.alibabacloud.com"
  try {
    const response = await fetch(`https://${host}/tokenplan/personal/api/v2/usage`, {
      headers: { cookie, accept: "application/json" },
      redirect: "error",
      signal: AbortSignal.timeout(5000),
    })
    if (!response.ok) return null
    return parseTokenPlanUsage(await response.json())
  } catch {
    return null
  }
}

function codexConfiguredModel(): string | undefined {
  try {
    const config = readFileSync(join(process.env.CODEX_HOME ?? join(homedir(), ".codex"), "config.toml"), "utf8")
    return /^\s*model\s*=\s*"([^"]+)"/m.exec(config)?.[1]
  } catch {
    return undefined
  }
}

function bailianKey(): string | undefined {
  if (process.env.QWEN_API_KEY) return process.env.QWEN_API_KEY
  try {
    const config = JSON.parse(readFileSync(join(homedir(), ".config/opencode/opencode.json"), "utf8"))
    return config.provider?.["bailian-cli"]?.options?.apiKey ?? undefined
  } catch {
    return undefined
  }
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

/** same CI/destructive denials, but file edits permitted for delegated work */
const CLAUDE_WRITE_DENIED = CLAUDE_DENIED.split(",")
  .filter((rule) => !["Write", "Edit", "MultiEdit", "NotebookEdit"].includes(rule))
  .join(",")

const CLAUDE_WRITE_ARGS = [
  "-p",
  "{prompt}",
  "--output-format",
  "json",
  "--max-turns",
  "60",
  "--allowedTools",
  "Read,Grep,Glob,Bash,Write,Edit,MultiEdit",
  "--disallowedTools",
  CLAUDE_WRITE_DENIED,
]

export type EngineMode = "read" | "write" | "chat"

export const ENGINES: EngineDef[] = [
  {
    id: "codex",
    label: "Codex (OpenAI)",
    bin: "codex",
    args: ["exec", "--sandbox", "read-only", "--output-schema", "{schema}", "-o", "{out}", "{prompt}"],
    argsWrite: ["exec", "--sandbox", "workspace-write", "--full-auto", "{prompt}"],
    argsChat: ["exec", "--sandbox", "read-only", "{prompt}"],
    readsOutFile: true,
    models: codexConfiguredModel() ? [codexConfiguredModel()!] : [],
    modelArgs: (model) => ["-m", model],
    effortArgs: (effort) => ["-c", `model_reasoning_effort="${effort}"`],
    authHint: "run `codex login` (ChatGPT subscription) or set OPENAI_API_KEY — CI: seed the CODEX_AUTH_JSON secret",
  },
  {
    id: "claude",
    label: "Claude Code (Anthropic)",
    bin: "claude",
    args: CLAUDE_ARGS,
    argsWrite: CLAUDE_WRITE_ARGS,
    usageProbe: claudeUsageProbe,
    models: ["sonnet", "opus", "haiku"],
    modelArgs: (model) => ["--model", model],
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
    argsWrite: CLAUDE_WRITE_ARGS,
    usageProbe: kimiUsageProbe,
    models: ["kimi-k3"],
    modelEnv: "ANTHROPIC_MODEL",
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
    argsWrite: CLAUDE_WRITE_ARGS,
    usageProbe: glmUsageProbe,
    models: ["glm-5.2", "glm-5.2[1m]"],
    modelEnv: "ANTHROPIC_MODEL",
    authHint: "set ZAI_API_KEY (GLM Coding Plan), or log in once via `opencode auth login` — the key is picked up from there",
  },
  {
    id: "grok",
    label: "Grok Build (xAI)",
    bin: "grok",
    args: ["--verbatim", "--reasoning-effort", "high", "--output-format", "json", "--json-schema", "{schemaJson}", "-p", "{prompt}"],
    argsWrite: ["--verbatim", "--reasoning-effort", "high", "--always-approve", "-p", "{prompt}"],
    argsChat: ["--verbatim", "--reasoning-effort", "high", "-p", "{prompt}"],
    models: ["grok-4.5"],
    modelArgs: (model) => ["-m", model],
    effortArgs: (effort) => ["--reasoning-effort", effort],
    authHint: "log in via `grok login`, or set GROK_API_KEY / XAI_API_KEY (console.x.ai)",
  },
  {
    id: "qwen",
    label: "Qwen (Alibaba Token Plan, via Claude Code)",
    bin: "claude",
    ready: () => (bailianKey() ? null : "no Token Plan key (set QWEN_API_KEY or run `bl config agent --agent opencode ...`)"),
    args: CLAUDE_ARGS,
    env: () => ({
      ANTHROPIC_BASE_URL:
        process.env.QWEN_BASE_URL ?? "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
      ANTHROPIC_AUTH_TOKEN: bailianKey(),
      ANTHROPIC_MODEL: process.env.QWEN_MODEL ?? "qwen3.8-max-preview",
      ANTHROPIC_API_KEY: undefined,
    }),
    argsWrite: CLAUDE_WRITE_ARGS,
    usageProbe: qwenUsageProbe,
    models: ["qwen3.8-max-preview"],
    modelEnv: "ANTHROPIC_MODEL",
    authHint: "set QWEN_API_KEY (Token Plan key), or run `bl config agent` once — the key is picked up from there",
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
  rateLimited?: boolean
  durationMs: number
}

/** A caller-supplied effort flag must replace the engine's built-in one, not duplicate it. */
function dedupeEffort(selectors: string[], template: string[]): string[] {
  if (selectors.length === 0) return template
  const flags = new Set(selectors.filter((token) => token.startsWith("-")))
  const kept: string[] = []
  for (let i = 0; i < template.length; i++) {
    const token = template[i]!
    if (flags.has(token)) {
      i++
      continue
    }
    kept.push(token)
  }
  return [...selectors, ...kept]
}

/** Run an engine CLI headless in the repo checkout and capture whatever it printed. */
export async function runEngineRaw(
  engine: EngineDef,
  prompt: string,
  schema: unknown,
  cwd: string,
  config: ReviewConfig,
  options: RunOptions = {},
): Promise<RawRun> {
  const mode = options.mode ?? "read"
  const override = config.overrides[engine.id]
  const started = Date.now()
  const workDir = await mkdtemp(join(tmpdir(), `kyora-review-${engine.id}-`))
  try {
    const schemaPath = join(workDir, "schema.json")
    const outPath = join(workDir, "out.json")
    await Bun.write(schemaPath, JSON.stringify(schema))

    const bin = override?.bin ?? engine.bin
    const baseTemplate =
      mode === "write"
        ? (engine.argsWrite ?? override?.args ?? engine.args)
        : mode === "chat"
          ? (engine.argsChat ?? override?.args ?? engine.args)
          : (override?.args ?? engine.args)
    const selectors: string[] = []
    if (options.model && engine.modelArgs) selectors.push(...engine.modelArgs(options.model))
    if (options.effort && engine.effortArgs) selectors.push(...engine.effortArgs(options.effort))
    const argTemplate = dedupeEffort(selectors, baseTemplate)
    const args = argTemplate.map((arg) =>
      arg
        .replace("{schemaJson}", () => JSON.stringify(schema))
        .replace("{schema}", () => schemaPath)
        .replace("{out}", () => outPath)
        .replace("{prompt}", () => prompt),
    )

    const modelOverride =
      options.model && engine.modelEnv ? { [engine.modelEnv]: options.model } : {}
    const env: Record<string, string> = {}
    for (const [key, value] of Object.entries({
      ...process.env,
      ...engine.env?.(),
      ...override?.env,
      ...modelOverride,
    })) {
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
    const failureContext = timedOut || exitCode !== 0 || raw.includes('"is_error":true')
    const rateLimited =
      failureContext && looksRateLimited(`${stderr.slice(-2000)}\n${raw.slice(-2000)}`)
    const cooldownMs = resetHintMs(`${stderr}\n${raw}`.slice(-3000)) ?? config.cooldownMinutes * 60_000
    markRun(engine.id, rateLimited ? "rate_limited" : failureContext ? "error" : "ok", cooldownMs)

    if (timedOut) {
      return { ok: false, raw, error: `timed out after ${Math.round(config.timeoutMs / 1000)}s`, rateLimited, durationMs }
    }
    if (exitCode !== 0 && !raw.trim()) {
      const tail = stderr.trim().split("\n").slice(-4).join("\n")
      return {
        ok: false,
        raw,
        error: `${rateLimited ? "rate-limited: " : ""}exit ${exitCode}: ${tail || "no output"}`,
        rateLimited,
        durationMs,
      }
    }
    return { ok: true, raw, rateLimited, durationMs }
  } catch (error) {
    return { ok: false, raw: "", error: String(error), durationMs: Date.now() - started }
  } finally {
    await rm(workDir, { recursive: true, force: true })
  }
}
