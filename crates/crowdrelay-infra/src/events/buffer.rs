#[derive(Clone, Debug)]
pub struct EventActionBuffer {
    sender: mpsc::Sender<EventAction>,
    metrics: Arc<EventActionBufferMetrics>,
}

impl EventActionBuffer {
    pub fn new(
        repository: Arc<dyn EventRepository>,
        config: ClickBufferConfig,
    ) -> Result<(Self, EventActionBatchWorker), EventActionBufferBuildError> {
        if config.capacity == 0
            || config.batch_size == 0
            || config.batch_size > config.capacity
            || config.batch_size > MAX_EVENT_ACTION_BATCH_ROWS
            || config.flush_interval.is_zero()
        {
            return Err(EventActionBufferBuildError);
        }
        let (sender, receiver) = mpsc::channel(config.capacity);
        let metrics = Arc::new(EventActionBufferMetrics::default());
        Ok((
            Self {
                sender,
                metrics: Arc::clone(&metrics),
            },
            EventActionBatchWorker {
                receiver,
                repository,
                batch_size: config.batch_size,
                flush_interval: config.flush_interval,
                metrics,
            },
        ))
    }

    pub fn try_send(&self, action: EventAction) -> EventActionEnqueueOutcome {
        match self.sender.try_send(action) {
            Ok(()) => {
                self.metrics.queued.fetch_add(1, Ordering::Relaxed);
                EventActionEnqueueOutcome::Queued
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
                EventActionEnqueueOutcome::DroppedFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
                EventActionEnqueueOutcome::DroppedClosed
            }
        }
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<EventActionBufferMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventActionEnqueueOutcome {
    Queued,
    DroppedFull,
    DroppedClosed,
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
#[error("event action buffer configuration is invalid")]
pub struct EventActionBufferBuildError;

#[derive(Debug, Default)]
pub struct EventActionBufferMetrics {
    queued: AtomicU64,
    persisted: AtomicU64,
    dropped: AtomicU64,
    persistence_failed: AtomicU64,
}

impl EventActionBufferMetrics {
    #[must_use]
    pub fn snapshot(&self) -> EventActionBufferSnapshot {
        EventActionBufferSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            persisted: self.persisted.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            persistence_failed: self.persistence_failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventActionBufferSnapshot {
    pub queued: u64,
    pub persisted: u64,
    pub dropped: u64,
    pub persistence_failed: u64,
}

pub struct EventActionBatchWorker {
    receiver: mpsc::Receiver<EventAction>,
    repository: Arc<dyn EventRepository>,
    batch_size: usize,
    flush_interval: Duration,
    metrics: Arc<EventActionBufferMetrics>,
}

impl EventActionBatchWorker {
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.flush_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut batch = Vec::with_capacity(self.batch_size);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                action = self.receiver.recv() => {
                    match action {
                        Some(action) => {
                            batch.push(action);
                            if batch.len() >= self.batch_size {
                                self.flush(&mut batch).await;
                            }
                        }
                        None => break,
                    }
                }
                _ = ticker.tick() => self.flush(&mut batch).await,
            }
        }
        while let Ok(action) = self.receiver.try_recv() {
            if batch.len() >= self.batch_size {
                self.flush(&mut batch).await;
            }
            batch.push(action);
        }
        self.flush(&mut batch).await;
    }

    async fn flush(&self, batch: &mut Vec<EventAction>) {
        if batch.is_empty() {
            return;
        }
        let count = batch.len();
        match self.repository.persist_event_action(batch).await {
            Ok(()) => {
                self.metrics
                    .persisted
                    .fetch_add(count as u64, Ordering::Relaxed);
            }
            Err(error) => {
                self.metrics
                    .persistence_failed
                    .fetch_add(count as u64, Ordering::Relaxed);
                self.metrics
                    .dropped
                    .fetch_add(count as u64, Ordering::Relaxed);
                tracing::warn!(%error, count, "event action batch persistence failed");
            }
        }
        batch.clear();
    }
}
