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
    /// Author/edit/reorder/delete sidebar `Widget`s (Editor tier, joining
    /// `ManageMenus`/`ManageTerms` — T6 ruling). INVARIANT: sanitization of
    /// widget-authored HTML (`sanitize_widget_html`) is UNCONDITIONAL for
    /// every role that holds this capability, forever — ferropress has no
    /// `unfiltered_html` tier. The sanitizer, not this capability check, is
    /// the XSS boundary; no future per-user grant mechanism may reopen it by
    /// bypassing the sanitizer for a "trusted" role.
    ManageWidgets,
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
                    ManageWidgets,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `ManageWidgets` joins `ManageMenus`/`ManageTerms` in the Editor tier
    /// (T6 ruling: Editor+, not Administrator-only) and is cumulative upward.
    #[test]
    fn manage_widgets_is_editor_plus() {
        assert!(!Role::Subscriber.has(Capability::ManageWidgets));
        assert!(!Role::Contributor.has(Capability::ManageWidgets));
        assert!(!Role::Author.has(Capability::ManageWidgets));
        assert!(Role::Editor.has(Capability::ManageWidgets));
        assert!(Role::Administrator.has(Capability::ManageWidgets));
    }
}
