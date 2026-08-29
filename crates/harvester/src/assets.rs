use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub const GIT_BRANCH_ICON_PATH: &str = "harvester/icons/git-branch.svg";
pub const TRASH_ICON_PATH: &str = "harvester/icons/trash-2.svg";

const EMBEDDED_ICONS: &[(&str, &[u8])] = &[
    (
        GIT_BRANCH_ICON_PATH,
        include_bytes!("assets/git-branch.svg"),
    ),
    (TRASH_ICON_PATH, include_bytes!("assets/trash-2.svg")),
];

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
                .map(|(embedded_path, _bytes)| (*embedded_path).into()),
        );

        Ok(listed)
    }
}
