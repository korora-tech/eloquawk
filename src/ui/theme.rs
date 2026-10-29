//! Colour palettes, the active-theme global and shared sizes (SPEC 8.3).
//!
//! `DARK` keeps antarctical's tokens; `LIGHT` is Eloquawk's own. Every view reads colours
//! through [`palette`], so switching the [`Theme`] global and refreshing the windows restyles
//! the whole app. The contrast tests hold both palettes to WCAG AA.

use std::sync::Arc;

use gpui::{App, FontFeatures, Global, Window, WindowAppearance};

use crate::config::ThemePref;

/// Every colour token, as `0xrrggbb`.
#[derive(Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: u32,
    pub panel: u32,
    pub panel_raised: u32,
    pub header: u32,
    pub sidebar: u32,
    pub border: u32,
    pub text: u32,
    pub secondary: u32,
    pub muted: u32,
    pub caption: u32,
    pub disabled: u32,
    pub accent: u32,
    pub accent_text: u32,
    pub on_accent: u32,
    pub green: u32,
    pub red: u32,
    pub amber: u32,
    pub selected: u32,
    pub selected_border: u32,
    pub hover: u32,
    /// Text selection fill; opaque, painted under the text.
    pub selection: u32,
    pub input_border: u32,
    pub skeleton: u32,
    pub link: u32,
}

pub static DARK: Palette = Palette {
    bg: 0x11131b,
    panel: 0x191c27,
    panel_raised: 0x222633,
    header: 0x171923,
    sidebar: 0x151721,
    border: 0x2b2f3d,
    text: 0xf5f3fa,
    secondary: 0xc3c6d1,
    muted: 0x9ba1b1,
    caption: 0x858c9c,
    disabled: 0x626878,
    accent: 0xa28cff,
    accent_text: 0xd9d0ff,
    on_accent: 0x11111a,
    green: 0x68d9aa,
    red: 0xff6b7a,
    amber: 0xffc36a,
    selected: 0x2a263c,
    selected_border: 0x5c4e85,
    hover: 0x292d3b,
    selection: 0x3d3470,
    input_border: 0x303646,
    skeleton: 0x2b2f3d,
    link: 0xb9a9ff,
};

pub static LIGHT: Palette = Palette {
    bg: 0xf7f7fb,
    panel: 0xffffff,
    panel_raised: 0xf0f0f6,
    header: 0xffffff,
    sidebar: 0xf0f0f6,
    border: 0xdcdde6,
    text: 0x1b1d26,
    secondary: 0x3d4150,
    muted: 0x5f6475,
    caption: 0x6b7080,
    disabled: 0xa0a4b0,
    accent: 0x6a52e0,
    accent_text: 0x4f37c7,
    on_accent: 0xffffff,
    green: 0x1f8a5c,
    red: 0xc8323f,
    amber: 0xb7791f,
    selected: 0xebe7fd,
    selected_border: 0xb9acf3,
    hover: 0xeceef4,
    selection: 0xcfc5fa,
    input_border: 0xc8cad6,
    skeleton: 0xe2e3ea,
    link: 0x4f37c7,
};

/// The palette every view draws with.
pub struct Theme(pub &'static Palette);

impl Global for Theme {}

/// The `--theme` flag or a demo `theme:` step: wins over the saved preference for this
/// session and is never saved.
pub struct ThemeOverride(pub Option<ThemePref>);

impl Global for ThemeOverride {}

/// The active palette (dark until a theme is applied).
pub fn palette(cx: &App) -> &'static Palette {
    cx.try_global::<Theme>().map_or(&DARK, |t| t.0)
}

/// The palette for a preference; System follows the window's appearance.
pub fn resolve(pref: ThemePref, appearance: WindowAppearance) -> &'static Palette {
    match pref {
        ThemePref::Light => &LIGHT,
        ThemePref::Dark => &DARK,
        ThemePref::System => match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => &DARK,
            WindowAppearance::Light | WindowAppearance::VibrantLight => &LIGHT,
        },
    }
}

