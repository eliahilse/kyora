import type { LiveUsage } from "@kyora-sh/usage"

export const PROVIDER_IDS = ["claude", "codex"] as const

export type ProviderId = (typeof PROVIDER_IDS)[number]

export interface Identity {
  account?: string
  org?: string
  plan?: string
}

export interface Snapshot {
  provider: ProviderId
  identity: Identity
  files: Record<string, string>
  capturedAt: number
}

export interface Provider {
  id: ProviderId
  label: string
  processName: string
  loginHint: string
  locations(): string[]
  capture(): Promise<Snapshot | null>
  forget(): Promise<void>
  restore(snapshot: Snapshot): Promise<void>
  quota?(snapshot: Snapshot): Promise<LiveUsage | null>
  credentialExpiry?(snapshot: Snapshot): number | undefined
  refresh?(snapshot: Snapshot): Promise<Snapshot | null>
  quotaHint?: string
}

export function isProviderId(value: string | undefined): value is ProviderId {
  return (PROVIDER_IDS as readonly string[]).includes(value ?? "")
}

export function describeIdentity(identity: Identity): string {
  const parts = [identity.account ?? "unknown account"]
  if (identity.org) parts.push(identity.org)
  if (identity.plan) parts.push(identity.plan)
  return parts.join(" · ")
}
