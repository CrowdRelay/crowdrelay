//! `AreaAdminRepository` implementation kept separate from shared row/validation helpers.

use async_trait::async_trait;
use crowdrelay_application::AreaAdminRepository;

use super::*;

#[async_trait]
impl AreaAdminRepository for PostgresAreaAdminRepository {
    async fn enabled(&self, workspace_id: WorkspaceId) -> Result<bool, AreaAdminError> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT COALESCE(
                (SELECT enabled FROM area_workspace_settings WHERE workspace_id=$1),
                false
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map_repo)
    }

    async fn set_enabled(
        &self,
        workspace_id: WorkspaceId,
        enabled: bool,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<bool, AreaAdminError> {
        let workspace_id = workspace_id.into_uuid();
        let mut tx = self.pool.begin().await.map_err(map_repo)?;
        let previous = sqlx::query_scalar::<_, bool>(
            "SELECT COALESCE((SELECT enabled FROM area_workspace_settings WHERE workspace_id=$1), false)",
        )
        .bind(workspace_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_repo)?;
        sqlx::query(
            r#"
            INSERT INTO area_workspace_settings (workspace_id, enabled)
            VALUES ($1, $2)
            ON CONFLICT (workspace_id) DO UPDATE
            SET enabled=EXCLUDED.enabled, updated_at=now()
            "#,
        )
        .bind(workspace_id)
        .bind(enabled)
        .execute(&mut *tx)
        .await
        .map_err(map_repo)?;
        if previous != enabled {
            audit_tx(
                &mut tx,
                workspace_id,
                "area.settings.updated",
                "area_settings",
                &workspace_id.to_string(),
                actor,
                request_id,
                json!({"enabled": enabled}),
            )
            .await?;
        }
        tx.commit().await.map_err(map_repo)?;
        Ok(enabled)
    }

    async fn list_cities(
        &self,
        query: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AreaCity>, AreaAdminError> {
        let term = query
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!("%{}%", value.to_lowercase()));
        let rows = sqlx::query_as::<_, CityRow>(
            r#"
            SELECT id, slug, name, country_code, region, latitude, longitude, moderation_status
            FROM cities
            WHERE ($1::text IS NULL OR lower(name) LIKE $1 OR lower(slug) LIKE $1)
              AND moderation_status='approved'
            ORDER BY request_count DESC, name, id
            LIMIT $2
            "#,
        )
        .bind(term)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await
        .map_err(map_repo)?;
        Ok(rows.into_iter().map(AreaCity::from).collect())
    }

    async fn create_city(
        &self,
        workspace_id: WorkspaceId,
        city: CreateAreaCityCommand,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaCity, AreaAdminError> {
        let slug = city.slug.trim().to_ascii_lowercase();
        let name = city.name.trim().to_owned();
        let country_code = city.country_code.trim().to_ascii_uppercase();
        let region = city
            .region
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or(AreaAdminError::Conflict("CITY_REGION_REQUIRED"))?;
        if !valid_city_slug(&slug)
            || name.is_empty()
            || country_code.len() != 2
            || !country_code.bytes().all(|byte| byte.is_ascii_uppercase())
            || !valid_public_coordinates(city.latitude, city.longitude)
        {
            return Err(AreaAdminError::Conflict("INVALID_CITY"));
        }

        let workspace_id = workspace_id.into_uuid();
        let mut tx = self.pool.begin().await.map_err(map_repo)?;
        let existing = sqlx::query_as::<_, CityRow>(
            r#"
            SELECT id, slug, name, country_code, region, latitude, longitude, moderation_status
            FROM cities
            WHERE country_code=$1 AND slug=$2
            FOR SHARE
            "#,
        )
        .bind(&country_code)
        .bind(&slug)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_repo)?;
        if let Some(existing) = existing {
            let same = existing.name == name
                && existing.region.as_deref() == Some(region.as_str())
                && existing.latitude == Some(city.latitude)
                && existing.longitude == Some(city.longitude)
                && existing.moderation_status == "approved";
            if same {
                tx.commit().await.map_err(map_repo)?;
                return Ok(existing.into());
            }
            return Err(AreaAdminError::Conflict("CITY_ALREADY_EXISTS"));
        }

        let inserted = sqlx::query_as::<_, CityRow>(
            r#"
            INSERT INTO cities (
                id, slug, name, country_code, region, latitude, longitude,
                moderation_status, request_count
            )
            VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6, 'approved', 0)
            ON CONFLICT (country_code, slug) DO NOTHING
            RETURNING id, slug, name, country_code, region, latitude, longitude, moderation_status
            "#,
        )
        .bind(&slug)
        .bind(&name)
        .bind(&country_code)
        .bind(&region)
        .bind(city.latitude)
        .bind(city.longitude)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_repo)?;
        let inserted = if let Some(row) = inserted {
            row
        } else {
            let existing = sqlx::query_as::<_, CityRow>(
                r#"
                SELECT id, slug, name, country_code, region, latitude, longitude, moderation_status
                FROM cities
                WHERE country_code=$1 AND slug=$2
                FOR SHARE
                "#,
            )
            .bind(&country_code)
            .bind(&slug)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_repo)?;
            let same = existing.name == name
                && existing.region.as_deref() == Some(region.as_str())
                && existing.latitude == Some(city.latitude)
                && existing.longitude == Some(city.longitude)
                && existing.moderation_status == "approved";
            if same {
                tx.commit().await.map_err(map_repo)?;
                return Ok(existing.into());
            }
            return Err(AreaAdminError::Conflict("CITY_ALREADY_EXISTS"));
        };
        audit_tx(
            &mut tx,
            workspace_id,
            "area.city.created",
            "city",
            &inserted.id.to_string(),
            actor,
            request_id,
            json!({"slug": inserted.slug, "countryCode": inserted.country_code}),
        )
        .await?;
        tx.commit().await.map_err(map_repo)?;
        Ok(inserted.into())
    }

    async fn list_drops(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<AreaDropSummary>, AreaAdminError> {
        let workspace_uuid = workspace_id.into_uuid();
        let query = format!(
            "{DROP_SELECT} WHERE d.workspace_id=$1 ORDER BY d.archived_at NULLS FIRST, d.sort_order, d.number, d.id"
        );
        let rows = sqlx::query_as::<_, DropRow>(&query)
            .bind(workspace_uuid)
            .fetch_all(&self.pool)
            .await
            .map_err(map_repo)?;
        let mut items = rows
            .into_iter()
            .map(detail_from_row)
            .map(|detail| detail.map(|detail| detail.summary))
            .collect::<Result<Vec<_>, _>>()?;

        let draft_query = format!("{DRAFT_ONLY_SELECT} ORDER BY draft.created_at, draft.drop_id");
        let drafts = sqlx::query_as::<_, DraftOnlyRow>(&draft_query)
            .bind(workspace_uuid)
            .fetch_all(&self.pool)
            .await
            .map_err(map_repo)?;
        for draft in drafts {
            items.push(detail_from_draft_only(draft)?.summary);
        }
        Ok(items)
    }

    async fn get_drop(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        let workspace_uuid = workspace_id.into_uuid();
        match get_row_pool(&self.pool, workspace_uuid, drop_id).await {
            Ok(row) => detail_from_row(row),
            Err(AreaAdminError::NotFound) => detail_from_draft_only(
                get_draft_only_pool(&self.pool, workspace_uuid, drop_id).await?,
            ),
            Err(error) => Err(error),
        }
    }

    async fn create_draft(
        &self,
        workspace_id: WorkspaceId,
        command: CreateAreaDropCommand,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        if !valid_area_drop_id(&command.drop_id) || !draft_storage_safe(&command.draft) {
            return Err(AreaAdminError::Conflict("INVALID_DRAFT"));
        }
        let workspace_uuid = workspace_id.into_uuid();
        let payload = serde_json::to_value(&command.draft)
            .map_err(|error| AreaAdminError::Repository(error.to_string()))?;
        let mut tx = self.pool.begin().await.map_err(map_repo)?;

        let published_exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM area_drops WHERE workspace_id=$1 AND id=$2)",
        )
        .bind(workspace_uuid)
        .bind(&command.drop_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_repo)?;
        if published_exists {
            return Err(AreaAdminError::Conflict("DROP_ALREADY_EXISTS"));
        }

        let city = sqlx::query_as::<_, CityRow>(
            r#"
            SELECT id, slug, name, country_code, region, latitude, longitude, moderation_status
            FROM cities
            WHERE id=$1 AND moderation_status='approved'
            "#,
        )
        .bind(command.draft.city_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_repo)?
        .ok_or(AreaAdminError::Conflict("CITY_UNKNOWN"))?;
        if city
            .region
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            return Err(AreaAdminError::Conflict("CITY_REGION_REQUIRED"));
        }

        sqlx::query(
            r#"
            INSERT INTO area_drop_drafts (workspace_id, drop_id, base_revision, payload)
            VALUES ($1,$2,0,$3)
            "#,
        )
        .bind(workspace_uuid)
        .bind(&command.drop_id)
        .bind(payload)
        .execute(&mut *tx)
        .await
        .map_err(map_draft_write)?;
        audit_tx(
            &mut tx,
            workspace_uuid,
            "area.drop.draft.created",
            "area_drop",
            &command.drop_id,
            actor,
            request_id,
            json!({"baseRevision": 0}),
        )
        .await?;
        tx.commit().await.map_err(map_repo)?;
        self.get_drop(workspace_id, &command.drop_id).await
    }

    async fn save_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        base_revision: i64,
        draft: AreaDropDraft,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        if !valid_area_drop_id(drop_id) {
            return Err(AreaAdminError::Conflict("INVALID_DROP_ID"));
        }
        if !draft_storage_safe(&draft) {
            return Err(AreaAdminError::Conflict("INVALID_DRAFT"));
        }
        let workspace_uuid = workspace_id.into_uuid();
        let payload = serde_json::to_value(&draft)
            .map_err(|error| AreaAdminError::Repository(error.to_string()))?;
        let mut tx = self.pool.begin().await.map_err(map_repo)?;
        let current_revision = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT revision
            FROM area_drops
            WHERE workspace_id=$1 AND id=$2 AND archived_at IS NULL
            FOR UPDATE
            "#,
        )
        .bind(workspace_uuid)
        .bind(drop_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_repo)?;

        let expected_revision = if let Some(revision) = current_revision {
            revision
        } else {
            let stored_base = sqlx::query_scalar::<_, i64>(
                r#"
                SELECT base_revision
                FROM area_drop_drafts
                WHERE workspace_id=$1 AND drop_id=$2
                FOR UPDATE
                "#,
            )
            .bind(workspace_uuid)
            .bind(drop_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_repo)?
            .ok_or(AreaAdminError::NotFound)?;
            if stored_base != 0 {
                return Err(AreaAdminError::Conflict("REVISION_CONFLICT"));
            }
            0
        };
        if expected_revision != base_revision {
            return Err(AreaAdminError::Conflict("REVISION_CONFLICT"));
        }

        sqlx::query(
            r#"
            INSERT INTO area_drop_drafts (workspace_id, drop_id, base_revision, payload)
            VALUES ($1,$2,$3,$4)
            ON CONFLICT (workspace_id, drop_id) DO UPDATE
            SET base_revision=EXCLUDED.base_revision,
                payload=EXCLUDED.payload,
                updated_at=now()
            "#,
        )
        .bind(workspace_uuid)
        .bind(drop_id)
        .bind(expected_revision)
        .bind(payload)
        .execute(&mut *tx)
        .await
        .map_err(map_draft_write)?;
        audit_tx(
            &mut tx,
            workspace_uuid,
            "area.drop.draft.updated",
            "area_drop",
            drop_id,
            actor,
            request_id,
            json!({"baseRevision": expected_revision}),
        )
        .await?;
        tx.commit().await.map_err(map_repo)?;
        self.get_drop(workspace_id, drop_id).await
    }

    async fn discard_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<(), AreaAdminError> {
        let workspace_uuid = workspace_id.into_uuid();
        let mut tx = self.pool.begin().await.map_err(map_repo)?;
        let exists = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT base_revision
            FROM area_drop_drafts
            WHERE workspace_id=$1 AND drop_id=$2
            FOR UPDATE
            "#,
        )
        .bind(workspace_uuid)
        .bind(drop_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_repo)?;
        if exists.is_none() {
            return Err(AreaAdminError::Conflict("DRAFT_MISSING"));
        }
        sqlx::query("DELETE FROM area_drop_drafts WHERE workspace_id=$1 AND drop_id=$2")
            .bind(workspace_uuid)
            .bind(drop_id)
            .execute(&mut *tx)
            .await
            .map_err(map_repo)?;
        audit_tx(
            &mut tx,
            workspace_uuid,
            "area.drop.draft.discarded",
            "area_drop",
            drop_id,
            actor,
            request_id,
            json!({}),
        )
        .await?;
        tx.commit().await.map_err(map_repo)?;
        Ok(())
    }

    async fn validate_draft(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
    ) -> Result<AreaValidationResult, AreaAdminError> {
        let detail = self.get_drop(workspace_id, drop_id).await?;
        let Some(draft) = detail.draft.as_ref() else {
            return Err(AreaAdminError::Conflict("DRAFT_MISSING"));
        };
        let issues = validation_issues_pool(
            &self.pool,
            workspace_id.into_uuid(),
            drop_id,
            &detail,
            draft,
        )
        .await?;
        let valid = issues.iter().all(|issue| issue.confirmation_required);
        Ok(AreaValidationResult { valid, issues })
    }

    async fn publish(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        confirmations: &[String],
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        let workspace_uuid = workspace_id.into_uuid();
        let mut tx = self.pool.begin().await.map_err(map_repo)?;
        let published_state =
            sqlx::query_as::<_, (i64, Option<OffsetDateTime>, Option<OffsetDateTime>)>(
                r#"
            SELECT revision, published_at, archived_at
            FROM area_drops
            WHERE workspace_id=$1 AND id=$2
            FOR UPDATE
            "#,
            )
            .bind(workspace_uuid)
            .bind(drop_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_repo)?;
        if published_state
            .as_ref()
            .is_some_and(|(_, _, archived_at)| archived_at.is_some())
        {
            return Err(AreaAdminError::Conflict("DROP_ARCHIVED"));
        }

        let (base_revision, payload) = sqlx::query_as::<_, (i64, Value)>(
            r#"
            SELECT base_revision, payload
            FROM area_drop_drafts
            WHERE workspace_id=$1 AND drop_id=$2
            FOR UPDATE
            "#,
        )
        .bind(workspace_uuid)
        .bind(drop_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_repo)?
        .ok_or(AreaAdminError::Conflict("DRAFT_MISSING"))?;

        let current_revision = published_state.as_ref().map_or(0, |state| state.0);
        if current_revision != base_revision {
            return Err(AreaAdminError::Conflict("REVISION_CONFLICT"));
        }
        let draft: AreaDropDraft = serde_json::from_value(payload)
            .map_err(|error| AreaAdminError::Repository(error.to_string()))?;

        let current_detail = if published_state.is_some() {
            detail_from_row(get_row_tx(&mut tx, workspace_uuid, drop_id).await?)?
        } else {
            AreaDropDetail {
                summary: AreaDropSummary {
                    id: drop_id.to_owned(),
                    number: draft.number.clone(),
                    city_id: draft.city_id,
                    city: String::new(),
                    region: String::new(),
                    status: AreaDropStatus::Draft,
                    active: false,
                    revision: 0,
                    has_draft: true,
                    has_exact_location: draft.exact_lat.is_some() && draft.exact_lng.is_some(),
                    claim_count: 0,
                    max_claims: draft.max_claims,
                    starts_at: draft.starts_at,
                    ends_at: draft.ends_at,
                },
                published: draft.clone(),
                draft: Some(draft.clone()),
                draft_base_revision: Some(0),
            }
        };
        let issues =
            validation_issues_tx(&mut tx, workspace_uuid, drop_id, &current_detail, &draft).await?;
        let hard_issues = issues
            .iter()
            .filter(|issue| !issue.confirmation_required)
            .cloned()
            .collect::<Vec<_>>();
        if !hard_issues.is_empty() {
            return Err(AreaAdminError::Invalid(hard_issues));
        }
        let missing_confirmations = issues
            .into_iter()
            .filter(|issue| {
                issue.confirmation_required && !confirmations.iter().any(|code| code == issue.code)
            })
            .collect::<Vec<_>>();
        if !missing_confirmations.is_empty() {
            return Err(AreaAdminError::Invalid(missing_confirmations));
        }

        let city = sqlx::query_as::<_, CityRow>(
            r#"
            SELECT id, slug, name, country_code, region, latitude, longitude, moderation_status
            FROM cities
            WHERE id=$1 AND moderation_status='approved'
            "#,
        )
        .bind(draft.city_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_repo)?;
        let region = city
            .region
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(AreaAdminError::Conflict("CITY_REGION_REQUIRED"))?;
        let next_revision = current_revision + 1;
        let changed = if published_state.is_some() {
            changed_area_fields(&current_detail.published, &draft)
        } else {
            vec!["created"]
        };

        if published_state.is_some() {
            sqlx::query(
                r#"
                UPDATE area_drops
                SET number=$3,
                    city_id=$4,
                    city=$5,
                    region=$6,
                    signal_city_slug=$7,
                    map_x=$8,
                    map_y=$9,
                    approximate_lat=$10,
                    approximate_lng=$11,
                    exact_lat=$12,
                    exact_lng=$13,
                    radius_meters=$14,
                    max_claims=$15,
                    starts_at=$16,
                    ends_at=$17,
                    clue_en=$18,
                    clue_pl=$19,
                    collectible_line=$20,
                    collectible_track=$21,
                    collectible_edition=$22,
                    collectible_riddle=$23,
                    sort_order=$24,
                    revision=$25,
                    published_at=COALESCE(published_at, now()),
                    updated_at=now()
                WHERE workspace_id=$1 AND id=$2
                "#,
            )
            .bind(workspace_uuid)
            .bind(drop_id)
            .bind(&draft.number)
            .bind(draft.city_id)
            .bind(&city.name)
            .bind(region)
            .bind(&city.slug)
            .bind(draft.map_x)
            .bind(draft.map_y)
            .bind(draft.approximate_lat)
            .bind(draft.approximate_lng)
            .bind(draft.exact_lat)
            .bind(draft.exact_lng)
            .bind(draft.radius_meters)
            .bind(draft.max_claims)
            .bind(draft.starts_at)
            .bind(draft.ends_at)
            .bind(&draft.clue.en)
            .bind(&draft.clue.pl)
            .bind(&draft.collectible.line)
            .bind(&draft.collectible.track)
            .bind(&draft.collectible.edition)
            .bind(&draft.collectible.riddle)
            .bind(draft.sort_order)
            .bind(next_revision)
            .execute(&mut *tx)
            .await
            .map_err(map_drop_write)?;
        } else {
            sqlx::query(
                r#"
                INSERT INTO area_drops (
                    workspace_id, id, number, city_id, city, region, signal_city_slug,
                    map_x, map_y, approximate_lat, approximate_lng, exact_lat, exact_lng,
                    radius_meters, max_claims, starts_at, ends_at, clue_en, clue_pl,
                    collectible_line, collectible_track, collectible_edition, collectible_riddle,
                    active, revision, sort_order, published_at, archived_at
                )
                VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,
                    $18,$19,$20,$21,$22,$23,false,$24,$25,now(),NULL
                )
                "#,
            )
            .bind(workspace_uuid)
            .bind(drop_id)
            .bind(&draft.number)
            .bind(draft.city_id)
            .bind(&city.name)
            .bind(region)
            .bind(&city.slug)
            .bind(draft.map_x)
            .bind(draft.map_y)
            .bind(draft.approximate_lat)
            .bind(draft.approximate_lng)
            .bind(draft.exact_lat)
            .bind(draft.exact_lng)
            .bind(draft.radius_meters)
            .bind(draft.max_claims)
            .bind(draft.starts_at)
            .bind(draft.ends_at)
            .bind(&draft.clue.en)
            .bind(&draft.clue.pl)
            .bind(&draft.collectible.line)
            .bind(&draft.collectible.track)
            .bind(&draft.collectible.edition)
            .bind(&draft.collectible.riddle)
            .bind(next_revision)
            .bind(draft.sort_order)
            .execute(&mut *tx)
            .await
            .map_err(map_drop_write)?;
        }
        sqlx::query("DELETE FROM area_drop_drafts WHERE workspace_id=$1 AND drop_id=$2")
            .bind(workspace_uuid)
            .bind(drop_id)
            .execute(&mut *tx)
            .await
            .map_err(map_repo)?;
        audit_tx(
            &mut tx,
            workspace_uuid,
            "area.drop.published",
            "area_drop",
            drop_id,
            actor,
            request_id,
            json!({"changed": changed, "revision": next_revision}),
        )
        .await?;
        tx.commit().await.map_err(map_repo)?;
        self.get_drop(workspace_id, drop_id).await
    }

    async fn set_active(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        active: bool,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        let workspace_uuid = workspace_id.into_uuid();
        let mut tx = self.pool.begin().await.map_err(map_repo)?;
        let row =
            sqlx::query_as::<_, (bool, Option<f64>, Option<f64>, Option<OffsetDateTime>, i64)>(
                r#"
            SELECT active, exact_lat, exact_lng, published_at, revision
            FROM area_drops
            WHERE workspace_id=$1 AND id=$2 AND archived_at IS NULL
            FOR UPDATE
            "#,
            )
            .bind(workspace_uuid)
            .bind(drop_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_repo)?
            .ok_or(AreaAdminError::NotFound)?;
        if active && (row.1.is_none() || row.2.is_none() || row.3.is_none()) {
            return Err(AreaAdminError::Conflict("DROP_NOT_PUBLISHABLE"));
        }
        if row.0 == active {
            tx.commit().await.map_err(map_repo)?;
            return self.get_drop(workspace_id, drop_id).await;
        }
        let next_revision = row.4 + 1;
        sqlx::query(
            "UPDATE area_drops SET active=$3, revision=$4, updated_at=now() WHERE workspace_id=$1 AND id=$2",
        )
        .bind(workspace_uuid)
        .bind(drop_id)
        .bind(active)
        .bind(next_revision)
        .execute(&mut *tx)
        .await
        .map_err(map_repo)?;
        audit_tx(
            &mut tx,
            workspace_uuid,
            if active {
                "area.drop.resumed"
            } else {
                "area.drop.paused"
            },
            "area_drop",
            drop_id,
            actor,
            request_id,
            json!({"changed": ["active"], "revision": next_revision}),
        )
        .await?;
        tx.commit().await.map_err(map_repo)?;
        self.get_drop(workspace_id, drop_id).await
    }

    async fn archive(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        let workspace_uuid = workspace_id.into_uuid();
        let mut tx = self.pool.begin().await.map_err(map_repo)?;
        let (archived_at, published_at, revision) =
            sqlx::query_as::<_, (Option<OffsetDateTime>, Option<OffsetDateTime>, i64)>(
                r#"
            SELECT archived_at, published_at, revision
            FROM area_drops
            WHERE workspace_id=$1 AND id=$2
            FOR UPDATE
            "#,
            )
            .bind(workspace_uuid)
            .bind(drop_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_repo)?
            .ok_or(AreaAdminError::NotFound)?;
        if archived_at.is_some() {
            tx.commit().await.map_err(map_repo)?;
            return self.get_drop(workspace_id, drop_id).await;
        }
        if published_at.is_none() {
            return Err(AreaAdminError::Conflict("DROP_NOT_PUBLISHED"));
        }
        let next_revision = revision + 1;
        sqlx::query(
            r#"
            UPDATE area_drops
            SET active=false, archived_at=now(), revision=$3, updated_at=now()
            WHERE workspace_id=$1 AND id=$2
            "#,
        )
        .bind(workspace_uuid)
        .bind(drop_id)
        .bind(next_revision)
        .execute(&mut *tx)
        .await
        .map_err(map_repo)?;
        sqlx::query("DELETE FROM area_drop_drafts WHERE workspace_id=$1 AND drop_id=$2")
            .bind(workspace_uuid)
            .bind(drop_id)
            .execute(&mut *tx)
            .await
            .map_err(map_repo)?;
        audit_tx(
            &mut tx,
            workspace_uuid,
            "area.drop.archived",
            "area_drop",
            drop_id,
            actor,
            request_id,
            json!({"changed": ["archivedAt", "active"], "revision": next_revision}),
        )
        .await?;
        tx.commit().await.map_err(map_repo)?;
        self.get_drop(workspace_id, drop_id).await
    }

    async fn duplicate(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        new_drop_id: &str,
        new_city_id: Uuid,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<AreaDropDetail, AreaAdminError> {
        if !valid_area_drop_id(new_drop_id) {
            return Err(AreaAdminError::Conflict("INVALID_DROP_ID"));
        }
        let source = self.get_drop(workspace_id, drop_id).await?;
        let destination = sqlx::query_as::<_, CityRow>(
            r#"
            SELECT id, slug, name, country_code, region, latitude, longitude, moderation_status
            FROM cities
            WHERE id=$1 AND moderation_status='approved'
            "#,
        )
        .bind(new_city_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_repo)?
        .ok_or(AreaAdminError::Conflict("CITY_UNKNOWN"))?;
        let (Some(public_lat), Some(public_lng)) = (destination.latitude, destination.longitude)
        else {
            return Err(AreaAdminError::Conflict("CITY_PUBLIC_LOCATION_REQUIRED"));
        };
        if destination
            .region
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            return Err(AreaAdminError::Conflict("CITY_REGION_REQUIRED"));
        }

        let mut draft = source.draft.unwrap_or(source.published);
        draft.exact_lat = None;
        draft.exact_lng = None;
        draft.city_id = new_city_id;
        draft.approximate_lat = public_lat;
        draft.approximate_lng = public_lng;
        draft.number = new_drop_id
            .rsplit_once('-')
            .map(|(_, number)| number.to_owned())
            .ok_or(AreaAdminError::Conflict("INVALID_DROP_ID"))?;
        self.create_draft(
            workspace_id,
            CreateAreaDropCommand {
                drop_id: new_drop_id.to_owned(),
                draft,
            },
            actor,
            request_id,
        )
        .await
    }

    async fn delete_unpublished(
        &self,
        workspace_id: WorkspaceId,
        drop_id: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<(), AreaAdminError> {
        let workspace_uuid = workspace_id.into_uuid();
        let mut tx = self.pool.begin().await.map_err(map_repo)?;
        let published_exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM area_drops WHERE workspace_id=$1 AND id=$2)",
        )
        .bind(workspace_uuid)
        .bind(drop_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_repo)?;
        if published_exists {
            return Err(AreaAdminError::Conflict("DROP_HAS_HISTORY"));
        }
        let draft_exists = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT base_revision
            FROM area_drop_drafts
            WHERE workspace_id=$1 AND drop_id=$2
            FOR UPDATE
            "#,
        )
        .bind(workspace_uuid)
        .bind(drop_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_repo)?;
        if draft_exists.is_none() {
            return Err(AreaAdminError::NotFound);
        }
        audit_tx(
            &mut tx,
            workspace_uuid,
            "area.drop.deleted",
            "area_drop",
            drop_id,
            actor,
            request_id,
            json!({}),
        )
        .await?;
        sqlx::query("DELETE FROM area_drop_drafts WHERE workspace_id=$1 AND drop_id=$2")
            .bind(workspace_uuid)
            .bind(drop_id)
            .execute(&mut *tx)
            .await
            .map_err(map_repo)?;
        tx.commit().await.map_err(map_repo)?;
        Ok(())
    }
}
