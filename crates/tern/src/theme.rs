// Adapted from zeron crates/theme/src/builtins.rs `zeron_dark` / `zeron_light` and
// crates/ui/src/theme.rs (MIT).
//! The Zeron Dark and Light palettes and layout tokens, values copied from zeron's source.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{Hsla, Pixels, hsla, px, rgb};
use tern_term::TerminalTheme;

use themes::Scheme;

/// Whether the window can blur the desktop behind it (macOS vibrancy). Windows and Linux
/// compositors differ too much, so there the window is opaque.
pub const BLURS_BEHIND: bool = cfg!(target_os = "macos");

/// Window frost over the blurred desktop (zeron `Theme::GLASS_ALPHA`, macOS).
pub const GLASS_ALPHA: f32 = 0.80;
pub const TITLEBAR_HEIGHT: f32 = 38.0;
pub const TITLEBAR_TOP_PAD: f32 = 4.0;
pub const SPACE_SM: f32 = 8.0;
/// zeron `Theme::SPACE_LG` (crates/ui/src/theme.rs:842): the gap between nav groups.
pub const SPACE_LG: f32 = 16.0;
pub const UI_FONT: &str = "Geist";
pub const MONO_FONT: &str = "Geist Mono";
pub const CONTROL_RADIUS: f32 = 6.0;

/// Whether the window is on a standard-density display (scale factor below 1.5) with sharper
/// text on. Set once per frame by `Shell::render`, so moving the window to another monitor
/// switches modes on the next frame; Retina never sees any of the helpers below change a value.
static LOW_DPI: AtomicBool = AtomicBool::new(false);

/// The scale factor under which a display counts as standard density.
pub const LOW_DPI_BELOW: f32 = 1.5;

pub fn set_low_dpi(on: bool) {
    LOW_DPI.store(on, Ordering::Relaxed);
}

pub fn low_dpi() -> bool {
    LOW_DPI.load(Ordering::Relaxed)
}

/// `v` rounded to a whole pixel at 1x, where a fraction puts glyph and icon edges on half
/// pixels; untouched on Retina.
pub fn snap(v: f32) -> f32 {
    if low_dpi() { v.round() } else { v }
}

/// `snap` as `Pixels`.
pub fn crisp(v: f32) -> Pixels {
    px(snap(v))
}

/// An icon size that centres in `outer` on whole pixels: at 1x an odd gap between box and icon
/// would leave the icon on a half pixel, so the icon grows by one.
pub fn fit(outer: f32, size: f32) -> f32 {
    if low_dpi() && ((outer - size).abs() % 2.0) > 0.5 {
        size + 1.0
    } else {
        size
    }
}

/// macOS's Increase contrast preference, as `Shell` last read it. `Theme::zeron` builds the
/// stronger palette from it, so the rest of the app only ever sees a `Theme`.
static INCREASE_CONTRAST: AtomicBool = AtomicBool::new(false);

/// macOS's Reduce transparency preference: nothing may show through a surface.
static REDUCE_TRANSPARENCY: AtomicBool = AtomicBool::new(false);

pub fn set_increase_contrast(on: bool) {
    INCREASE_CONTRAST.store(on, Ordering::Relaxed);
}

pub fn increase_contrast() -> bool {
    INCREASE_CONTRAST.load(Ordering::Relaxed)
}

pub fn set_reduce_transparency(on: bool) {
    REDUCE_TRANSPARENCY.store(on, Ordering::Relaxed);
}

pub fn reduce_transparency() -> bool {
    REDUCE_TRANSPARENCY.load(Ordering::Relaxed)
}

/// `retina` normally, `standard` on a 1x display: used for opacities that need a step more
/// contrast where grayscale-only text is thin.
pub fn by_dpi(retina: f32, standard: f32) -> f32 {
    if low_dpi() { standard } else { retina }
}

const ANSI_DARK: [u32; 16] = [
    0x242424, 0xf87171, 0x4ade80, 0xfacc15, 0x60a5fa, 0xc084fc, 0x22d3ee, 0xd4d4d8, 0x52525b,
    0xfca5a5, 0x86efac, 0xfde047, 0x93c5fd, 0xd8b4fe, 0x67e8f9, 0xfafafa,
];

