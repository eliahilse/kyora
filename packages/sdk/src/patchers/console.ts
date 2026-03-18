import type { Observer } from "../observer"
import type { ConsoleLevel } from "../types"

const LEVELS: ConsoleLevel[] = ["log", "warn", "error", "info", "debug"]
const originals = new Map<ConsoleLevel, (...args: unknown[]) => void>()

export function patchConsole(observer: Observer): void {
  for (const level of LEVELS) {
    originals.set(level, (console[level as keyof Console] as (...args: unknown[]) => void).bind(console))

    ;(console as unknown as Record<string, unknown>)[level] = (...args: unknown[]) => {
      originals.get(level)!(...args)

      observer.emit({
        type: "console",
        timestamp: Date.now(),
        data: {
          level,
          args: args.map(safeSerialize),
        },
      })
    }
  }
}

export function unpatchConsole(): void {
  for (const [level, fn] of originals) {
    ;(console as unknown as Record<string, unknown>)[level] = fn
  }
  originals.clear()
}

function safeSerialize(value: unknown): unknown {
  try {
    JSON.stringify(value)
    return value
  } catch {
    return String(value)
  }
}
