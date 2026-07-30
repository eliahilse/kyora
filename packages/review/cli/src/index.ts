import { parseArgs } from "node:util"
import { buildContext, repoRoot } from "./diff"
import { ENGINES, engineById, engineStatus, runEngineRaw, type EngineDef } from "./engines"
import { extractFindings, extractPayload } from "./extract"
import { detectRepo, fetchPr, getToken, postReview } from "./github"
import { mergeFindings } from "./merge"
import { renderReport } from "./report"
import { FINDINGS_SCHEMA, reviewPrompt } from "./schema"
import { SEVERITIES, SEVERITY_RANK, type EngineRun, type PrInfo, type ReviewConfig, type Severity } from "./types"
import { verifySingles } from "./verify"

const DEFAULTS: ReviewConfig = {
  engines: ["auto"],
  verify: false,
  post: false,
  base: "main",
  failOn: "none",
  maxDiffBytes: 300_000,
  timeoutMs: 900_000,
  maxFindingsPerEngine: 20,
  overrides: {},
}

const log = (message: string) => console.error(`[kyora-review] ${message}`)

function die(message: string): never {
  log(message)
  process.exit(2)
}

const HELP = `kyora-review — multi-engine AI code review on your own subscriptions

usage:
  kyora-review [review] [options]   review the working branch (diff vs --base)
  kyora-review doctor               show engine availability and auth hints

options:
  --pr <number>        review a GitHub PR (resolves base, enables --post)
  --base <ref>         base ref for local mode (default: main)
  --engines <ids>      comma-separated: codex,claude,kimi,grok,qwen (default: all available)
  --verify             cross-examine single-engine findings with another engine
  --post               submit results as a PR review (requires --pr + GITHUB_TOKEN or gh)
  --fail-on <sev>      exit 1 if findings at/above severity (critical|major|minor|nit)
  --out <file>         also write the markdown report to a file
  --json               print machine-readable JSON instead of markdown
  -h, --help           this help

config: kyora-review.config.json at the repo root (same keys, plus per-engine overrides).`

async function loadConfig(root: string): Promise<Partial<ReviewConfig>> {
  const file = Bun.file(`${root}/kyora-review.config.json`)
  if (!(await file.exists())) return {}
  try {
    return (await file.json()) as Partial<ReviewConfig>
  } catch {
    die(`kyora-review.config.json exists but is not valid JSON`)
  }
}

interface Flags {
  pr?: string
  base?: string
  engines?: string
  verify?: boolean
  post?: boolean
  "fail-on"?: string
  out?: string
  json?: boolean
  help?: boolean
}

function selectEngines(config: ReviewConfig): EngineDef[] {
  const statuses = ENGINES.map((engine) => engineStatus(engine, config.overrides[engine.id]))
  const auto = config.engines.length === 1 && config.engines[0] === "auto"
  const requested = auto
    ? statuses.map((status) => status.engine)
    : config.engines.map((id) => engineById(id.trim()) ?? die(`unknown engine "${id}" — valid: ${ENGINES.map((engine) => engine.id).join(", ")}`))

  const selected: EngineDef[] = []
  for (const engine of requested) {
    const status = statuses.find((candidate) => candidate.engine.id === engine.id)!
    if (status.available) selected.push(engine)
    else if (!auto) log(`skipping ${engine.id}: ${status.reason} (${engine.authHint})`)
  }
  return selected
}

