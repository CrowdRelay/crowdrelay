//! Tenant-uploaded media rows — the storage half of
//! `crowdrelay-api::media`, which owns the trust decisions (sniffing, size,
//! naming). This repository owns only the bytes: insert with dedupe, read
//! back for the public GET. The api-sql ratchet keeps the write out of the
//! HTTP layer; that is the whole reason this module exists.

use sqlx::PgPool;
use uuid::Uuid;

#[derive(Clone)]
pub struct MediaRepository {
    pool: PgPool,
}

/// What the public read returns: the stored content type (what the sniffer
/// decided at upload, never a header's claim) and the bytes.
pub struct MediaObject {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl MediaRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Stores the object, or returns the id of the row the dedupe key already
    /// holds — the same file uploads to the same object.
    pub async fn store(
        &self,
        workspace_id: Uuid,
        sha256: &str,
        content_type: &str,
        byte_len: i32,
        bytes: &[u8],
        name: &str,
    ) -> Result<Uuid, sqlx::Error> {
        let inserted: Option<Uuid> = sqlx::query_scalar(
            r#"
            INSERT INTO media_objects (workspace_id, sha256, content_type, byte_len, bytes, name)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (workspace_id, sha256) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(sha256)
        .bind(content_type)
        .bind(byte_len)
        .bind(bytes)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(id) = inserted {
            return Ok(id);
        }
        // The conflict path — a repeat upload resolves to the existing row.
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM media_objects WHERE workspace_id = $1 AND sha256 = $2",
        )
        .bind(workspace_id)
        .bind(sha256)
        .fetch_optional(&self.pool)
        .await?
        // A row that vanished between the two statements is not a state this
        // table produces — append-only — so None here is a database error in
        // disguise; surface it as one rather than inventing an id.
        .ok_or(sqlx::Error::RowNotFound)
    }

    /// The public read. Unguessable uuid, no listing route — the exposure is
    /// the rows a tenant uploaded. Scoped to the workspace anyway: an id that
    /// exists under another tenant answers not-found, not a cross-tenant
    /// image, and the ratchet keeps it that way.
    pub async fn get(
        &self,
        workspace_id: Uuid,
        id: Uuid,
    ) -> Result<Option<MediaObject>, sqlx::Error> {
        sqlx::query_as::<_, (String, Vec<u8>)>(
            "SELECT content_type, bytes FROM media_objects WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map(|row| {
            row.map(|(content_type, bytes)| MediaObject {
                content_type,
                bytes,
            })
        })
    }
}
