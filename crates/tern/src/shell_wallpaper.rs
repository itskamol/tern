// CubicBezier and the crossfade timing are zeron's (crates/ui/src/motion.rs WALLPAPER_CROSSFADE,
// MIT); the history and contrast guard follow crates/ui/src/settings/wallpaper.rs (MIT).
//! The wallpaper: loading it off the UI thread, drawing it as the hero of the empty view with a
//! crossfade on change, taking the window's colours from it, and the Settings controls.

use crate::hover::Accessible as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::AppContext as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use super::Shell;
use super::toast::CubicBezier;
use crate::settings_widgets as w;
use crate::theme::Theme;
use crate::theme_tint::{self, rgb_of};
use crate::wallpaper::{self, Prepared};
use crate::wallpaper_fx::Effect;
use crate::wallpaper_panel::{self, Panel};

/// zeron `WALLPAPER_CROSSFADE`: an immediate attack with a short, soft landing.
pub const CROSSFADE: Duration = Duration::from_millis(180);
const CROSSFADE_CURVE: CubicBezier = CubicBezier::new(1.0 / 3.0, 1.0, 2.0 / 3.0, 1.0);

/// What the window should be showing: a file with an effect.
#[derive(Debug, Clone, PartialEq)]
struct Want {
    source: PathBuf,
    effect: Effect,
    /// Effects print on white paper in the light appearance, black in the dark.
    light: bool,
}

struct Shown {
    want: Want,
    prepared: Prepared,
    /// `window_fill` for the palette and default scheme it was worked out under. Kept with the
    /// picture, so a new picture starts without it.
    fill: wallpaper_panel::Memo<(Theme, Option<String>)>,
}

struct Fade {
    /// The picture being replaced, kept under the new one until it has landed.
    from: Option<PathBuf>,
    since: Instant,
}

#[derive(Default)]
pub(super) struct State {
    shown: Option<Shown>,
    loading: Option<Want>,
    /// A want that failed to load, so a bad file is not retried on every frame.
    failed: Option<Want>,
    fade: Option<Fade>,
    pub(super) error: Option<String>,
    pub(super) gallery: super::gallery_ui::Gallery,
}

impl Shell {
    /// The wallpaper to draw: the chosen file, if it is still there.
    pub(crate) fn active_wallpaper(&self) -> Option<&str> {
        self.settings
            .wallpaper
            .as_deref()
            .filter(|p| Path::new(p).is_file())
    }

    fn wanted(&self) -> Option<Want> {
        self.active_wallpaper().map(|p| Want {
            source: PathBuf::from(p),
            effect: self.settings.wallpaper_effect,
            light: self.theme.light,
        })
    }

