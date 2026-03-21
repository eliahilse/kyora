import { init, shutdown, watch, trace } from "@kyora/sdk"

const DATA_DIR = import.meta.dir + "/../.kyora"
init({ dataDir: DATA_DIR })

// @kyora.watch
const appState = watch({ requestCount: 0, lastEndpoint: "", errors: 0 }, "appState")

// @kyora.trace
const processRequest = trace(function processRequest(endpoint: string) {
  appState.requestCount++
  appState.lastEndpoint = endpoint
  return { processed: true, count: appState.requestCount }
}, "processRequest")

// @kyora.trace
const riskyOperation = trace(function riskyOperation() {
  appState.errors++
  throw new Error("intentional test error")
}, "riskyOperation")

const server = Bun.serve({
  port: 3456,
  routes: {
    "/": () => {
      processRequest("/")
      return new Response("kyora test app")
    },

    "/api/users": async () => {
      processRequest("/api/users")
      const res = await fetch("https://jsonplaceholder.typicode.com/users")
      const users = await res.json()
      return Response.json(users)
    },

    "/api/posts": async () => {
      processRequest("/api/posts")
      const res = await fetch("https://jsonplaceholder.typicode.com/posts?_limit=5")
      const posts = await res.json()
      return Response.json(posts)
    },

    "/api/error": () => {
      try { riskyOperation() } catch {}
      return Response.json({ error: "intentional" }, { status: 500 })
    },
  },

  error(err) {
    return Response.json({ error: err.message }, { status: 500 })
  },
})

console.log(`test server running on http://localhost:${server.port}`)

process.on("SIGINT", async () => {
  await shutdown()
  process.exit(0)
})
