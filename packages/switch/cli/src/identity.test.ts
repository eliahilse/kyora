import { expect, test } from "bun:test"
import { claudeIdentity, codexIdentity, jwtPayload } from "./identity"

function token(payload: Record<string, unknown>): string {
  const body = Buffer.from(JSON.stringify(payload)).toString("base64url")
  return `header.${body}.signature`
}

test("jwtPayload decodes base64url without padding", () => {
  expect(jwtPayload(token({ email: "a@b.co", sub: "x" }))).toEqual({ email: "a@b.co", sub: "x" })
})

test("jwtPayload returns null on garbage instead of throwing", () => {
  expect(jwtPayload("not-a-jwt")).toBeNull()
  expect(jwtPayload("a.!!!.c")).toBeNull()
  expect(jwtPayload("")).toBeNull()
})

test("codexIdentity reads the account and plan out of the id_token", () => {
  const auth = {
    OPENAI_API_KEY: null,
    tokens: {
      id_token: token({
        email: "me@example.com",
        "https://api.openai.com/auth": { chatgpt_plan_type: "pro", chatgpt_account_id: "acct-1" },
      }),
      account_id: "acct-1",
    },
  }
  expect(codexIdentity(auth)).toEqual({ account: "me@example.com", plan: "pro" })
})

test("codexIdentity falls back to account_id, and flags api-key auth", () => {
  expect(codexIdentity({ OPENAI_API_KEY: "sk-test", tokens: { account_id: "acct-2" } })).toEqual({
    account: "acct-2",
    plan: "api key",
  })
})

test("codexIdentity survives a missing or malformed auth file", () => {
  expect(codexIdentity(null)).toEqual({ account: undefined, plan: undefined })
  expect(codexIdentity({ tokens: { id_token: "broken" } })).toEqual({ account: undefined, plan: undefined })
})

test("claudeIdentity reads the oauthAccount slice", () => {
  const slice = {
    oauthAccount: { emailAddress: "me@work.dev", organizationName: "Acme", seatTier: "max" },
    userID: "u1",
  }
  expect(claudeIdentity(slice)).toEqual({ account: "me@work.dev", org: "Acme", plan: "max" })
})

test("claudeIdentity falls back to the account uuid when there is no email", () => {
  expect(claudeIdentity({ oauthAccount: { accountUuid: "uuid-1", billingType: "team" } })).toEqual({
    account: "uuid-1",
    org: undefined,
    plan: "team",
  })
})
