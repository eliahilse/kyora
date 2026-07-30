import { mkdirSync, readFileSync, writeFileSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"
import {
  ENGINES,
  engineById,
  engineStatus,
  runEngineRaw,
  type EngineDef,
  type EngineMode,
} from "@kyora-sh/review/engines"
import { cooldownRemainingMs, lastRunAt, loadUsage } from "@kyora-sh/review/usage"
import type { ReviewConfig } from "@kyora-sh/review/types"

export const RUN_CONFIG: ReviewConfig = {
  engines: ["auto"],
  verify: false,
  post: false,
  base: "main",
  failOn: "none",
  maxDiffBytes: 100_000,
  timeoutMs: Number(process.env.KYORA_COUNCIL_TIMEOUT_MS ?? 600_000),
  maxFindingsPerEngine: 20,
  cooldownMinutes: 60,
  maxEngines: 0,
  overrides: {},
}

export interface EngineHealth {
  id: string
  label: string
  available: boolean
  reason: string
  cooldownMinutes: number
  remainingPct: number | null
  writeCapable: boolean
}

export async function councilStatus(): Promise<EngineHealth[]> {
  const usage = loadUsage()
  return Promise.all(
    ENGINES.map(async (engine) => {
      const status = engineStatus(engine, undefined)
      const live = status.available && engine.usageProbe ? await engine.usageProbe() : null
      return {
        id: engine.id,
        label: engine.label,
        available: status.available,
        reason: status.reason,
        cooldownMinutes: Math.ceil(cooldownRemainingMs(engine.id, usage) / 60_000),
        remainingPct: live?.remainingPct ?? null,
        writeCapable: Boolean(engine.argsWrite),
      }
    }),
  )
}

/** Engines that can actually be spent right now, cheapest-to-quota first. */
export async function healthyEngines(requested?: string[], mode: EngineMode = "read"): Promise<EngineDef[]> {
  const usage = loadUsage()
  const pool = requested?.length
    ? requested.map((id) => engineById(id.trim())).filter((engine): engine is EngineDef => Boolean(engine))
    : ENGINES
  const checked = await Promise.all(
    pool.map(async (engine) => {
      if (!engineStatus(engine, undefined).available) return null
      if (mode === "write" && !engine.argsWrite) return null
      if (cooldownRemainingMs(engine.id, usage) > 0) return null
      const live = engine.usageProbe ? await engine.usageProbe() : null
      if (live !== null && live.remainingPct <= 0) return null
      return { engine, headroom: live?.remainingPct ?? 50 }
    }),
  )
  return checked
    .filter((item): item is { engine: EngineDef; headroom: number } => item !== null)
    .sort((a, b) => b.headroom - a.headroom || lastRunAt(a.engine.id, usage) - lastRunAt(b.engine.id, usage))
    .map((item) => item.engine)
}

const ASK_PROMPT = (question: string, context: string | undefined) =>
  `You are consulted as an independent expert from a different model family than the agent asking. Give your own honest assessment — do not defer to the framing of the question, and say so plainly if you think the premise is wrong.

You are inside the repository checkout and may read files and run small read-only probes to ground your answer. Do not modify files. Do not run the project's test suites or builds.

Be concise and concrete: lead with your position, then the reasoning that would change someone's mind. If you are uncertain, say what would resolve the uncertainty.
${context ? `\nCONTEXT FROM THE AGENT:\n${context}\n` : ""}
QUESTION:
${question}`

const TASK_PROMPT = (task: string, context: string | undefined, write: boolean) =>
  `You are delegated a task by another coding agent. ${
    write
      ? "You may edit files in this repository to complete it."
      : "Work read-only: investigate and report, do not modify files."
  } Do not run the project's test suites or builds — CI covers those. Do not commit, push, or install packages.

When done, report what you did (or found) and anything the delegating agent must know — especially surprises, things you could not verify, and follow-ups you deliberately left.
${context ? `\nCONTEXT:\n${context}\n` : ""}
TASK:
${task}`

export interface EngineReply {
  engine: string
  ok: boolean
  text: string
  durationMs: number
  rateLimited?: boolean
}

function cleanText(raw: string): string {
  const trimmed = raw.trim()
  try {
    const parsed = JSON.parse(trimmed)
    if (parsed && typeof parsed === "object") {
      const obj = parsed as Record<string, unknown>
      for (const key of ["result", "last_message", "text", "message"]) {
        if (typeof obj[key] === "string") return (obj[key] as string).trim()
      }
      if (obj.structuredOutput && typeof obj.structuredOutput === "object") {
        return JSON.stringify(obj.structuredOutput, null, 2)
      }
    }
  } catch {}
  // NDJSON event streams: concatenate text parts, newest run last
  const texts: string[] = []
  for (const line of trimmed.split("\n")) {
    try {
      const event = JSON.parse(line) as Record<string, unknown>
      const part = event.part as Record<string, unknown> | undefined
      if (event.type === "text" && part && typeof part.text === "string") texts.push(part.text)
      else if (typeof event.text === "string") texts.push(event.text)
    } catch {}
  }
  return texts.length > 0 ? texts.join("\n").trim() : trimmed
}

export async function askEngine(
  engine: EngineDef,
  prompt: string,
  cwd: string,
  mode: EngineMode = "read",
): Promise<EngineReply> {
  const run = await runEngineRaw(engine, prompt, {}, cwd, RUN_CONFIG, mode)
  return {
    engine: engine.id,
    ok: run.ok,
    text: run.ok ? cleanText(run.raw) : (run.error ?? "failed"),
    durationMs: run.durationMs,
    ...(run.rateLimited ? { rateLimited: true } : {}),
  }
}

export async function convene(opts: {
  question: string
  context?: string
  engines?: string[]
  size?: number
  cwd: string
}): Promise<{ replies: EngineReply[]; skipped: string[] }> {
  const available = await healthyEngines(opts.engines)
  const size = opts.size && opts.size > 0 ? opts.size : available.length
  const seated = available.slice(0, size)
  const skipped = ENGINES.filter((engine) => !seated.some((chosen) => chosen.id === engine.id)).map(
    (engine) => engine.id,
  )
  if (seated.length === 0) return { replies: [], skipped }
  const prompt = ASK_PROMPT(opts.question, opts.context)
  const replies = await Promise.all(seated.map((engine) => askEngine(engine, prompt, opts.cwd)))
  return { replies, skipped }
}

export function askPrompt(question: string, context?: string): string {
  return ASK_PROMPT(question, context)
}

export function taskPrompt(task: string, context: string | undefined, write: boolean): string {
  return TASK_PROMPT(task, context, write)
}

export interface Job {
  id: string
  kind: "council" | "task"
  status: "running" | "done" | "failed"
  question: string
  engines: string[]
  startedAt: number
  finishedAt?: number
  replies?: EngineReply[]
  error?: string
}

function jobsDir(): string {
  return (
    process.env.KYORA_COUNCIL_STATE_DIR ??
    join(process.env.XDG_STATE_HOME ?? join(homedir(), ".local", "state"), "kyora-council")
  )
}

export function saveJob(job: Job): void {
  try {
    mkdirSync(jobsDir(), { recursive: true })
    writeFileSync(join(jobsDir(), `${job.id}.json`), JSON.stringify(job, null, 2))
  } catch {}
}

export function loadJob(id: string): Job | null {
  try {
    return JSON.parse(readFileSync(join(jobsDir(), `${id.replace(/[^\w-]/g, "")}.json`), "utf8")) as Job
  } catch {
    return null
  }
}

export function newJobId(kind: string, seed: number): string {
  return `${kind}-${seed.toString(36)}`
}

/** Points where replies disagree matter more than where they agree. */
export function summarizeReplies(replies: EngineReply[]): string {
  const ok = replies.filter((reply) => reply.ok)
  const failed = replies.filter((reply) => !reply.ok)
  const lines = [
    `${ok.length} of ${replies.length} council members responded${
      failed.length > 0 ? ` (${failed.map((reply) => `${reply.engine}: ${reply.text.slice(0, 80)}`).join("; ")})` : ""
    }.`,
    "",
    "Read the takes below as independent opinions, not votes to average. Where they agree, confidence is high. Where they disagree, that disagreement is the signal — resolve it against the code before acting.",
  ]
  for (const reply of ok) {
    lines.push("", `### ${reply.engine} (${Math.round(reply.durationMs / 1000)}s)`, "", reply.text)
  }
  return lines.join("\n")
}
