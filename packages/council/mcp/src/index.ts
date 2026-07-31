import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js"
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js"
import { z } from "zod"
import { engineById } from "@kyora-sh/review/engines"
import {
  askEngine,
  askPrompt,
  convene,
  councilStatus,
  engineCatalog,
  fanout,
  healthyEngines,
  loadJob,
  newJobId,
  pickEngine,
  saveJob,
  summarizeReplies,
  taskPrompt,
  type Job,
} from "./council"

const server = new McpServer({ name: "kyora-council", version: "0.1.0" })

const text = (body: string) => ({ content: [{ type: "text" as const, text: body }] })

function cwdOf(dir?: string): string {
  return dir ?? process.env.KYORA_COUNCIL_CWD ?? process.cwd()
}

server.tool(
  "council_convene",
  "Ask several other model families the same question at once and get their independent takes. Use before high-stakes or irreversible decisions: architecture choices, security-sensitive changes, tricky debugging conclusions, or when you are about to commit to an approach you cannot cheaply undo.",
  {
    question: z.string().describe("the decision or question, stated so a model with no prior context can judge it"),
    context: z.string().optional().describe("your current reasoning, constraints, and what you are about to do"),
    engines: z.array(z.string()).optional().describe("engine ids to seat (default: all with quota)"),
    size: z.number().optional().describe("cap the council to N members, highest remaining quota first"),
    effort: z.string().optional().describe("reasoning effort for members that support it"),
    cwd: z.string().optional().describe("repository path the council should reason inside"),
  },
  async ({ question, context, engines, size, effort, cwd }) => {
    const result = await convene({ question, context, engines, size, effort, cwd: cwdOf(cwd) })
    if (result.replies.length === 0) {
      return text(
        `No council member could be seated — every engine is unavailable, cooling down, or out of quota. Skipped: ${result.skipped.join(", ")}. Proceed on your own judgment, and say so.`,
      )
    }
    return text(summarizeReplies(result.replies))
  },
)

server.tool(
  "council_convene_async",
  "Start a council in the background and return a job id immediately. Use when you want other model families deliberating while you keep working; collect the verdict later with council_result.",
  {
    question: z.string(),
    context: z.string().optional(),
    engines: z.array(z.string()).optional(),
    size: z.number().optional(),
    cwd: z.string().optional(),
  },
  async ({ question, context, engines, size, cwd }) => {
    const started = Date.now()
    const id = newJobId("council", started)
    const seated = await healthyEngines(engines)
    const job: Job = {
      id,
      kind: "council",
      status: "running",
      question,
      engines: seated.slice(0, size && size > 0 ? size : seated.length).map((engine) => engine.id),
      startedAt: started,
    }
    saveJob(job)
    void convene({ question, context, engines, size, cwd: cwdOf(cwd) })
      .then((result) =>
        saveJob({ ...job, status: "done", finishedAt: Date.now(), replies: result.replies }),
      )
      .catch((error: unknown) =>
        saveJob({ ...job, status: "failed", finishedAt: Date.now(), error: String(error) }),
      )
    return text(
      job.engines.length > 0
        ? `Council ${id} convened in the background with ${job.engines.join(", ")}. Collect it with council_result when you reach a natural checkpoint.`
        : `Council ${id} could not seat any member (no engine has quota right now).`,
    )
  },
)

server.tool(
  "council_result",
  "Collect the verdict of a background council or delegated task by job id.",
  { job_id: z.string() },
  async ({ job_id }) => {
    const job = loadJob(job_id)
    if (!job) return text(`No job ${job_id} found.`)
    if (job.status === "running") {
      return text(`Job ${job_id} is still running (${Math.round((Date.now() - job.startedAt) / 1000)}s so far).`)
    }
    if (job.status === "failed") return text(`Job ${job_id} failed: ${job.error}`)
    return text(summarizeReplies(job.replies ?? []))
  },
)