/// zeron `ANSI_LIGHT` (crates/theme/src/builtins.rs:234-237).
const ANSI_LIGHT: [u32; 16] = [
    0x1f1f1f, 0xdc2626, 0x16a34a, 0xb45309, 0x2563eb, 0x9333ea, 0x0e7490, 0x3f3f46, 0x71717a,
    0xb91c1c, 0x15803d, 0x92400e, 0x1d4ed8, 0x7e22ce, 0x155e75, 0x18181b,
];

/// WCAG AA for small text: what `Theme::secondary` keeps on the shell and popup.
const SECONDARY_CONTRAST: f32 = 4.5;

/// zeron `INK_HAIRLINE_SCALE` (crates/ui/src/theme.rs:374): light hairlines carry 1.35x the
/// alpha so a 1 px edge survives a bright surround.
const INK_HAIRLINE_SCALE: f32 = 1.35;
/// zeron `INK_FILL_SCALE` (crates/ui/src/theme.rs:368).
const INK_FILL_SCALE: f32 = 1.0;
/// zeron `SCRIM_ALPHA_DARK` (crates/ui/src/theme.rs:1734): the dark-mode modal backdrop.
pub const SCRIM_ALPHA_DARK: f32 = 0.60;

/// Hairline ink: white on dark, black on light (zeron `hairline_for`, theme.rs:1715-1721).
fn hairline_for(light: bool, alpha: f32) -> Hsla {
    if light {
        hsla(0.0, 0.0, 0.0, (alpha * INK_HAIRLINE_SCALE).min(0.5))
    } else {
        hsla(0.0, 0.0, 1.0, alpha)
    }
}

/// Interactive-state wash (zeron `wash_for`, theme.rs:1728-1733).
fn wash_for(light: bool, alpha: f32) -> Hsla {
    if light {
        hsla(0.0, 0.0, 0.10, alpha * INK_FILL_SCALE)
    } else {
        hsla(0.0, 0.0, 0.92, alpha)
    }
}

/// Where titlebar content starts: clear of the traffic lights, or the edge in fullscreen
/// (zeron `titlebar_cluster_start`).
pub fn titlebar_content_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Whether this is the light palette.
    pub light: bool,
    /// Whether macOS's Increase contrast is on: secondary text at full strength, strong edges.
    pub increase_contrast: bool,
    pub shell: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    pub accent: Hsla,
    /// Text on an accent background, chosen to keep contrast (at least 4.5:1).
    pub on_accent: Hsla,
    pub success: Hsla,
    pub danger: Hsla,
    /// Amber: degraded but working (a slow link).
    pub warning: Hsla,
    pub border: Hsla,
    /// Row washes (zeron `wash(0.10)` active, `wash(0.05)` hover).
    pub row_active: Hsla,
    pub row_hover: Hsla,
    /// Divider inside floating cards (zeron `hairline(0.06)`, dark).
    pub hairline: Hsla,
    /// Floating card fill. zeron frosts its palette over a 16 px backdrop blur; tern has no
    /// per-element blur, so this is the shell colour lifted and opaque: any transparency lets
    /// the panel's text show through unblurred.
    pub popup: Hsla,
    pub terminal_background: Hsla,
    /// The wallpaper tint mixed into the surfaces, when theme colours from the wallpaper are
    /// on (see `theme_tint`); terminal schemes are mixed with it too.
    pub tint: Option<[u8; 3]>,
}

impl Theme {
    /// The palette for the appearance, strengthened when Increase contrast is on.
    pub fn zeron(light: bool) -> Self {
        let base = if light {
            Self::zeron_light()
        } else {
            Self::zeron_dark()
        };
        if increase_contrast() {
            base.with_increased_contrast()
        } else {
            base
        }
    }

    /// This palette for macOS's Increase contrast: faint text as strong as muted, and edges
    /// that keep WCAG's 3:1 for non-text against the shell (the default 8% hairline is nearer 1.2:1).
    pub fn with_increased_contrast(mut self) -> Self {
        self.increase_contrast = true;
        self.faint = self.muted;
        self.border = hairline_for(self.light, 0.40);
        self.hairline = hairline_for(self.light, 0.30);
        self
    }

