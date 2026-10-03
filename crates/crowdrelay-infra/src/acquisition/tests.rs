// Acquisition unit tests — `include!`d back into `acquisition.rs` so they
// keep `use super::*` scope. Split for the source-size ratchet: the parent
// caps at 1000 lines under the modularity contract.
#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crowdrelay_domain::VisitorId;

    use super::*;

    #[derive(Default)]
    struct FakeRepository {
        persisted: Mutex<Vec<Vec<ClickEvent>>>,
        fail_clicks: bool,
    }

    #[async_trait]
    impl AcquisitionRepository for FakeRepository {
        async fn resolve_workspace(
            &self,
            _slug: &WorkspaceSlug,
        ) -> Result<Option<WorkspaceId>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn load_redirect_context(&self) -> Result<Option<RedirectContext>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn persist_click_batch(&self, clicks: &[ClickEvent]) -> Result<(), RepositoryError> {
            if self.fail_clicks {
                return Err(RepositoryError::Unavailable);
            }
            self.persisted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(clicks.to_vec());
            Ok(())
        }

        async fn persist_fan_signup(
            &self,
            _command: &SignupFanCommand,
        ) -> Result<FanSignupResult, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn list_city_signals(
            &self,
            _workspace_id: WorkspaceId,
            _limit: u32,
        ) -> Result<Vec<CitySignal>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn upsert_smart_link<'a>(
            &self,
            _command: &UpsertSmartLinkCommand<'a>,
        ) -> Result<UpsertedSmartLink, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn list_smart_links(
            &self,
            _workspace_id: WorkspaceId,
        ) -> Result<Vec<UpsertedSmartLink>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn link_click_stats(
            &self,
            _workspace_id: WorkspaceId,
            _slugs: &[String],
            _now: OffsetDateTime,
        ) -> Result<Vec<LinkClickStats>, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }

        async fn load_or_create_fan_referral_code(
            &self,
            _workspace_id: WorkspaceId,
            _fan_id: FanId,
        ) -> Result<ReferralCode, RepositoryError> {
            unreachable!("not used by click buffer tests")
        }
    }

    fn click_event() -> Result<ClickEvent, Box<dyn std::error::Error>> {
        let link = ResolvedSmartLink::new(
            SmartLinkId::new(),
            WorkspaceId::new(),
            None,
            SmartLinkSlug::parse("tour")?,
            DestinationUrl::parse("https://virya.music/join")?,
            1,
            None,
            None,
        )?;
        Ok(ClickEvent::from_link(
            &link,
            Some(VisitorId::new()),
            Some("example.com".to_owned()),
            OffsetDateTime::UNIX_EPOCH,
        )?)
    }

    #[test]
    fn rejects_invalid_smart_link_rows_instead_of_loading_unsafe_urls() {
        let row = SmartLinkRow {
            id: Uuid::now_v7(),
            workspace_id: Uuid::now_v7(),
            campaign_id: None,
            slug: "safe-link".to_owned(),
            destination_url: "javascript:alert(1)".to_owned(),
            version: 1,
            channel_source: None,
            channel_community: None,
        };

        assert!(ResolvedSmartLink::try_from(row).is_err());
    }

    #[test]
    fn rejects_negative_database_counters() {
        let row = CitySignalRow {
            city_id: Uuid::now_v7(),
            slug: "wroclaw".to_owned(),
            name: "Wrocław".to_owned(),
            country_code: "PL".to_owned(),
            fan_count: -1,
        };

        assert!(CitySignal::try_from(row).is_err());
    }

    #[tokio::test]
    async fn full_click_channel_falls_back_to_durable_persistence()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(FakeRepository::default());
        let (buffer, _worker) = ClickBuffer::new(
            repository.clone(),
            crate::config::ClickBufferConfig {
                capacity: 1,
                batch_size: 1,
                flush_interval: Duration::from_secs(1),
            },
        )?;

        assert_eq!(
            buffer.submit(click_event()?).await,
            ClickSubmissionOutcome::Queued
        );
        assert_eq!(
            buffer.submit(click_event()?).await,
            ClickSubmissionOutcome::OverflowPersisted
        );
        assert_eq!(
            repository
                .persisted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1,
            "the overflow click must already be durable before submit returns"
        );
        assert_eq!(
            buffer.metrics().snapshot(),
            ClickBufferSnapshot {
                queued: 1,
                persisted: 1,
                overflowed: 1,
                overflow_recovered: 1,
                dropped: 0,
                persistence_failed: 0,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn overflow_failure_is_explicit_and_counts_as_real_loss()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(FakeRepository {
            persisted: Mutex::new(Vec::new()),
            fail_clicks: true,
        });
        let (buffer, _worker) = ClickBuffer::new(
            repository,
            crate::config::ClickBufferConfig {
                capacity: 1,
                batch_size: 1,
                flush_interval: Duration::from_secs(60),
            },
        )?;

        assert_eq!(
            buffer.submit(click_event()?).await,
            ClickSubmissionOutcome::Queued
        );
        assert_eq!(
            buffer.submit(click_event()?).await,
            ClickSubmissionOutcome::Unavailable,
            "overflow plus unavailable durable storage must be visible to the HTTP boundary"
        );
        assert_eq!(
            buffer.metrics().snapshot(),
            ClickBufferSnapshot {
                queued: 1,
                persisted: 0,
                overflowed: 1,
                overflow_recovered: 0,
                dropped: 1,
                persistence_failed: 1,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_flush_is_bounded_to_one_batch() -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(FakeRepository::default());
        let (buffer, worker) = ClickBuffer::new(
            repository,
            crate::config::ClickBufferConfig {
                capacity: 4,
                batch_size: 2,
                flush_interval: Duration::from_secs(60),
            },
        )?;
        for _ in 0..4 {
            assert_eq!(
                buffer.submit(click_event()?).await,
                ClickSubmissionOutcome::Queued
            );
        }
        let (shutdown_sender, shutdown) = watch::channel(true);
        worker.run(shutdown).await;
        drop(shutdown_sender);

        let snapshot = buffer.metrics().snapshot();
        assert_eq!(snapshot.queued, 4);
        assert_eq!(snapshot.persisted, 2);
        assert_eq!(snapshot.overflowed, 0);
        assert_eq!(snapshot.overflow_recovered, 0);
        assert_eq!(snapshot.dropped, 2);
        assert_eq!(snapshot.persistence_failed, 0);
        Ok(())
    }
}
