//! Live redirect table for the public read path — WP-style 301 on a moved URL.
//!
//! When a `Page` (or `Post`) is renamed or re-parented its public URL moves. Rather than
//! 404 the old URL (correct, but link-losing) the admin handler records a
//! [`Redirect`](ferropress_core::REDIRECT_TYPE)`{from_path, to_path, status_code}` so the old
//! URL **301s** to the new one — SEO-preserving, WP parity.
//!
//! The table is resolved **live**, exactly like [`SiteSettings`](crate::settings) and the
//! [`AuthorDirectory`](crate::authors): an in-memory [`RedirectMap`] the regen loop refreshes
//! off the change feed. A `Redirect` create/delete rides the feed to EVERY instance, so the
//! HTTP read path 301s the moved URL on every node with no page regeneration — the
//! multi-instance-correct property the per-instance alternatives (an in-memory old-path map,
//! or an admin-only local eviction) could not offer. [`load_redirects`] seeds it at boot and
//! fully reloads it on each `Redirect` change (redirects are low-volume, so a full rescan is
//! cheap and sidesteps any incremental-staleness worry — the same discipline the settings
//! snapshot uses).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ferropress_core::REDIRECT_TYPE;
use ferropress_core::error::Result;
use ferropress_core::store::RhypeStore;
use ferropress_core::value::{TypeName, Value};
use parking_lot::RwLock;

/// The default redirect status when a `Redirect` row carries none — a permanent move.
const DEFAULT_STATUS: u16 = 301;

/// A hard cap on how many redirect hops [`RedirectMap::lookup`] will follow. Write-time
/// chain-collapse means a chain should never form, so this is purely a defensive bound so a
/// stale/hand-edited chain (or cycle) can never loop or 301 to a further redirect.
const MAX_REDIRECT_HOPS: usize = 8;

/// A redirect target: where to send the browser + the HTTP status to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectTarget {
    /// The absolute destination path (the `Location` header value), e.g. `"/company/team"`.
    pub to: String,
    /// The HTTP redirect status (301 permanent for a moved page; 302 otherwise).
    pub status: u16,
}

/// In-memory map from a normalized request path (absolute, no trailing slash) to its
/// redirect target. Cheap to clone (reloaded wholesale on a `Redirect` change) and read (a
/// `HashMap` lookup, no store round-trip).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedirectMap {
    by_from: HashMap<String, RedirectTarget>,
}

impl RedirectMap {
    /// Build a map from `(from_path, to_path, status)` triples — for tests + the loader.
    /// Paths are normalized; a self-redirect or a from-root entry is dropped (never a loop).
    pub fn from_triples(triples: impl IntoIterator<Item = (String, String, u16)>) -> Self {
        let mut map = RedirectMap::default();
        for (from, to, status) in triples {
            map.insert(&from, &to, status);
        }
        map
    }

    /// Insert one redirect, normalizing both paths. A self-redirect (`from == to`) or a
    /// from-root entry is dropped so the table can never 301 the front page or loop on itself.
    fn insert(&mut self, from: &str, to: &str, status: u16) {
        let from = normalize_path(from);
        let to = normalize_path(to);
        if from == to || from == "/" {
            return;
        }
        self.by_from.insert(from, RedirectTarget { to, status });
    }

    /// Resolve a request path to its FINAL redirect target, following a (defensive) chain to
    /// its terminus — bounded by [`MAX_REDIRECT_HOPS`] with a visited-set, so a stray chain or
    /// cycle can never loop. `None` when the path is not a redirect source. Write-time
    /// chain-collapse means the common case is a single hop.
    pub fn lookup(&self, path: &str) -> Option<RedirectTarget> {
        let start = normalize_path(path);
        let mut target = self.by_from.get(&start)?.clone();
        let mut seen: HashSet<String> = HashSet::from([start]);
        for _ in 0..MAX_REDIRECT_HOPS {
            // Stop if the destination is itself a redirect source we've already visited (a
            // cycle), or a fresh one we can collapse through.
            if !seen.insert(target.to.clone()) {
                break;
            }
            match self.by_from.get(&target.to) {
                Some(next) => target = next.clone(),
                None => break,
            }
        }
        Some(target)
    }

    /// Number of redirects in the table (tests + tracing).
    pub fn len(&self) -> usize {
        self.by_from.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.by_from.is_empty()
    }
}

/// Normalize a site path for redirect keying: ensure a single leading `/` and drop any
/// trailing `/` (the root stays `"/"`). So `"/about/team/"`, `"about/team"`, and
/// `"/about/team"` all normalize to `"/about/team"` — matching how a browser sends the path
/// and how the admin handler records it.
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        format!("/{trimmed}")
    }
}

