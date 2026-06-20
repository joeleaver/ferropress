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

use std::path::{Path, PathBuf};

use async_trait::async_trait;

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

    /// Resolve a [`BlobKey`] to an absolute path under `root`, rejecting any key
    /// that contains an absolute component or a `..` that would escape the root.
    /// Returns `CoreError::Validation` for an unsafe key.
    fn resolve(&self, _key: &BlobKey) -> CoreResult<PathBuf> {
        // TODO: split key.0 on '/', reject "" / "." noise, reject ".." and any
        // absolute component, then join onto self.root. This is the path-traversal
        // guard — it must run before every filesystem op.
        todo!("validate the BlobKey is root-relative + traversal-free, then join")
    }
}

#[async_trait]
impl BlobStore for LocalFsBlobStore {
    async fn put(&self, key: &BlobKey, _bytes: Vec<u8>) -> CoreResult<()> {
        let _path = self.resolve(key)?;
        // TODO: create_dir_all(path.parent()), then tokio::fs::write(path, bytes);
        // map io::Error -> CoreError::Store(e.to_string()). Overwrites by design.
        todo!("create parent dirs then tokio::fs::write the bytes")
    }

    async fn get(&self, key: &BlobKey) -> CoreResult<Vec<u8>> {
        let _path = self.resolve(key)?;
        // TODO: tokio::fs::read(path); map ErrorKind::NotFound ->
        // CoreError::NotFound { type_name: "blob", id: 0 } (id is not meaningful
        // for a path key), other io errors -> CoreError::Store.
        todo!("tokio::fs::read; map NotFound -> CoreError::NotFound")
    }

    async fn delete(&self, key: &BlobKey) -> CoreResult<()> {
        let _path = self.resolve(key)?;
        // TODO: tokio::fs::remove_file(path); treat ErrorKind::NotFound as Ok
        // (delete is idempotent per the port contract); other errors -> Store.
        todo!("tokio::fs::remove_file; NotFound is Ok (idempotent)")
    }

    async fn exists(&self, key: &BlobKey) -> CoreResult<bool> {
        let _path = self.resolve(key)?;
        // TODO: tokio::fs::try_exists(path); map io errors -> CoreError::Store.
        todo!("tokio::fs::try_exists -> bool")
    }
}
