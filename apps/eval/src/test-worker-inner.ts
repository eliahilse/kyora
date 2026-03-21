declare var self: Worker

try {
  console.log("worker: starting, importing createDb...")
  const { createDb } = await import("@kyora/db")

  const dataDir = import.meta.dir + "/../.kyora"
  console.log("worker: creating db at", dataDir)
  const db = await createDb(dataDir)
  console.log("worker: db ready!")

  self.onmessage = (e) => {
    console.log("worker: got message", e.data)
    self.postMessage({ type: "ok" })
  }
} catch (err) {
  console.error("worker: FATAL", err)
}