/// Read the whole redirect table from the store (one `Redirect` scan). Used for the boot seed
/// AND the per-change reload — redirects are low-volume, so a full reload on each change is
/// cheap and robust (no from-path-edit staleness). Rows with a missing `from_path`/`to_path`,
/// a self-redirect, or a from-root are skipped (never serve a loop or shadow the front page).
pub async fn load_redirects(store: &Arc<dyn RhypeStore>) -> Result<RedirectMap> {
    let mut map = RedirectMap::default();
    for r in store.scan(&TypeName::from(REDIRECT_TYPE)).await? {
        let (Some(Value::String(from)), Some(Value::String(to))) =
            (r.get("from_path"), r.get("to_path"))
        else {
            continue;
        };
        let status = match r.get("status_code") {
            Some(Value::U32(c)) => u16::try_from(*c).unwrap_or(DEFAULT_STATUS),
            _ => DEFAULT_STATUS,
        };
        map.insert(from, to, status);
    }
    Ok(map)
}

/// A cheaply-cloneable handle to the current live [`RedirectMap`], shared between the HTTP
/// read path (which 301s a moved URL) and the regen loop (which reloads it off the feed). Same
/// read/write discipline as [`AuthorsHandle`](crate::authors::AuthorsHandle) /
/// [`SettingsHandle`](crate::settings::SettingsHandle): reads clone the inner `Arc` under a
/// short read lock; a reload swaps the `Arc` under a short write lock.
#[derive(Clone)]
pub struct RedirectHandle(Arc<RwLock<Arc<RedirectMap>>>);

impl RedirectHandle {
    /// Seed the handle with an initial table (built at startup from the store).
    pub fn new(initial: RedirectMap) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(initial))))
    }

    /// The current table snapshot (cheap `Arc` clone).
    pub fn current(&self) -> Arc<RedirectMap> {
        Arc::clone(&self.0.read())
    }

    /// Replace the current table (a full reload).
    pub fn set(&self, next: RedirectMap) {
        *self.0.write() = Arc::new(next);
    }

    /// Resolve a request path to its redirect target, if any (the HTTP read path's entry point).
    pub fn lookup(&self, path: &str) -> Option<RedirectTarget> {
        self.current().lookup(path)
    }
}

impl Default for RedirectHandle {
    /// An empty table — the state before the store is read (and what tests get without wiring
    /// redirects; every path resolves normally, none 301s).
    fn default() -> Self {
        Self::new(RedirectMap::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_is_slash_canonical() {
        assert_eq!(normalize_path("/about/team/"), "/about/team");
        assert_eq!(normalize_path("about/team"), "/about/team");
        assert_eq!(normalize_path("/about/team"), "/about/team");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path(""), "/");
    }

    #[test]
    fn lookup_matches_regardless_of_trailing_slash() {
        let map = RedirectMap::from_triples([("about".to_owned(), "/company".to_owned(), 301)]);
        let hit = map.lookup("/about/").expect("trailing slash still matches");
        assert_eq!(hit.to, "/company");
        assert_eq!(hit.status, 301);
        assert!(map.lookup("/elsewhere").is_none());
    }

    #[test]
    fn self_and_root_redirects_are_dropped() {
        let map = RedirectMap::from_triples([
            ("about".to_owned(), "/about".to_owned(), 301), // self → dropped
            ("/".to_owned(), "/home".to_owned(), 301),      // from root → dropped
            ("old".to_owned(), "/new".to_owned(), 301),
        ]);
        assert_eq!(map.len(), 1);
        assert!(map.lookup("/about").is_none());
        assert!(map.lookup("/").is_none());
        assert_eq!(map.lookup("/old").unwrap().to, "/new");
    }

    #[test]
    fn lookup_follows_a_chain_to_its_terminus() {
        // a → b → c should resolve /a straight to /c (defensive collapse at read time).
        let map = RedirectMap::from_triples([
            ("a".to_owned(), "/b".to_owned(), 301),
            ("b".to_owned(), "/c".to_owned(), 301),
        ]);
        assert_eq!(map.lookup("/a").unwrap().to, "/c");
        assert_eq!(map.lookup("/b").unwrap().to, "/c");
    }

    #[test]
    fn lookup_terminates_on_a_cycle() {
        // a → b → a: the bounded, visited-guarded follow must not loop.
        let map = RedirectMap::from_triples([
            ("a".to_owned(), "/b".to_owned(), 301),
            ("b".to_owned(), "/a".to_owned(), 301),
        ]);
        // Whichever terminus it settles on, it must return (not hang) and be one of the two.
        let hit = map.lookup("/a").expect("still resolves");
        assert!(hit.to == "/a" || hit.to == "/b");
    }
}
