const worker = new Worker(new URL("./test-worker-inner.ts", import.meta.url).href, {
  type: "module",
})

worker.onmessage = (e) => console.log("from worker:", e.data)
worker.onerror = (e) => console.error("worker error:", e)

setTimeout(() => {
  console.log("sending test message")
  worker.postMessage({ type: "test" })
}, 3000)

setTimeout(() => process.exit(0), 5000)
