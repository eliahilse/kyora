import { AsyncLocalStorage } from "node:async_hooks"

const traceStorage = new AsyncLocalStorage<{ traceId: string }>()

export function withTrace<T>(fn: () => T, traceId?: string): T {
  return traceStorage.run(
    { traceId: traceId ?? crypto.randomUUID() },
    fn,
  )
}

export function getTraceId(): string | undefined {
  return traceStorage.getStore()?.traceId
}
