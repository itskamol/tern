//! A view that only draws, for a page that shows a terminal without one: the Settings
//! sample. It shares everything but input with a session's view.

use gpui::{Context, Entity, Window};

use super::{TERM_PADDING, TerminalView};
use crate::{Terminal, TerminalTheme};

impl TerminalView {
    /// A view that only draws: it takes no focus, ignores keys and the pointer, and writes
    /// nothing, so a canned `Terminal` can sit on a page (the Settings sample).
    pub fn display_only(
        terminal: Entity<Terminal>,
        theme: TerminalTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(terminal, theme, true, window, cx)
    }

    /// Height of a view showing `rows` rows in `theme`: the rows at the theme's line height
    /// plus the padding the view keeps on both sides.
    pub fn height_for_rows(theme: &TerminalTheme, rows: u16) -> f32 {
        let line_h = (theme.font_size * theme.line_height_ratio).round();
        f32::from(rows) * line_h + 2.0 * TERM_PADDING
    }
}