/// Resolves the effective preference (override, else `saved`) against `window`, sets the
/// global and redraws every window when the palette changed.
pub fn apply(saved: ThemePref, window: &Window, cx: &mut App) {
    let pref = cx
        .try_global::<ThemeOverride>()
        .and_then(|o| o.0)
        .unwrap_or(saved);
    let next = resolve(pref, window.appearance());
    if !std::ptr::eq(palette(cx), next) || !cx.has_global::<Theme>() {
        cx.set_global(Theme(next));
        cx.refresh_windows();
    }
}

/// Mixes `color` into `base` by `amount` (0–1).
pub fn tint(color: u32, base: u32, amount: f32) -> u32 {
    let ch = |c: u32, shift: u32| ((c >> shift) & 0xff) as f32;
    let mix = |shift| {
        let v = ch(base, shift) + (ch(color, shift) - ch(base, shift)) * amount;
        (v.round().clamp(0.0, 255.0) as u32) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// Tabular figures, so dates line up in the thread list.
pub fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// The window header.
pub const HEADER_H: f32 = 56.0;
/// The header's icon slot (filled with the app icon in M14).
pub const ICON_SLOT: f32 = 28.0;
pub const SIDEBAR_W: f32 = 240.0;
pub const LIST_W: f32 = 420.0;
/// One thread row; fixed, because the list is a `uniform_list`.
pub const ROW_H: f32 = 68.0;
/// One sidebar label row.
pub const LABEL_ROW_H: f32 = 28.0;
/// Header controls.
pub const CONTROL_H: f32 = 30.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast ratio between two colours.
    fn contrast(a: u32, b: u32) -> f32 {
        let lum = |c: u32| {
            let ch = |shift: u32| {
                let v = ((c >> shift) & 0xff) as f32 / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
        };
        let (la, lb) = (lum(a), lum(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    fn check(name: &str, p: &Palette) {
        let surfaces = [("bg", p.bg), ("panel", p.panel), ("selected", p.selected)];
        let strong = [
            ("text", p.text),
            ("secondary", p.secondary),
            ("accent_text", p.accent_text),
            ("link", p.link),
        ];
        let soft = [("muted", p.muted), ("caption", p.caption)];
        for (sn, s) in surfaces {
            for (fname, f) in strong {
                let r = contrast(f, s);
                assert!(r >= 4.5, "{name}: {fname} on {sn} is {r:.2}");
            }
            for (fname, f) in soft {
                let r = contrast(f, s);
                assert!(r >= 3.0, "{name}: {fname} on {sn} is {r:.2}");
            }
        }
    }

    #[test]
    fn dark_palette_meets_contrast() {
        check("dark", &DARK);
    }

    #[test]
    fn light_palette_meets_contrast() {
        check("light", &LIGHT);
    }

    #[test]
    fn controls_and_sidebars_stay_legible() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            assert!(contrast(p.on_accent, p.accent) >= 4.5, "{name} on_accent");
            for s in [p.sidebar, p.header, p.panel_raised, p.hover] {
                assert!(contrast(p.text, s) >= 4.5, "{name} text on {s:06x}");
                assert!(contrast(p.caption, s) >= 3.0, "{name} caption on {s:06x}");
            }
            // Selected text stays readable over its highlight.
            assert!(contrast(p.text, p.selection) >= 4.5, "{name} selection");
        }
    }

    #[test]
    fn system_follows_appearance() {
        assert!(std::ptr::eq(
            resolve(ThemePref::System, WindowAppearance::Dark),
            &DARK
        ));
        assert!(std::ptr::eq(
            resolve(ThemePref::System, WindowAppearance::VibrantLight),
            &LIGHT
        ));
        assert!(std::ptr::eq(
            resolve(ThemePref::Light, WindowAppearance::Dark),
            &LIGHT
        ));
        assert!(std::ptr::eq(
            resolve(ThemePref::Dark, WindowAppearance::Light),
            &DARK
        ));
    }

    #[test]
    fn tint_mixes() {
        assert_eq!(tint(0xffffff, 0x000000, 0.5), 0x808080);
        assert_eq!(tint(0x123456, 0xabcdef, 0.0), 0xabcdef);
        assert_eq!(tint(0x123456, 0xabcdef, 1.0), 0x123456);
    }
}
