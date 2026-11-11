//! How the window looks: colour schemes per host, light and dark appearance, restyling open
//! terminals, and the status line under the terminal.
//!
//! The window's frame: one composition, the same with and without a wallpaper.
//!
//! Layers, back to front, each drawn once:
//! 1. the base at the window root: the opaque shell colour, or, with no wallpaper, the macOS
//!    frost (the window's own blurred appearance behind the shell colour at `GLASS_ALPHA`);
//! 2. when a wallpaper fills the window, its blurred copy, cover-fit to the window, one element;
//! 3. tiles side by side, edge to edge, with no margins, cards or gaps: the titlebar band across
//!    the top, then sidebar | main (or Settings nav | page). A tile is only a tint at the panel
//!    alpha over the layers behind it, so panels never carry a picture of their own;
//! 4. one neutral 1 px hairline on the sidebar's right edge, which moves with it as it tweens.
//!
//! The sharp picture is only the empty view's hero, laid out inside the main tile. Sidebar width
//! is held in whole device pixels so no edge lands between two.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, Context, Hsla, IntoElement, ParentElement, Styled, Window,
    WindowBackgroundAppearance, div, px,
};

use super::*;

impl Shell {
    /// The scheme a host's tabs use: its own, else the default, else zeron's (`None`).
    pub(super) fn scheme_for(&self, alias: &str) -> Option<&'static crate::theme::themes::Scheme> {
        self.scheme_name_for(alias)
            .and_then(crate::theme::themes::find)
    }

    /// The name of that scheme, without looking it up.
    pub(super) fn scheme_name_for(&self, alias: &str) -> Option<&str> {
        self.settings
            .host_themes
            .get(alias)
            .or(self.settings.terminal_theme.as_ref())
            .map(String::as_str)
    }

    /// Switches the palette when the chosen appearance (or the system's, for System) differs
    /// from the one showing, and restyles everything that holds a colour: open terminals, text
    /// fields, the wallpaper tint.
    pub(crate) fn apply_appearance(&mut self, cx: &mut Context<Self>) {
        let light = self.settings.appearance.is_light(self.system_light);
        if light == self.theme.light {
            return;
        }
        self.theme = Theme::zeron(light);
        self.refresh_theme(cx);
        self.install_input_colors(cx);
        self.restyle_tabs(cx);
        cx.notify();
    }

    /// Reads macOS's Increase contrast and Reduce transparency and applies what changed: the
    /// palette is rebuilt (stronger edges and secondary text), and open terminals restyled
    /// (their see-through background under a wallpaper goes opaque).
    pub(crate) fn sync_display_preferences(&mut self, cx: &mut Context<Self>) {
        let contrast = crate::platform::system_increase_contrast();
        let transparency = crate::platform::system_reduce_transparency();
        let moved = (contrast, transparency)
            != (
                crate::theme::increase_contrast(),
                crate::theme::reduce_transparency(),
            );
        if !moved {
            return;
        }
        crate::theme::set_increase_contrast(contrast);
        crate::theme::set_reduce_transparency(transparency);
        self.refresh_theme(cx);
        self.restyle_tabs(cx);
        cx.notify();
    }

    /// Standard-density mode for this frame, from the monitor the window is on right now.
    pub(super) fn sync_density(&self, window: &Window) {
        crate::theme::set_low_dpi(
            self.settings.sharp_text && window.scale_factor() < crate::theme::LOW_DPI_BELOW,
        );
    }

    /// Text fields read their colours from this global, so open ones follow the theme.
    pub(crate) fn install_input_colors(&self, cx: &mut Context<Self>) {
        let t = self.theme;
        cx.set_global(crate::text_input::InputColors {
            text: t.text,
            placeholder: t.faint,
            cursor: t.accent,
            selection: t.accent.opacity(0.35),
        });
    }

    pub(super) fn terminal_theme(&self, alias: &str) -> tern_term::TerminalTheme {
        let mut theme = self
            .theme
            .terminal(self.settings.terminal_font_size, self.scheme_for(alias));
        if let Some(family) = &self.settings.terminal_font_family {
            theme.font_family = family.clone().into();
        }
        // Over a full-window wallpaper the terminal is a panel too: the picture shows through.
        if let Some(alpha) = self.window_fill() {
            theme.background_alpha = alpha;
        }
        theme
    }

    /// Re-applies font and scheme to every open terminal; each re-measures its cells and the
    /// remote side is told the new grid size.
    pub(super) fn restyle_tabs(&self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            for pane in tab.panes() {
                let theme = self.terminal_theme(&tab.alias);
                let view = pane.session.read(cx).view.clone();
                view.update(cx, |v, cx| v.set_theme(theme, cx));
            }
        }
    }

    pub(super) fn change_font(
        &mut self,
        change: impl FnOnce(&mut Settings),
        cx: &mut Context<Self>,
    ) {
        self.update_settings(change, cx);
        self.restyle_tabs(cx);
    }

    /// Background of the main panel: the active tab's scheme, so no seam shows around it.
    pub(super) fn panel_background(&self) -> gpui::Hsla {
        match self.tabs.active_tab() {
            Some(tab) => self.terminal_theme(&tab.alias).background,
            None => self.theme.terminal_background,
        }
    }

    /// The strip under the active tab's terminal, when the setting is on.
    pub(super) fn render_status_line(
        &self,
        bg: gpui::Hsla,
        cx: &App,
    ) -> Option<impl IntoElement + use<>> {
        if !self.settings.show_status_line {
            return None;
        }
        let session = self.tabs.active_tab()?.session().read(cx);
        Some(statusline::render(
            &self.theme,
            &session.label(),
            session.dot(),
            session.latency(),
            session.elapsed(),
            bg,
        ))
    }

    /// Repaints once a second while the active tab is connected, so its session time counts.
    pub(super) fn start_clock(&mut self, cx: &mut Context<Self>) {
        self._clock = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let alive = this.update(cx, |s, cx| {
                    let ticking = s.settings.show_status_line
                        && s.tabs
                            .get(s.tabs.active())
                            .is_some_and(|t| t.session().read(cx).status() == Status::Connected);
                    if ticking {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }
}

/// Text over the wallpaper hero sits on a plate of the panel colour, so it never depends on
/// what the picture holds.
pub(super) fn plate(el: gpui::Div, panel: gpui::Hsla) -> gpui::Div {
    use gpui::Styled;
    el.px(gpui::px(20.))
        .py(gpui::px(12.))
        .rounded(gpui::px(12.))
        .bg(panel.opacity(0.85))
}

/// `value` (logical px) rounded to a whole number of device pixels at `scale`.
pub(crate) fn snap(value: f32, scale: f32) -> f32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (value * scale).round() / scale
}

/// What the frame remembers between frames.
pub(super) struct FrameState {
    /// The window's scale factor at its last render, for snapping widths outside `render`.
    pub(super) scale: f32,
    /// The window appearance last set, so it is only touched when it changes.
    blurred: Option<bool>,
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            scale: 1.0,
            blurred: None,
        }
    }
}

