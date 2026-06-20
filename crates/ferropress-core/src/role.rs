//! Users, roles, and capabilities — the WP 5-tier cumulative ladder, retyped.
//!
//! WP stores roles+caps as a serialized PHP array in usermeta. We model `Role`
//! as an enum and `Capability` as an explicit typed permission set, with a
//! `capabilities()` mapping that encodes the cumulative hierarchy (each tier
//! includes everything below it). Per-content-type caps can extend this later
//! via the type registry; the base ladder is fixed because it is exactly what WP
//! users expect.

use std::collections::BTreeSet;

/// The five cumulative roles. Order matters: each includes all caps of those
/// before it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Subscriber,
    Contributor,
    Author,
    Editor,
    Administrator,
}

/// A single typed capability. Replaces WP's stringly-typed cap names. Extend as
/// surfaces grow; keep it an explicit enum so permission checks are exhaustive.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Read,
    CommentModerate,
    UploadMedia,
    EditOwnContent,
    PublishOwnContent,
    EditOthersContent,
    PublishOthersContent,
    ManageTerms,
    ManageMenus,
    ManageSettings,
    ManageUsers,
    ManagePlugins,
    ManageThemes,
}

impl Role {
    /// The full capability set granted by this role (cumulative).
    pub fn capabilities(self) -> BTreeSet<Capability> {
        use Capability::*;
        let mut caps = BTreeSet::new();
        // Each arm falls through conceptually by inserting its own tier then the
        // lower tiers; implemented explicitly to stay exhaustive + auditable.
        match self {
            Role::Administrator => {
                caps.extend([ManageUsers, ManagePlugins, ManageThemes, ManageSettings]);
                caps.extend(Role::Editor.capabilities());
            }
            Role::Editor => {
                caps.extend([
                    EditOthersContent,
                    PublishOthersContent,
                    ManageTerms,
                    ManageMenus,
                    CommentModerate,
                ]);
                caps.extend(Role::Author.capabilities());
            }
            Role::Author => {
                caps.extend([PublishOwnContent, UploadMedia]);
                caps.extend(Role::Contributor.capabilities());
            }
            Role::Contributor => {
                caps.extend([EditOwnContent]);
                caps.extend(Role::Subscriber.capabilities());
            }
            Role::Subscriber => {
                caps.insert(Read);
            }
        }
        caps
    }

    pub fn has(self, cap: Capability) -> bool {
        self.capabilities().contains(&cap)
    }
}
