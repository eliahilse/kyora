import { observer } from "./shared"

const PROXY_FLAG = Symbol("kyora_proxy")

function deepClone(obj: unknown): unknown {
  if (obj === null || typeof obj !== "object") return obj
  if (Array.isArray(obj)) return obj.map(deepClone)
  const clone: Record<string, unknown> = {}
  for (const key of Object.keys(obj as Record<string, unknown>)) {
    clone[key] = deepClone((obj as Record<string, unknown>)[key])
  }
  return clone
}

function emitSnapshot(key: string, value: unknown, source?: string) {
  observer.emit({
    type: "state_snapshot",
    data: { key, value: deepClone(value), source },
    timestamp: Date.now(),
  })
}

function createProxy<T extends object>(obj: T, rootKey: string, root: T, source?: string): T {
  if ((obj as any)[PROXY_FLAG]) return obj

  return new Proxy(obj, {
    get(target, prop, receiver) {
      if (prop === PROXY_FLAG) return true
      const val = Reflect.get(target, prop, receiver)
      if (val !== null && typeof val === "object" && !((val as any)[PROXY_FLAG])) {
        return createProxy(val as object, rootKey, root, source)
      }
      return val
    },

    set(target, prop, value, receiver) {
      const result = Reflect.set(target, prop, value, receiver)
      if (result) emitSnapshot(rootKey, root, source)
      return result
    },

    deleteProperty(target, prop) {
      const result = Reflect.deleteProperty(target, prop)
      if (result) emitSnapshot(rootKey, root, source)
      return result
    },
  })
}

export function watch<T extends object>(obj: T, key?: string, source?: string): T {
  const resolvedKey = key ?? "anonymous"
  const proxied = createProxy(obj, resolvedKey, obj, source)
  emitSnapshot(resolvedKey, obj, source)
  return proxied
}
