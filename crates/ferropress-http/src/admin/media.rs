//! Media upload — `POST /admin/api/media` (multipart), gated on
//! [`Capability::UploadMedia`] (Author+). Accepts an image file plus optional `alt`
//! text, stores the original bytes in the [`BlobStore`](ferropress_core::ports::BlobStore),
//! creates a `Media` row, attributes it to the uploader, and returns the new id +
//! its served URL so the editor can insert an image block that round-trips.
//!
//! Mirrors [`posts::create`](super::posts::create): build a `FieldMap`, `store.create`,
//! then set the to-one relation via `store.link` with rollback so a failure leaves no
//! orphan. The one addition is the upload transport (multipart) and byte handling.
//!
//! ## Trust boundary
//!
//! The image type + dimensions come from SNIFFING the bytes (`imagesize`), never the
//! client's `Content-Type`: a request claiming `image/png` but carrying a script, or
//! a format we don't serve, is rejected with 400. `blob_key` is derived from a fresh
//! `uuid` (so it can't collide with the `@unique` constraint or embed a hostile
//! filename), and the raw filename is kept only as display metadata.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Multipart, State};
use serde::Serialize;

use uuid::Uuid;

use ferropress_core::MEDIA_TYPE;
use ferropress_core::media_url;
use ferropress_core::ports::BlobKey;
use ferropress_core::query::Edge;
use ferropress_core::role::Capability;
use ferropress_core::value::{FieldMap, Object, ObjectId, TypeName, Value, now_millis};

use super::{AdminError, AuthedUser, datetime_field, str_field};
use crate::AppState;

/// Max accepted image size, per file. The upload route sets a body limit a little
/// above this (see [`api_routes`](super::api_routes)) to leave room for multipart
/// framing; this constant is the semantic per-file cap the handler enforces.
pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// Max accepted alt-text length (bytes). Alt text is a short a11y description; a cap
/// stops a caller stuffing megabytes into `alt_text` AND the `@vectorize` `plaintext`
/// source (which would bloat the row + the embedding queue).
const MAX_ALT_BYTES: usize = 2000;

/// Max accepted image dimension (px, per side). Rejects a small file that DECLARES
/// enormous dimensions (a decompression bomb) before it can be stored and later served
/// to public viewers, whose browsers would decode it to a huge bitmap.
const MAX_IMAGE_DIMENSION: usize = 20_000;

/// `POST /admin/api/media` success body: the new media id + its served URL (the same
/// `/media/{id}` the public page renders and the bridge reverses on save).
#[derive(Serialize)]
pub struct UploadResponse {
    pub id: u64,
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
}