async function review(flags: Flags): Promise<void> {
  const root = (await repoRoot()) ?? die("not inside a git repository")
  const fileConfig = await loadConfig(root)
  const config: ReviewConfig = {
    ...DEFAULTS,
    ...fileConfig,
    ...(flags.base ? { base: flags.base } : {}),
    ...(flags.engines ? { engines: flags.engines.split(",") } : {}),
    ...(flags.verify ? { verify: true } : {}),
    ...(flags.post ? { post: true } : {}),
    ...(flags["fail-on"] ? { failOn: flags["fail-on"] as Severity } : {}),
    overrides: { ...fileConfig.overrides },
  }
  if (config.failOn !== "none" && !(SEVERITIES as string[]).includes(config.failOn)) {
    die(`--fail-on must be one of: ${SEVERITIES.join(", ")}, none`)
  }

  let pr: PrInfo | undefined
  let token: string | null = null
  if (flags.pr) {
    const number = parseInt(flags.pr, 10)
    if (!Number.isInteger(number) || number <= 0) die(`--pr expects a positive number`)
    token = (await getToken()) ?? die("--pr needs a GitHub token (GITHUB_TOKEN, GH_TOKEN, or `gh auth login`)")
    const repo = (await detectRepo()) ?? die("cannot detect GitHub repo (set GITHUB_REPOSITORY or add an origin remote)")
    const fetched = await fetchPr(repo.owner, repo.repo, number, token)
    if ("error" in fetched) die(`cannot load PR #${number}: ${fetched.error}`)
    pr = fetched
  } else if (config.post) {
    log("--post requires --pr; running locally without posting")
    config.post = false
  }

  const ctx = await buildContext({ root, base: config.base, maxDiffBytes: config.maxDiffBytes, ...(pr ? { pr } : {}) })
  if ("error" in ctx) die(ctx.error)
  if (!ctx.diff.trim()) {
    log("no changes between base and HEAD — nothing to review")
    return
  }

  const selected = selectEngines(config)
  if (selected.length === 0) {
    log("no review engines available on this machine — run `kyora-review doctor` to see how to enable them")
    return
  }
  log(`reviewing ${ctx.changedFiles.length} changed file(s) with: ${selected.map((engine) => engine.id).join(", ")}`)

  const prompt = reviewPrompt(ctx, config.maxFindingsPerEngine)
  const runs: EngineRun[] = await Promise.all(
    selected.map(async (engine): Promise<EngineRun> => {
      log(`${engine.id}: starting`)
      const raw = await runEngineRaw(engine, prompt, FINDINGS_SCHEMA, root, config)
      if (!raw.ok) {
        log(`${engine.id}: failed (${raw.error})`)
        return { engine: engine.id, ok: false, findings: [], error: raw.error ?? "failed", durationMs: raw.durationMs }
      }
      if (extractPayload(raw.raw, "findings") === null) {
        log(`${engine.id}: returned no parseable findings payload`)
        return { engine: engine.id, ok: false, findings: [], error: "no parseable findings payload", durationMs: raw.durationMs }
      }
      const findings = extractFindings(raw.raw, engine.id, config.maxFindingsPerEngine)
      log(`${engine.id}: done — ${findings.length} finding(s) in ${Math.round(raw.durationMs / 1000)}s`)
      return { engine: engine.id, ok: true, findings, durationMs: raw.durationMs }
    }),
  )

  let merged = mergeFindings(runs.flatMap((run) => run.findings))
  if (config.verify && selected.length > 1 && merged.some((finding) => finding.tier === "single")) {
    merged = await verifySingles(merged, selected, ctx, config, log)
  }

  const report = renderReport(ctx, merged, runs)
  if (flags.json) {
    console.log(JSON.stringify({ summary: { findings: merged.length }, findings: merged, runs }, null, 2))
  } else {
    console.log(report)
  }
  if (flags.out) await Bun.write(flags.out, report)

  if (config.post && pr && token) {
    const result = await postReview(ctx, merged, runs, token)
    if (result.posted) {
      log(`posted PR review (${result.inline} inline comment(s)${result.fallback ? ", body-only fallback" : ""})`)
    } else {
      log(`failed to post PR review: ${result.error}`)
      process.exitCode = 2
    }
  }

  if (config.failOn !== "none") {
    const threshold = SEVERITY_RANK[config.failOn as Severity]
    if (merged.some((finding) => SEVERITY_RANK[finding.severity] >= threshold)) process.exitCode = 1
  }
}

async function doctor(): Promise<void> {
  const root = await repoRoot()
  const fileConfig = root ? await loadConfig(root) : {}
  console.log("kyora-review doctor\n")
  for (const engine of ENGINES) {
    const status = engineStatus(engine, fileConfig.overrides?.[engine.id])
    const mark = status.available ? "✓" : "✗"
    console.log(`${mark} ${engine.id.padEnd(8)} ${engine.label}`)
    console.log(`    ${status.available ? "ready" : status.reason} — ${engine.authHint}`)
  }
  const available = ENGINES.filter((engine) => engineStatus(engine, fileConfig.overrides?.[engine.id]).available)
  console.log(
    available.length > 0
      ? `\ndefault run would use: ${available.map((engine) => engine.id).join(", ")}`
      : "\nno engines available — install/log in to at least one CLI above",
  )
}

async function main(): Promise<void> {
  const { values, positionals } = parseArgs({
    args: Bun.argv.slice(2),
    allowPositionals: true,
    options: {
      pr: { type: "string" },
      base: { type: "string" },
      engines: { type: "string" },
      verify: { type: "boolean" },
      post: { type: "boolean" },
      "fail-on": { type: "string" },
      out: { type: "string" },
      json: { type: "boolean" },
      help: { type: "boolean", short: "h" },
    },
  })
  const flags = values as Flags
  const command = positionals[0] ?? "review"
  if (flags.help || command === "help") {
    console.log(HELP)
    return
  }
  if (command === "doctor") return doctor()
  if (command === "review") return review(flags)
  die(`unknown command "${command}" — try: review, doctor, help`)
}

main()
