export async function processClaimedBatches(rawBatches, handlers) {
  if (!Array.isArray(rawBatches)) throw new Error("batch.claim_response")
  let processed = 0

  for (const rawBatch of rawBatches) {
    const batch = handlers.validate(rawBatch)
    try {
      const confirmation = await handlers.publish(batch)
      await handlers.savePending({ batch, confirmation })
      await handlers.confirm(batch.id, confirmation)
      await handlers.clearPending()
      processed += 1
      handlers.onConfirmed?.(batch, confirmation)
    } catch (error) {
      if (await handlers.hasPending()) throw error
      await handlers.fail(batch, error)
      processed += 1
      handlers.onFailed?.(batch, error)
    }
  }

  return processed
}