/// Handle a multipart image upload. Fields: `file` (required, the image bytes) and
/// `alt` (optional, the accessibility text). Extractor order matters: `State` +
/// `AuthedUser` (both `FromRequestParts`) precede the body-consuming `Multipart`.
pub async fn upload(
    State(state): State<AppState>,
    who: AuthedUser,
    mut multipart: Multipart,
) -> Result<Json<UploadResponse>, AdminError> {
    who.require(Capability::UploadMedia)?;

    // Collect the file bytes + filename + alt from the multipart body. Field order is
    // not assumed (we scan all fields), so a client may send `alt` before `file`.
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut filename = String::new();
    let mut alt = String::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AdminError::BadRequest(format!("malformed upload: {e}")))?
    {
        match field.name() {
            Some("file") => {
                filename = field.file_name().unwrap_or_default().to_owned();
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| AdminError::BadRequest(format!("reading the file: {e}")))?;
                if data.len() > MAX_UPLOAD_BYTES {
                    return Err(AdminError::BadRequest(format!(
                        "image is too large ({} bytes; max {MAX_UPLOAD_BYTES})",
                        data.len()
                    )));
                }
                file_bytes = Some(data.to_vec());
            }
            Some("alt") => {
                alt = field
                    .text()
                    .await
                    .map_err(|e| AdminError::BadRequest(format!("reading alt text: {e}")))?;
            }
            // Ignore any other fields rather than failing (forward-compatible).
            _ => {}
        }
    }

    let bytes =
        file_bytes.ok_or_else(|| AdminError::BadRequest("no file was uploaded".to_owned()))?;
    if bytes.is_empty() {
        return Err(AdminError::BadRequest(
            "the uploaded file is empty".to_owned(),
        ));
    }
    if alt.len() > MAX_ALT_BYTES {
        return Err(AdminError::BadRequest(format!(
            "alt text is too long ({} bytes; max {MAX_ALT_BYTES})",
            alt.len()
        )));
    }

    // Sniff the REAL image type + dimensions from the bytes — never trust the client's
    // content-type. An unrecognized format, or a non-image, is rejected here.
    let kind = imagesize::image_type(&bytes)
        .map_err(|_| AdminError::BadRequest("not a recognized image".to_owned()))?;
    let (mime, ext) = image_mime_ext(&kind)
        .ok_or_else(|| AdminError::BadRequest("unsupported image format".to_owned()))?;
    let dims = imagesize::blob_size(&bytes)
        .map_err(|_| AdminError::BadRequest("could not read image dimensions".to_owned()))?;
    // Reject absurd DECLARED dimensions before conversion — a small file claiming a
    // giant canvas is a decompression bomb aimed at every public viewer's browser.
    // (Checked as usize so a value above u32::MAX can't wrap to a small stored dim.)
    if dims.width > MAX_IMAGE_DIMENSION || dims.height > MAX_IMAGE_DIMENSION {
        return Err(AdminError::BadRequest(format!(
            "image dimensions too large ({}x{}; max {MAX_IMAGE_DIMENSION} per side)",
            dims.width, dims.height
        )));
    }
    // Safe casts: both are now ≤ MAX_IMAGE_DIMENSION, well within u32.
    let width = dims.width as u32;
    let height = dims.height as u32;
    let byte_size = bytes.len() as u64;

    // `uuid` and `blob_key` are both `@unique`; deriving the key from the fresh uuid
    // makes a collision impossible and keeps any hostile filename out of the path.
    let uuid = Uuid::now_v7().to_string();
    let blob_key = format!("media/{uuid}.{ext}");

    // Store the original bytes first. If the DB create below fails, we clean the blob
    // back up so a failure leaves neither an orphan row nor orphan bytes.
    state
        .blobs
        .put(&BlobKey(blob_key.clone()), bytes)
        .await
        .map_err(AdminError::from)?;

    let slug = slugify_filename(&filename).unwrap_or_else(|| uuid.clone());
    // The `@vectorize` source (Media.search) — the a11y text is the primary signal,
    // the filename a weak fallback. Empty parts are dropped.
    let plaintext = [alt.as_str(), filename.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    let now = now_millis();
    let mut fields: FieldMap = HashMap::new();
    fields.insert("uuid".to_owned(), Value::String(uuid.clone()));
    fields.insert("slug".to_owned(), Value::String(slug));
    fields.insert("filename".to_owned(), Value::String(filename));
    fields.insert("mime_type".to_owned(), Value::String(mime.to_owned()));
    fields.insert("byte_size".to_owned(), Value::U64(byte_size));
    fields.insert("width".to_owned(), Value::U32(width));
    fields.insert("height".to_owned(), Value::U32(height));
    fields.insert("alt_text".to_owned(), Value::String(alt));
    fields.insert("caption".to_owned(), Value::String(String::new()));
    fields.insert("description".to_owned(), Value::String(String::new()));
    fields.insert("blob_key".to_owned(), Value::String(blob_key.clone()));
    fields.insert("plaintext".to_owned(), Value::String(plaintext));
    // Focal point defaults to dead-center (art-direction crops adjust it later).
    fields.insert("focal_x".to_owned(), Value::F32(0.5));
    fields.insert("focal_y".to_owned(), Value::F32(0.5));
    fields.insert("created_at".to_owned(), Value::DateTime(now));
    fields.insert(
        "meta".to_owned(),
        Value::Json(serde_json::Value::Object(serde_json::Map::new())),
    );

    let id = match state
        .store
        .create(&TypeName::from(MEDIA_TYPE), fields)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            // Roll the just-written blob back so the failed create leaves no orphan.
            if let Err(cleanup) = state.blobs.delete(&BlobKey(blob_key.clone())).await {
                tracing::error!(error = %cleanup, blob_key, "failed to clean up media blob after create failure");
            }
            return Err(e.into());
        }
    };

    // Attribute the upload to its uploader. `uploaded_by` is a to-one RELATION set via
    // the link API (not a scalar field), exactly like posts' `author`. On failure roll
    // back BOTH the row and the blob so there is no orphan.
    let edge = Edge {
        type_name: TypeName::from(MEDIA_TYPE),
        id,
        field: "uploaded_by".to_owned(),
    };
    if let Err(e) = state.store.link(&edge, who.id, FieldMap::new()).await {
        if let Err(rb) = state.store.delete(&TypeName::from(MEDIA_TYPE), id).await {
            tracing::error!(error = %rb, "failed to roll back media row after uploaded_by link failure");
        }
        if let Err(cleanup) = state.blobs.delete(&BlobKey(blob_key)).await {
            tracing::error!(error = %cleanup, "failed to clean up media blob after link failure");
        }
        return Err(e.into());
    }

    Ok(Json(UploadResponse {
        id: id.0,
        // The public URL is keyed by the unguessable uuid, NOT the sequential id.
        url: media_url(&uuid),
        width,
        height,
        mime_type: mime.to_owned(),
    }))
}

/// One row in the media library (`GET /admin/api/media`): the object `id` (echoed
/// back by a picker to set a media reference) and its served `url` (`/media/{uuid}`,
/// for the thumbnail), plus enough metadata to label the grid. The unguessable uuid
/// lives only inside `url`, keeping the id an authenticated-admin handle — the same
/// posture as the featured-image DTO.
#[derive(Serialize)]
pub struct MediaSummary {
    pub id: u64,
    pub url: String,
    pub filename: String,
    pub alt: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub uploaded_at: Option<i64>,
}

