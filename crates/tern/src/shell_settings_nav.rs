// Adapted from zeron crates/ui/src/shell.rs (render_settings_nav, starts_nav_group) and
// crates/ui/src/settings/widgets.rs (tab_selection_t, section_tab) (MIT).
//! The Settings sidebar: a title row with Back, then the sections in labelled groups. The
//! selected tab's wash travels in (150 ms ease-out), the text blends muted to full as the pointer
//! or selection arrives, and ↑/↓/Home/End move the selection and the focus together.
//! zeron's `scroll_faded` edge fade (a 451-line `edge_fade.rs`) is not ported: the list scrolls
//! plain when the window is too short.

use std::time::Instant;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, Div, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Stateful, StatefulInteractiveElement, Styled, Window, div, px,
};

use super::{Section, Shell};
use crate::hover;
use crate::hover::Accessible as _;
use crate::icons::icon;
use crate::shell::CubicBezier;
use crate::theme::{self, SPACE_LG, SPACE_SM, Theme};

/// The nav's groups in order, each a small muted label over its sections.
pub(super) const GROUPS: [(&str, &[Section]); 4] = [
    (
        "General",
        &[Section::Appearance, Section::Terminal, Section::Shortcuts],
    ),
    ("Connections", &[Section::Connections, Section::Snippets]),
    (
        "Security & sync",
        &[Section::Vault, Section::History, Section::Sync],
    ),
    // A lone section needs no label repeating its own name; spacing sets it apart.
    ("", &[Section::About]),
];

/// Where a navigation key lands from `current`. Up/down wrap, as zeron's do; Home and End jump.
/// Anything else is not the nav's key.
pub(super) fn target(current: Section, key: &str) -> Option<Section> {
    let all = Section::ALL;
    let at = all.iter().position(|s| *s == current)?;
    let next = match key {
        "up" => (at + all.len() - 1) % all.len(),
        "down" => (at + 1) % all.len(),
        "home" => 0,
        "end" => all.len() - 1,
        _ => return None,
    };
    Some(all[next])
}

/// zeron `section_tab`'s selected fill: `glass_selected_bg` (theme.rs:1781), the 11% wash in
/// dark and 6% in light.
fn selected_bg(t: &Theme) -> gpui::Hsla {
    t.ink(if t.light { 0.06 } else { 0.11 })
}

/// zeron `TAB_SLIDE` (motion.rs:446): 150 ms CSS ease-out (motion.rs:356).
const SLIDE_SECS: f32 = 0.150;
const EASE_OUT: CubicBezier = CubicBezier::new(0.0, 0.0, 0.58, 1.0);

struct Travel {
    from: f32,
    target: f32,
    started: Instant,
}

impl Travel {
    fn value(&self, now: Instant) -> f32 {
        let raw = (now.saturating_duration_since(self.started).as_secs_f32() / SLIDE_SECS).min(1.0);
        self.from + (self.target - self.from) * EASE_OUT.eval(raw)
    }
}

/// Retargetable selected-state fade (zeron `tab_selection_t`, widgets.rs:998). First paint and
/// reduced motion settle at once instead of flashing an entrance.
fn selection_t(window: &mut Window, key: String, selected: bool, reduced: bool) -> f32 {
    let now = Instant::now();
    let target = if selected { 1.0 } else { 0.0 };
    let value = window.with_global_id(key.into(), |id, window| {
        window.with_element_state(id, |previous: Option<Travel>, _| {
            let mut travel = previous.unwrap_or(Travel {
                from: target,
                target,
                started: now,
            });
            let current = travel.value(now);
            if travel.target != target {
                travel = Travel {
                    from: current,
                    target,
                    started: now,
                };
            }
            if reduced {
                travel.from = target;
                travel.target = target;
            }
            (travel.value(now), travel)
        })
    });
    if (value - target).abs() > 0.001 {
        window.request_animation_frame();
    }
    value
}

/// zeron `section_tab` (widgets.rs:1036): radius 8, 8 px across, 6 down, at least 32 tall, 13 pt,
/// medium when selected; the fill runs clear to the selected wash by `t`, hover lifts it to the
/// theme hover wash. A 2 px edge is held transparent for the focus ring so focusing never
/// shifts the label.
fn tab(th: &Theme, selected: bool, t: f32, id: &'static str, hover_key: String) -> Stateful<Div> {
    let base_bg = hover::mix(th.ink(0.0), selected_bg(th), t);
    let base_text = hover::mix(th.muted, th.text, t);
    let hover_bg = if selected { base_bg } else { th.row_hover };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .rounded(px(8.))
        .px(px(SPACE_SM - 2.))
        .py(px(4.))
        .border_2()
        .border_color(gpui::transparent_black())
        .min_h(px(32.))
        .flex_shrink_0()
        .text_size(theme::crisp(13.))
        .when(selected, |el| el.font_weight(FontWeight::MEDIUM))
        .text_color(hover::blend(&hover_key, base_text, th.text))
        .bg(hover::blend(&hover_key, base_bg, hover_bg))
        .id(id)
        .on_hover(hover::hover_listener(hover_key.into()))
}

