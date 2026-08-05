import test from "node:test"
import assert from "node:assert/strict"
import { processClaimedBatches } from "./batch-runner.mjs"

const batches = [
  { id: "batch-1" },
  { id: "batch-2" },
]

test("processes every batch returned by one claim", async () => {
  const published = []
  const confirmed = []
  const cleared = []
  let pending = null

  const processed = await processClaimedBatches(batches, {
    validate: (batch) => batch,
    publish: async (batch) => {
      published.push(batch.id)
      return { entry_uuid: `entry-${batch.id}` }
    },
    savePending: async (value) => { pending = value },
    confirm: async (id) => { confirmed.push(id) },
    clearPending: async () => { pending = null; cleared.push(true) },
    hasPending: async () => pending,
    fail: async () => assert.fail("fail callback must not run"),
  })

  assert.equal(processed, 2)
  assert.deepEqual(published, ["batch-1", "batch-2"])
  assert.deepEqual(confirmed, ["batch-1", "batch-2"])
  assert.equal(cleared.length, 2)
})

test("preserves uploaded entry journal and stops before publishing the next batch", async () => {
  const published = []
  let pending = null

  await assert.rejects(
    processClaimedBatches(batches, {
      validate: (batch) => batch,
      publish: async (batch) => {
        published.push(batch.id)
        return { entry_uuid: `entry-${batch.id}` }
      },
      savePending: async (value) => { pending = value },
      confirm: async () => { throw new Error("confirmation unavailable") },
      clearPending: async () => { pending = null },
      hasPending: async () => pending,
      fail: async () => assert.fail("an uploaded entry must never be marked failed"),
    }),
    /confirmation unavailable/,
  )

  assert.deepEqual(published, ["batch-1"])
  assert.equal(pending.batch.id, "batch-1")
})

test("marks a pre-upload failure and continues with the rest of the claim", async () => {
  const failed = []
  const confirmed = []

  const processed = await processClaimedBatches(batches, {
    validate: (batch) => batch,
    publish: async (batch) => {
      if (batch.id === "batch-1") throw new Error("rekor unavailable")
      return { entry_uuid: `entry-${batch.id}` }
    },
    savePending: async () => {},
    confirm: async (id) => { confirmed.push(id) },
    clearPending: async () => {},
    hasPending: async () => null,
    fail: async (batch) => { failed.push(batch.id) },
  })

  assert.equal(processed, 2)
  assert.deepEqual(failed, ["batch-1"])
  assert.deepEqual(confirmed, ["batch-2"])
})
