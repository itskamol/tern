// Adapted from zeron crates/ui/src/shell/command_palette.rs (palette card, header, footer,
// overlay) (MIT).
//! The theme picker: fuzzy search over the bundled schemes with a colour strip per row.
//! Moving the highlight previews the scheme on the affected tabs; Enter keeps it, Escape
//! puts the saved one back. It sets the default scheme, or one host's.

use crate::hover::HoverFade as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window, anchored, deferred,
    div, point, px,
};

use super::Shell;
use crate::picker::{Key, classify, rank_labels};
use crate::theme::hex;
use crate::theme::themes::{self, Scheme};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThemeTarget {
    Default,
    Host(String),
}

#[derive(Debug, Clone, Copy)]
enum Choice {
    /// Host target only: drop the host's own scheme and follow the default.
    FollowDefault,
    Zeron,
    Scheme(&'static Scheme),
}

impl Choice {
    fn label(self, light: bool) -> String {
        match self {
            Choice::FollowDefault => "Use the default theme".into(),
            Choice::Zeron => themes::default_name(light).into(),
            Choice::Scheme(s) => s.name.clone(),
        }
    }

    /// The setting value this choice writes: `None` clears it.
    fn value(self) -> Option<String> {
        match self {
            Choice::FollowDefault | Choice::Zeron => None,
            Choice::Scheme(s) => Some(s.name.clone()),
        }
    }
}

pub(super) struct ThemePicker {
    target: ThemeTarget,
    query: String,
    active: usize,
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// The value before the picker opened, restored on Escape.
    saved: Option<String>,
}

fn choices(target: &ThemeTarget) -> Vec<Choice> {
    let mut out = Vec::with_capacity(themes::all().len() + 2);
    if matches!(target, ThemeTarget::Host(_)) {
        out.push(Choice::FollowDefault);
    }
    if matches!(target, ThemeTarget::Default) {
        out.push(Choice::Zeron);
    }
    out.extend(themes::all().iter().map(Choice::Scheme));
    out
}

impl Shell {
    pub(crate) fn open_theme_picker(
        &mut self,
        target: ThemeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = self.theme_value(&target);
        let all = choices(&target);
        // Start on the current choice: for a host without its own theme that is "use the
        // default" (row 0); otherwise the row whose value matches.
        let active = all
            .iter()
            .position(|c| match c {
                Choice::FollowDefault => saved.is_none(),
                other => other.value() == saved,
            })
            .unwrap_or(0);
        let picker = ThemePicker {
            target,
            query: String::new(),
            active,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            saved,
        };
        picker.scroll.scroll_to_item(active);
        window.focus(&picker.focus, cx);
        self.picker = None;
        self.theme_picker = Some(picker);
        cx.notify();
    }

    fn theme_value(&self, target: &ThemeTarget) -> Option<String> {
        match target {
            ThemeTarget::Default => self.settings.terminal_theme.clone(),
            ThemeTarget::Host(alias) => self.settings.host_themes.get(alias).cloned(),
        }
    }

    /// Sets the value in memory and restyles the tabs; `save` also writes settings.json.
    fn set_theme_value(
        &mut self,
        target: &ThemeTarget,
        value: Option<&str>,
        save: bool,
        cx: &mut Context<Self>,
    ) {
        let value = value.map(str::to_owned);
        let apply = |s: &mut crate::settings::Settings| match target {
            ThemeTarget::Default => s.terminal_theme = value.clone(),
            ThemeTarget::Host(alias) => match &value {
                Some(v) => {
                    s.host_themes.insert(alias.clone(), v.clone());
                }
                None => {
                    s.host_themes.remove(alias);
                }
            },
        };
        if save {
            self.update_settings(apply, cx);
        } else {
            // A preview: shown, not saved.
            apply(&mut self.settings);
            self.restyle_tabs(cx);
        }
        cx.notify();
    }

    fn close_theme_picker(&mut self, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.theme_picker.take() else {
            return;
        };
        if keep {
            let value = self.theme_value(&picker.target);
            self.set_theme_value(&picker.target, value.as_deref(), true, cx);
        } else {
            self.set_theme_value(&picker.target, picker.saved.as_deref(), false, cx);
        }
        if self.settings_page.is_some() {
            window.focus(&self.focus, cx);
        } else {
            self.restore_focus(window, cx);
        }
    }

    fn matches(&self) -> Vec<Choice> {
        let Some(p) = &self.theme_picker else {
            return Vec::new();
        };
        let all = choices(&p.target);
        let labels: Vec<String> = all.iter().map(|c| c.label(self.theme.light)).collect();
        rank_labels(&p.query, &labels)
            .into_iter()
            .map(|ix| all[ix])
            .collect()
    }

    fn preview_active(&mut self, cx: &mut Context<Self>) {
        let matches = self.matches();
        let Some(p) = &self.theme_picker else {
            return;
        };
        if let Some(choice) = matches.get(p.active).copied() {
            let target = p.target.clone();
            self.set_theme_value(&target, choice.value().as_deref(), false, cx);
        }
    }

