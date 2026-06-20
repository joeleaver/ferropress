//! `Redirect` — old-slug -> new-target redirects, a first-class ADD (WP only
//! does this via plugins). Populated automatically on slug change and editable
//! by hand. The router/serve layer consults these to emit 301s, and the static
//! build can bake them into the host's redirect map.

use crate::value::ObjectId;

#[derive(Debug, Clone, PartialEq)]
pub struct Redirect {
    pub id: Option<ObjectId>,
    /// The old path, unique + indexed for lookup (e.g. `"/old-post"`).
    pub from_path: String,
    /// The destination path or URL.
    pub to_path: String,
    /// HTTP status (301 permanent / 302 temporary).
    pub status_code: u32,
}