    /// Starts rendering whatever the settings now ask for. Called every frame; cheap when
    /// nothing changed.
    pub(super) fn sync_wallpaper(&mut self, cx: &mut Context<Self>) {
        self.sync_thumbs(cx);
        let Some(want) = self.wanted() else {
            self.wp.failed = None;
            self.wp.error = None;
            self.wp.loading = None;
            if self.wp.shown.take().is_some() {
                self.wp.fade = None;
                // The accent is restored outside this frame: it restyles the open terminals.
                cx.spawn(async move |this, cx| {
                    this.update(cx, |s, cx| s.retheme(cx)).ok();
                })
                .detach();
            }
            return;
        };
        if self.wp.shown.as_ref().is_some_and(|s| s.want == want)
            || self.wp.loading.as_ref() == Some(&want)
            || self.wp.failed.as_ref() == Some(&want)
        {
            return;
        }
        let Some(config) = crate::settings::dir() else {
            return;
        };
        self.wp.loading = Some(want.clone());
        let job = want.clone();
        let task = cx.background_spawn(async move {
            wallpaper::prepare(&job.source, job.effect, job.light, &config)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |s, cx| s.wallpaper_ready(want, result, cx))
                .ok();
        })
        .detach();
    }

    fn wallpaper_ready(
        &mut self,
        want: Want,
        result: Result<Prepared, String>,
        cx: &mut Context<Self>,
    ) {
        if self.wp.loading.as_ref() != Some(&want) {
            return;
        }
        self.wp.loading = None;
        if self.wanted().as_ref() != Some(&want) {
            // The choice moved on while this rendered; the next frame starts the new one.
            cx.notify();
            return;
        }
        match result {
            Ok(prepared) => {
                let from = self.wp.shown.as_ref().map(|s| s.prepared.image.clone());
                self.wp.fade = (!cx.reduce_motion()).then(|| Fade {
                    from,
                    since: Instant::now(),
                });
                self.wp.error = None;
                self.wp.shown = Some(Shown {
                    want,
                    prepared,
                    fill: wallpaper_panel::Memo::default(),
                });
            }
            Err(error) => {
                tracing::warn!(error = %error, "wallpaper_failed");
                self.wp.error = Some(error);
                self.wp.failed = Some(want);
            }
        }
        self.retheme(cx);
        self.restyle_tabs(cx);
        cx.notify();
    }

    /// What the palette is built from now: the appearance setting (or the system's, for
    /// System), the display preferences last read, and the wallpaper.
    pub(super) fn look(&self) -> crate::theme::Look {
        let shown = self.wp.shown.as_ref();
        crate::theme::Look {
            light: self.settings.appearance.is_light(self.system_light),
            increase_contrast: self.theme.increase_contrast,
            reduce_transparency: self.theme.reduce_transparency,
            wallpaper_fills: self.settings.wallpaper_fills_window && shown.is_some(),
            tint: self
                .settings
                .wallpaper_theme_colors
                .then(|| shown.and_then(|s| s.prepared.accent))
                .flatten(),
        }
    }

    /// The empty view's picture, filling the main tile. Only the empty view has it: with a tab
    /// or the Settings page open nothing sharp sits behind the content.
    pub(super) fn empty_view_hero(&mut self, window: &mut Window) -> Option<AnyElement> {
        if self.tabs.active_tab().is_some() || self.settings_page.is_some() {
            return None;
        }
        self.wallpaper_hero(window)
    }

    /// The blurred copy of the picture, when it should fill the window.
    pub(super) fn blurred_backdrop(&self) -> Option<PathBuf> {
        self.window_fill()?;
        Some(self.wp.shown.as_ref()?.prepared.blurred.clone())
    }

    /// A wallpaper is showing (hero or full window).
    pub(crate) fn has_wallpaper(&self) -> bool {
        self.wp.shown.is_some()
    }

    /// The panel opacity while the picture fills the whole window; `None` in hero-only mode and
    /// without a picture. The smallest alpha at which text, muted and faint text (and the
    /// terminal's own text) keep 4.5:1 over the picture's brightest and darkest regions. `None`
    /// under Reduce transparency: the panels stay opaque and the picture never shows through.
    pub(crate) fn window_fill(&self) -> Option<f32> {
        if !self.settings.wallpaper_fills_window || self.theme.reduce_transparency {
            return None;
        }
        let shown = self.wp.shown.as_ref()?;
        // Called several times a frame and worked out from the palette, the default scheme and
        // the picture alone, so it is remembered until one of them changes.
        let scheme = self.scheme_name_for("");
        if let Some(alpha) = shown
            .fill
            .recall(|(theme, name)| *theme == self.theme && name.as_deref() == scheme)
        {
            return Some(alpha);
        }
        let t = &self.theme;
        let term = t.terminal(self.settings.terminal_font_size, self.scheme_for(""));
        // Secondary text is plain muted while the panels are see-through (`Theme::secondary`);
        // the status line on the terminal's panel uses the palette that suits its background.
        let words = |t: &Theme, extra: Option<Hsla>| {
            let mut v = vec![
                (rgb_of(t.text), theme_tint::TEXT_CONTRAST),
                (rgb_of(t.muted), theme_tint::TEXT_CONTRAST),
                (rgb_of(t.faint), theme_tint::FAINT_CONTRAST),
            ];
            v.extend(extra.map(|c| (rgb_of(c), theme_tint::TEXT_CONTRAST)));
            v
        };
        let (shell_words, terminal_words) = (
            words(t, None),
            words(&t.on(term.background), Some(term.foreground)),
        );
        let alpha = wallpaper_panel::panel_alpha(
            &shown.prepared.backdrop,
            &[
                Panel {
                    surface: rgb_of(t.shell),
                    texts: &shell_words,
                },
                Panel {
                    surface: rgb_of(term.background),
                    texts: &terminal_words,
                },
            ],
        );
        shown
            .fill
            .remember((self.theme, scheme.map(str::to_owned)), alpha);
        Some(alpha)
    }

    /// `colour` at the panel opacity while the picture fills the window, else as it is.
    pub(super) fn veil(&self, colour: Hsla) -> Hsla {
        colour.opacity(self.window_fill().unwrap_or(1.0))
    }

    /// The hero: the sharp picture filling the empty view's tile, cover-fit, at full strength.
    /// It fills the whole tile rather than a band, so no edge shows where it would end. `None`
    /// without a wallpaper; the crossfade layers sit inside it.
    pub(super) fn wallpaper_hero(&mut self, window: &mut Window) -> Option<AnyElement> {
        let layers = self.wallpaper_layers(window);
        if layers.is_empty() {
            return None;
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .overflow_hidden()
                .children(layers)
                .into_any_element(),
        )
    }

    /// The picture layers, back to front: the one being replaced at full strength, with the new
    /// one fading in over it.
    pub(super) fn wallpaper_layers(&mut self, window: &mut Window) -> Vec<AnyElement> {
        let Some(shown) = self.wp.shown.as_ref() else {
            return Vec::new();
        };
        let opacity = self.settings.wallpaper_hero_opacity;
        let current = shown.prepared.image.clone();
        let progress = self.wp.fade.as_ref().and_then(|fade| {
            let t = fade.since.elapsed().as_secs_f32() / CROSSFADE.as_secs_f32();
            (t < 1.0).then(|| (CROSSFADE_CURVE.eval(t), fade.from.clone()))
        });
        let mut layers = Vec::new();
        match progress {
            Some((eased, from)) => {
                layers.extend(from.map(|p| picture(p, opacity)));
                layers.push(picture(current, opacity * eased));
                window.request_animation_frame();
            }
            None => {
                self.wp.fade = None;
                layers.push(picture(current, opacity));
            }
        }
        layers
    }

    /// Copies the picked file into tern's folder off the UI thread, then shows it.
    pub(super) fn pick_wallpaper(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose wallpaper".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let (Some(source), Some(config)) = (paths.into_iter().next(), crate::settings::dir())
            else {
                return;
            };
            let copy = cx
                .background_executor()
                .spawn(async move { wallpaper::import(&source, &config) })
                .await;
            this.update(cx, |s, cx| match copy {
                Ok(path) => s.set_wallpaper(path.to_string_lossy().into_owned(), cx),
                Err(error) => s.wp.error = Some(error),
            })
            .ok();
        })
        .detach();
    }

    /// Shows `path` (a copy in tern's folder), remembers it, and drops copies that fell out of
    /// the history.
    pub(super) fn set_wallpaper(&mut self, path: String, cx: &mut Context<Self>) {
        self.wp.error = None;
        self.wp.failed = None;
        self.update_settings(|st| crate::wallpaper_gallery::use_picked(st, path), cx);
        if let Some(config) = crate::settings::dir() {
            wallpaper::prune(&config, &self.settings.wallpaper_history);
        }
    }

    /// Shows a built-in's file; it stays out of the history.
    pub(super) fn show_builtin(&mut self, path: String, cx: &mut Context<Self>) {
        self.wp.error = None;
        self.wp.failed = None;
        self.update_settings(|st| crate::wallpaper_gallery::use_builtin(st, path), cx);
    }

    pub(super) fn clear_wallpaper(&mut self, cx: &mut Context<Self>) {
        self.update_settings(|st| st.wallpaper = None, cx);
    }

    pub(super) fn wallpaper_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let name_of = |p: &str| {
            Path::new(p)
                .file_name()
                .map_or_else(|| p.to_owned(), |n| n.to_string_lossy().into_owned())
        };
        let current = match self.settings.wallpaper.as_deref() {
            Some(p) if self.active_wallpaper().is_some() => name_of(p),
            Some(_) => "File not found".to_owned(),
            None => "None".to_owned(),
        };
        let status = match &self.wp.error {
            Some(e) => e.clone(),
            None => format!("{current} · fills the window; the empty view shows it sharp"),
        };
        let card = w::card(&t).child(self.wallpaper_gallery(status, cx));
        let mut effects = div().flex().gap(px(6.));
        for (id, label, effect) in EFFECTS {
            let selected = self.settings.wallpaper_effect == effect;
            effects = effects.child(
                w::button(&t, id, label)
                    .when(selected, |el| {
                        el.bg(t.ink(0.18)).font_weight(FontWeight::MEDIUM)
                    })
                    .on_click(cx.listener(move |s, _, _, cx| {
                        s.update_settings(|st| st.wallpaper_effect = effect, cx)
                    })),
            );
        }
        let pct = (self.settings.wallpaper_hero_opacity * 100.0).round();
        let (minus, value, plus) = w::stepper(&t, "wallpaper-opacity", format!("{pct:.0}%"));
        let step = |delta: f32| {
            move |s: &mut Shell, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Shell>| {
                s.update_settings(|st| st.wallpaper_hero_opacity += delta, cx)
            }
        };
        let on = self.settings.wallpaper_theme_colors;
        let fills = self.settings.wallpaper_fills_window;
        card.child(w::row(
            &t,
            false,
            "Effect",
            Some("Rendered once and cached".into()),
            effects,
        ))
        .child(w::row(
            &t,
            false,
            "Visibility",
            Some("How strongly the picture shows".into()),
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(minus.on_click(cx.listener(step(-0.1))))
                .child(value)
                .child(plus.on_click(cx.listener(step(0.1)))),
        ))
        .child(w::row(
            &t,
            false,
            "Wallpaper fills the window",
            Some(
                "Panels carry the text, sized to the picture; off keeps it to the empty view"
                    .into(),
            ),
            div()
                .id("toggle-wallpaper-fills")
                .switch("Wallpaper fills the window", fills)
                .cursor_pointer()
                .on_click(cx.listener(|s, _, _, cx| {
                    s.update_settings(
                        |st| st.wallpaper_fills_window = !st.wallpaper_fills_window,
                        cx,
                    );
                    s.retheme(cx);
                    s.restyle_tabs(cx);
                }))
                .child(w::toggle(&t, fills, "wallpaper-fills")),
        ))
        .child(w::row(
            &t,
            false,
            "Theme colours from wallpaper",
            Some(
                "Surfaces, accent and hover washes lean toward the image's dominant colour".into(),
            ),
            div()
                .id("toggle-wallpaper-colours")
                .switch("Theme colours from wallpaper", on)
                .cursor_pointer()
                .on_click(cx.listener(|s, _, _, cx| {
                    s.update_settings(
                        |st| st.wallpaper_theme_colors = !st.wallpaper_theme_colors,
                        cx,
                    );
                    s.retheme(cx);
                }))
                .child(w::toggle(&t, on, "wallpaper-colours")),
        ))
    }
}

