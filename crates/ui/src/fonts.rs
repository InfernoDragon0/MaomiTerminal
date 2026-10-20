//! Bundled fonts (Cascadia Code, SIL OFL 1.1; licence in assets/fonts/OFL.txt).

use std::borrow::Cow;

use gpui_kit::App;

/// Family name of the bundled terminal font.
pub const TERMINAL_FAMILY: &str = "Cascadia Code";

const FONTS: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/CascadiaCode-Regular.ttf"),
    include_bytes!("../assets/fonts/CascadiaCode-Bold.ttf"),
    include_bytes!("../assets/fonts/CascadiaCode-Italic.ttf"),
    include_bytes!("../assets/fonts/CascadiaCode-BoldItalic.ttf"),
];

/// Registers the bundled fonts with GPUI's text system.
pub(crate) fn load(cx: &mut App) {
    let fonts = FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%err, "could not load the bundled terminal font");
    }
}
