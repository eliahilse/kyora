import type { Observer } from "../observer"

let originalFetch: typeof globalThis.fetch | null = null

export function patchFetch(observer: Observer): void {
  originalFetch = globalThis.fetch

  // @ts-expect-error bun's fetch type includes preconnect which we don't need to patch
  globalThis.fetch = async function patchedFetch(
    input: RequestInfo | URL,
    init?: RequestInit,
  ): Promise<Response> {
    const start = performance.now()
    const method = init?.method ?? "GET"
    const url = input instanceof Request ? input.url : input.toString()

    let requestBody: string | undefined
    if (init?.body) {
      try {
        requestBody = typeof init.body === "string" ? init.body : JSON.stringify(init.body)
      } catch {
        requestBody = "[unserializable]"
      }
    }

    try {
      const response = await originalFetch!(input, init)
      const durationMs = performance.now() - start

      const cloned = response.clone()
      let responseBody: string | undefined
      try {
        responseBody = await cloned.text()
      } catch {
        responseBody = "[unreadable]"
      }

      observer.emit({
        type: "http",
        timestamp: Date.now(),
        data: {
          method,
          url,
          status: response.status,
          durationMs,
          requestBody,
          responseBody,
          requestHeaders: init?.headers ? Object.fromEntries(new Headers(init.headers as HeadersInit)) : undefined,
          responseHeaders: Object.fromEntries(response.headers),
        },
      })

      return response
    } catch (error) {
      const durationMs = performance.now() - start

      observer.emit({
        type: "http",
        timestamp: Date.now(),
        data: {
          method,
          url,
          status: 0,
          durationMs,
          requestBody,
          error: error instanceof Error ? error.message : String(error),
        },
      })

      throw error
    }
  }
}

export function unpatchFetch(): void {
  if (originalFetch) {
    globalThis.fetch = originalFetch
    originalFetch = null
  }
}