const EFFECTS: [(&str, &str, Effect); 5] = [
    ("wp-fx-none", "None", Effect::None),
    ("wp-fx-scanlines", "Scanlines", Effect::Scanlines),
    ("wp-fx-ascii", "ASCII", Effect::Ascii),
    ("wp-fx-halftone", "Halftone", Effect::Halftone),
    ("wp-fx-dither", "Dither", Effect::Dither),
];

fn picture(path: PathBuf, opacity: f32) -> AnyElement {
    use gpui::StyledImage as _;
    gpui::img(path)
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .object_fit(gpui::ObjectFit::Cover)
        .opacity(opacity)
        .into_any_element()
}

/// The wallpaper's opacity for a crossfade `t` seconds in; exposed for the tests.
#[cfg(test)]
pub(super) fn fade_in(t: f32) -> f32 {
    CROSSFADE_CURVE.eval(t / CROSSFADE.as_secs_f32())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn crossfade_attacks_at_once_and_lands_softly() {
        assert_eq!(fade_in(0.0), 0.0);
        // zeron's curve (1/3, 1, 2/3, 1) is nearly there by the midpoint...
        assert!(fade_in(0.09) > 0.85, "{}", fade_in(0.09));
        // ...and reaches 1 exactly at the end.
        assert!((fade_in(0.18) - 1.0).abs() < 1e-3);
        assert!(fade_in(0.05) < fade_in(0.1));
    }

    #[test]
    fn crossfade_matches_zerons_180_ms() {
        assert_eq!(CROSSFADE, Duration::from_millis(180));
    }
}