    /// Small secondary text (host addresses, section labels): the dimmest `muted` that still
    /// keeps 4.5:1 on the shell and popup, as an opaque colour so the wallpaper panel maths
    /// (`window_fill`) sees what is drawn. Full strength under Increase contrast and on a
    /// standard-density display, where thin grayscale text needs the extra step.
    pub fn secondary(&self) -> Hsla {
        if self.increase_contrast || low_dpi() {
            return self.muted;
        }
        let (shell, popup) = (rgb_u32(self.shell), rgb_u32(self.popup));
        (12..=20)
            .map(|step| flatten(self.muted.opacity(step as f32 * 0.05), self.shell))
            .find(|c| {
                let c = rgb_u32(*c);
                [shell, popup]
                    .iter()
                    .all(|s| crate::wallpaper_fx::contrast_ratio(c, *s) >= SECONDARY_CONTRAST)
            })
            .unwrap_or(self.muted)
    }

    /// zeron `zeron_light` (crates/theme/src/builtins.rs:273-304); washes, hairlines and the
    /// popup from crates/ui/src/theme.rs:1715-1733 (`popup` is the variant's `card`, #ffffff).
    pub fn zeron_light() -> Self {
        Self {
            light: true,
            increase_contrast: false,
            shell: hex(0xf3f3f5),
            text: hex(0x303035),
            muted: hex(0x62626a),
            faint: hex(0x797981),
            accent: hex(0x5b43e8),
            on_accent: on_accent_for(hex(0x5b43e8)),
            success: hex(0x15803d),
            danger: hex(0xdc2626),
            warning: hex(0xb45309),
            border: hairline_for(true, 0.08),
            row_active: wash_for(true, 0.10),
            row_hover: wash_for(true, 0.05),
            hairline: hairline_for(true, 0.06),
            popup: hex(0xffffff),
            terminal_background: hex(0xfafafa),
            tint: None,
        }
    }

    pub fn zeron_dark() -> Self {
        Self {
            light: false,
            increase_contrast: false,
            shell: hex(0x0d0d0d),
            text: hex(0xe8e8ea),
            muted: hex(0xa9a9ae),
            faint: hex(0x85858a),
            accent: hex(0x8b7cf6),
            on_accent: on_accent_for(hex(0x8b7cf6)),
            success: hex(0x34d399),
            danger: hex(0xf87171),
            warning: hex(0xfbbf24),
            border: hairline_for(false, 0.08),
            row_active: wash_for(false, 0.10),
            row_hover: wash_for(false, 0.05),
            hairline: hairline_for(false, 0.06),
            popup: hsla(0.0, 0.0, 0.09, 1.0),
            terminal_background: hex(0x090909),
            tint: None,
        }
    }

    /// The terminal palette: a bundled scheme when one is chosen, else zeron's dark one.
    pub fn terminal(&self, font_size: f32, scheme: Option<&Scheme>) -> TerminalTheme {
        let mut t = TerminalTheme {
            background: self.terminal_background,
            foreground: self.text,
            cursor: self.text,
            selection: self.accent.opacity(0.35),
            font_family: MONO_FONT.into(),
            font_size,
            ..TerminalTheme::default()
        };
        let ansi = match scheme {
            Some(s) => {
                t.background = hex(s.background);
                t.foreground = hex(s.foreground);
                t.cursor = hex(s.cursor);
                t.selection = hex(s.selection).opacity(0.6);
                s.ansi
            }
            None if self.light => ANSI_LIGHT,
            None => ANSI_DARK,
        };
        for (slot, color) in t.ansi.iter_mut().zip(ansi) {
            *slot = hex(color);
        }
        if let Some(tint) = self.tint {
            crate::theme_tint::tint_terminal(&mut t, tint, self.light);
        }
        t
    }

    /// Ink at `alpha`: zeron's `wash`, a pale grey over dark surfaces, a soft black over light.
    pub fn ink(&self, alpha: f32) -> Hsla {
        wash_for(self.light, alpha)
    }

    /// Modal backdrop at `alpha_dark` (quoted in dark-mode terms): black in both, about half
    /// as strong on light (zeron `scrim_for`, theme.rs:1750-1755).
    pub fn scrim(&self, alpha_dark: f32) -> Hsla {
        if self.light {
            hsla(0.0, 0.0, 0.0, 0.32 * (alpha_dark / SCRIM_ALPHA_DARK))
        } else {
            hsla(0.0, 0.0, 0.0, alpha_dark)
        }
    }

