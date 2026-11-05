#![allow(clippy::print_stderr, clippy::unwrap_used)]

//! What a frame costs with real CoreText shaping and no GPU, with the terminal as the app embeds
//! it (a cached child) and as it did before (a plain child):
//!
//! - streaming: the terminal changes every frame, as under `cat` of a big log;
//! - idle: the terminal does not change but its parent re-renders, as the shell does for a hover
//!   fade or a toast.
//!
//! The macOS text system must be made on the main thread, which a test cannot, hence an example:
//! `cargo +1.99.1 run -p tern-term --release --example frame_cost`; with `TERN_FRAME_STATS=1` it
//! also logs the per-second prepaint figures the app logs.

use std::time::Instant;

use gpui::prelude::*;
use gpui::{Context, Entity, IntoElement, Render, TestApp, TestAppWindow, Window, div};
use tern_term::{Terminal, TerminalTheme, TerminalView};

struct Host {
    view: Entity<TerminalView>,
    cached: bool,
    renders: usize,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let _timed = tern_term::frame_stats::start(tern_term::frame_stats::Span::ShellRender);
        self.renders += 1;
        div().size_full().child(div().size_full().when_else(
            self.cached,
            |slot| slot.child(TerminalView::cached_child(&self.view)),
            |slot| slot.child(self.view.clone()),
        ))
    }
}

fn log_line(i: usize) -> String {
    format!(
        "2026-11-05T12:00:{:02}Z \x1b[32mINFO\x1b[0m \x1b[38;2;120;180;240m/srv/app/handler.rs\x1b[0m request {i:08x} took {} us -> ok\r\n",
        i % 60,
        i * 7 % 9000
    )
}

fn renders(window: &TestAppWindow<Host>) -> usize {
    window.read(|host, _| host.renders)
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tern_term::frame_stats::init();
    let frames = 600;
    for cached in [false, true] {
        let platform = gpui_platform::current_platform(true);
        let mut app = TestApp::with_text_system(platform.text_system());
        let terminal = app.new_entity(|_| Terminal::new(80, 24));
        let mut window = app.open_window(|window, cx| {
            let view = cx.new(|cx| {
                TerminalView::new(terminal.clone(), TerminalTheme::default(), window, cx)
            });
            Host {
                view,
                cached,
                renders: 0,
            }
        });
        let mut next = 0;
        let mut stream = |app: &mut TestApp, lines: usize| {
            let chunk: String = (next..next + lines).map(log_line).collect();
            next += lines;
            app.update_entity(&terminal, |t, cx| t.feed(chunk.as_bytes(), cx));
        };
        stream(&mut app, 200);
        window.draw();
        let (cols, rows) = app.read_entity(&terminal, |t, _| (t.cols(), t.rows()));

        let before = renders(&window);
        let started = Instant::now();
        for _ in 0..frames {
            stream(&mut app, 30);
        }
        let elapsed = started.elapsed();
        let drawn = renders(&window) - before;
        eprintln!(
            "cached={cached} {cols}x{rows} streaming 30 lines a frame: {:.0} us/frame ({drawn} frames)",
            elapsed.as_secs_f64() * 1e6 / drawn as f64,
        );

        let before = renders(&window);
        let started = Instant::now();
        for _ in 0..frames {
            window.update(|_, _, cx| cx.notify());
        }
        let elapsed = started.elapsed();
        let drawn = renders(&window) - before;
        eprintln!(
            "cached={cached} {cols}x{rows} idle terminal, parent re-renders: {:.0} us/frame ({drawn} frames)",
            elapsed.as_secs_f64() * 1e6 / drawn as f64,
        );
    }
}