pub(super) fn render(
    shell: &Shell,
    section: Section,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let th = shell.theme;
    let handles: Vec<FocusHandle> = window
        .use_keyed_state("settings-nav-focus", cx, |_, cx| {
            Section::ALL
                .iter()
                .map(|_| cx.focus_handle())
                .collect::<Vec<_>>()
        })
        .read(cx)
        .clone();
    let reduced = cx.reduce_motion();
    let faint = th.secondary();

    let header = div()
        .flex_none()
        .px(px(SPACE_SM))
        .pb(px(4.))
        .h(px(32.))
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            crate::sidebar::icon_button("nav-back", crate::icons::CHEVRON_LEFT, 24., 16., &th)
                .icon_button("Back", &th)
                .on_click(cx.listener(|s, _, window, cx| s.toggle_settings(window, cx))),
        )
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(th.text)
                .child("Settings"),
        );

    let mut list = div()
        .id("settings-sections")
        .role(gpui::Role::TabList)
        .aria_label("Settings sections")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(2.));
    for (n, (label, members)) in GROUPS.iter().enumerate() {
        if label.is_empty() {
            list = list.child(div().h(px(SPACE_LG)));
        } else {
            list = list.child(
                div()
                    .when(n > 0, |el| el.mt(px(SPACE_LG - 2.)))
                    .px(px(SPACE_SM))
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(faint)
                    .child(*label),
            );
        }
        for &item in *members {
            let index = Section::ALL.iter().position(|s| *s == item).unwrap_or(0);
            let selected = item == section;
            let hover_key = format!("{}-hover", item.id());
            let t = selection_t(
                window,
                format!("{}-selection", item.id()),
                selected,
                reduced,
            );
            let glyph = hover::blend(&hover_key, hover::mix(th.muted, th.text, t), th.text);
            let handles = handles.clone();
            list = list.child(
                tab(&th, selected, t, item.id(), hover_key)
                    .role(gpui::Role::Tab)
                    .aria_label(item.label())
                    .aria_selected(selected)
                    .track_focus(&handles[index].clone().tab_stop(selected))
                    .focus_visible(|s| s.border_color(th.accent))
                    .cursor_pointer()
                    .on_key_down(
                        cx.listener(move |shell, e: &gpui::KeyDownEvent, window, cx| {
                            let Some(next) = target(item, e.keystroke.key.as_str()) else {
                                return;
                            };
                            shell.select_section(next, cx);
                            let at = Section::ALL.iter().position(|s| *s == next).unwrap_or(0);
                            window.focus(&handles[at], cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_click(cx.listener(move |shell, _, _, cx| shell.select_section(item, cx)))
                    .child(
                        icon(item.icon())
                            .size(px(theme::fit(32., 16.)))
                            .text_color(glyph),
                    )
                    .child(item.label()),
            );
        }
    }

    div()
        .w(px(shell.settings.sidebar_width))
        .flex_none()
        .h_full()
        .px(px(SPACE_SM))
        .pt(px(12.))
        .pb(px(SPACE_SM))
        .flex()
        .flex_col()
        .gap(px(SPACE_SM))
        .child(header)
        .child(search_field(shell, &th))
        .child(list)
        .into_any_element()
}

/// The Settings search, in zeron's `search_input_frame` as the hosts sidebar draws it (10 x 6
/// padding, 8 radius, 4 % ink, a 13 pt magnifier).
fn search_field(shell: &Shell, th: &Theme) -> impl IntoElement + use<> {
    div().px(px(SPACE_SM)).pb(px(8.)).child(
        div()
            .px(px(10.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(8.))
            .bg(th.ink(0.04))
            .text_size(px(13.))
            .child(
                crate::icons::icon(crate::icons::SEARCH)
                    .size(px(13.))
                    .text_color(th.muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(shell.settings_search_input().clone()),
            ),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn every_section_is_in_exactly_one_group_in_nav_order() {
        let flat: Vec<Section> = GROUPS.iter().flat_map(|(_, m)| m.iter().copied()).collect();
        assert_eq!(flat, Section::ALL.to_vec());
    }

    #[test]
    fn groups_split_where_the_owner_asked() {
        let of = |s: Section| GROUPS.iter().position(|(_, m)| m.contains(&s)).unwrap();
        assert_eq!(of(Section::Shortcuts), 0);
        assert_eq!(of(Section::Connections), 1);
        assert_eq!(of(Section::Snippets), 1);
        assert_eq!(of(Section::Vault), 2);
        assert_eq!(of(Section::History), 2);
        assert_eq!(of(Section::Sync), 2);
        assert_eq!(of(Section::About), 3);
    }

    #[test]
    fn arrows_walk_the_list_and_wrap_at_both_ends() {
        assert_eq!(target(Section::Terminal, "down"), Some(Section::Shortcuts));
        assert_eq!(target(Section::Terminal, "up"), Some(Section::Appearance));
        assert_eq!(target(Section::About, "down"), Some(Section::Appearance));
        assert_eq!(target(Section::Appearance, "up"), Some(Section::About));
    }

    #[test]
    fn home_and_end_jump_from_the_middle() {
        assert_eq!(target(Section::Vault, "home"), Some(Section::Appearance));
        assert_eq!(target(Section::Vault, "end"), Some(Section::About));
    }

    #[test]
    fn other_keys_are_left_alone() {
        assert_eq!(target(Section::Vault, "enter"), None);
        assert_eq!(target(Section::Vault, "left"), None);
    }

    #[test]
    fn selection_travel_eases_out_and_retargets_from_where_it_was() {
        let t0 = Instant::now();
        let up = Travel {
            from: 0.0,
            target: 1.0,
            started: t0,
        };
        let mid = up.value(t0 + Duration::from_millis(75));
        assert!(mid > 0.5 && mid < 1.0, "{mid}");
        assert_eq!(up.value(t0 + Duration::from_millis(150)), 1.0);
        let back = Travel {
            from: mid,
            target: 0.0,
            started: t0,
        };
        assert_eq!(back.value(t0), mid);
        assert_eq!(back.value(t0 + Duration::from_millis(150)), 0.0);
    }
}
