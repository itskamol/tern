//! The sample terminal on Settings → Appearance and Terminal: the real terminal view over a
//! few canned lines, so a font, colour or cursor change shows at once. The entity exists only
//! while one of those pages is open. It is display-only: no session, no focus, no input.

use gpui::{AppContext as _, Context, IntoElement, ParentElement, Styled, Window, div, px};
use tern_term::{Terminal, TerminalView};

use super::Shell;
use super::settings_ui::Section;
use crate::settings_widgets as w;

/// Rows shown: a prompt with a command, three listing lines and a prompt holding the cursor.
const SAMPLE_ROWS: u16 = 5;
/// Starting width; the view re-measures to the real one on its first frame.
const SAMPLE_COLS: u16 = 60;
/// A prompt and an `ls -l` in ANSI colours (blue directory, green executable, plain file),
/// ending at an empty prompt so the cursor shows.
const SAMPLE: &[u8] = b"\x1b[32muser@tern\x1b[0m:\x1b[34m~/src\x1b[0m$ ls -l\r\n\
\x1b[1;34mdrwxr-xr-x\x1b[0m  docs\r\n\
\x1b[1;32m-rwxr-xr-x\x1b[0m  deploy.sh\r\n\
-rw-r--r--  README.md\r\n\
\x1b[32muser@tern\x1b[0m:\x1b[34m~/src\x1b[0m$ ";

impl Shell {
    /// The sample sits on the Settings tile, not over the wallpaper, so its background is solid.
    fn sample_theme(&self) -> tern_term::TerminalTheme {
        let mut theme = self.terminal_theme("");
        theme.background_alpha = 1.0;
        theme
    }

    /// Creates the sample when a page that shows it is open and drops it when none is, so
    /// nothing is held while Settings is closed.
    pub(super) fn sync_sample(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = matches!(
            self.settings_page,
            Some(Section::Appearance | Section::Terminal)
        );
        if wanted == self.sample.is_some() {
            return;
        }
        self.sample = wanted.then(|| {
            let theme = self.sample_theme();
            let options = self.terminal_options();
            let terminal = cx.new(|_| Terminal::with_content(SAMPLE_COLS, SAMPLE_ROWS, SAMPLE));
            cx.new(|cx| {
                let mut view = TerminalView::display_only(terminal, theme, window, cx);
                view.set_options(options, cx);
                view
            })
        });
    }

    /// Carries the current font, colours and cursor settings to the sample.
    pub(super) fn restyle_sample(&self, cx: &mut Context<Self>) {
        let Some(view) = self.sample.clone() else {
            return;
        };
        let theme = self.sample_theme();
        let options = self.terminal_options();
        view.update(cx, |v, cx| {
            v.set_theme(theme, cx);
            v.set_options(options, cx);
        });
    }

    /// The "Preview" block of a page. Its card is the settings card's radius (12, zeron) and
    /// its height is [`SAMPLE_ROWS`] terminal rows at the current line height.
    pub(super) fn sample_terminal(&self) -> impl IntoElement + use<> {
        let height = TerminalView::height_for_rows(&self.sample_theme(), SAMPLE_ROWS);
        let card = div()
            .rounded(px(12.))
            .overflow_hidden()
            .h(px(height))
            .children(self.sample.clone());
        w::section(&self.theme, "Preview", card)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tern_term::CellColor;

    fn sample() -> Terminal {
        Terminal::with_content(SAMPLE_COLS, SAMPLE_ROWS, SAMPLE)
    }

    #[test]
    fn canned_text_fills_every_row_and_leaves_the_cursor_on_the_prompt() {
        let t = sample();
        assert_eq!(t.row_text(0), "user@tern:~/src$ ls -l");
        assert_eq!(t.row_text(1), "drwxr-xr-x  docs");
        assert_eq!(t.row_text(2), "-rwxr-xr-x  deploy.sh");
        assert_eq!(t.row_text(3), "-rw-r--r--  README.md");
        assert_eq!(t.row_text(4), "user@tern:~/src$");
        let cursor = t.cursor().unwrap();
        assert_eq!(cursor.row, usize::from(SAMPLE_ROWS) - 1);
        assert_eq!(cursor.col, "user@tern:~/src$ ".len());
        assert_eq!(t.history_size(), 0);
    }

    #[test]
    fn listing_uses_ansi_colours_the_scheme_can_restyle() {
        let t = sample();
        assert_eq!(t.line(1)[0].fg, CellColor::Indexed(4));
        assert_eq!(t.line(2)[0].fg, CellColor::Indexed(2));
        assert_eq!(t.line(3)[0].fg, CellColor::Foreground);
    }
}
