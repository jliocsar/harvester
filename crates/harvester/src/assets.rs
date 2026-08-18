//! The one [`AssetSource`] gpui gets, and the icons Harvester ships itself.
//!
//! gpui accepts exactly one asset source per application, and `gpui-component`
//! resolves every `IconName` through it. Handing gpui our own source wholesale
//! would blank all 99 library icons, so this one tries Harvester's embedded
//! icons first and falls back to `gpui-component-assets` for everything else.
//!
//! Our paths live under `harvester/icons/` rather than the library's `icons/`,
//! so an icon added here can never shadow — or be shadowed by — a library one.
//! Render code asks for them by path: `Icon::default().path(GIT_BRANCH_ICON_PATH)`.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// Lucide `git-branch`, for the chat prompt strip's branch indicator.
pub const GIT_BRANCH_ICON_PATH: &str = "harvester/icons/git-branch.svg";
/// Lucide `trash-2`, for forgetting a machine that is no longer listed.
pub const TRASH_ICON_PATH: &str = "harvester/icons/trash-2.svg";

/// Every icon this repo embeds, by the path render code asks for.
///
/// `gpui-component` ships neither of these: it has no `git-branch` counterpart
/// at all, and its `Delete` is the backspace glyph rather than a wastebasket.
const EMBEDDED_ICONS: &[(&str, &[u8])] = &[
    (
        GIT_BRANCH_ICON_PATH,
        include_bytes!("assets/git-branch.svg"),
    ),
    (TRASH_ICON_PATH, include_bytes!("assets/trash-2.svg")),
];

/// Harvester's embedded icons, backed by the `gpui-component` asset bundle.
pub struct HarvesterAssets;

impl AssetSource for HarvesterAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let embedded = EMBEDDED_ICONS
            .iter()
            .find(|(embedded_path, _bytes)| *embedded_path == path);

        match embedded {
            Some((_embedded_path, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => gpui_component_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut listed = gpui_component_assets::Assets.list(path)?;

        listed.extend(
            EMBEDDED_ICONS
                .iter()
                .filter(|(embedded_path, _bytes)| embedded_path.starts_with(path))
                .map(|(embedded_path, _bytes)| SharedString::from(*embedded_path)),
        );

        Ok(listed)
    }
}
