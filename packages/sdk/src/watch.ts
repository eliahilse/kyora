import { observer } from "./shared"

const PROXY_FLAG = Symbol("kyora_proxy")
const FULL_SNAPSHOT_INTERVAL = 20

const pendingSnapshots = new Map<string, { root: object; source?: string }>()
let microtaskScheduled = false

const previousSnapshots = new Map<string, unknown>()
const snapshotCounters = new Map<string, number>()

function deepClone(obj: unknown): unknown {
  if (obj === null || typeof obj !== "object") return obj
  if (typeof obj === "function") return "[function]"
  if (Array.isArray(obj)) return obj.map(deepClone)
  const clone: Record<string, unknown> = {}
  for (const key of Object.keys(obj as Record<string, unknown>)) {
    const value = (obj as Record<string, unknown>)[key]
    clone[key] = typeof value === "function" ? "[function]" : deepClone(value)
  }
  return clone
}

function jsonDiff(prev: unknown, next: unknown): Record<string, unknown> | null {
  if (prev === next) return null
  if (prev === null || next === null || typeof prev !== "object" || typeof next !== "object") {
    return { $set: next }
  }
  if (Array.isArray(prev) || Array.isArray(next)) {
    return { $set: next }
  }

  const prevObj = prev as Record<string, unknown>
  const nextObj = next as Record<string, unknown>
  const diff: Record<string, unknown> = {}
  let hasDiff = false

  for (const key of Object.keys(nextObj)) {
    if (!(key in prevObj)) {
      diff[key] = { $set: nextObj[key] }
      hasDiff = true
    } else {
      const childDiff = jsonDiff(prevObj[key], nextObj[key])
      if (childDiff) {
        diff[key] = childDiff
        hasDiff = true
      }
    }
  }

  for (const key of Object.keys(prevObj)) {
    if (!(key in nextObj)) {
      diff[key] = { $delete: true }
      hasDiff = true
    }
  }

  return hasDiff ? diff : null
}

function scheduleFlush(key: string, root: object, source?: string) {
  pendingSnapshots.set(key, { root, source })
  if (!microtaskScheduled) {
    microtaskScheduled = true
    queueMicrotask(flushPending)
  }
}

function flushPending() {
  for (const [key, { root, source }] of pendingSnapshots) {
    const currentClone = deepClone(root)
    const count = snapshotCounters.get(key) ?? 0
    const isCheckpoint = count === 0 || count >= FULL_SNAPSHOT_INTERVAL

    if (isCheckpoint) {
      observer.emit({
        type: "state_snapshot",
        data: { key, value: currentClone, diff: null, source },
        timestamp: Date.now(),
      })
      snapshotCounters.set(key, 1)
    } else {
      const prev = previousSnapshots.get(key)
      const diff = jsonDiff(prev, currentClone)
      if (diff) {
        observer.emit({
          type: "state_snapshot",
          data: { key, value: null, diff, source },
          timestamp: Date.now(),
        })
      }
      snapshotCounters.set(key, count + 1)
    }

    previousSnapshots.set(key, currentClone)
  }

  pendingSnapshots.clear()
  microtaskScheduled = false
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
      if (result) scheduleFlush(rootKey, root, source)
      return result
    },

    deleteProperty(target, prop) {
      const result = Reflect.deleteProperty(target, prop)
      if (result) scheduleFlush(rootKey, root, source)
      return result
    },
  })
}

export function watch<T extends object>(obj: T, key?: string, source?: string): T {
  const resolvedKey = key ?? "anonymous"
  const proxied = createProxy(obj, resolvedKey, obj, source)
  scheduleFlush(resolvedKey, obj, source)
  return proxied
}
