// Adapted from zeron crates/ui/src/icons.rs (icon_assets! macro, AssetSource, icon()) (MIT).
// Icons: Solar Icons (Linear) by 480 Design, CC BY 4.0, as bundled by zeron.
//! Embedded SVG icons and the gpui asset source that serves them. Render with
//! `icon(icons::PLUS).size(px(16.)).text_color(…)`; gpui tints them with the text colour.

use std::borrow::Cow;

use gpui::{
    AssetSource, Div, Hsla, ParentElement as _, Result, SharedString, Styled as _, Svg, div, px,
    svg,
};

macro_rules! icon_assets {
    ($(($name:ident, $file:literal)),+ $(,)?) => {
        $(pub const $name: &str = concat!("icons/", $file, ".svg");)+

        pub struct Assets;

        impl AssetSource for Assets {
            fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
                if let Some(rest) = path.strip_prefix(ICONS_1X) {
                    let source = self.load(&format!("icons/{rest}"))?;
                    return Ok(source.map(|b| Cow::Owned(snap_strokes(&b))));
                }
                Ok(match path {
                    $(concat!("icons/", $file, ".svg") => Some(Cow::Borrowed(
                        include_bytes!(concat!("../assets/icons/", $file, ".svg")).as_slice(),
                    )),)+
                    _ => None,
                })
            }

            fn list(&self, path: &str) -> Result<Vec<SharedString>> {
                Ok([$(concat!("icons/", $file, ".svg")),+]
                    .into_iter()
                    .filter(|p| p.starts_with(path))
                    .map(SharedString::from)
                    .collect())
            }
        }
    };
}

/// Path prefix of the 1x variant of every icon: the same SVG with its strokes snapped.
const ICONS_1X: &str = "icons@1x/";

/// Solar strokes are 1.25 to 1.75 wide, so at 1x each straddles two pixel rows and the icon
/// blurs. Every width becomes 1, which lands on whole pixels at the sizes tern draws icons.
fn snap_strokes(svg: &[u8]) -> Vec<u8> {
    const KEY: &str = "stroke-width=\"";
    let text = String::from_utf8_lossy(svg);
    let mut out = String::with_capacity(text.len());
    let mut rest: &str = &text;
    while let Some(at) = rest.find(KEY) {
        let value = at + KEY.len();
        out.push_str(&rest[..value]);
        rest = &rest[value..];
        match rest.find('"') {
            Some(end) => {
                out.push('1');
                rest = &rest[end..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out.into_bytes()
}

icon_assets!(
    (PLUS, "plus"),
    (SETTINGS, "settings"),
    (CLOSE, "close"),
    (PEN, "pen"),
    (TRASH, "trash-bin-minimalistic"),
    (COPY, "copy"),
    (PALETTE, "sun"),
    (TERMINAL, "terminal"),
    (SERVER, "remote-server"),
    (RESTART, "restart"),
    (KEYBOARD, "keyboard"),
    (CLOUD, "cloud"),
    (KEY, "key-minimalistic"),
    (INFO, "info-circle"),
    (CHEVRON_RIGHT, "alt-arrow-right"),
    (CHEVRON_LEFT, "alt-arrow-left"),
    (SEARCH, "magnifer"),
    (GLOBE, "global"),
    (FOLDER, "folder"),
    (DOCUMENT, "document"),
    (ARROW_UP, "arrow-up"),
    (HOME, "home"),
    (FILE_CODE, "file-code"),
);

pub fn icon(path: &'static str) -> Svg {
    svg().path(variant(path)).flex_none()
}

/// The 1x variant of `path` on a standard-density display, `path` itself otherwise.
fn variant(path: &'static str) -> SharedString {
    match path.strip_prefix("icons/") {
        Some(rest) if crate::theme::low_dpi() => format!("{ICONS_1X}{rest}").into(),
        _ => path.into(),
    }
}

/// zeron's sidebar glyph: a rounded frame with a panel 5.5 wide when the sidebar is open and
/// 1.75 when closed, drawn from quads in the source's 24-unit space (gpui SVGs are static).
pub fn sidebar_glyph(open: bool, size: f32, color: Hsla) -> Div {
    let s = size / 24.0;
    let stroke = 1.75;
    let frame = div()
        .absolute()
        .left(px((3.0 - stroke / 2.0) * s))
        .top(px((4.0 - stroke / 2.0) * s))
        .w(px((18.0 + stroke) * s))
        .h(px((16.0 + stroke) * s))
        .rounded(px((4.0 + stroke / 2.0) * s))
        .border(px(stroke * s))
        .border_color(color);
    let panel = div()
        .absolute()
        .left(px(6.5 * s))
        .top(px(7.5 * s))
        .w(px(if open { 5.5 } else { 1.75 } * s))
        .h(px(9.0 * s))
        .rounded(px(0.875 * s))
        .bg(color);
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(frame)
        .child(panel)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strokes_snap_to_one_and_the_rest_is_untouched() {
        let svg = br#"<svg stroke-width="1.5"><path d="M1.5 2" stroke-width="1.25"/><g stroke-width="1.75"/></svg>"#;
        let got = String::from_utf8(snap_strokes(svg)).unwrap();
        assert_eq!(
            got,
            r#"<svg stroke-width="1"><path d="M1.5 2" stroke-width="1"/><g stroke-width="1"/></svg>"#
        );
    }

    #[test]
    fn the_1x_path_serves_the_snapped_file() {
        let a = Assets;
        let plain = a.load(PLUS).unwrap().unwrap();
        let sharp = a.load("icons@1x/plus.svg").unwrap().unwrap();
        assert!(String::from_utf8_lossy(&plain).contains("1.75"));
        assert!(!String::from_utf8_lossy(&sharp).contains("1.75"));
        assert!(a.load("icons@1x/nope.svg").unwrap().is_none());
    }
}
