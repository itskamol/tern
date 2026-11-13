//! The list of copies in progress, as the Files tab and the Files panel both show it: each
//! batch's label, progress and route, and a name clash answered in the batch's own row. The
//! rows are the shell's (`Transfers`); a click goes straight to them, so the list needs no view
//! of its own.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, px,
};
use tern_ssh::{Conflict, Question};

use crate::files_tab_parts::{action, tick_box};
use crate::files_text;
use crate::hover::Accessible as _;
use crate::theme::Theme;
use crate::transfers::{self, Row, Transfers};

/// The list's height when it is full: the Files tab's, so a long list never hides the files.
const MAX_HEIGHT: f32 = 180.0;

/// The rows, under a header with Cancel all; `None` when nothing is copying.
pub fn transfer_list(transfers: &Entity<Transfers>, t: Theme, cx: &App) -> Option<AnyElement> {
    let rows = transfers.read(cx).rows();
    if rows.is_empty() {
        return None;
    }
    let all = transfers.clone();
    let header = div()
        .flex_none()
        .px(px(12.))
        .h(px(26.))
        .flex()
        .items_center()
        .justify_between()
        .text_size(px(11.))
        .text_color(t.faint)
        .child("Transfers")
        .child(
            div()
                .id("transfers-cancel-all")
                .cursor_pointer()
                .text_color(t.muted)
                .hover(|s| s.text_color(t.text))
                .child("Cancel all")
                .on_click(move |_, _, cx| all.update(cx, |t, _| t.cancel_all())),
        );
    // A row waiting on an answer comes first, so it is never below the list's fold.
    let state = transfers.read(cx);
    let asking = rows
        .iter()
        .filter(|r| state.question_for(r.batch).is_some());
    let rest = rows
        .iter()
        .filter(|r| state.question_for(r.batch).is_none());
    let list = asking.chain(rest).fold(
        div()
            .id("transfers-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll(),
        |list, row| {
            let question = state.question_for(row.batch);
            list.child(row_view(transfers, row, question, t))
        },
    );
    Some(
        div()
            .flex_none()
            .max_h(px(MAX_HEIGHT))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(t.hairline)
            .child(header)
            .child(list)
            .into_any_element(),
    )
}

fn row_view(
    transfers: &Entity<Transfers>,
    row: &Row,
    question: Option<&Question>,
    t: Theme,
) -> AnyElement {
    let batch = row.batch;
    let detail = match (&row.progress, question) {
        (_, Some(_)) => "Waiting for an answer".to_owned(),
        (Some(p), None) => transfers::detail(p),
        (None, None) => "Waiting\u{2026}".to_owned(),
    };
    let pct = row
        .progress
        .as_ref()
        .map_or(0, |p| files_text::percent(p.done, p.total));
    let cancel = transfers.clone();
    let text = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .truncate()
                .child(SharedString::from(row.label.clone())),
        )
        .child(
            div().h(px(3.)).rounded(px(2.)).bg(t.row_active).child(
                div()
                    .h_full()
                    .rounded(px(2.))
                    .bg(t.accent)
                    .w(gpui::relative(pct as f32 / 100.0)),
            ),
        )
        .child(
            div()
                .truncate()
                .text_size(px(11.))
                .text_color(t.faint)
                .child(SharedString::from(detail)),
        )
        .child(
            div()
                .id(("transfer-route", batch as usize))
                .truncate()
                .text_size(px(11.))
                .text_color(t.faint)
                .hint(row.route.clone(), &t)
                .child(SharedString::from(row.route.clone())),
        );
    let top = div().flex().items_center().gap(px(10.)).child(text).child(
        action(("transfer-cancel", batch as usize), "Cancel", false, &t)
            .on_click(move |_, _, cx| cancel.update(cx, |t, _| t.cancel(batch))),
    );
    div()
        .px(px(12.))
        .py(px(4.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .text_size(px(12.))
        .child(top)
        .when_some(question, |el, q| el.child(clash(transfers, row, q, t)))
        .into_any_element()
}

/// A name that is taken, asked in the row: Skip, Keep both (its hover names the copy's new
/// name) or Replace, and "for the rest" when more of the batch could clash.
fn clash(transfers: &Entity<Transfers>, row: &Row, q: &Question, t: Theme) -> AnyElement {
    let batch = row.batch;
    let choose = |id: &'static str, text: &'static str, choice: Conflict, primary: bool| {
        let transfers = transfers.clone();
        action((id, batch as usize), text, primary, &t)
            .on_click(move |_, _, cx| transfers.update(cx, |t, cx| t.answer(batch, choice, cx)))
    };
    let what = if q.is_dir { "folder" } else { "file" };
    let buttons = div()
        .flex()
        .flex_wrap()
        .gap(px(6.))
        .child(choose("transfer-skip", "Skip", Conflict::Skip, false))
        .child(
            choose("transfer-keep-both", "Keep both", Conflict::KeepBoth, false).hint(
                format!("Saves the copy as \u{201c}{}\u{201d}", q.keep_both),
                &t,
            ),
        )
        .child(choose(
            "transfer-replace",
            "Replace",
            Conflict::Replace,
            true,
        ));
    let rest = row.many.then(|| {
        let (transfers, on) = (transfers.clone(), row.rest);
        div()
            .id(("transfer-rest", batch as usize))
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(t.muted)
            .switch("For the rest", on)
            .on_click(move |_, _, cx| transfers.update(cx, |t, cx| t.toggle_rest(batch, cx)))
            .child(tick_box(on, 12., &t))
            .child("for the rest")
    });
    div()
        .p(px(8.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .rounded(px(7.))
        .bg(t.warning.opacity(0.08))
        .child(div().text_color(t.text).child(SharedString::from(format!(
            "A {what} named \u{201c}{}\u{201d} is already there",
            q.name
        ))))
        .child(buttons)
        .children(rest)
        .into_any_element()
}
