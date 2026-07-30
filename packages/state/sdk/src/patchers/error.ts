import type { Observer } from "../observer"

let unhandledRejectionHandler: ((event: PromiseRejectionEvent) => void) | null = null
let errorHandler: ((event: ErrorEvent) => void) | null = null

export function patchErrors(observer: Observer): void {
  unhandledRejectionHandler = (event: PromiseRejectionEvent) => {
    const reason = event.reason
    observer.emit({
      type: "error",
      timestamp: Date.now(),
      data: {
        message: reason instanceof Error ? reason.message : String(reason),
        stack: reason instanceof Error ? reason.stack : undefined,
        type: reason instanceof Error ? reason.constructor.name : typeof reason,
        unhandled: true,
      },
    })
  }

  errorHandler = (event: ErrorEvent) => {
    observer.emit({
      type: "error",
      timestamp: Date.now(),
      data: {
        message: event.message,
        stack: event.error?.stack,
        type: event.error?.constructor.name ?? "Error",
        filename: event.filename,
        lineno: event.lineno,
        colno: event.colno,
        unhandled: true,
      },
    })
  }

  globalThis.addEventListener("unhandledrejection", unhandledRejectionHandler)
  globalThis.addEventListener("error", errorHandler)
}

export function unpatchErrors(): void {
  if (unhandledRejectionHandler) {
    globalThis.removeEventListener("unhandledrejection", unhandledRejectionHandler)
    unhandledRejectionHandler = null
  }
  if (errorHandler) {
    globalThis.removeEventListener("error", errorHandler)
    errorHandler = null
  }
}
