import { Observer } from "./observer"
import { Transport } from "./transport"
import { patchAll, unpatchAll } from "./patchers"
import { DEFAULT_CONFIG, type KyoraConfig } from "./types"

let initialized = false

export const observer = new Observer()
let transport: Transport | null = null

export function init(config?: Partial<KyoraConfig> & { dataDir?: string }): void {
  if (initialized) return

  transport = new Transport(config?.dataDir, config?.flushMs ?? DEFAULT_CONFIG.flushMs)
  transport.connect(observer)
  patchAll(observer)
  initialized = true
}

export async function shutdown(): Promise<void> {
  if (!initialized) return
  unpatchAll()
  await transport?.stop()
  initialized = false
}

export { Observer } from "./observer"
export { Transport } from "./transport"
export { patchAll, unpatchAll } from "./patchers"
export * from "./types"
