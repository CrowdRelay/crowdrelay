#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use crowdrelay_application::{EventRepository, RegisterEventInterestCommand, RepositoryError};
    use crowdrelay_domain::{
        EventAction, EventActionKind, EventId, EventInterestResult, FanEventInterest,
        FanSessionToken, PublicEvent, WorkspaceId,
    };
    use time::OffsetDateTime;

    use super::*;

    struct FailingEventRepository;

    #[async_trait]
    impl EventRepository for FailingEventRepository {
        async fn load_published_events(&self) -> Result<Vec<PublicEvent>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn persist_event_action(
            &self,
            _actions: &[EventAction],
        ) -> Result<(), RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn register_interest(
            &self,
            _command: &RegisterEventInterestCommand,
        ) -> Result<EventInterestResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn list_fan_interests(
            &self,
            _workspace_id: WorkspaceId,
            _session_token: &FanSessionToken,
            _limit: u32,
        ) -> Result<Vec<FanEventInterest>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }
    }

    #[tokio::test]
    async fn failed_event_action_batches_are_counted_as_dropped()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_sender, receiver) = mpsc::channel(1);
        let metrics = Arc::new(EventActionBufferMetrics::default());
        let worker = EventActionBatchWorker {
            receiver,
            repository: Arc::new(FailingEventRepository),
            batch_size: 1,
            flush_interval: Duration::from_secs(1),
            metrics: Arc::clone(&metrics),
        };
        let mut batch = vec![EventAction::new(
            WorkspaceId::new(),
            EventId::new(),
            EventActionKind::PageView,
            None,
            None,
            None,
            OffsetDateTime::now_utc(),
        )?];

        worker.flush(&mut batch).await;

        assert!(batch.is_empty());
        assert_eq!(
            metrics.snapshot(),
            EventActionBufferSnapshot {
                queued: 0,
                persisted: 0,
                dropped: 1,
                persistence_failed: 1,
            }
        );
        Ok(())
    }
}