    fn on_theme_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = classify(event);
        if key == Key::Ignore {
            return;
        }
        cx.stop_propagation();
        let count = self.matches().len();
        let Some(p) = self.theme_picker.as_mut() else {
            return;
        };
        match key {
            Key::Move(delta) if count > 0 => {
                p.active = crate::tabs::step(p.active.min(count - 1), count, delta);
                p.scroll.scroll_to_item(p.active);
            }
            Key::Type(text) => {
                p.query.push_str(&text);
                p.active = 0;
            }
            Key::Erase => {
                p.query.pop();
                p.active = 0;
            }
            Key::Pick => return self.close_theme_picker(true, window, cx),
            Key::Close => return self.close_theme_picker(false, window, cx),
            _ => return,
        }
        self.preview_active(cx);
    }

    pub(super) fn render_theme_picker(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = self.theme_picker.as_ref()?;
        let t = self.theme;
        let viewport = window.viewport_size();
        let matches = self.matches();
        let active = p.active.min(matches.len().saturating_sub(1));
        let title = match &p.target {
            ThemeTarget::Default => "Default terminal theme".to_owned(),
            ThemeTarget::Host(alias) => format!("Theme for {alias}"),
        };
        let rows = matches.iter().enumerate().map(|(row, choice)| {
            let choice = *choice;
            div()
                .id(("theme", row))
                .mx(px(4.))
                .px(px(12.))
                .py(px(4.))
                .min_h(px(34.))
                .rounded(px(10.))
                .flex()
                .items_center()
                .gap(px(12.))
                .cursor_pointer()
                .hover_fade(
                    format!("theme-{row}"),
                    gpui::transparent_black(),
                    t.row_active,
                )
                .when(row == active, |el| el.bg(t.row_active))
                .on_click(cx.listener(move |shell, _, window, cx| {
                    if let Some(p) = shell.theme_picker.as_mut() {
                        p.active = row;
                    }
                    shell.preview_active(cx);
                    shell.close_theme_picker(true, window, cx);
                }))
                .child(swatch(choice, &t))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(SharedString::from(choice.label(t.light))),
                )
        });
        let query: SharedString = if p.query.is_empty() {
            format!("Search {} themes…", themes::all().len()).into()
        } else {
            p.query.clone().into()
        };
        let card = div()
            .id("theme-picker")
            .track_focus(&p.focus)
            .w(px(560.0_f32.min(f32::from(viewport.width) - 32.0)))
            .flex()
            .flex_col()
            .rounded(px(16.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .text_color(t.text)
            .on_key_down(cx.listener(Self::on_theme_key))
            .on_mouse_down_out(
                cx.listener(|s, _, window, cx| s.close_theme_picker(false, window, cx)),
            )
            .child(
                div()
                    .px(px(16.))
                    .pt(px(12.))
                    .text_xs()
                    .text_color(t.muted)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .min_h(px(40.))
                    .px(px(16.))
                    .py(px(6.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(t.hairline)
                    .text_size(px(14.))
                    .when(p.query.is_empty(), |el| el.text_color(t.faint))
                    .child(query),
            )
            .child(
                div()
                    .id("theme-results")
                    .py(px(8.))
                    .max_h(px((f32::from(viewport.height) - 180.0).clamp(100.0, 360.0)))
                    .overflow_y_scroll()
                    .track_scroll(&p.scroll)
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(rows),
            )
            .child(
                div()
                    .px(px(16.))
                    .py(px(7.))
                    .border_t_1()
                    .border_color(t.hairline)
                    .text_size(px(10.))
                    .text_color(t.muted)
                    .child("↑ ↓ preview · ↵ keep · Esc revert"),
            );
        Some(
            deferred(
                anchored().position(point(px(0.), px(0.))).child(
                    div()
                        .occlude()
                        .w(viewport.width)
                        .h(viewport.height)
                        .bg(t.scrim(0.35))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(card),
                ),
            )
            .priority(2)
            .into_any_element(),
        )
    }
}

/// A small sample of the scheme: its background with "Aa" in its foreground and six of its
/// ANSI colours. The follow-default choice shows an empty outline.
fn swatch(choice: Choice, t: &crate::theme::Theme) -> impl IntoElement + use<> {
    let (bg, fg, colors) = match choice {
        Choice::FollowDefault => {
            return div()
                .flex_none()
                .w(px(96.))
                .h(px(22.))
                .rounded(px(5.))
                .border_1()
                .border_color(t.border);
        }
        Choice::Zeron => (t.terminal_background, t.text, {
            let ansi = t.terminal(13.0, None).ansi;
            [ansi[1], ansi[2], ansi[3], ansi[4], ansi[5], ansi[6]]
        }),
        Choice::Scheme(s) => (
            hex(s.background),
            hex(s.foreground),
            [
                s.ansi[1], s.ansi[2], s.ansi[3], s.ansi[4], s.ansi[5], s.ansi[6],
            ]
            .map(hex),
        ),
    };
    div()
        .flex_none()
        .w(px(96.))
        .h(px(22.))
        .px(px(6.))
        .rounded(px(5.))
        .border_1()
        .border_color(t.border)
        .bg(bg)
        .flex()
        .items_center()
        .gap(px(3.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(fg)
                .mr(px(2.))
                .child("Aa"),
        )
        .children(
            colors
                .into_iter()
                .map(|c| div().size(px(7.)).rounded_full().bg(c)),
        )
}
