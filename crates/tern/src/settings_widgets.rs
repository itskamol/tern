// Adapted from zeron crates/ui/src/settings/widgets.rs (page_column, page_header, section,
// section_card, card_row, row_title, meta_line, toggle_switch) (MIT).
//! The building blocks of the Settings page, with zeron's sizes. zeron frosts its switch with
//! gradients; tern's surfaces are opaque, so the flat tones are used.
//!
//! Search filters the rows where the pages build them, so a match keeps its own control: while
//! a [`Searching`] scope lives (one Settings frame), [`row`] draws only rows that match, with
//! the words highlighted, and [`section`] drops a section none of whose rows did. The scope is
//! per thread and ends with the frame, so no page needs to know about search.

use crate::hover::Accessible as _;
use std::time::Instant;

use gpui::prelude::FluentBuilder;
use gpui::{
    App, Div, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, Stateful, Styled, Window, div, px,
};

use std::cell::RefCell;

use gpui::{HighlightStyle, StyledText};

use crate::hover::HoverFade as _;
use crate::settings_search::{Hits, Query, row_matches};
use crate::theme::Theme;

/// The search running while this frame's pages are built, and how many rows it has let through
/// in the card, section and page being built.
struct Scope {
    query: Query,
    page: &'static str,
    in_card: usize,
    in_section: usize,
    in_page: usize,
}

