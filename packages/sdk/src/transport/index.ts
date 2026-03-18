import type { Observer, ObserverEvent } from "../observer"
import type { KyoraDb } from "@kyora/db"
import { events } from "@kyora/db/schema"

export class Transport {
  private buffer: ObserverEvent[] = []
  private flushInterval: ReturnType<typeof setInterval> | null = null

  constructor(
    private db: KyoraDb,
    private batchSize = 50,
    private flushMs = 1000,
  ) {}

  connect(observer: Observer): void {
    observer.on("*", (event) => {
      this.buffer.push(event)
      if (this.buffer.length >= this.batchSize) {
        this.flush()
      }
    })

    this.flushInterval = setInterval(() => this.flush(), this.flushMs)
  }

  async flush(): Promise<void> {
    if (this.buffer.length === 0) return
    const batch = this.buffer.splice(0)

    await this.db.insert(events).values(
      batch.map((e) => ({
        type: e.type,
        timestamp: new Date(e.timestamp),
        data: e.data,
        sessionId: e.sessionId,
        traceId: e.traceId,
      })),
    )
  }

  async stop(): Promise<void> {
    if (this.flushInterval) clearInterval(this.flushInterval)
    await this.flush()
  }
}
