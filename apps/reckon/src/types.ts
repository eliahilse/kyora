export interface Task {
  name: string
  dir: string
  description: string
}

export interface RunResult {
  task: string
  mode: "kyora" | "blind"
  passed: number
  total: number
  timeMs: number
  inputTokens: number
  outputTokens: number
  cacheReadTokens: number
  cacheWriteTokens: number
  numTurns: number
  costUsd: number
}

export interface BenchmarkReport {
  timestamp: string
  model: string
  results: RunResult[]
}