server.tool(
  "council_ask",
  "Ask one specific model family for its take, read-only. Use when a particular perspective is what you want — a second opinion from a different lineage than your own.",
  {
    engine: z.string().describe("engine id: codex, claude, kimi, glm, grok, qwen"),
    question: z.string(),
    context: z.string().optional(),
    model: z.string().optional().describe("specific model for that family (see council_models)"),
    effort: z.string().optional().describe("reasoning effort where supported"),
    cwd: z.string().optional(),
  },
  async ({ engine, question, context, model, effort, cwd }) => {
    const def = engineById(engine)
    if (!def) return text(`Unknown engine "${engine}". Run council_status to see who is available.`)
    const seated = await healthyEngines([engine])
    if (seated.length === 0) return text(`${engine} is unavailable, cooling down, or out of quota right now.`)
    const reply = await askEngine(seated[0]!, askPrompt(question, context), cwdOf(cwd), {
      ...(model ? { model } : {}),
      ...(effort ? { effort } : {}),
    })
    return text(reply.ok ? reply.text : `${engine} failed: ${reply.text}`)
  },
)

server.tool(
  "council_task",
  "Delegate a concrete piece of work to one model family. Read-only by default; pass write:true to let it edit files in the repository. Use to parallelize work or to hand a task to a lineage better suited to it.",
  {
    engine: z.string(),
    task: z.string().describe("what to do, stated completely — the delegate has none of your conversation"),
    context: z.string().optional(),
    model: z.string().optional(),
    effort: z.string().optional(),
    write: z.boolean().optional().describe("allow file edits (default false)"),
    background: z.boolean().optional().describe("return a job id immediately instead of waiting"),
    cwd: z.string().optional(),
  },
  async ({ engine, task, context, model, effort, write, background, cwd }) => {
    const mode = write ? ("write" as const) : ("chat" as const)
    const seated = await healthyEngines([engine], mode)
    if (seated.length === 0) {
      const def = engineById(engine)
      if (def && mode === "write" && !def.argsWrite) return text(`${engine} has no write-capable invocation.`)
      return text(`${engine} is unavailable, cooling down, or out of quota right now.`)
    }
    const prompt = taskPrompt(task, context, Boolean(write))
    const runOptions = { mode, ...(model ? { model } : {}), ...(effort ? { effort } : {}) }
    if (!background) {
      const reply = await askEngine(seated[0]!, prompt, cwdOf(cwd), runOptions)
      return text(reply.ok ? reply.text : `${engine} failed: ${reply.text}`)
    }
    const started = Date.now()
    const id = newJobId("task", started)
    const job: Job = { id, kind: "task", status: "running", question: task, engines: [engine], startedAt: started }
    saveJob(job)
    void askEngine(seated[0]!, prompt, cwdOf(cwd), runOptions)
      .then((reply) => saveJob({ ...job, status: "done", finishedAt: Date.now(), replies: [reply] }))
      .catch((error: unknown) => saveJob({ ...job, status: "failed", finishedAt: Date.now(), error: String(error) }))
    return text(`Task ${id} delegated to ${engine} in the background. Collect it with council_result.`)
  },
)

server.tool(
  "agent_spawn",
  "Spawn a subagent from another model family to do a piece of work for you — like your own subagents, but a different lineage, on a separate subscription. You do not have to pick who: the least-spent capable family is chosen automatically. Read-only unless write is set. Use for independent workstreams, second implementations, or work better suited to another family.",
  {
    task: z.string().describe("what to do, stated completely — the subagent has none of your conversation"),
    context: z.string().optional().describe("background it needs: constraints, prior decisions, file pointers"),
    prefer: z.string().optional().describe("preferred engine id; ignored if that family has no quota"),
    model: z.string().optional().describe("specific model for that family (see council_models)"),
    effort: z.string().optional().describe("reasoning effort where supported: low, medium, high, max"),
    write: z.boolean().optional().describe("allow file edits (default false)"),
    background: z.boolean().optional().describe("return a job id immediately instead of waiting"),
    cwd: z.string().optional(),
  },
  async ({ task, context, prefer, model, effort, write, background, cwd }) => {
    const mode = write ? ("write" as const) : ("chat" as const)
    const engine = await pickEngine(prefer, mode)
    if (!engine) {
      return text(
        `No model family can be spawned right now — all are unavailable, cooling down, or out of quota${
          mode === "write" ? " (write-capable)" : ""
        }. Do the work yourself, or retry later.`,
      )
    }
    const prompt = taskPrompt(task, context, Boolean(write))
    const runOptions = { mode, ...(model ? { model } : {}), ...(effort ? { effort } : {}) }
    const tag = model ? `${engine.id}/${model}` : engine.id
    if (!background) {
      const reply = await askEngine(engine, prompt, cwdOf(cwd), runOptions)
      return text(reply.ok ? `[${tag}]\n\n${reply.text}` : `${tag} failed: ${reply.text}`)
    }
    const started = Date.now()
    const id = newJobId("agent", started)
    const job: Job = { id, kind: "task", status: "running", question: task, engines: [engine.id], startedAt: started }
    saveJob(job)
    void askEngine(engine, prompt, cwdOf(cwd), runOptions)
      .then((reply) => saveJob({ ...job, status: "done", finishedAt: Date.now(), replies: [reply] }))
      .catch((error: unknown) => saveJob({ ...job, status: "failed", finishedAt: Date.now(), error: String(error) }))
    return text(`Spawned ${tag} as subagent ${id} in the background. Collect it with council_result.`)
  },
)

