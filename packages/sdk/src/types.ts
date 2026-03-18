export type EventType = "http" | "console" | "error" | "timer" | "custom"
export type ConsoleLevel = "log" | "warn" | "error" | "info" | "debug"

export interface HttpEventData {
  method: string
  url: string
  status: number
  durationMs: number
  requestHeaders?: Record<string, string>
  requestBody?: string
  responseHeaders?: Record<string, string>
  responseBody?: string
}

export interface ConsoleEventData {
  level: ConsoleLevel
  args: unknown[]
}

export interface ErrorEventData {
  message: string
  stack?: string
  type: string
  unhandled?: boolean
}

export interface KyoraConfig {
  maxEvents: number
  maxStateSnapshots: number
  maxFunctionCalls: number
  pruneInterval: number
}

export const DEFAULT_CONFIG: KyoraConfig = {
  maxEvents: 10_000,
  maxStateSnapshots: 5_000,
  maxFunctionCalls: 5_000,
  pruneInterval: 100,
}