    /// The shell surface as frost: the blurred desktop shows through at 1 - `GLASS_ALPHA`.
    pub fn glass(&self) -> Hsla {
        self.shell.opacity(GLASS_ALPHA)
    }

    /// The window fill: frost, except on a standard-density display, where the translucent
    /// fill lowers text contrast, or under Reduce transparency, where the plain shell colour
    /// is used. Only macOS blurs what is
    /// behind the window; elsewhere the window is opaque, so the shell colour it is.
    pub fn surface(&self) -> Hsla {
        if low_dpi() || !BLURS_BEHIND || reduce_transparency() {
            self.shell
        } else {
            self.glass()
        }
    }
}

/// `fg` composited over `bg`, as an opaque colour (zeron `theme::flatten`).
pub fn flatten(fg: Hsla, bg: Hsla) -> Hsla {
    let (f, b) = (gpui::Rgba::from(fg), gpui::Rgba::from(bg));
    let a = fg.a;
    gpui::Rgba {
        r: f.r * a + b.r * (1.0 - a),
        g: f.g * a + b.g * (1.0 - a),
        b: f.b * a + b.b * (1.0 - a),
        a: 1.0,
    }
    .into()
}

pub fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

/// `c` as `0xRRGGBB`, alpha ignored.
fn rgb_u32(c: Hsla) -> u32 {
    let c = gpui::Rgba::from(c);
    let ch = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
    ch(c.r) << 16 | ch(c.g) << 8 | ch(c.b)
}

/// The text colour that sits on `accent`: white or near-black, chosen to keep contrast (at least 4.5:1).
pub fn on_accent_for(accent: Hsla) -> Hsla {
    let acc_rgb = rgb_u32(accent);
    let white_contrast = crate::wallpaper_fx::contrast_ratio(0xff_ffff, acc_rgb);
    let dark_contrast = crate::wallpaper_fx::contrast_ratio(0x0d_0d_0d, acc_rgb);
    if white_contrast >= 4.5 {
        hex(0xff_ffff)
    } else if dark_contrast >= 4.5 {
        hex(0x0d_0d_0d)
    } else if white_contrast >= dark_contrast {
        hex(0xff_ffff)
    } else {
        hex(0x0d_0d_0d)
    }
}

pub mod themes {
    //! Terminal colour schemes: the 725 of iTerm2-Color-Schemes (mbadolato, MIT), bundled in
    //! Ghostty's format by `scripts/import-themes.sh` and parsed once on first use.

    use std::sync::OnceLock;

    /// The built-in scheme: zeron's dark palette, used when no other is chosen.
    const DEFAULT_NAME: &str = "Zeron Dark";
    const DEFAULT_NAME_LIGHT: &str = "Zeron Light";

    /// The name shown for the built-in palette that follows the appearance.
    pub fn default_name(light: bool) -> &'static str {
        if light {
            DEFAULT_NAME_LIGHT
        } else {
            DEFAULT_NAME
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Scheme {
        pub name: String,
        pub background: u32,
        pub foreground: u32,
        pub cursor: u32,
        pub selection: u32,
        pub ansi: [u32; 16],
    }

    static ALL: OnceLock<Vec<Scheme>> = OnceLock::new();

    pub fn all() -> &'static [Scheme] {
        ALL.get_or_init(|| parse(include_str!("../assets/themes.txt")))
    }

