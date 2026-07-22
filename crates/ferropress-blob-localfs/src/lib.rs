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

    async fn delete_prefix(&self, prefix: &BlobKey) -> CoreResult<()> {
        // `resolve` drops a trailing `/` (Path::components does) and — crucially —
        // rejects a prefix that names the ROOT itself ("", ".", "./"), so a bulk
        // evict can never wipe the whole store by accident.
        let path = self.resolve(prefix)?;
        // Segment-aligned semantics fall out of the filesystem layout: the prefix
        // is either a directory (a key subtree) or a plain file (an exact key) —
        // `a/b` can never match `a/bc`.
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            // Not a directory → fall through to the single-file case.
            Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {}
            Err(e) => return Err(CoreError::Store(e.to_string())),
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::Store(e.to_string())),
        }
    }

    async fn list_prefix(&self, prefix: &BlobKey) -> CoreResult<Vec<BlobKey>> {
        let root = self.resolve(prefix)?;
        let base = prefix.0.trim_end_matches('/');

        // The prefix names an exact blob (a leaf), not a subtree.
        match tokio::fs::metadata(&root).await {
            Ok(m) if m.is_file() => return Ok(vec![BlobKey(base.to_owned())]),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(CoreError::Store(e.to_string())),
        }

        // Iterative subtree walk. `dirs` pairs each pending directory with its
        // key-space path (relative keys stay slash-delimited regardless of the
        // platform separator).
        let mut out = Vec::new();
        let mut dirs: Vec<(std::path::PathBuf, String)> = vec![(root, base.to_owned())];
        while let Some((dir, key_base)) = dirs.pop() {
            let mut entries = tokio::fs::read_dir(&dir)
                .await
                .map_err(|e| CoreError::Store(e.to_string()))?;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| CoreError::Store(e.to_string()))?
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                let child_key = format!("{key_base}/{name}");
                let ftype = entry
                    .file_type()
                    .await
                    .map_err(|e| CoreError::Store(e.to_string()))?;
                if ftype.is_dir() {
                    dirs.push((entry.path(), child_key));
                } else if !(name.starts_with('.') && name.ends_with(".tmp")) {
                    // Skip the adapter's own in-flight temp files (see
                    // [`tmp_sibling`]) — they are not blobs and vanish on rename.
                    out.push(BlobKey(child_key));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
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
    async fn delete_prefix_evicts_the_subtree_and_nothing_else() {
        let (_dir, store) = store();
        for k in [
            "prerender/listing/term/category/news.html",
            "prerender/listing/term/category/news/page/2.html",
            "prerender/listing/term/tag/rust.html",
            "prerender/listing/index.html",
            "prerender/page/about.html",
        ] {
            store.put(&BlobKey(k.into()), b"x".to_vec()).await.unwrap();
        }

        // Trailing slash tolerated; the whole subtree goes, siblings stay.
        store
            .delete_prefix(&BlobKey("prerender/listing/term/".into()))
            .await
            .unwrap();
        for gone in [
            "prerender/listing/term/category/news.html",
            "prerender/listing/term/category/news/page/2.html",
            "prerender/listing/term/tag/rust.html",
        ] {
            assert!(
                !store.exists(&BlobKey(gone.into())).await.unwrap(),
                "{gone}"
            );
        }
        for kept in ["prerender/listing/index.html", "prerender/page/about.html"] {
            assert!(store.exists(&BlobKey(kept.into())).await.unwrap(), "{kept}");
        }

        // Idempotent on an already-empty prefix; also works on an EXACT leaf key.
        store
            .delete_prefix(&BlobKey("prerender/listing/term".into()))
            .await
            .unwrap();
        store
            .delete_prefix(&BlobKey("prerender/listing/index.html".into()))
            .await
            .unwrap();
        assert!(
            !store
                .exists(&BlobKey("prerender/listing/index.html".into()))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn delete_prefix_never_names_the_root() {
        let (_dir, store) = store();
        store
            .put(&BlobKey("keep.bin".into()), b"x".to_vec())
            .await
            .unwrap();
        // The root-wipe shapes are Validation errors, not a store nuke.
        for raw in ["", ".", "./", "/"] {
            assert!(
                store.delete_prefix(&BlobKey(raw.into())).await.is_err(),
                "prefix {raw:?} must be rejected"
            );
        }
        assert!(store.exists(&BlobKey("keep.bin".into())).await.unwrap());
    }

    #[tokio::test]
    async fn list_prefix_walks_sorted_and_skips_tmp_files() {
        let (_dir, store) = store();
        for k in [
            "t/b/two.html",
            "t/a/one.html",
            "t/zero.html",
            "other/x.html",
        ] {
            store.put(&BlobKey(k.into()), b"x".to_vec()).await.unwrap();
        }
        // A lingering in-flight temp file (crashed put) must not be listed.
        tokio::fs::write(store.root().join("t/.orphan.html.1.2.tmp"), b"x")
            .await
            .unwrap();

        let keys = store.list_prefix(&BlobKey("t/".into())).await.unwrap();
        assert_eq!(
            keys.iter().map(|k| k.0.as_str()).collect::<Vec<_>>(),
            vec!["t/a/one.html", "t/b/two.html", "t/zero.html"],
        );

        // A leaf key lists itself; an empty prefix subtree lists nothing.
        assert_eq!(
            store
                .list_prefix(&BlobKey("t/zero.html".into()))
                .await
                .unwrap(),
            vec![BlobKey("t/zero.html".into())]
        );
        assert!(
            store
                .list_prefix(&BlobKey("nowhere/".into()))
                .await
                .unwrap()
                .is_empty()
        );
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
