import { afterEach, beforeEach, expect, test } from "bun:test"
import { chmod, mkdtemp, rm, stat } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { accountSlice, claudeConfigPath, claudeProvider, mergeAccountIntoConfig, refreshCredentials } from "./claude"

test("mergeAccountIntoConfig swaps identity and keeps unrelated machine state", () => {
  const config = {
    oauthAccount: { emailAddress: "old@work.dev" },
    projects: { "/repo": { history: [1] } },
    numStartups: 42,
  }
  const merged = mergeAccountIntoConfig(config, { oauthAccount: { emailAddress: "new@home.dev" } })

  expect(merged.oauthAccount).toEqual({ emailAddress: "new@home.dev" })
  expect(merged.projects).toEqual({ "/repo": { history: [1] } })
  expect(merged.numStartups).toBe(42)
})

test("mergeAccountIntoConfig leaves userID and machineID alone, they identify the install", () => {
  const merged = mergeAccountIntoConfig(
    { userID: "install-id", machineID: "machine-id" },
    { oauthAccount: {}, userID: "other-install" },
  )
  expect(merged.userID).toBe("install-id")
  expect(merged.machineID).toBe("machine-id")
})

test("mergeAccountIntoConfig drops the previous account's entitlement caches", () => {
  const merged = mergeAccountIntoConfig(
    { modelAccessCache: { a: 1 }, hasAvailableSubscription: true, subscriptionNoticeCount: 3, autoUpdates: true },
    { oauthAccount: {} },
  )
  expect(merged).not.toHaveProperty("modelAccessCache")
  expect(merged).not.toHaveProperty("hasAvailableSubscription")
  expect(merged).not.toHaveProperty("subscriptionNoticeCount")
  expect(merged.autoUpdates).toBe(true)
})

test("mergeAccountIntoConfig removes the account when a slot does not carry one", () => {
  expect(mergeAccountIntoConfig({ oauthAccount: { a: 1 } }, {})).toEqual({})
})

test("mergeAccountIntoConfig does not mutate its input", () => {
  const config = { oauthAccount: { emailAddress: "old@work.dev" }, modelAccessCache: {} }
  mergeAccountIntoConfig(config, { oauthAccount: { emailAddress: "new@home.dev" } })
  expect(config.oauthAccount).toEqual({ emailAddress: "old@work.dev" })
  expect(config).toHaveProperty("modelAccessCache")
})

test("accountSlice takes only the account", () => {
  expect(accountSlice({ oauthAccount: { a: 1 }, userID: "u", projects: {} })).toEqual({ oauthAccount: { a: 1 } })
  expect(accountSlice(null)).toEqual({})
})

let dir: string
const previous = process.env.CLAUDE_CONFIG_DIR

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "kyora-switch-claude-"))
  process.env.CLAUDE_CONFIG_DIR = dir
})

afterEach(async () => {
  if (previous === undefined) delete process.env.CLAUDE_CONFIG_DIR
  else process.env.CLAUDE_CONFIG_DIR = previous
  await rm(dir, { recursive: true, force: true })
})

async function seed(email: string, credentials: string): Promise<void> {
  await Bun.write(join(dir, ".credentials.json"), credentials)
  await Bun.write(
    claudeConfigPath(),
    JSON.stringify({ oauthAccount: { emailAddress: email, organizationName: "Acme" }, userID: "install-id", numStartups: 7 }),
  )
  await Bun.write(join(dir, "policy-limits.json"), `{"for":"${email}"}`)
}

test("capture returns null when nothing is logged in", async () => {
  expect(await claudeProvider.capture()).toBeNull()
})

test("capture then restore round-trips the account and the side files", async () => {
  await seed("work@acme.dev", '{"claudeAiOauth":{"accessToken":"work-token"}}')
  const work = (await claudeProvider.capture())!
  expect(work.identity).toEqual({ account: "work@acme.dev", org: "Acme", plan: undefined })

  await seed("home@me.dev", '{"claudeAiOauth":{"accessToken":"home-token"}}')
  await claudeProvider.restore(work)

  const config = JSON.parse(await Bun.file(claudeConfigPath()).text())
  expect(config.oauthAccount.emailAddress).toBe("work@acme.dev")
  expect(config.userID).toBe("install-id")
  expect(config.numStartups).toBe(7)
  expect(await Bun.file(join(dir, ".credentials.json")).text()).toBe('{"claudeAiOauth":{"accessToken":"work-token"}}')
  expect(await Bun.file(join(dir, "policy-limits.json")).text()).toBe('{"for":"work@acme.dev"}')
})

test("restore clears a side file the slot does not carry", async () => {
  await seed("work@acme.dev", "{}")
  const withoutSideFiles = (await claudeProvider.capture())!
  delete withoutSideFiles.files["policy-limits.json"]

  await claudeProvider.restore(withoutSideFiles)
  expect(await Bun.file(join(dir, "policy-limits.json")).exists()).toBe(false)
})

test("restore refuses a slot with no credentials", async () => {
  await expect(claudeProvider.restore({ provider: "claude", identity: {}, files: {}, capturedAt: 0 })).rejects.toThrow(
    /no Claude credentials/,
  )
})

test("restore tightens a world-readable credentials file to 0600", async () => {
  await seed("work@acme.dev", "{}")
  const work = (await claudeProvider.capture())!
  await chmod(join(dir, ".credentials.json"), 0o644)

  await claudeProvider.restore(work)
  expect((await stat(join(dir, ".credentials.json"))).mode & 0o777).toBe(0o600)
})