/// `GET /admin/api/media` — the media library: the stored images this user may
/// browse, newest first, for a picker to choose from. Gated on
/// [`Capability::UploadMedia`] (Author+) — the same bar as uploading — and then scoped
/// to authorship exactly like the post list: Editor+ (`EditOthersContent`) sees the
/// whole library; a lower role sees only media they uploaded. So an Author can never
/// enumerate another author's `/media/{uuid}` URLs (which the un-authenticated public
/// route protects only by the uuid being unguessable), matching WordPress's
/// per-author media visibility.
///
/// Scans the `Media` table and sorts in Rust (v1 has no compound "ORDER BY created_at
/// DESC" primitive; `Media.created_at` is `@indexed`, and the table is small). This is
/// an admin handle to media metadata, not the public `/media` route.
pub async fn list(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<Vec<MediaSummary>>, AdminError> {
    who.require(Capability::UploadMedia)?;

    let objs: Vec<Object> = state.store.scan(&TypeName::from(MEDIA_TYPE)).await?;

    // Scope to the uploader. `uploaded_by` has no `@inverse`, so ownership is resolved
    // with ONE batched link read over the scanned ids (the id-only `get_links_many`
    // fast path — a single round-trip, not N+1), mirroring `posts::list`. A media row
    // with no `uploaded_by` (a legacy/CLI upload) belongs to no one, so it is hidden
    // from the own-only view.
    let objs: Vec<Object> = if who.can_edit_others() {
        objs
    } else {
        let ids: Vec<ObjectId> = objs.iter().map(|o| o.id).collect();
        let uploaders = state
            .store
            .get_links_many(&TypeName::from(MEDIA_TYPE), &ids, "uploaded_by")
            .await?;
        objs.into_iter()
            .zip(uploaders)
            .filter(|(_, uploaders)| uploaders.contains(&who.id))
            .map(|(obj, _)| obj)
            .collect()
    };

    let mut media: Vec<MediaSummary> = objs.iter().map(media_summary).collect();
    // Newest first; untimed rows (which shouldn't exist — created_at is always set on
    // upload) sink to the bottom.
    media.sort_by_key(|m| std::cmp::Reverse(m.uploaded_at));
    Ok(Json(media))
}

/// Project a `Media` object into a [`MediaSummary`]. The public URL is keyed by the
/// unguessable uuid; an object missing its uuid yields an empty `url` (it can't be
/// served) rather than erroring the whole listing.
fn media_summary(obj: &Object) -> MediaSummary {
    MediaSummary {
        id: obj.id.0,
        url: str_field(obj, "uuid")
            .map(|uuid| media_url(&uuid))
            .unwrap_or_default(),
        filename: str_field(obj, "filename").unwrap_or_default(),
        alt: str_field(obj, "alt_text").unwrap_or_default(),
        mime_type: str_field(obj, "mime_type").unwrap_or_default(),
        width: u32_field(obj, "width"),
        height: u32_field(obj, "height"),
        uploaded_at: datetime_field(obj, "created_at"),
    }
}

/// Read a `U32` field off an object (media dimensions), or `0` if absent / wrong type.
fn u32_field(obj: &Object, field: &str) -> u32 {
    match obj.get(field) {
        Some(Value::U32(n)) => *n,
        _ => 0,
    }
}

/// Map a sniffed image type to its canonical `(mime_type, file extension)`, or `None`
/// for a format we don't serve. Deliberately a small web-image allow-list — anything
/// else (incl. SVG, which `imagesize` can't measure and which is an XSS vector when
/// served inline) is rejected rather than stored.
fn image_mime_ext(kind: &imagesize::ImageType) -> Option<(&'static str, &'static str)> {
    use imagesize::ImageType;
    Some(match kind {
        ImageType::Jpeg => ("image/jpeg", "jpg"),
        ImageType::Png => ("image/png", "png"),
        ImageType::Gif => ("image/gif", "gif"),
        ImageType::Webp => ("image/webp", "webp"),
        ImageType::Bmp => ("image/bmp", "bmp"),
        ImageType::Tiff => ("image/tiff", "tiff"),
        _ => return None,
    })
}

/// A tidy, lowercase slug from a filename's stem. Media is served by id, so this is
/// display metadata only (`Media.slug` is `@indexed`, not `@unique`); `None` if the
/// stem has no slug-able characters.
fn slugify_filename(filename: &str) -> Option<String> {
    // Drop any path prefix, then the extension.
    let stem = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);

    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash {
                slug.push('-');
                pending_dash = false;
            }
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() {
            pending_dash = true;
        }
    }
    if slug.is_empty() { None } else { Some(slug) }
}

#[cfg(test)]
mod tests {
    use super::slugify_filename;

    #[test]
    fn slugifies_filenames() {
        assert_eq!(
            slugify_filename("My Photo.JPG").as_deref(),
            Some("my-photo")
        );
        assert_eq!(
            slugify_filename("/tmp/a  b__c.png").as_deref(),
            Some("a-b-c")
        );
        assert_eq!(
            slugify_filename("C:\\Users\\me\\Sunset.webp").as_deref(),
            Some("sunset")
        );
        assert_eq!(slugify_filename("____.png"), None);
        assert_eq!(slugify_filename(""), None);
    }
}
