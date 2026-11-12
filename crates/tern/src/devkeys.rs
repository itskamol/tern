//! Debug builds, and release builds made with the `dev-keys` feature (never shipped): `TERN_DEV_KEYS="cmd-k s t b"` replays keystrokes into the window after
//! start-up (`sleep-500` waits half a second, for a prompt to arrive; `click:x,y` and
//! `dblclick:x,y` click, `scroll:x,y,dy` scrolls, `type:text` types, `rclick:x,y` right-clicks and `drag:x0,y0,x1,y1` drags at window points),
//! start-up, so a scripted visual check can drive the app without taking keyboard focus from
//! whatever window the person at the machine is using.

use std::time::Duration;

use gpui::{
    App, AppContext, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PlatformInput, Point, WindowHandle, point, px,
};

use crate::shell::Shell;

pub fn replay(window: WindowHandle<Shell>, cx: &mut App) {
    let Ok(script) = std::env::var("TERN_DEV_KEYS") else {
        return;
    };
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(1500))
            .await;
        for source in script.split_whitespace() {
            if let Some(ms) = source.strip_prefix("sleep-").and_then(|n| n.parse().ok()) {
                cx.background_executor()
                    .timer(Duration::from_millis(ms))
                    .await;
                continue;
            }
            // `type:abc-1` types each character as itself; `-` cannot be a parsed key name.
            if let Some(text) = source.strip_prefix("type:") {
                for ch in text.chars() {
                    let key = ch.to_string();
                    let keystroke = Keystroke {
                        modifiers: Modifiers::default(),
                        key: key.clone(),
                        key_char: Some(key),
                    };
                    let _ = cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_keystroke(keystroke, cx)
                    });
                    cx.background_executor()
                        .timer(Duration::from_millis(40))
                        .await;
                }
                continue;
            }
            if let Some(events) = mouse(source) {
                for event in events {
                    let _ = cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(event, cx);
                    });
                    cx.background_executor()
                        .timer(Duration::from_millis(30))
                        .await;
                }
                continue;
            }
            let Ok(keystroke) = Keystroke::parse(source) else {
                tracing::warn!(key = source, "dev_key_unparsed");
                continue;
            };
            let handled = cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_keystroke(keystroke, cx)
            });
            tracing::debug!(key = source, ?handled, "dev_key");
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
        }
    })
    .detach();
}

fn at(x: f32, y: f32) -> Point<gpui::Pixels> {
    point(px(x), px(y))
}

fn down(p: Point<gpui::Pixels>, clicks: usize) -> PlatformInput {
    down_with(p, clicks, MouseButton::Left)
}

fn down_with(p: Point<gpui::Pixels>, clicks: usize, button: MouseButton) -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button,
        position: p,
        modifiers: Modifiers::default(),
        click_count: clicks,
        first_mouse: false,
    })
}

fn up(p: Point<gpui::Pixels>, clicks: usize) -> PlatformInput {
    up_with(p, clicks, MouseButton::Left)
}

fn up_with(p: Point<gpui::Pixels>, clicks: usize, button: MouseButton) -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button,
        position: p,
        modifiers: Modifiers::default(),
        click_count: clicks,
    })
}

fn moved(p: Point<gpui::Pixels>, pressed: bool) -> PlatformInput {
    PlatformInput::MouseMove(MouseMoveEvent {
        position: p,
        pressed_button: pressed.then_some(MouseButton::Left),
        modifiers: Modifiers::default(),
    })
}

/// The window events for a `click:`, `dblclick:` or `drag:` token.
fn mouse(token: &str) -> Option<Vec<PlatformInput>> {
    let (kind, args) = token.split_once(':')?;
    let n: Vec<f32> = args.split(',').filter_map(|v| v.parse().ok()).collect();
    match (kind, n.as_slice()) {
        ("rclick", [x, y]) => Some(vec![
            moved(at(*x, *y), false),
            down_with(at(*x, *y), 1, MouseButton::Right),
            up_with(at(*x, *y), 1, MouseButton::Right),
        ]),
        ("click", [x, y]) => Some(vec![
            moved(at(*x, *y), false),
            down(at(*x, *y), 1),
            up(at(*x, *y), 1),
        ]),
        ("dblclick", [x, y]) => Some(vec![
            moved(at(*x, *y), false),
            down(at(*x, *y), 1),
            up(at(*x, *y), 1),
            down(at(*x, *y), 2),
            up(at(*x, *y), 2),
        ]),
        ("scroll", [x, y, dy]) => Some(vec![
            moved(at(*x, *y), false),
            PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position: at(*x, *y),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(*dy))),
                modifiers: Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            }),
        ]),
        ("drag", [x0, y0, x1, y1]) => {
            let mut events = vec![moved(at(*x0, *y0), false), down(at(*x0, *y0), 1)];
            for step in 1..=8 {
                let f = step as f32 / 8.0;
                events.push(moved(at(x0 + (x1 - x0) * f, y0 + (y1 - y0) * f), true));
            }
            events.push(up(at(*x1, *y1), 1));
            Some(events)
        }
        _ => None,
    }
}
