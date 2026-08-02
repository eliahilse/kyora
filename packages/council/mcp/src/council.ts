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
  type RunOptions,
} from "@kyora-sh/review/engines"
import { configForCwd } from "@kyora-sh/review/config"
import { cooldownRemainingMs, lastRunAt, loadUsage } from "@kyora-sh/review/usage"
import type { RunConfig } from "@kyora-sh/review/types"
import { assignTasks, type DelegatedTask } from "./assign"

export type { DelegatedTask }

export const DEFAULT_TIMEOUT_MS = 600_000
export const DEFAULT_COOLDOWN_MINUTES = 60

/** An explicit env override beats the checked-in config; both beat the default. */
function envTimeoutMs(): number | undefined {
  const raw = Number(process.env.KYORA_COUNCIL_TIMEOUT_MS)
  return Number.isFinite(raw) && raw > 0 ? raw : undefined
}

export function cwdOf(dir?: string): string {
  return dir ?? process.env.KYORA_COUNCIL_CWD ?? process.cwd()
}

const configs = new Map<string, Promise<RunConfig>>()

/**
 * The council spawns the same vendor CLIs as `kyora-review`, so it reads the
 * same `kyora-review.config.json`: a `bin`/`args`/`env` override has to mean the
 * same thing to both, or fixing a vendor flag change fixes only half the repo.
 * Resolved once per checkout — config does not change under a running server.
 */