    pub fn find(name: &str) -> Option<&'static Scheme> {
        all().iter().find(|s| s.name == name)
    }

    /// Parses `## Name` blocks of `key=value` lines. A block missing its background or
    /// foreground, or with fewer than 16 palette entries, is skipped rather than half-applied.
    pub fn parse(text: &str) -> Vec<Scheme> {
        let mut out = Vec::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let Some(name) = line.strip_prefix("## ") else {
                continue;
            };
            let mut ansi = [None; 16];
            let (mut bg, mut fg, mut cursor, mut selection) = (None, None, None, None);
            while let Some(next) = lines.peek() {
                if next.starts_with("## ") {
                    break;
                }
                let entry = lines.next().unwrap_or_default();
                let Some((key, value)) = entry.split_once('=') else {
                    continue;
                };
                match key {
                    "palette" => {
                        if let Some((ix, hex)) = value.split_once('=')
                            && let (Ok(ix), Some(c)) = (ix.parse::<usize>(), color(hex))
                            && ix < 16
                        {
                            ansi[ix] = Some(c);
                        }
                    }
                    "background" => bg = color(value),
                    "foreground" => fg = color(value),
                    "cursor-color" => cursor = color(value),
                    "selection-background" => selection = color(value),
                    _ => {}
                }
            }
            let (Some(background), Some(foreground)) = (bg, fg) else {
                continue;
            };
            if ansi.iter().any(Option::is_none) {
                continue;
            }
            out.push(Scheme {
                name: name.to_owned(),
                background,
                foreground,
                cursor: cursor.unwrap_or(foreground),
                selection: selection.unwrap_or(ansi[8].unwrap_or(foreground)),
                ansi: ansi.map(|c| c.unwrap_or(0)),
            });
        }
        out
    }

    fn color(hex: &str) -> Option<u32> {
        u32::from_str_radix(hex.trim().trim_start_matches('#'), 16).ok()
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    mod tests {
        use super::*;

        #[test]
        fn the_bundle_parses_every_scheme() {
            let all = all();
            assert!(all.len() > 700, "{}", all.len());
            let dracula = find("Dracula").unwrap();
            assert_eq!(dracula.background, 0x282a36);
            assert_eq!(dracula.ansi[1], 0xff5555);
            assert_eq!(dracula.selection, 0x44475a);
        }

        #[test]
        fn incomplete_blocks_are_skipped_and_the_next_one_still_parses() {
            let text = "## Broken\nbackground=#000000\npalette=0=#111111\n\
                    ## Good\nbackground=#101010\nforeground=#eeeeee\n"
                .to_owned()
                + &(0..16)
                    .map(|i| format!("palette={i}=#0000{i:02x}\n"))
                    .collect::<String>();
            let parsed = parse(&text);
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].name, "Good");
            assert_eq!(parsed[0].ansi[15], 0x00000f);
            assert_eq!(parsed[0].cursor, 0xeeeeee);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::wallpaper_fx::contrast_ratio;

    fn rgb_of(c: Hsla) -> u32 {
        let c = gpui::Rgba::from(c);
        let ch = |v: f32| (v * 255.0).round() as u32;
        ch(c.r) << 16 | ch(c.g) << 8 | ch(c.b)
    }

    #[test]
    fn light_palette_is_zerons() {
        let t = Theme::zeron_light();
        assert!(t.light);
        assert_eq!(rgb_of(t.shell), 0xf3f3f5);
        assert_eq!(rgb_of(t.text), 0x303035);
        assert_eq!(rgb_of(t.muted), 0x62626a);
        assert_eq!(rgb_of(t.faint), 0x797981);
        assert_eq!(rgb_of(t.accent), 0x5b43e8);
        assert_eq!(rgb_of(t.on_accent), 0xffffff);
        assert_eq!(rgb_of(t.terminal_background), 0xfafafa);
        assert_eq!(rgb_of(t.popup), 0xffffff);
        assert!(!Theme::zeron_dark().light);
        assert_eq!(rgb_of(Theme::zeron_dark().on_accent), 0x0d0d0d);
        assert!(Theme::zeron(true).light && !Theme::zeron(false).light);
    }

    #[test]
    fn washes_and_hairlines_flip_tone_with_appearance() {
        let (dark, light) = (Theme::zeron_dark(), Theme::zeron_light());
        // Dark: pale ink; light: soft black at the same alpha.
        assert!(dark.ink(0.1).l > 0.9 && light.ink(0.1).l < 0.2);
        assert!((light.ink(0.1).a - 0.1).abs() < 1e-6);
        // Light hairlines carry 1.35x the alpha; dark ones are unscaled.
        assert!((light.hairline.a - 0.06 * 1.35).abs() < 1e-6 && light.hairline.l == 0.0);
        assert!((dark.hairline.a - 0.06).abs() < 1e-6 && dark.hairline.l == 1.0);
        assert!((light.border.a - 0.08 * 1.35).abs() < 1e-6);
        // The scaled alpha is capped at 0.5.
        assert_eq!(hairline_for(true, 0.9).a, 0.5);
    }

    #[test]
    fn scrim_is_black_and_gentler_on_light() {
        let (dark, light) = (Theme::zeron_dark(), Theme::zeron_light());
        assert_eq!(dark.scrim(0.6).a, 0.6);
        assert!((light.scrim(0.6).a - 0.32).abs() < 1e-6);
        assert!((light.scrim(0.3).a - 0.16).abs() < 1e-6);
        assert_eq!(light.scrim(0.6).l, 0.0);
    }

    #[test]
    fn text_is_readable_on_its_surfaces_in_both_appearances() {
        for t in [Theme::zeron_dark(), Theme::zeron_light()] {
            let term = t.terminal(13.0, None);
            let shell = rgb_of(t.shell);
            assert!(contrast_ratio(rgb_of(t.text), shell) >= 7.0);
            assert!(contrast_ratio(rgb_of(t.muted), shell) >= 4.5);
            assert!(contrast_ratio(rgb_of(term.foreground), rgb_of(term.background)) >= 7.0);
            assert!(contrast_ratio(rgb_of(t.accent), shell) >= 3.0);
            assert!(contrast_ratio(rgb_of(t.on_accent), rgb_of(t.accent)) >= 4.5);
        }
    }

    /// Both palettes, plain, strengthened, and leaning toward a wallpaper colour.
    fn palettes() -> Vec<(&'static str, Theme)> {
        let mut all = Vec::new();
        for base in [Theme::zeron_dark(), Theme::zeron_light()] {
            let tag = if base.light { "light" } else { "dark" };
            all.push((tag, base));
            all.push((tag, base.with_increased_contrast()));
            all.push((tag, base.tinted([20, 60, 140])));
        }
        all
    }

    #[test]
    fn secondary_text_keeps_aa_contrast_on_the_shell_and_popup() {
        for (tag, t) in palettes() {
            let text = rgb_of(flatten(t.secondary(), t.shell));
            for surface in [t.shell, t.popup] {
                let ratio = contrast_ratio(text, rgb_of(surface));
                assert!(
                    ratio >= 4.5,
                    "{tag} (strong {}): {ratio}",
                    t.increase_contrast
                );
            }
        }
    }

    #[test]
    fn secondary_text_is_full_strength_under_increase_contrast() {
        for base in [Theme::zeron_dark(), Theme::zeron_light()] {
            let strong = base.with_increased_contrast();
            assert_eq!(strong.secondary(), strong.muted);
            assert_eq!(strong.faint, strong.muted);
            assert_ne!(base.faint, base.muted);
        }
    }

    #[test]
    fn increase_contrast_edges_keep_three_to_one_on_the_shell() {
        for base in [Theme::zeron_dark(), Theme::zeron_light()] {
            let edge =
                |t: Theme| contrast_ratio(rgb_of(flatten(t.border, t.shell)), rgb_of(t.shell));
            assert!(edge(base.with_increased_contrast()) >= 3.0);
            assert!(edge(base) < 1.5);
        }
    }

    #[test]
    fn default_terminal_palette_follows_the_appearance() {
        let dark = Theme::zeron_dark().terminal(13.0, None);
        let light = Theme::zeron_light().terminal(13.0, None);
        assert_eq!(rgb_of(dark.ansi[1]), 0xf87171);
        assert_eq!(rgb_of(light.ansi[1]), 0xdc2626);
        assert_eq!(rgb_of(light.ansi[15]), 0x18181b);
        assert_eq!(rgb_of(light.background), 0xfafafa);
    }

    #[test]
    fn a_chosen_scheme_stays_as_chosen_in_light_mode() {
        let scheme = themes::find("Dracula").unwrap();
        let on_light = Theme::zeron_light().terminal(13.0, Some(scheme));
        let on_dark = Theme::zeron_dark().terminal(13.0, Some(scheme));
        assert_eq!(on_light.background, on_dark.background);
        assert_eq!(on_light.ansi, on_dark.ansi);
        assert_eq!(rgb_of(on_light.background), scheme.background);
    }
}
