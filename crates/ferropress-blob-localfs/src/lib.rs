//! # ferropress-blob-localfs
//!
//! The baseline [`BlobStore`] adapter: store bytes on the local filesystem under
//! a single root directory. Backs BOTH media originals and the prerendered HTML
//! output cache (the serve layer puts rendered pages here; the DB only ever holds
//! the [`BlobKey`], never the bytes).
//!
//! Key mapping: a [`BlobKey`] is a relative, slash-delimited path under `root`.
//! To stay portable and safe, the adapter rejects keys that would escape `root`
//! (absolute paths, `..` components) — see [`LocalFsBlobStore::resolve`].
//!
//! All I/O goes through `tokio::fs` so the async [`BlobStore`] methods never
//! block the runtime. A future object-store adapter (S3/GCS/jkbase blob) is a
//! separate crate implementing the same port.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;

use ferropress_core::error::CoreError;
use ferropress_core::error::Result as CoreResult;
use ferropress_core::ports::{BlobKey, BlobStore};

/// Local-filesystem blob storage rooted at a single directory.
#[derive(Debug, Clone)]
pub struct LocalFsBlobStore {
    root: PathBuf,
}

impl LocalFsBlobStore {
    /// Create a store rooted at `root`. The directory is created on first write
    /// (per-key parent `create_dir_all`), so construction itself does no I/O.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The configured root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a [`BlobKey`] to a path under `root`, rejecting any key that could
    /// escape the root via an absolute component, a `..` segment, a NUL byte, or
    /// a backslash (which some platforms treat as a separator). Returns
    /// `CoreError::Validation` for an unsafe or empty key.
    ///
    /// This is the path-traversal guard — it must run before every filesystem op.
    fn resolve(&self, key: &BlobKey) -> CoreResult<PathBuf> {
        let raw = key.0.as_str();

        if raw.is_empty() {
            return Err(CoreError::Validation("blob key is empty".into()));
        }
        // NUL bytes can't appear in a path and may truncate it at the syscall
        // boundary; reject outright. Backslashes are rejected because they are a
        // path separator on some platforms and would let a key sidestep the
        // slash-delimited component checks below.
        if raw.contains('\0') {
            return Err(CoreError::Validation(format!(
                "blob key {raw:?} contains a NUL byte"
            )));
        }
        if raw.contains('\\') {
            return Err(CoreError::Validation(format!(
                "blob key {raw:?} contains a backslash"
            )));
        }

        // The key is a relative, slash-delimited path. Reject any leading slash
        // (absolute) before we even split, so "/etc/passwd" can't be read as a
        // sequence of harmless-looking components.
        if raw.starts_with('/') {
            return Err(CoreError::Validation(format!(
                "blob key {raw:?} must be relative (no leading '/')"
            )));
        }

        let mut path = self.root.clone();
        let mut pushed_any = false;

        // Walk the key as a `Path` and inspect each component. Only plain normal
        // segments are allowed; `.` is skipped as no-op noise, everything else
        // (`..`, a root/prefix component, …) is a traversal attempt and rejected.
        for component in Path::new(raw).components() {
            match component {
                Component::Normal(seg) => {
                    path.push(seg);
                    pushed_any = true;
                }
                Component::CurDir => {
                    // "." / "a/./b" — harmless noise, drop it.
                }
                Component::ParentDir => {
                    return Err(CoreError::Validation(format!(
                        "blob key {raw:?} contains a '..' component"
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(CoreError::Validation(format!(
                        "blob key {raw:?} must be relative (no root/prefix)"
                    )));
                }
            }
        }

        // A key made entirely of "." segments (e.g. "." or "./") resolves to the
        // root itself, which is not a blob path.
        if !pushed_any {
            return Err(CoreError::Validation(format!(
                "blob key {raw:?} does not name a file under root"
            )));
        }

        Ok(path)
    }
}

#[async_trait]
impl BlobStore for LocalFsBlobStore {
    async fn put(&self, key: &BlobKey, bytes: Vec<u8>) -> CoreResult<()> {
        let path = self.resolve(key)?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::Store(e.to_string()))?;
        }

        // Write atomically-ish: write to a sibling temp file in the same parent
        // directory, then rename over the target. Rename within a directory is
        // atomic on POSIX, so a concurrent `get` sees either the old bytes or the
        // new ones — never a half-written file. Overwrites by design.
        let tmp = tmp_sibling(&path);
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(|e| CoreError::Store(e.to_string()))?;

        match tokio::fs::rename(&tmp, &path).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Best-effort cleanup of the orphaned temp file before surfacing
                // the rename failure.
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(CoreError::Store(e.to_string()))
            }
        }
    }

    async fn get(&self, key: &BlobKey) -> CoreResult<Vec<u8>> {
        let path = self.resolve(key)?;
        tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                // `id` is not meaningful for a path-addressed key.
                CoreError::NotFound {
                    type_name: "blob".into(),
                    id: 0,
                }
            } else {
                CoreError::Store(e.to_string())
            }
        })
    }

    async fn delete(&self, key: &BlobKey) -> CoreResult<()> {
        let path = self.resolve(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // Deleting a missing key is a no-op per the port contract.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::Store(e.to_string())),
        }
    }

    async fn exists(&self, key: &BlobKey) -> CoreResult<bool> {
        let path = self.resolve(key)?;
        tokio::fs::try_exists(&path)
            .await
            .map_err(|e| CoreError::Store(e.to_string()))
    }
}