impl Shell {
    /// Records the scale factor and switches the window between its blurred appearance (no
    /// wallpaper: the desktop shows through the frost) and an opaque one (a wallpaper is set:
    /// nothing behind the window may show). Reduce transparency forces the opaque one.
    pub(super) fn sync_frame(&mut self, window: &mut Window) {
        self.frame.scale = window.scale_factor();
        let blurred = crate::theme::BLURS_BEHIND
            && !self.has_wallpaper()
            && !crate::theme::reduce_transparency();
        if self.frame.blurred != Some(blurred) {
            self.frame.blurred = Some(blurred);
            window.set_background_appearance(if blurred {
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Opaque
            });
        }
    }

    /// The window root's colour: always opaque under a wallpaper, frost otherwise.
    pub(super) fn window_base(&self) -> Hsla {
        if self.has_wallpaper() {
            self.theme.shell
        } else {
            self.theme.surface()
        }
    }

    /// The blurred picture across the window, drawn once; empty unless the picture fills it.
    pub(super) fn backdrop(&self) -> Vec<AnyElement> {
        use gpui::StyledImage as _;
        let Some(blurred) = self.blurred_backdrop() else {
            return Vec::new();
        };
        vec![
            gpui::img(blurred)
                .absolute()
                .inset_0()
                .size_full()
                .object_fit(gpui::ObjectFit::Cover)
                .opacity(self.settings.wallpaper_hero_opacity)
                .into_any_element(),
        ]
    }

    /// The tint a tile of colour `colour` lays over what is behind it: the panel alpha over a
    /// full-window picture, nothing otherwise (the window base already is that colour).
    pub(super) fn tile_tint(&self, colour: Hsla) -> Option<Hsla> {
        self.window_fill().map(|alpha| colour.opacity(alpha))
    }

    /// A fixed-width tile at the left of the body (the sidebar, or Settings' nav) with the
    /// separator on its right edge. Clipped to `width`, so collapsing never reflows its rows.
    pub(super) fn side_tile(&self, width: f32, content: impl IntoElement) -> gpui::Div {
        div()
            .flex_none()
            .relative()
            .h_full()
            .w(px(width))
            .overflow_hidden()
            .when_some(self.tile_tint(self.theme.shell), |el, tint| el.bg(tint))
            .child(content)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(px(1.))
                    .bg(self.theme.hairline),
            )
    }

    /// The main tile: fills what the side tile leaves. When it shows a terminal over a
    /// full-window wallpaper it paints nothing, as the terminal draws its own glass at the same
    /// alpha; anything else it shows (Settings, the empty view) sits on the tile's tint.
    pub(super) fn main_tile(&self, panel: Hsla) -> gpui::Div {
        let shows_terminal = !self.tabs.is_empty() && self.settings_page.is_none();
        let terminal_glass = shows_terminal && self.window_fill().is_some();
        div()
            .flex_1()
            .min_w_0()
            .relative()
            .overflow_hidden()
            .flex()
            .flex_col()
            .when(!terminal_glass, |el| {
                el.bg(self.tile_tint(panel).unwrap_or(panel))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::snap;

    #[test]
    fn snap_lands_on_whole_device_pixels() {
        for (value, scale) in [(256.4, 1.0), (256.4, 2.0), (257.3, 2.0), (300.7, 1.5)] {
            let device = snap(value, scale) * scale;
            assert!((device - device.round()).abs() < 1e-3, "{value}@{scale}");
        }
    }

    #[test]
    fn snap_rounds_to_the_nearest_not_down() {
        assert_eq!(snap(256.4, 1.0), 256.0);
        assert_eq!(snap(256.6, 1.0), 257.0);
        // At 2x half a point is a whole device pixel; a quarter rounds to the nearer edge.
        assert_eq!(snap(256.5, 2.0), 256.5);
        assert_eq!(snap(256.25, 2.0), 256.5);
        assert_eq!(snap(256.2, 2.0), 256.0);
    }

    #[test]
    fn snap_survives_a_bad_scale() {
        assert_eq!(snap(10.4, 0.0), 10.0);
        assert_eq!(snap(10.4, f32::NAN), 10.0);
    }
}
