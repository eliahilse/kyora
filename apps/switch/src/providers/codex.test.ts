import { afterEach, beforeEach, expect, test } from "bun:test"
import { mkdtemp, rm, stat } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { codexDir, codexProvider } from "./codex"

let dir: string
const previous = process.env.CODEX_HOME

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "kyora-switch-codex-"))
  process.env.CODEX_HOME = dir
})

afterEach(async () => {
  if (previous === undefined) delete process.env.CODEX_HOME
  else process.env.CODEX_HOME = previous
  await rm(dir, { recursive: true, force: true })
})

function auth(email: string): string {
  const payload = Buffer.from(JSON.stringify({ email, "https://api.openai.com/auth": { chatgpt_plan_type: "pro" } })).toString(
    "base64url",
  )
  return JSON.stringify({ OPENAI_API_KEY: null, tokens: { id_token: `h.${payload}.s`, account_id: "acct-1" } })
}

test("codexDir honours CODEX_HOME", () => {
  expect(codexDir()).toBe(dir)
})

test("capture returns null when there is no auth.json", async () => {
  expect(await codexProvider.capture()).toBeNull()
})

test("capture reads the identity, restore writes the file back at 0600", async () => {
  await Bun.write(join(dir, "auth.json"), auth("work@acme.dev"))
  const work = (await codexProvider.capture())!
  expect(work.identity).toEqual({ account: "work@acme.dev", plan: "pro" })

  await Bun.write(join(dir, "auth.json"), auth("home@me.dev"))
  await codexProvider.restore(work)

  expect(await Bun.file(join(dir, "auth.json")).text()).toBe(auth("work@acme.dev"))
  expect((await stat(join(dir, "auth.json"))).mode & 0o777).toBe(0o600)
})

test("capture reports a corrupt auth.json instead of silently skipping it", async () => {
  await Bun.write(join(dir, "auth.json"), "{not json")
  await expect(codexProvider.capture()).rejects.toThrow(/not valid JSON/)
})

test("restore refuses a slot with no auth file", async () => {
  await expect(codexProvider.restore({ provider: "codex", identity: {}, files: {}, capturedAt: 0 })).rejects.toThrow(
    /no Codex credentials/,
  )
})

test("forget removes auth.json and is a no-op when there is none", async () => {
  await Bun.write(join(dir, "auth.json"), auth("work@acme.dev"))
  await codexProvider.forget()
  expect(await Bun.file(join(dir, "auth.json")).exists()).toBe(false)
  expect(await codexProvider.capture()).toBeNull()

  await codexProvider.forget()
  expect(await Bun.file(join(dir, "auth.json")).exists()).toBe(false)
})

test("forget leaves config.toml alone", async () => {
  await Bun.write(join(dir, "auth.json"), auth("work@acme.dev"))
  await Bun.write(join(dir, "config.toml"), 'model = "gpt-5.6-luna"\n')
  await codexProvider.forget()
  expect(await Bun.file(join(dir, "config.toml")).text()).toBe('model = "gpt-5.6-luna"\n')
})