export function runConfig(cwd: string): Promise<RunConfig> {
  let pending = configs.get(cwd)
  if (!pending) {
    pending = configForCwd(cwd).then(({ config }) => ({
      timeoutMs: envTimeoutMs() ?? config.timeoutMs ?? DEFAULT_TIMEOUT_MS,
      cooldownMinutes: config.cooldownMinutes ?? DEFAULT_COOLDOWN_MINUTES,
      overrides: config.overrides ?? {},
    }))
    configs.set(cwd, pending)
  }
  return pending
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

export interface EngineCatalogEntry {
  id: string
  label: string
  available: boolean
  models: string[]
  defaultModel: string | null
  supportsEffort: boolean
  writeCapable: boolean
}

export async function engineCatalog(cwd: string): Promise<EngineCatalogEntry[]> {
  const config = await runConfig(cwd)
  return ENGINES.map((engine) => ({
    id: engine.id,
    label: engine.label,
    available: engineStatus(engine, config.overrides[engine.id]).available,
    models: engine.models ?? [],
    defaultModel: engine.models?.[0] ?? null,
    supportsEffort: Boolean(engine.effortArgs),
    writeCapable: Boolean(engine.argsWrite),
  }))
}

export async function councilStatus(cwd: string): Promise<EngineHealth[]> {
  const usage = loadUsage()
  const config = await runConfig(cwd)
  return Promise.all(
    ENGINES.map(async (engine) => {
      const status = engineStatus(engine, config.overrides[engine.id])
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
export async function healthyEngines(
  requested: string[] | undefined,
  cwd: string,
  mode: EngineMode = "chat",
): Promise<EngineDef[]> {
  const usage = loadUsage()
  const config = await runConfig(cwd)
  const pool = requested?.length
    ? requested.map((id) => engineById(id.trim())).filter((engine): engine is EngineDef => Boolean(engine))
    : ENGINES
  const checked = await Promise.all(
    pool.map(async (engine) => {
      if (!engineStatus(engine, config.overrides[engine.id]).available) return null
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
  `You are consulted as an independent expert, seated alongside models from other families. Give your own honest assessment — do not defer to the framing of the question, and say so plainly if you think the premise is wrong.

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
  model?: string
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
  options: RunOptions = {},
): Promise<EngineReply> {
  const run = await runEngineRaw(engine, prompt, {}, cwd, await runConfig(cwd), { mode: "chat", ...options })
  return {
    engine: engine.id,
    ...(options.model ? { model: options.model } : {}),
    ok: run.ok,
    text: run.ok ? cleanText(run.raw) : (run.error ?? "failed"),
    durationMs: run.durationMs,
    ...(run.rateLimited ? { rateLimited: true } : {}),
  }
}

export interface Seating {
  seated: EngineDef[]
  skipped: string[]
}

/**
 * Seating is a separate step from asking so the async path can report who was
 * actually seated without probing live quota a second time — two independent
 * selections could name one set of engines and run another.
 */
export async function seatCouncil(opts: { engines?: string[]; size?: number; cwd: string }): Promise<Seating> {
  const available = await healthyEngines(opts.engines, opts.cwd)
  const size = opts.size && opts.size > 0 ? opts.size : available.length
  const seated = available.slice(0, size)
  return {
    seated,
    skipped: ENGINES.filter((engine) => !seated.some((chosen) => chosen.id === engine.id)).map((engine) => engine.id),
  }
}

export async function convene(opts: {
  question: string
  context?: string
  seated: EngineDef[]
  cwd: string
  effort?: string
}): Promise<EngineReply[]> {
  const prompt = ASK_PROMPT(opts.question, opts.context)
  return Promise.all(
    opts.seated.map((engine) =>
      askEngine(engine, prompt, opts.cwd, { mode: "chat", ...(opts.effort ? { effort: opts.effort } : {}) }),
    ),
  )
}

export function askPrompt(question: string, context?: string): string {
  return ASK_PROMPT(question, context)
}

/**
 * Pick a delegate without the caller naming one: honor a preference when that
 * engine is actually spendable, otherwise take whoever has the most headroom.
 */
export async function pickEngine(prefer: string | undefined, mode: EngineMode, cwd: string): Promise<EngineDef | null> {
  if (prefer) {
    const preferred = await healthyEngines([prefer], cwd, mode)
    if (preferred.length > 0) return preferred[0]!
  }
  const pool = await healthyEngines(undefined, cwd, mode)
  return pool[0] ?? null
}

export interface FanoutResult extends EngineReply {
  task: string
}

/**
 * Distinct tasks in parallel across distinct families — a worker pool rather
 * than a council. Each task goes to a different engine where supply allows, so
 * one subscription does not absorb the whole batch.
 */
export async function fanout(opts: {
  tasks: DelegatedTask[]
  write?: boolean
  cwd: string
}): Promise<{ results: FanoutResult[]; unassigned: string[]; serialized: boolean }> {
  const mode: EngineMode = opts.write ? "write" : "chat"
  const pool = await healthyEngines(undefined, opts.cwd, mode)
  const { assignments, unassigned } = assignTasks(
    opts.tasks,
    pool.map((engine) => engine.id),
  )
  const run = ({ task, engine }: (typeof assignments)[number]) =>
    askEngine(
      pool.find((candidate) => candidate.id === engine)!,
      TASK_PROMPT(task.task, task.context, Boolean(opts.write)),
      opts.cwd,
      { mode, ...(task.model ? { model: task.model } : {}), ...(task.effort ? { effort: task.effort } : {}) },
    ).then((reply) => ({ task: task.task, ...reply }))

  // Read-only delegates cannot interfere, so they run together. Write-capable
  // ones share a single working tree with no locking or isolation between them,
  // so a parallel fanout is a silent clobber waiting to happen: run them in turn
  // until each task can be given its own checkout.
  if (!opts.write) return { results: await Promise.all(assignments.map(run)), unassigned, serialized: false }
  const results: FanoutResult[] = []
  for (const assignment of assignments) results.push(await run(assignment))
  return { results, unassigned, serialized: assignments.length > 1 }
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
  /** past this, a job still marked running cannot be in flight — see `loadJob` */
  deadlineAt: number
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
    const job = JSON.parse(readFileSync(join(jobsDir(), `${id.replace(/[^\w-]/g, "")}.json`), "utf8")) as Job
    return reconcile(job)
  } catch {
    return null
  }
}

/**
 * A background job is a promise owned by this process, not a durable unit of
 * work: if the server is restarted or killed mid-flight, nothing will ever move
 * the record off "running". Past its deadline, say so instead of reporting a
 * job that no longer exists as still in progress.
 */
export function reconcile(job: Job): Job {
  if (job.status !== "running" || !job.deadlineAt || Date.now() <= job.deadlineAt) return job
  return {
    ...job,
    status: "failed",
    error: "no result was recorded before the deadline — the server was most likely restarted mid-run; re-run it",
  }
}

let sequence = 0

/** Ids are minted per start; a bare timestamp collides when two jobs begin in the same millisecond. */
export function newJobId(kind: string, seed: number = Date.now()): string {
  return `${kind}-${seed.toString(36)}-${(sequence++).toString(36)}`
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
