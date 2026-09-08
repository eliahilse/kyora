#!/usr/bin/env bun
import type { LiveUsage } from "@kyora-sh/usage"
import { parseArgs } from "node:util"
import { PROVIDERS, providerById, runningSessions } from "./providers"
import {
  assertName,
  listSlots,
  readSlot,
  readSnapshot,
  removeSlot,
  renameSlot,
  rootDir,
  slotDir,
  writeBackup,
  writeSnapshot,
} from "./store"
import { describeIdentity, isProviderId, type Provider, type ProviderId, type Snapshot } from "./types"

const HELP = `kyora-switch — hot-swap Claude Code and Codex logins

usage:
  kyora-switch claude save <slot>       store the Claude account you are logged into
  kyora-switch claude load <slot>       log Claude Code back into a stored account
  kyora-switch claude list              slots for Claude Code
  kyora-switch claude usage             how much quota each stored account has left
  kyora-switch claude rm <slot>         delete a slot
  kyora-switch claude rename <a> <b>    rename a slot

  kyora-switch codex save <slot>        same commands for Codex
  kyora-switch codex load <slot>

  kyora-switch status                   which account each CLI is logged into
  kyora-switch usage                    remaining quota across every stored account
  kyora-switch doctor                   where each CLI keeps its auth on this machine

options:
  --json         machine-readable output (list, status, usage)
  -y, --yes      skip the confirmation on rm
  -h, --help     this help

Slots live in ~/.kyora/switch/<provider> (override with KYORA_SWITCH_DIR). Every
load backs the outgoing login up first, so a switch made before saving is still
recoverable.`

function die(message: string): never {
  console.error(`[kyora-switch] ${message}`)
  process.exit(2)
}

const note = (message: string) => console.error(`[kyora-switch] ${message}`)

const pad = (id: string) => id.padEnd(7)

async function prompt(question: string): Promise<string> {
  process.stdout.write(question)
  for await (const line of console) return line.trim()
  return ""
}

async function save(provider: Provider, name: string): Promise<void> {
  const snapshot = await provider.capture()
  if (!snapshot) die(`${provider.label} is not logged in — ${provider.loginHint}`)

  await writeSnapshot(assertName(name), snapshot)
  console.log(`saved ${provider.id} "${name}" — ${describeIdentity(snapshot.identity)}`)
  console.log(`  ${slotDir(provider.id, name)}`)
}

async function load(provider: Provider, name: string): Promise<void> {
  const snapshot = (await readSnapshot(provider.id, assertName(name))) ?? die(`no ${provider.id} slot "${name}"`)

  const outgoing = await provider.capture()
  if (outgoing) {
    const backup = await writeBackup(outgoing)
    const slots = await listSlots(provider.id)
    const stored = slots.some((slot) => slot.identity.account === outgoing.identity.account)
    if (!stored) {
      note(`the login you just replaced (${describeIdentity(outgoing.identity)}) was in no slot — it is in ${backup}`)
    }
  }

  await provider.restore(snapshot)
  console.log(`${provider.id} → "${name}" — ${describeIdentity(snapshot.identity)}`)
  if ((await runningSessions(provider)) > 0) {
    note(`${provider.label} is running — restart it, the old token is already in memory`)
  }
}

async function list(provider: Provider, json: boolean): Promise<void> {
  const slots = await listSlots(provider.id)
  if (json) return console.log(JSON.stringify(slots, null, 2))
  if (slots.length === 0) {
    return console.log(`no ${provider.id} slots yet — run \`kyora-switch ${provider.id} save <slot>\` while logged in`)
  }

  const live = await provider.capture()
  for (const slot of slots) {
    const active = live && slot.identity.account === live.identity.account ? "  (active)" : ""
    console.log(`${pad(slot.name)} ${describeIdentity(slot.identity)}${active}`)
  }
}