/// Build a temp-file path that is a sibling of `path` (same parent directory, so
/// the subsequent rename stays within one filesystem and is atomic). The suffix
/// is derived from the destination file name plus a process+timestamp tag to
/// avoid colliding with a concurrent `put` of the same key.
fn tmp_sibling(path: &Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "blob".to_string());

    let tmp_name = format!(".{name}.{pid}.{nanos}.tmp");
    match path.parent() {
        Some(parent) => parent.join(tmp_name),
        None => PathBuf::from(tmp_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, LocalFsBlobStore) {
        let dir = TempDir::new().expect("create temp dir");
        let store = LocalFsBlobStore::new(dir.path());
        (dir, store)
    }

    #[tokio::test]
    async fn put_get_delete_roundtrip() {
        let (_dir, store) = store();
        let key = BlobKey("media/2026/photo.bin".into());
        let bytes = b"hello ferropress".to_vec();

        // Absent before put.
        assert!(!store.exists(&key).await.unwrap());

        // put creates nested parent dirs and writes.
        store.put(&key, bytes.clone()).await.unwrap();
        assert!(store.exists(&key).await.unwrap());
        assert_eq!(store.get(&key).await.unwrap(), bytes);

        // put overwrites by design.
        let bytes2 = b"new contents".to_vec();
        store.put(&key, bytes2.clone()).await.unwrap();
        assert_eq!(store.get(&key).await.unwrap(), bytes2);

        // delete removes; afterwards it's gone.
        store.delete(&key).await.unwrap();
        assert!(!store.exists(&key).await.unwrap());
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (_dir, store) = store();
        let key = BlobKey("never/written.bin".into());
        // Deleting a missing key is Ok per the contract.
        store.delete(&key).await.unwrap();
        store.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let (_dir, store) = store();
        let key = BlobKey("missing.bin".into());
        match store.get(&key).await {
            Err(CoreError::NotFound { type_name, id }) => {
                assert_eq!(type_name, "blob");
                assert_eq!(id, 0);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn traversal_key_is_rejected() {
        let (_dir, store) = store();
        for raw in [
            "../escape",
            "a/../../escape",
            "/etc/passwd",
            "",
            ".",
            "./",
            "win\\path",
            "has\0nul",
        ] {
            let key = BlobKey(raw.into());
            let err = store
                .resolve(&key)
                .expect_err(&format!("key {raw:?} should be rejected"));
            assert!(
                matches!(err, CoreError::Validation(_)),
                "key {raw:?} should be a Validation error, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn unsafe_key_blocks_io_ops() {
        let (_dir, store) = store();
        let key = BlobKey("../escape".into());
        // The guard runs before any FS op, so every method rejects the key
        // without touching the filesystem.
        assert!(store.put(&key, vec![1, 2, 3]).await.is_err());
        assert!(store.get(&key).await.is_err());
        assert!(store.delete(&key).await.is_err());
        assert!(store.exists(&key).await.is_err());
    }

    #[tokio::test]
    async fn dot_segments_are_normalized() {
        let (_dir, store) = store();
        // Interior "." segments are harmless noise and should resolve fine.
        let key = BlobKey("a/./b/c.bin".into());
        store.put(&key, b"ok".to_vec()).await.unwrap();
        assert_eq!(store.get(&key).await.unwrap(), b"ok".to_vec());
        // The resolved path stays under root.
        let resolved = store.resolve(&key).unwrap();
        assert!(resolved.starts_with(store.root()));
        assert!(resolved.ends_with("a/b/c.bin"));
    }
}
