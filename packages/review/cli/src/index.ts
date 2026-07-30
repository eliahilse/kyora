import { parseArgs } from "node:util"
import { ciCoveredCommands } from "./ci"
import { buildContext, repoRoot } from "./diff"
import { ENGINES, engineById, engineStatus, runEngineRaw, type EngineDef } from "./engines"
import { extractFindings, extractPayload } from "./extract"
import { detectRepo, fetchPr, getToken, postReview } from "./github"
import { mergeFindings } from "./merge"
import { renderReport } from "./report"
import { FINDINGS_SCHEMA, reviewPrompt } from "./schema"
import { SEVERITIES, SEVERITY_RANK, type EngineRun, type PrInfo, type ReviewConfig, type Severity } from "./types"
import { cooldownRemainingMs, lastRunAt, loadUsage } from "./usage"
import { verifySingles } from "./verify"

const DEFAULTS: ReviewConfig = {
  engines: ["auto"],
  verify: false,
  post: false,
  base: "main",
  failOn: "none",
  maxDiffBytes: 100_000,
  timeoutMs: 900_000,
  maxFindingsPerEngine: 20,
  cooldownMinutes: 60,
  maxEngines: 0,
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
  kyora-review usage                show per-engine quota state and cooldowns

options:
  --pr <number>        review a GitHub PR (resolves base, enables --post)
  --base <ref>         base ref for local mode (default: main)
  --engines <ids>      comma-separated: codex,claude,kimi,glm,grok,qwen (default: all available)
  --verify             cross-examine single-engine findings with another engine
  --post               submit results as a PR review (requires --pr + GITHUB_TOKEN or gh)
  --fail-on <sev>      exit 1 if findings at/above severity (critical|major|minor|nit)
  --max-engines <n>    run only the n least-recently-used healthy engines (spread quota)
  --ignore-quota       run engines even while they are cooling down after a rate limit
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
  "max-engines"?: string
  "ignore-quota"?: boolean
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
    ...(flags["max-engines"] ? { maxEngines: parseInt(flags["max-engines"], 10) || 0 } : {}),
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

  let selected = selectEngines(config)
  if (selected.length === 0) {
    log("no review engines available on this machine — run `kyora-review doctor` to see how to enable them")
    return
  }

  const usage = loadUsage()
  if (!flags["ignore-quota"]) {
    for (const engine of selected) {
      const remaining = cooldownRemainingMs(engine.id, usage)
      if (remaining > 0) log(`${engine.id}: cooling down after a rate limit (${Math.ceil(remaining / 60_000)}m left) — skipped`)
    }
    selected = selected.filter((engine) => cooldownRemainingMs(engine.id, usage) === 0)
    if (selected.length === 0) {
      log("every available engine is cooling down — nothing launched (use --ignore-quota to force)")
      return
    }
  }
  const probed = await Promise.all(
    selected.map(async (engine) => ({ engine, live: engine.usageProbe ? await engine.usageProbe() : null })),
  )
  for (const { engine, live } of probed) {
    if (live) log(`${engine.id}: live quota ${live.remainingPct}% — ${live.detail}`)
  }
  if (!flags["ignore-quota"]) {
    for (const { engine, live } of probed) {
      if (live !== null && live.remainingPct <= 0) log(`${engine.id}: out of quota per live probe — skipped`)
    }
    selected = probed.filter(({ live }) => live === null || live.remainingPct > 0).map(({ engine }) => engine)
    if (selected.length === 0) {
      log("every available engine is out of quota — nothing launched (use --ignore-quota to force)")
      return
    }
  }
  if (config.maxEngines > 0 && selected.length > config.maxEngines) {
    selected = [...selected]
      .sort((a, b) => lastRunAt(a.id, usage) - lastRunAt(b.id, usage))
      .slice(0, config.maxEngines)
    log(`quota rotation: least-recently-used ${config.maxEngines} of the panel — ${selected.map((engine) => engine.id).join(", ")}`)
  }
  log(`reviewing ${ctx.changedFiles.length} changed file(s) with: ${selected.map((engine) => engine.id).join(", ")}`)

  const ciCovered = await ciCoveredCommands(root)
  if (ciCovered.length > 0) log(`execution policy: ${ciCovered.length} CI-covered command(s) off-limits to engines`)
  const prompt = reviewPrompt(ctx, config.maxFindingsPerEngine, ciCovered)
  const runs: EngineRun[] = await Promise.all(
    selected.map(async (engine): Promise<EngineRun> => {
      log(`${engine.id}: starting`)
      const raw = await runEngineRaw(engine, prompt, FINDINGS_SCHEMA, root, config)
      if (raw.rateLimited) log(`${engine.id}: hit a usage limit — cooling down for future runs`)
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

  if (runs.every((run) => !run.ok)) {
    log("every engine failed — refusing to report a clean review")
    for (const run of runs) log(`  ${run.engine}: ${run.error}`)
    process.exitCode = 2
    return
  }

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

async function usageReport(): Promise<void> {
  const state = loadUsage()
  console.log("kyora-review usage\n")
  for (const engine of ENGINES) {
    const entry = state.engines[engine.id]
    const live = engine.usageProbe ? await engine.usageProbe() : null
    const liveNote = live ? `   live: ${live.remainingPct}% (${live.detail})` : ""
    if (!entry?.lastRun) {
      console.log(`${engine.id.padEnd(8)} never run${liveNote}`)
      continue
    }
    const ago = Math.round((Date.now() - entry.lastRun) / 60_000)
    const cooldown = cooldownRemainingMs(engine.id, state)
    const status = cooldown > 0 ? `cooling down, ${Math.ceil(cooldown / 60_000)}m left` : (entry.lastOutcome ?? "ok")
    console.log(`${engine.id.padEnd(8)} ${String(entry.runs ?? 0).padStart(3)} run(s)   last: ${ago}m ago   ${status}${liveNote}`)
  }
}

async function doctor(): Promise<void> {
  const root = await repoRoot()
  const fileConfig = root ? await loadConfig(root) : {}
  console.log("kyora-review doctor\n")
  for (const engine of ENGINES) {
    const status = engineStatus(engine, fileConfig.overrides?.[engine.id])
    const mark = status.available ? "✓" : "✗"
    const cooldown = cooldownRemainingMs(engine.id)
    const quotaNote = cooldown > 0 ? ` (cooling down, ${Math.ceil(cooldown / 60_000)}m left)` : ""
    console.log(`${mark} ${engine.id.padEnd(8)} ${engine.label}${quotaNote}`)
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
      "max-engines": { type: "string" },
      "ignore-quota": { type: "boolean" },
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
  if (command === "usage") return usageReport()
  if (command === "review") return review(flags)
  die(`unknown command "${command}" — try: review, doctor, help`)
}

main()
