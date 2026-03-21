import { observer } from "./shared"

export function trace<T extends (...args: any[]) => any>(fn: T, name?: string): T {
  const fnName = name ?? fn.name ?? "anonymous"

  const wrapped = function (this: any, ...args: any[]) {
    const start = performance.now()
    let returnValue: unknown
    let error: string | undefined

    try {
      returnValue = fn.apply(this, args)
    } catch (err) {
      error = err instanceof Error ? err.message : String(err)
      emitCall(fnName, args, undefined, error, performance.now() - start)
      throw err
    }

    if (returnValue instanceof Promise) {
      return returnValue
        .then((resolved) => {
          emitCall(fnName, args, resolved, undefined, performance.now() - start)
          return resolved
        })
        .catch((err) => {
          error = err instanceof Error ? err.message : String(err)
          emitCall(fnName, args, undefined, error, performance.now() - start)
          throw err
        })
    }

    emitCall(fnName, args, returnValue, undefined, performance.now() - start)
    return returnValue
  } as unknown as T

  Object.defineProperty(wrapped, "name", { value: fnName })
  return wrapped
}

function emitCall(
  name: string,
  args: unknown[],
  returnValue: unknown,
  error: string | undefined,
  durationMs: number,
) {
  observer.emit({
    type: "function_call",
    data: {
      name,
      args: safeSerialize(args),
      returnValue: safeSerialize(returnValue),
      error,
      durationMs,
    },
    timestamp: Date.now(),
  })
}

function safeSerialize(val: unknown): unknown {
  try {
    JSON.stringify(val)
    return val
  } catch {
    return "[unserializable]"
  }
}
