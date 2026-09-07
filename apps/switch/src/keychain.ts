import { userInfo } from "node:os"

export const KEYCHAIN_SERVICE = "Claude Code-credentials"

const STDIN_LIMIT = 4096

export function keychainAccount(): string {
  return userInfo().username
}

/** Set KYORA_SWITCH_NO_KEYCHAIN=1 to keep credentials in files only. */
export function keychainSupported(): boolean {
  return process.platform === "darwin" && process.env.KYORA_SWITCH_NO_KEYCHAIN !== "1"
}

/**
 * Reads the blob back, decoding the hex form `security` falls back to whenever the
 * value holds a non-ASCII or control byte. A JSON credential document is never all-hex.
 */
export async function keychainRead(service = KEYCHAIN_SERVICE): Promise<string | null> {
  const result = await Bun.$`security find-generic-password -a ${keychainAccount()} -w -s ${service}`.quiet().nothrow()
  if (result.exitCode !== 0) return null

  const value = result.stdout.toString().trim()
  if (value.length === 0) return null
  if (value.length % 2 === 0 && /^[0-9a-f]+$/.test(value)) return Buffer.from(value, "hex").toString("utf8")
  return value
}

async function run(command: string[], input?: string): Promise<{ ok: boolean; stderr: string }> {
  const proc = Bun.spawn(command, {
    stdin: input === undefined ? "ignore" : new TextEncoder().encode(input),
    stdout: "ignore",
    stderr: "pipe",
  })
  const [exitCode, stderr] = await Promise.all([proc.exited, new Response(proc.stderr).text()])
  return { ok: exitCode === 0, stderr: stderr.trim() || `exit ${exitCode}` }
}

/**
 * Stores the blob the way Claude Code does: hex-encoded through `security -i`,
 * which keeps the secret out of the process arguments, falling back to argv
 * because `security -i` truncates any command line past 4096 characters.
 */
export async function keychainWrite(secret: string, service = KEYCHAIN_SERVICE): Promise<void> {
  const account = keychainAccount()
  if (/["\\]/.test(account) || /["\\]/.test(service)) {
    throw new Error(`cannot quote keychain account "${account}" or service "${service}"`)
  }

  const hex = Buffer.from(secret, "utf8").toString("hex")
  const command = `add-generic-password -U -a "${account}" -s "${service}" -X ${hex}\n`

  if (command.length <= STDIN_LIMIT && (await run(["security", "-i"], command)).ok) return

  const argv = await run(["security", "add-generic-password", "-U", "-a", account, "-s", service, "-X", hex])
  if (!argv.ok) throw new Error(`keychain write failed: ${argv.stderr}`)
}
