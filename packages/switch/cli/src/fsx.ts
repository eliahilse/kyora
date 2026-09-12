import { chmod, mkdir, rename, rm, stat } from "node:fs/promises"
import { dirname } from "node:path"

export async function readTextIfExists(path: string): Promise<string | null> {
  const file = Bun.file(path)
  return (await file.exists()) ? await file.text() : null
}

export async function readJsonIfExists(path: string): Promise<Record<string, unknown> | null> {
  const text = await readTextIfExists(path)
  if (text === null) return null
  try {
    const parsed: unknown = JSON.parse(text)
    return parsed !== null && typeof parsed === "object" ? (parsed as Record<string, unknown>) : null
  } catch {
    throw new Error(`${path} is not valid JSON`)
  }
}

async function modeOf(path: string): Promise<number | null> {
  try {
    return (await stat(path)).mode & 0o777
  } catch {
    return null
  }
}

/**
 * Replaces a file in one step so a running CLI never reads a half-written config.
 * An explicit `mode` is enforced; without one the existing mode is preserved.
 */
export async function writeFileAtomic(path: string, contents: string, mode?: number): Promise<void> {
  await mkdir(dirname(path), { recursive: true, mode: 0o700 })
  const temp = `${path}.kyora-switch.${process.pid}.tmp`
  try {
    await Bun.write(temp, contents)
    await chmod(temp, mode ?? (await modeOf(path)) ?? 0o600)
    await rename(temp, path)
  } catch (error) {
    await rm(temp, { force: true })
    throw error
  }
}