function relative(timestamp: number): string {
  const ms = timestamp - Date.now()
  if (ms <= 0) return "now"
  const minutes = Math.round(ms / 60_000)
  if (minutes < 60) return `${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return minutes % 60 === 0 ? `${hours}h` : `${hours}h ${minutes % 60}m`
  return `${Math.round(hours / 24)}d`
}

function describeQuota(quota: LiveUsage): string {
  const windows = quota.windows ?? []
  if (windows.length === 0) return `${`${quota.remainingPct}% left`.padEnd(9)}  ${quota.detail}`
  const detail = windows
    .map((window) => `${window.label} ${window.usedPct}% used${window.resetsAt ? `, resets in ${relative(window.resetsAt)}` : ""}`)
    .join(" · ")
  return `${`${quota.remainingPct}% left`.padEnd(9)}  ${detail}`
}

interface UsageRow {
  provider: ProviderId
  slot: string | null
  account: string
  live: boolean
  quota: LiveUsage | null
  note?: string
}

async function usageRows(provider: Provider): Promise<UsageRow[]> {
  const active = await provider.capture()
  const slots = await listSlots(provider.id)
  const rows: UsageRow[] = []

  for (const slot of slots) {
    const snapshot = await readSnapshot(provider.id, slot.name)
    rows.push({
      provider: provider.id,
      slot: slot.name,
      account: describeIdentity(slot.identity),
      live: Boolean(active && active.identity.account === slot.identity.account),
      quota: snapshot && provider.quota ? await provider.quota(snapshot) : null,
    })
  }

  if (active && !rows.some((row) => row.live)) {
    rows.unshift({
      provider: provider.id,
      slot: null,
      account: describeIdentity(active.identity),
      live: true,
      quota: provider.quota ? await provider.quota(active) : null,
      note: "not saved to a slot",
    })
  }
  return rows
}

function renderUsage(provider: Provider, rows: UsageRow[]): void {
  if (rows.length === 0) {
    console.log(`${provider.id}: nothing logged in and no slots saved`)
    return
  }
  const width = Math.max(...rows.map((row) => (row.slot ?? "live").length))
  for (const row of rows) {
    const name = (row.slot ?? "live").padEnd(width)
    const marker = row.live ? "*" : " "
    const quota = row.quota
      ? describeQuota(row.quota)
      : (provider.quotaHint ?? "no quota reported — load the slot and start the CLI to refresh its token")
    console.log(`${marker} ${name}  ${row.account}`)
    console.log(`  ${" ".repeat(width)}  ${quota}`)
    if (row.note) console.log(`  ${" ".repeat(width)}  ${row.note}`)
  }
}

async function usage(providers: Provider[], json: boolean): Promise<void> {
  const collected = await Promise.all(providers.map(async (provider) => [provider, await usageRows(provider)] as const))
  if (json) return console.log(JSON.stringify(Object.fromEntries(collected.map(([p, rows]) => [p.id, rows])), null, 2))

  collected.forEach(([provider, rows], index) => {
    if (index > 0) console.log()
    if (providers.length > 1) console.log(`${provider.id} — ${provider.label}`)
    renderUsage(provider, rows)
  })
}

async function remove(provider: Provider, name: string, yes: boolean): Promise<void> {
  if (!(await readSlot(provider.id, assertName(name)))) die(`no ${provider.id} slot "${name}"`)
  if (!yes) {
    if (!process.stdin.isTTY) die(`refusing to delete ${provider.id} "${name}" without --yes`)
    if ((await prompt(`delete ${provider.id} slot "${name}"? [y/N] `)).toLowerCase() !== "y") {
      return console.log("cancelled")
    }
  }
  await removeSlot(provider.id, name)
  console.log(`deleted ${provider.id} "${name}"`)
}

async function rename(provider: Provider, from: string, to: string): Promise<void> {
  if (!(await readSlot(provider.id, assertName(from)))) die(`no ${provider.id} slot "${from}"`)
  if (await readSlot(provider.id, assertName(to))) die(`${provider.id} slot "${to}" already exists`)
  await renameSlot(provider.id, from, to)
  console.log(`renamed ${provider.id} "${from}" to "${to}"`)
}

async function status(json: boolean): Promise<void> {
  const rows = []
  for (const provider of PROVIDERS) {
    const live: Snapshot | null = await provider.capture()
    const slots = await listSlots(provider.id)
    const slot = slots.find((candidate) => live && candidate.identity.account === live.identity.account)
    rows.push({ provider: provider.id, identity: live?.identity ?? null, slot: slot?.name ?? null })
  }
  if (json) return console.log(JSON.stringify(rows, null, 2))

  for (const row of rows) {
    const provider = providerById(row.provider)
    if (!row.identity) {
      console.log(`${pad(row.provider)} not logged in — ${provider.loginHint}`)
      continue
    }
    console.log(`${pad(row.provider)} ${describeIdentity(row.identity)}  (${row.slot ? `slot "${row.slot}"` : "no slot"})`)
  }
}

async function doctor(): Promise<void> {
  console.log(`slots: ${rootDir()}`)
  for (const provider of PROVIDERS) {
    console.log(`\n${provider.id} — ${provider.label}`)
    for (const location of provider.locations()) console.log(`  ${location}`)

    const snapshot = await provider.capture().catch((error: Error) => error)
    if (snapshot instanceof Error) console.log(`  status:  unreadable — ${snapshot.message}`)
    else if (!snapshot) console.log(`  status:  not logged in — ${provider.loginHint}`)
    else console.log(`  status:  ${describeIdentity(snapshot.identity)}`)

    const running = await runningSessions(provider)
    console.log(`  running: ${running > 0 ? `yes (${running} process${running === 1 ? "" : "es"})` : "no"}`)
    console.log(`  slots:   ${(await listSlots(provider.id)).length}`)
  }
}

async function runProvider(provider: Provider, args: string[], json: boolean, yes: boolean): Promise<void> {
  const [command = "list", first, second] = args
  switch (command) {
    case "save":
      return await save(provider, first ?? die(`save needs a slot name`))
    case "load":
    case "use":
      return await load(provider, first ?? die(`load needs a slot name`))
    case "list":
    case "ls":
      return await list(provider, json)
    case "usage":
      return await usage([provider], json)
    case "rm":
    case "remove":
    case "delete":
      return await remove(provider, first ?? die(`rm needs a slot name`), yes)
    case "rename":
      return await rename(provider, first ?? die("rename needs the current name"), second ?? die("rename needs the new name"))
    default:
      die(`unknown ${provider.id} command "${command}" — run \`kyora-switch --help\``)
  }
}

async function main(): Promise<void> {
  let parsed
  try {
    parsed = parseArgs({
      args: Bun.argv.slice(2),
      options: {
        json: { type: "boolean" },
        yes: { type: "boolean", short: "y" },
        help: { type: "boolean", short: "h" },
      },
      allowPositionals: true,
      strict: true,
    })
  } catch (error) {
    die((error as Error).message)
  }

  const { values, positionals } = parsed
  if (values.help) return console.log(HELP)

  const [head = "status", ...rest] = positionals
  if (isProviderId(head)) return await runProvider(providerById(head), rest, Boolean(values.json), Boolean(values.yes))

  switch (head) {
    case "status":
      return await status(Boolean(values.json))
    case "usage":
      return await usage(PROVIDERS, Boolean(values.json))
    case "doctor":
      return await doctor()
    case "help":
      return console.log(HELP)
    default:
      die(`unknown command "${head}" — run \`kyora-switch --help\``)
  }
}

main().catch((error: Error) => die(error.message))