test("restore leaves the mode of .claude.json alone", async () => {
  await seed("work@acme.dev", "{}")
  const work = (await claudeProvider.capture())!
  await chmod(claudeConfigPath(), 0o644)

  await claudeProvider.restore(work)
  expect((await stat(claudeConfigPath())).mode & 0o777).toBe(0o644)
})

test("forget drops the credentials and the account, keeping machine state", async () => {
  await seed("work@acme.dev", "{}")
  await claudeProvider.forget()

  expect(await Bun.file(join(dir, ".credentials.json")).exists()).toBe(false)
  expect(await Bun.file(join(dir, "policy-limits.json")).exists()).toBe(false)
  expect(await claudeProvider.capture()).toBeNull()

  const config = JSON.parse(await Bun.file(claudeConfigPath()).text())
  expect(config).not.toHaveProperty("oauthAccount")
  expect(config.userID).toBe("install-id")
  expect(config.numStartups).toBe(7)
})

test("a slot saved before forget still restores afterwards", async () => {
  await seed("work@acme.dev", '{"claudeAiOauth":{"accessToken":"work-token"}}')
  const saved = (await claudeProvider.capture())!

  await claudeProvider.forget()
  await claudeProvider.restore(saved)

  expect(await Bun.file(join(dir, ".credentials.json")).text()).toBe('{"claudeAiOauth":{"accessToken":"work-token"}}')
  expect(JSON.parse(await Bun.file(claudeConfigPath()).text()).oauthAccount.emailAddress).toBe("work@acme.dev")
})

test("credentialExpiry reads the OAuth expiry, and copes with a blob without one", async () => {
  await seed("work@acme.dev", JSON.stringify({ claudeAiOauth: { accessToken: "t", expiresAt: 1788849727085 } }))
  expect(claudeProvider.credentialExpiry!((await claudeProvider.capture())!)).toBe(1788849727085)

  await seed("work@acme.dev", JSON.stringify({ claudeAiOauth: { accessToken: "t" } }))
  expect(claudeProvider.credentialExpiry!((await claudeProvider.capture())!)).toBeUndefined()

  await seed("work@acme.dev", "{not json")
  expect(claudeProvider.credentialExpiry!((await claudeProvider.capture())!)).toBeUndefined()
})

test("refreshCredentials rejects a blob it cannot use", async () => {
  expect(await refreshCredentials("{not json")).toBeNull()
  expect(await refreshCredentials(JSON.stringify({ claudeAiOauth: { accessToken: "a" } }))).toBeNull()
})

test("refreshCredentials rotates the tokens and keeps everything else in the blob", async () => {
  const server = Bun.serve({
    port: 0,
    fetch: async (request) => {
      const body = (await request.json()) as Record<string, unknown>
      if (body.grant_type !== "refresh_token" || body.refresh_token !== "old-refresh") {
        return new Response("bad", { status: 400 })
      }
      return Response.json({
        access_token: "new-access",
        refresh_token: "new-refresh",
        expires_in: 28800,
        refresh_token_expires_in: 2_332_800,
        scope: "user:inference user:profile",
      })
    },
  })
  process.env.KYORA_CLAUDE_TOKEN_URL = server.url.href

  const before = Date.now()
  const refreshed = await refreshCredentials(
    JSON.stringify({
      mcpOAuth: { linear: { accessToken: "keep-me" } },
      claudeAiOauth: {
        accessToken: "old-access",
        refreshToken: "old-refresh",
        expiresAt: 1,
        subscriptionType: "max",
        scopes: ["user:inference"],
      },
    }),
  )
  server.stop(true)
  delete process.env.KYORA_CLAUDE_TOKEN_URL

  const blob = JSON.parse(refreshed!)
  expect(blob.claudeAiOauth.accessToken).toBe("new-access")
  expect(blob.claudeAiOauth.refreshToken).toBe("new-refresh")
  expect(blob.claudeAiOauth.expiresAt).toBeGreaterThanOrEqual(before + 28_800_000)
  expect(blob.claudeAiOauth.scopes).toEqual(["user:inference", "user:profile"])
  expect(blob.claudeAiOauth.subscriptionType).toBe("max")
  expect(blob.mcpOAuth).toEqual({ linear: { accessToken: "keep-me" } })
})

test("refreshCredentials keeps the sent refresh token when the response omits one", async () => {
  const server = Bun.serve({
    port: 0,
    fetch: () => Response.json({ access_token: "new-access", expires_in: 100 }),
  })
  process.env.KYORA_CLAUDE_TOKEN_URL = server.url.href

  const refreshed = await refreshCredentials(
    JSON.stringify({ claudeAiOauth: { accessToken: "old", refreshToken: "old-refresh", refreshTokenExpiresAt: 42 } }),
  )
  server.stop(true)
  delete process.env.KYORA_CLAUDE_TOKEN_URL

  const oauth = JSON.parse(refreshed!).claudeAiOauth
  expect(oauth.refreshToken).toBe("old-refresh")
  expect(oauth.refreshTokenExpiresAt).toBe(42)
})

test("refreshCredentials returns null when the endpoint refuses the token", async () => {
  const server = Bun.serve({ port: 0, fetch: () => new Response("invalid_grant", { status: 400 }) })
  process.env.KYORA_CLAUDE_TOKEN_URL = server.url.href

  const refreshed = await refreshCredentials(JSON.stringify({ claudeAiOauth: { refreshToken: "spent" } }))
  server.stop(true)
  delete process.env.KYORA_CLAUDE_TOKEN_URL

  expect(refreshed).toBeNull()
})
