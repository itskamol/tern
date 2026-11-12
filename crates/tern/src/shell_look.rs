//! How the window looks: colour schemes per host, light and dark appearance, restyling open
//! terminals, and the status line under the terminal.
//!
//! The window's frame. Layers, back to front, each drawn once:
//! 1. the base at the window root: the macOS frost (the window's own blurred appearance behind
//!    the shell colour at `GLASS_ALPHA`), or the opaque shell colour under Reduce transparency;
//! 2. tiles side by side, edge to edge, with no margins, cards or gaps: the titlebar band across
//!    the top, then sidebar | main (or Settings nav | page);
//! 3. one neutral 1 px hairline on the sidebar's right edge, which moves with it as it tweens.
//!
//! Sidebar width is held in whole device pixels so no edge lands between two.

use gpui::{
    App, Context, Hsla, IntoElement, ParentElement, Styled, Window, WindowBackgroundAppearance,
    div, px,
};

use super::*;
use crate::theme::Look;

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

    /// Rebuilds the palette after one of its inputs changed (appearance, a display
    /// preference). When it is different, everything that holds
    /// a colour is restyled once: open terminals, text fields, the window.
    /// Returns whether it changed (and so restyled).
    pub(crate) fn retheme(&mut self, cx: &mut Context<Self>) -> bool {
        self.retheme_with(self.look(), cx)
    }

    /// What the palette is built from now: the appearance setting (or the system's, for
    /// System) and the display preferences last read.
    pub(super) fn look(&self) -> Look {
        Look {
            light: self.settings.appearance.is_light(self.system_light),
            increase_contrast: self.theme.increase_contrast,
            reduce_transparency: self.theme.reduce_transparency,
        }
    }

    fn retheme_with(&mut self, look: Look, cx: &mut Context<Self>) -> bool {
        let next = Theme::resolve(look);
        if next == self.theme {
            return false;
        }
        self.theme = next;
        self.install_input_colors(cx);
        self.restyle_tabs(cx);
        self.retheme_files(cx);
        cx.notify();
        true
    }

    /// Reads macOS's Increase contrast and Reduce transparency; a change rebuilds the palette
    /// (stronger edges and secondary text; opaque panels).
    pub(crate) fn sync_display_preferences(&mut self, cx: &mut Context<Self>) {
        let look = Look {
            increase_contrast: crate::platform::system_increase_contrast(),
            reduce_transparency: crate::platform::system_reduce_transparency(),
            ..self.look()
        };
        self.retheme_with(look, cx);
    }

    /// Standard-density mode for this frame, from the monitor the window is on right now.
    pub(super) fn sync_density(&self, window: &Window) {
        crate::theme::set_low_dpi(
            self.settings.sharp_text && window.scale_factor() < crate::theme::LOW_DPI_BELOW,
        );
    }

    /// Text fields read their colours from this global, so open ones follow the theme.
    pub(crate) fn install_input_colors(&self, cx: &mut Context<Self>) {
        cx.set_global(self.theme.input_colors());
    }

    pub(super) fn terminal_theme(&self, alias: &str) -> tern_term::TerminalTheme {
        let mut theme = self
            .theme
            .terminal(self.settings.terminal_font_size, self.scheme_for(alias));
        if let Some(family) = &self.settings.terminal_font_family {
            theme.font_family = family.clone().into();
        }
        theme
    }

    /// Re-applies font and scheme to every open terminal; each re-measures its cells and the
    /// remote side is told the new grid size.
    pub(super) fn restyle_tabs(&self, cx: &mut Context<Self>) {
        self.restyle_sample(cx);
        for tab in &self.tabs {
            for session in tab.panes().iter().filter_map(Pane::session) {
                let theme = self.terminal_theme(&tab.alias);
                let view = session.read(cx).view.clone();
                view.update(cx, |v, cx| v.set_theme(theme, cx));
            }
        }
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
        activity: Option<gpui::SharedString>,
        cx: &App,
    ) -> Option<impl IntoElement + use<>> {
        if !self.status_line_showing() {
            return None;
        }
        let session = self.tabs.active_tab()?.session()?.read(cx);
        Some(statusline::render(
            &self.theme,
            &session.label(),
            session.dot(),
            session.latency(),
            session.elapsed(),
            activity,
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
                            .and_then(|t| t.session())
                            .is_some_and(|s| s.read(cx).status() == Status::Connected);
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
    /// Records the scale factor and switches the window between its blurred appearance (the
    /// desktop shows through the frost) and an opaque one, which Reduce transparency forces.
    pub(super) fn sync_frame(&mut self, window: &mut Window) {
        self.frame.scale = window.scale_factor();
        let blurred = crate::theme::BLURS_BEHIND && !self.theme.reduce_transparency;
        if self.frame.blurred != Some(blurred) {
            self.frame.blurred = Some(blurred);
            window.set_background_appearance(if blurred {
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Opaque
            });
        }
    }

    /// The window root's colour: the frost, or the shell colour where the window is opaque.
    pub(super) fn window_base(&self) -> Hsla {
        self.theme.surface()
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

    /// The main tile: fills what the side tile leaves, on the panel's colour.
    pub(super) fn main_tile(&self, panel: Hsla) -> gpui::Div {
        div()
            .flex_1()
            .min_w_0()
            .relative()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(panel)
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
