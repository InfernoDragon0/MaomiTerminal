//! The app's asset source: icons the component kit does not embed, then the kit's own assets.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

/// Lucide icons (ISC) outside gpui-kit's default set, at the paths their `IconName` uses.
const ICONS: &[(&str, &[u8])] = &[
    ("icons/link.svg", include_bytes!("../assets/icons/link.svg")),
    (
        "icons/folder-search.svg",
        include_bytes!("../assets/icons/folder-search.svg"),
    ),
    (
        "icons/square-code.svg",
        include_bytes!("../assets/icons/square-code.svg"),
    ),
    (
        "icons/circle.svg",
        include_bytes!("../assets/icons/circle.svg"),
    ),
    (
        "icons/pencil.svg",
        include_bytes!("../assets/icons/pencil.svg"),
    ),
    ("icons/pin.svg", include_bytes!("../assets/icons/pin.svg")),
    (
        "icons/pin-off.svg",
        include_bytes!("../assets/icons/pin-off.svg"),
    ),
    (
        "icons/square-arrow-out-up-right.svg",
        include_bytes!("../assets/icons/square-arrow-out-up-right.svg"),
    ),
    (
        "icons/square-arrow-down-left.svg",
        include_bytes!("../assets/icons/square-arrow-down-left.svg"),
    ),
    (
        "icons/mouse-pointer-2.svg",
        include_bytes!("../assets/icons/mouse-pointer-2.svg"),
    ),
    (
        "icons/mouse-pointer-2-off.svg",
        include_bytes!("../assets/icons/mouse-pointer-2-off.svg"),
    ),
    (
        "icons/captions.svg",
        include_bytes!("../assets/icons/captions.svg"),
    ),
    (
        "icons/droplet.svg",
        include_bytes!("../assets/icons/droplet.svg"),
    ),
    (
        "icons/square-kanban.svg",
        include_bytes!("../assets/icons/square-kanban.svg"),
    ),
    (
        "icons/message-square.svg",
        include_bytes!("../assets/icons/message-square.svg"),
    ),
    (
        "icons/clock.svg",
        include_bytes!("../assets/icons/clock.svg"),
    ),
    (
        "icons/square-check.svg",
        include_bytes!("../assets/icons/square-check.svg"),
    ),
    (
        "icons/file-code.svg",
        include_bytes!("../assets/icons/file-code.svg"),
    ),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match ICONS.iter().find(|(p, _)| *p == path) {
            Some((_, bytes)) => Ok(Some(Cow::Borrowed(bytes))),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut list = gpui_kit::assets::Assets.list(path)?;
        list.extend(
            ICONS
                .iter()
                .filter(|(p, _)| p.starts_with(path))
                .map(|(p, _)| SharedString::from(*p)),
        );
        Ok(list)
    }
}
