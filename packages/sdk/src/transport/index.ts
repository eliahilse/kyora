import type { Observer, ObserverEvent } from "../observer"

export class Transport {
  private worker: Worker
  private flushInterval: ReturnType<typeof setInterval> | null = null

  constructor(dataDir?: string, private flushMs = 1000) {
    this.worker = new Worker(new URL("./worker.ts", import.meta.url).href, {
      type: "module",
      // @ts-expect-error bun worker data
      workerData: { dataDir },
    })
  }

  connect(observer: Observer): void {
    observer.on("*", (event) => {
      this.worker.postMessage({
        type: "event",
        payload: {
          type: event.type,
          timestamp: new Date(event.timestamp),
          data: event.data,
          sessionId: event.sessionId,
          traceId: event.traceId,
        },
      })
    })

    this.flushInterval = setInterval(() => this.flush(), this.flushMs)
  }

  flush(): Promise<void> {
    return new Promise((resolve) => {
      const handler = (e: MessageEvent) => {
        if (e.data.type === "flushed") {
          this.worker.removeEventListener("message", handler)
          resolve()
        }
      }
      this.worker.addEventListener("message", handler)
      this.worker.postMessage({ type: "flush" })
    })
  }

  stop(): Promise<void> {
    if (this.flushInterval) clearInterval(this.flushInterval)
    return new Promise((resolve) => {
      const handler = (e: MessageEvent) => {
        if (e.data.type === "shutdown_complete") {
          this.worker.removeEventListener("message", handler)
          resolve()
        }
      }
      this.worker.addEventListener("message", handler)
      this.worker.postMessage({ type: "shutdown" })
    })
  }
}
