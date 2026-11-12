//! GPUI terminal view driven by an arbitrary byte stream (an SSH channel; there
//! is no local PTY). `Terminal` is the model, `TerminalView` the interactive
//! view, `TerminalTheme` the palette.

mod box_drawing;
mod config;
mod element;
pub mod frame_stats;
mod links;
mod mappings;
mod search;
mod terminal;
mod view;

pub use config::TerminalTheme;
pub use config::{CursorStyleSetting, TerminalOptions};
pub use links::Link;
pub use search::SearchMark;
pub use terminal::{
    CellColor, CellSnapshot, CursorSnapshot, GridPos, PromptMark, SCROLLBACK_LINES, ShellFolder,
    Terminal, TerminalEvent,
};
pub use view::{TerminalView, paste_bytes};
