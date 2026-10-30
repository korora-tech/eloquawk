//! Shared branding: the app id and the embedded app icon. Running outside the source tree
//! needs no asset directory.

use std::sync::{Arc, LazyLock};

use gpui::{Image, ImageFormat, Img, Styled, img, px};

/// The application id used for the Wayland `app_id`, the desktop file, the icon name, the
/// notification icon and the keyring service.
pub const APP_ID: &str = "korora.tech.Eloquawk";

/// The app icon's SVG source, embedded so the header and sign-in screen need no files.
const ICON_SVG: &[u8] = include_bytes!("../data/branding/eloquawk-app-icon.svg");

static ICON: LazyLock<Arc<Image>> =
    LazyLock::new(|| Arc::new(Image::from_bytes(ImageFormat::Svg, ICON_SVG.to_vec())));

/// The app icon at `size` logical pixels square.
pub fn icon(size: f32) -> Img {
    img(Arc::clone(&ICON)).size(px(size)).flex_shrink_0()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_is_reverse_dns() {
        assert_eq!(APP_ID, "korora.tech.Eloquawk");
        assert!(!APP_ID.contains('/'));
    }

    #[test]
    fn icon_svg_is_self_contained() {
        let svg = std::str::from_utf8(ICON_SVG).expect("utf-8 svg");
        assert!(svg.contains("viewBox=\"0 0 512 512\""));
        for banned in ["<image", "<script", "<font", "href=", "url("] {
            assert!(!svg.contains(banned), "icon svg contains {banned}");
        }
    }

    #[test]
    fn icon_png_is_512_square() {
        // The installed 512x512 hicolor icon; regenerate it with rsvg-convert when the SVG
        // changes (data/branding/README.md).
        let png = include_bytes!("../data/branding/eloquawk-app-icon.png");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        assert_eq!((width, height), (512, 512));
    }

    #[test]
    fn desktop_entry_matches_the_app_id() {
        let entry = include_str!("../data/korora.tech.Eloquawk.desktop");
        let field = |key: &str| {
            entry
                .lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .unwrap_or_else(|| panic!("desktop entry has no {key}"))
        };
        assert_eq!(field("Icon"), APP_ID);
        assert_eq!(field("StartupWMClass"), APP_ID);
        assert_eq!(field("Exec"), "eloquawk %u");
        assert_eq!(field("MimeType"), "x-scheme-handler/mailto;");
        assert_eq!(field("Categories"), "Network;Email;");
    }
}