thread_local! {
    static SEARCH: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// While it lives, the pages built are filtered by `query`; dropping it ends the search.
#[must_use]
pub struct Searching(());

impl Searching {
    /// `None` for an empty query: the page is built whole.
    pub fn start(query: Query) -> Option<Self> {
        if query.is_empty() {
            return None;
        }
        SEARCH.with(|s| {
            *s.borrow_mut() = Some(Scope {
                query,
                page: "",
                in_card: 0,
                in_section: 0,
                in_page: 0,
            });
        });
        Some(Self(()))
    }

    /// The page about to be built, whose name also matches; returns nothing.
    pub fn page(&self, name: &'static str) {
        with_scope(|s| {
            s.page = name;
            s.in_page = 0;
        });
    }

    /// Rows the page being built has let through: none, and the page is left out.
    pub fn matched(&self) -> usize {
        with_scope(|s| s.in_page).unwrap_or(0)
    }
}

impl Drop for Searching {
    fn drop(&mut self) {
        SEARCH.with(|s| s.borrow_mut().take());
    }
}

fn with_scope<R>(f: impl FnOnce(&mut Scope) -> R) -> Option<R> {
    SEARCH.with(|s| s.borrow_mut().as_mut().map(f))
}

/// The search's tint on matched words: the drop zone's accent wash (`shell_split.rs`).
fn highlighted(text: SharedString, ranges: Vec<std::ops::Range<usize>>, t: &Theme) -> StyledText {
    let style = HighlightStyle {
        background_color: Some(t.accent.opacity(0.22)),
        ..Default::default()
    };
    StyledText::new(text).with_highlights(ranges.into_iter().map(|r| (r, style)))
}

const PAGE_MAX_WIDTH: f32 = 760.0;
const PAGE_PAD_X: f32 = 40.0;
const SECTION_LABEL_INSET: f32 = 8.0;
const SECTION_GAP: f32 = 32.0;

/// Centered content column, 760 wide with 40 of air each side.
pub fn page_column() -> Div {
    div()
        .w_full()
        .max_w(px(PAGE_MAX_WIDTH))
        .mx_auto()
        .px(px(PAGE_PAD_X))
        .pt(px(16.0))
        .pb(px(48.0))
        .flex()
        .flex_col()
}

pub fn page_header(t: &Theme, title: &'static str) -> Div {
    div()
        .px(px(SECTION_LABEL_INSET))
        .text_size(px(20.))
        .line_height(px(26.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.text)
        .child(title)
}

pub fn page_subtitle(t: &Theme, copy: impl Into<SharedString>) -> Div {
    div()
        .mt(px(4.))
        .px(px(SECTION_LABEL_INSET))
        .text_size(px(13.))
        .line_height(px(17.))
        .text_color(t.muted)
        .child(copy.into())
}

/// A labeled block: the muted label, then the card 8 below it, 32 below the previous block.
pub fn section(t: &Theme, label: &'static str, block: impl IntoElement) -> Div {
    // `block`'s rows were built before this call: none of them matched, so neither does it.
    if with_scope(|s| std::mem::take(&mut s.in_section)) == Some(0) {
        return div();
    }
    div()
        .mt(px(SECTION_GAP))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .px(px(SECTION_LABEL_INSET))
                .text_size(px(13.))
                .line_height(px(17.))
                .text_color(t.muted)
                .child(label),
        )
        .child(block)
}

/// zeron `section_card`: radius 12, a 4.5% ink fill, no outline.
pub fn card(t: &Theme) -> Div {
    with_scope(|s| s.in_card = 0);
    div()
        .rounded(px(12.))
        .bg(t.ink(0.045))
        .overflow_hidden()
        .flex()
        .flex_col()
}

/// One row: text on the left, control on the right, a hairline above all but the first.
pub fn row(
    t: &Theme,
    first: bool,
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
) -> Div {
    let title: SharedString = title.into();
    let found = with_scope(|s| {
        let description = description.as_deref().unwrap_or_default();
        let hits = row_matches(&s.query, s.page, &title, description)?;
        let first_shown = s.in_card == 0;
        s.in_card += 1;
        s.in_section += 1;
        s.in_page += 1;
        Some((hits, first_shown))
    });
    let (hits, first) = match found {
        None => (Hits::default(), first),
        Some(None) => return div(),
        Some(Some((hits, first_shown))) => (hits, first_shown),
    };
    let title = highlighted(title, hits.title, t);
    let description = description.map(|d| highlighted(d, hits.description, t));
    div()
        .mx(px(16.))
        .py(px(12.))
        .min_h(px(60.))
        .when(!first, |el| {
            el.border_t_1().border_color(t.border.opacity(0.6))
        })
        .flex()
        .items_center()
        .gap(px(16.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(17.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text)
                        .child(title),
                )
                .when_some(description, |el, d| {
                    el.child(
                        div()
                            .mt(px(1.))
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(t.muted)
                            .child(d),
                    )
                }),
        )
        .child(div().flex_none().child(control))
}

/// zeron `action_button`: radius 8, at least 32 tall, 10×5 padding, 12.5 pt.
pub fn button(
    t: &Theme,
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let id = id.into();
    let key = format!("button-{id}");
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded(px(8.))
        .min_h(px(32.))
        .px(px(10.))
        .py(px(5.))
        .text_size(crate::theme::crisp(12.5))
        .cursor_pointer()
        .hover_fade(key, t.ink(0.06), t.ink(0.10))
        .text_color(t.text)
        .child(label.into())
}

/// A − value + stepper built from two buttons, for numbers with small ranges.
pub fn stepper(
    t: &Theme,
    id: &'static str,
    value: impl Into<SharedString>,
) -> (Stateful<Div>, Div, Stateful<Div>) {
    (
        button(t, (id, 0usize), "−").icon_button("Decrease", t),
        div()
            .min_w(px(44.))
            .flex()
            .justify_center()
            .text_size(px(13.))
            .text_color(t.text)
            .child(value.into()),
        button(t, (id, 1usize), "+").icon_button("Increase", t),
    )
}

const SWITCH_WIDTH: f32 = 44.8;
const SWITCH_HEIGHT: f32 = 28.8;
const SWITCH_TRACK_HEIGHT: f32 = 20.8;
const SWITCH_SIDE_INSET: f32 = 1.6;
const SWITCH_THUMB_WIDTH: f32 = 24.0;
const SWITCH_THUMB_HEIGHT: f32 = SWITCH_TRACK_HEIGHT - 2.0 * SWITCH_SIDE_INSET;
const SWITCH_MARK_SIZE: f32 = 7.2;
/// Thumb travel: 180 ms ease-out cubic.
const SWITCH_TRAVEL_SECS: f32 = 0.18;

/// A pill switch whose thumb slides between the off ring and the on bar. The caller wraps it
/// in something clickable; `key` keeps each switch's travel state apart.
pub fn toggle(t: &Theme, on: bool, key: &'static str) -> Div {
    div()
        .flex_none()
        .w(px(SWITCH_WIDTH))
        .h(px(SWITCH_HEIGHT))
        .child(Switch { theme: *t, on, key })
}

#[derive(IntoElement)]
struct Switch {
    theme: Theme,
    on: bool,
    key: &'static str,
}

struct Travel {
    from: f32,
    target: f32,
    started: Instant,
}

impl Travel {
    fn value(&self, now: Instant) -> f32 {
        let t = (now.duration_since(self.started).as_secs_f32() / SWITCH_TRAVEL_SECS).min(1.0);
        self.from + (self.target - self.from) * (1.0 - (1.0 - t).powi(3))
    }
}

impl RenderOnce for Switch {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let now = Instant::now();
        let target = if self.on { 1.0 } else { 0.0 };
        let reduced = cx.reduce_motion();
        let id = gpui::ElementId::Name(format!("switch-{}", self.key).into());
        let position = window.with_global_id(id, |id, window| {
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
        if (position - target).abs() > 0.001 {
            window.request_animation_frame();
        }
        let t = self.theme;
        let track: Hsla = if self.on {
            crate::theme::flatten(gpui::black().opacity(0.14), t.accent)
        } else {
            crate::theme::flatten(t.ink(0.18), t.shell)
        };
        let thumb = crate::theme::flatten(gpui::white().opacity(0.96), t.shell);
        let empty_width = SWITCH_WIDTH - SWITCH_THUMB_WIDTH - SWITCH_SIDE_INSET;
        let mark_padding = (empty_width - SWITCH_MARK_SIZE) / 2.0;
        let thumb_left = SWITCH_SIDE_INSET
            + (SWITCH_WIDTH - SWITCH_THUMB_WIDTH - 2.0 * SWITCH_SIDE_INSET) * position;
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .absolute()
                    .top(px((SWITCH_HEIGHT - SWITCH_TRACK_HEIGHT) / 2.0))
                    .left_0()
                    .w(px(SWITCH_WIDTH))
                    .h(px(SWITCH_TRACK_HEIGHT))
                    .rounded_full()
                    .bg(track)
                    .border_1()
                    .border_color(if self.on {
                        crate::theme::flatten(gpui::white().opacity(0.12), track)
                    } else {
                        crate::theme::flatten(t.border, track)
                    })
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .px(px(mark_padding))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .size(px(SWITCH_MARK_SIZE))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .opacity(position)
                                    .child(
                                        div()
                                            .w(px(1.2))
                                            .h(px(7.2))
                                            .rounded_full()
                                            .bg(gpui::white().opacity(0.96)),
                                    ),
                            )
                            .child(
                                div()
                                    .size(px(SWITCH_MARK_SIZE))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .opacity(1.0 - position)
                                    .child(
                                        div()
                                            .size(px(6.4))
                                            .rounded_full()
                                            .border(px(1.0))
                                            .border_color(gpui::white().opacity(0.92)),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px((SWITCH_HEIGHT - SWITCH_THUMB_HEIGHT) / 2.0))
                    .left(px(thumb_left))
                    .w(px(SWITCH_THUMB_WIDTH))
                    .h(px(SWITCH_THUMB_HEIGHT))
                    .rounded_full()
                    .bg(thumb)
                    .border_1()
                    .border_color(crate::theme::flatten(gpui::black().opacity(0.10), thumb)),
            )
    }
}
