import type { Observer } from "../observer"
import { patchFetch, unpatchFetch } from "./fetch"
import { patchConsole, unpatchConsole } from "./console"
import { patchErrors, unpatchErrors } from "./error"

export function patchAll(observer: Observer): void {
  patchFetch(observer)
  patchConsole(observer)
  patchErrors(observer)
}

export function unpatchAll(): void {
  unpatchFetch()
  unpatchConsole()
  unpatchErrors()
}

export { patchFetch, unpatchFetch } from "./fetch"
export { patchConsole, unpatchConsole } from "./console"
export { patchErrors, unpatchErrors } from "./error"
