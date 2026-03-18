import type { EventType } from "../types"

export interface ObserverEvent {
  type: EventType
  data: Record<string, unknown>
  timestamp: number
  sessionId?: string
  traceId?: string
}

type Listener = (event: ObserverEvent) => void

export class Observer {
  private listeners: Map<string, Set<Listener>> = new Map()

  on(type: EventType | "*", listener: Listener): void {
    const set = this.listeners.get(type) ?? new Set()
    set.add(listener)
    this.listeners.set(type, set)
  }

  off(type: EventType | "*", listener: Listener): void {
    this.listeners.get(type)?.delete(listener)
  }

  emit(event: ObserverEvent): void {
    this.listeners.get(event.type)?.forEach((fn) => fn(event))
    this.listeners.get("*")?.forEach((fn) => fn(event))
  }
}
