//! Tenant-uploaded media: operator-provided files (the join-ask app
//! screenshot first) stored as rows so blue/green containers, the worker and
//! backups share one source of bytes without a shared volume.
//!
//! Two routes, two trust levels:
//!
//! - `POST /v1/control-plane/media` — the upload. Control-plane bearer, raw
//!   image body. The type is what the bytes say they are (magic-byte sniff),
//!   never what a header claims; png/jpeg/webp only. Deduped on
//!   `(workspace_id, sha256)` — the same file uploads to the same object.
//! - `GET /v1/public/media/{id}` — the read. Public on purpose: Meta's
//!   crawler fetches the URL when a post publishes, the same way it fetches
//!   a press-asset URL. The id is an unguessable uuid and the table has no
//!   listing route, so the surface is the rows the tenant uploaded. Rows are
//!   append-only — no update path exists — so the immutable cache header is
//!   honest.
//!
//! The URL the upload returns is minted from `CROWDRELAY_PUBLIC_API_ORIGIN`:
//! the origin Meta can actually reach. When it is unset the upload still
//! stores the bytes but returns a relative path — the setting validator for
//! `join_ask_image_url` requires `https://`, so an operator sees a relative
//! URL as "the origin is not configured", not as a broken image later.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::CACHE_CONTROL, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sha2::Digest;
use uuid::Uuid;

use crate::{Problem, request_id};

/// The public read surface, merged from `routing.rs`. Kept here so the route
/// table stays inside the source-size ratchet — the same pattern
/// `synesthesia::gated_public_router` already uses.
pub(crate) fn public_routes() -> axum::Router<crate::AppState> {
    axum::Router::new()
        // Public on purpose: Meta's crawler fetches the image when a post
        // carrying it publishes. Unguessable uuid ids, no listing route.
        .route("/v1/public/media/{id}", axum::routing::get(get_media))
}

/// Largest image Meta will fetch for a post; anything past it is rejected
/// before the row exists. The route raises the body limit to this — the
/// control-plane router default (8 KiB) is for JSON payloads, not files.
pub const MAX_MEDIA_BODY_BYTES: usize = 8 * 1024 * 1024;

const PUBLIC_IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// What the sniffed bytes say the image is — the stored content type the
/// public GET serves. A file that is not one of these is not an image we
/// publish; the upload refuses it.
#[derive(Clone, Copy)]
enum SniffedImage {
    Png,
    Jpeg,
    WebP,
}

impl SniffedImage {
    fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
        }
    }
}

fn sniff_image(bytes: &[u8]) -> Option<SniffedImage> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(SniffedImage::Png);
    }
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        return Some(SniffedImage::Jpeg);
    }
    if bytes.len() >= 12
        && bytes.starts_with(b"RIFF")
        && bytes.get(8..12) == Some(b"WEBP".as_slice())
    {
        return Some(SniffedImage::WebP);
    }
    None
}

/// A filename is display text only — it is never used as a path, a query
/// parameter or a header value. Trimmed to basename-ish characters so what
/// the panel shows is what was uploaded, not markup.
fn sanitize_name(raw: &str) -> String {
    raw.rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .take(128)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[derive(Serialize)]
pub struct UploadedMedia {
    pub id: Uuid,
    pub url: String,
    #[serde(rename = "contentType")]
    pub content_type: &'static str,
    #[serde(rename = "byteLen")]
    pub byte_len: i32,
    pub name: String,
}

/// `POST /v1/control-plane/media` — body is the raw image; the optional
/// `X-Media-Name` header carries the original filename for display.
pub async fn upload_media(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id_value = request_id(&headers);
    if body.is_empty() {
        return Problem::bad_request_because("the media body is empty", request_id_value)
            .into_response();
    }
    if body.len() > MAX_MEDIA_BODY_BYTES {
        return Problem::bad_request_because(
            "the image is over 8 MiB — Meta will not fetch it for a post",
            request_id_value,
        )
        .into_response();
    }
    let Some(kind) = sniff_image(&body) else {
        return Problem::bad_request_because(
            "not a PNG, JPEG or WebP image — the file's own bytes say what it is",
            request_id_value,
        )
        .into_response();
    };
    let name = headers
        .get("x-media-name")
        .and_then(|value| value.to_str().ok())
        .map(sanitize_name)
        .unwrap_or_default();
    let sha256 = hex::encode(sha2::Sha256::digest(&body));
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let byte_len = i32::try_from(body.len()).unwrap_or(i32::MAX);

    // Repeat uploads resolve to the existing row — same bytes, same answer,
    // no second copy.
    let id = match state
        .media
        .store(
            workspace_id,
            &sha256,
            kind.content_type(),
            byte_len,
            body.as_ref(),
            &name,
        )
        .await
    {
        Ok(id) => id,
        Err(_) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };

    let url = state
        .public_api_origin
        .as_ref()
        .map(|origin| {
            format!(
                "{}/v1/public/media/{id}",
                origin.as_str().trim_end_matches('/')
            )
        })
        .unwrap_or_else(|| format!("/v1/public/media/{id}"));

    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(UploadedMedia {
            id,
            url,
            content_type: kind.content_type(),
            byte_len,
            name,
        }),
    )
        .into_response()
}

/// `GET /v1/public/media/{id}` — public read for the crawlers that fetch the
/// URL at publish time. No auth, no listing route, unguessable uuid ids.
pub async fn get_media(State(state): State<crate::AppState>, Path(id): Path<Uuid>) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let Ok(Some(object)) = state.media.get(workspace_id, id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(content_type) = HeaderValue::from_str(&object.content_type) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, content_type),
            (CACHE_CONTROL, HeaderValue::from_static(PUBLIC_IMMUTABLE)),
        ],
        object.bytes,
    )
        .into_response()
}

const PRIVATE_NO_STORE: &str = "private, no-store";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_the_three_kinds() {
        assert!(matches!(
            sniff_image(b"\x89PNG\r\n\x1a\nrest"),
            Some(SniffedImage::Png)
        ));
        assert!(matches!(
            sniff_image(b"\xFF\xD8\xFF\xE0rest"),
            Some(SniffedImage::Jpeg)
        ));
        assert!(matches!(
            sniff_image(b"RIFF\x04\x00\x00\x00WEBPrest"),
            Some(SniffedImage::WebP)
        ));
    }

    #[test]
    fn rejects_non_images_and_truncated_headers() {
        assert!(sniff_image(b"<!DOCTYPE html>").is_none());
        assert!(sniff_image(b"GIF89a").is_none());
        assert!(sniff_image(b"RIFF").is_none());
        assert!(sniff_image(b"").is_none());
    }

    #[test]
    fn sanitizes_names_to_display_text() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a<b>.png"), "ab.png");
        assert_eq!(sanitize_name("  shot 01.PNG  "), "shot 01.PNG");
        assert_eq!(sanitize_name(""), "");
    }
}