server.tool(
  "agent_fanout",
  "Run several DIFFERENT tasks in parallel, each on a different model family. Use to parallelize independent workstreams across subscriptions — not to ask one question many ways (that is council_convene).",
  {
    tasks: z
      .array(
        z.object({
          task: z.string(),
          engine: z.string().optional().describe("pin this task to a family; otherwise assigned automatically"),
          context: z.string().optional(),
          model: z.string().optional(),
          effort: z.string().optional(),
        }),
      )
      .describe("independent tasks; each is handled by its own subagent"),
    write: z.boolean().optional().describe("allow file edits — only for tasks that touch disjoint files"),
    cwd: z.string().optional(),
  },
  async ({ tasks, write, cwd }) => {
    if (tasks.length === 0) return text("No tasks given.")
    const { results, unassigned } = await fanout({ tasks, write, cwd: cwdOf(cwd) })
    if (results.length === 0) return text("No model family had quota to take these tasks.")
    const blocks = results.map(
      (result) =>
        `### ${result.engine} — ${result.task.slice(0, 120)}\n\n${result.ok ? result.text : `failed: ${result.text}`}`,
    )
    if (unassigned.length > 0) {
      blocks.push(`### unassigned\n\n${unassigned.length} task(s) pinned to a family with no quota: ${unassigned.join("; ")}`)
    }
    return text(blocks.join("\n\n---\n\n"))
  },
)

server.tool(
  "council_models",
  "List the model families that can be summoned and the specific models and reasoning efforts each one accepts. Check this before passing model or effort to agent_spawn, council_ask, or council_task.",
  {},
  async () => {
    const lines = engineCatalog().map((entry) => {
      const models = entry.models.length > 0 ? entry.models.join(", ") : "engine default only"
      const effort = entry.supportsEffort ? " · effort: low|medium|high|max" : ""
      const write = entry.writeCapable ? "" : " · read-only"
      return `${entry.available ? "✓" : "✗"} ${entry.id.padEnd(6)} ${models}${effort}${write}`
    })
    return text(
      [
        ...lines,
        "",
        "First model listed is the default. Models are passed straight to the vendor CLI, so a newer id it accepts will work even if it is not listed here.",
      ].join("\n"),
    )
  },
)

server.tool(
  "council_status",
  "Show which model families can be summoned right now, with remaining subscription quota where the vendor exposes it.",
  {},
  async () => {
    const health = await councilStatus()
    const lines = health.map((engine) => {
      const quota = engine.remainingPct === null ? "" : ` · ${engine.remainingPct}% quota left`
      const cooling = engine.cooldownMinutes > 0 ? ` · cooling down ${engine.cooldownMinutes}m` : ""
      const write = engine.writeCapable ? "" : " · read-only"
      return `${engine.available ? "✓" : "✗"} ${engine.id.padEnd(6)} ${engine.available ? "ready" : engine.reason}${quota}${cooling}${write}`
    })
    return text(lines.join("\n"))
  },
)

await server.connect(new StdioServerTransport())
